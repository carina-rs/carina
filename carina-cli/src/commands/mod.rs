pub mod apply;
pub mod destroy;
pub mod docs;
pub mod export;
pub mod fmt;
pub(crate) mod iam_preflight;
pub mod init;
pub mod lint;
pub mod migrate_state;
pub mod module;
pub mod plan;
pub(crate) mod shared;
pub mod skills;
pub mod state;
pub mod validate;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use carina_core::module_resolver;
use carina_core::parser::{BackendConfig, ProviderContext};
use carina_core::resource::{ConcreteValue, Value};
use carina_core::upstream_exports::UpstreamRefDiagnostic;
use carina_state::{
    BackendConfig as StateBackendConfig, BackendError, BackendLock, StateBackend,
    resolve_backend_anchored,
};

use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, compute_anonymous_identifiers_with_ctx,
    resolve_names_with_ctx, validate_attribute_param_ref_types_with_ctx,
    validate_deferred_populate_refs_with_ctx, validate_depends_on_with_ctx,
    validate_module_attribute_param_types, validate_module_calls, validate_no_empty_interpolations,
    validate_provider_region_with_ctx, validate_resource_ref_types_with_ctx,
    validate_resources_with_ctx, validate_wait_bindings_with_ctx,
};

#[must_use = "Drifted must be handled before mutating state — apply/destroy must refuse, init/plan must warn"]
#[derive(Debug, PartialEq)]
pub enum BackendDriftStatus {
    Fresh,
    Unchanged,
    Drifted {
        existing: BackendLock,
        configured: BackendLock,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftCommand {
    Apply,
    Destroy,
    RefreshState,
}

impl DriftCommand {
    fn verb_phrase(self) -> &'static str {
        match self {
            Self::Apply => "Cannot apply",
            Self::Destroy => "Cannot destroy",
            Self::RefreshState => "Cannot refresh state",
        }
    }
}

pub fn inspect_backend_drift(
    base_dir: &Path,
    backend_config: Option<&BackendConfig>,
) -> Result<BackendDriftStatus, AppError> {
    let configured = BackendLock::for_config(backend_config)?;
    let existing = BackendLock::load(base_dir).map_err(AppError::Backend)?;

    Ok(match existing {
        None => BackendDriftStatus::Fresh,
        Some(existing) if existing == configured => BackendDriftStatus::Unchanged,
        Some(existing) => BackendDriftStatus::Drifted {
            existing,
            configured,
        },
    })
}

#[must_use = "VerifiedBackend proves Fresh and Drifted were rejected before mutating state"]
#[derive(Debug, Clone)]
pub struct VerifiedBackend {
    parser_config: Option<BackendConfig>,
    state_config: Option<StateBackendConfig>,
    base_dir: PathBuf,
}

impl VerifiedBackend {
    pub async fn resolve(&self) -> Result<Box<dyn StateBackend>, BackendError> {
        resolve_backend_anchored(self.state_config.as_ref(), &self.base_dir).await
    }

    pub fn is_configured(&self) -> bool {
        self.parser_config.is_some()
    }

    pub fn string_attribute(&self, key: &str) -> Option<&str> {
        self.parser_config.as_ref().and_then(|config| {
            config.attributes.get(key).and_then(|value| match value {
                Value::Concrete(ConcreteValue::String(value)) => Some(value.as_str()),
                _ => None,
            })
        })
    }

    pub fn bool_attribute(&self, key: &str) -> Option<bool> {
        self.parser_config.as_ref().and_then(|config| {
            config.attributes.get(key).and_then(|value| match value {
                Value::Concrete(ConcreteValue::Bool(value)) => Some(*value),
                _ => None,
            })
        })
    }
}

pub fn verify_for_mutation(
    base_dir: &Path,
    backend_config: Option<&BackendConfig>,
    command: DriftCommand,
) -> Result<VerifiedBackend, AppError> {
    let verified_base_dir = base_dir
        .canonicalize()
        .unwrap_or_else(|_| base_dir.to_path_buf());

    match inspect_backend_drift(&verified_base_dir, backend_config)? {
        BackendDriftStatus::Fresh => Err(AppError::Config(
            "Backend lock file not found. Run 'carina init' to initialize the project.".to_string(),
        )),
        BackendDriftStatus::Unchanged => Ok(VerifiedBackend {
            parser_config: backend_config.cloned(),
            state_config: backend_config.map(StateBackendConfig::from),
            base_dir: verified_base_dir,
        }),
        BackendDriftStatus::Drifted {
            existing,
            configured,
        } => Err(AppError::Config(drift_error_message(
            command,
            &existing,
            &configured,
        ))),
    }
}

fn backend_drift_header(existing: &BackendLock, configured: &BackendLock) -> String {
    format!(
        "Backend configuration changed since the last state migration:\n{}",
        existing.describe_diff(configured)
    )
}

pub fn drift_warning(existing: &BackendLock, configured: &BackendLock) -> String {
    format!(
        "{}\n\n    plan reads state from the OLD backend recorded in carina-backend.lock.\n    \
         Before running apply or destroy, run `carina init --migrate-state .`\n    \
         to migrate state from the OLD backend to the new one.\n\n    \
         To revert instead, restore the backend block to match the lock.",
        backend_drift_header(existing, configured)
    )
}

pub fn drift_error_message(
    command: DriftCommand,
    existing: &BackendLock,
    configured: &BackendLock,
) -> String {
    format!(
        "{}\n\n{} without first migrating the state. State migration is an\n\
         explicit, named operation:\n\n    carina init --migrate-state .\n\n\
         Or revert the `backend` block to match the lock if the change was unintended.",
        backend_drift_header(existing, configured),
        command.verb_phrase()
    )
}

/// Error message for a provider block that declares no `source` attribute.
/// Shared between `init` (pre-resolution check) and validation (post-load check)
/// so both surfaces report the same text.
pub fn missing_provider_source_message(name: &str) -> String {
    format!(
        "Provider '{}' has no source configured. Add `source = 'github.com/...'` to the provider block.",
        name
    )
}

/// Save the backend lock file for the current configuration.
/// Called after state is successfully written to ensure the lock
/// exists for future backend-change detection.
pub fn ensure_backend_lock(
    base_dir: &Path,
    backend_config: Option<&BackendConfig>,
) -> Result<(), AppError> {
    let lock_path = BackendLock::lock_path(base_dir);
    if lock_path.exists() {
        return Ok(());
    }
    BackendLock::for_config(backend_config)?
        .save(base_dir)
        .map_err(AppError::Backend)
}

/// Run the common validation and module resolution pipeline.
///
/// Steps:
/// 1. Validate provider region
/// 2. Validate module call arguments (before expansion)
/// 3. Resolve module imports and expand module calls
/// 4. Resolve names (let bindings -> resource names)
/// 5. Validate resources (schema checks) -- skipped when `skip_resource_validation` is true
/// 6. Validate resource ref types -- skipped when `skip_resource_validation` is true
/// 7. Compute anonymous identifiers
///
/// `skip_resource_validation` is used by destroy and state refresh, which only need
/// name resolution and identifier computation without full schema validation.
#[allow(dead_code)] // Used by snapshot tests
pub fn validate_and_resolve(
    parsed: &mut carina_core::parser::InferredFile,
    base_dir: &Path,
    skip_resource_validation: bool,
) -> Result<(), AppError> {
    validate_and_resolve_with_config(parsed, base_dir, skip_resource_validation)
}

/// Create a `ProviderContext` with custom type validators extracted from
/// the already-collected schema map and factory-based validation for WASM providers.
fn enrich_provider_context(
    schemas: &carina_core::schema::SchemaRegistry,
    factories: Arc<Vec<Box<dyn carina_core::provider::ProviderFactory>>>,
) -> ProviderContext {
    ProviderContext {
        decryptor: None,
        validators: carina_core::provider::collect_custom_type_validators(schemas),
        custom_type_validator: Some(Box::new(
            move |identity: &carina_core::schema::TypeIdentity, value: &str| {
                for factory in factories.iter() {
                    factory.validate_custom_type(identity, value)?;
                }
                Ok(())
            },
        )),
        resource_types: ProviderContext::resource_types_from_schema_registry(schemas),
        // carina#3239: schemas are loaded at this point, so the strict
        // "unknown custom type in type position" parser check applies.
        customs_loaded: true,
    }
}

/// Collapse an accumulated error Vec into a single `AppError`.
///
/// Panics on empty input — callers must guard with `is_empty()` first.
/// A single accumulated error passes through unchanged so variants like
/// `AppError::Config` reach callers (e.g. `run_validate`) with their
/// original kind; multiple errors are joined as one
/// `AppError::Validation` for the existing combined-message surface.
pub(crate) fn collapse_errors(errors: Vec<AppError>) -> AppError {
    if errors.len() == 1 {
        return errors.into_iter().next().unwrap();
    }
    AppError::Validation(
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

pub fn validate_and_resolve_with_config(
    parsed: &mut carina_core::parser::InferredFile,
    base_dir: &Path,
    skip_resource_validation: bool,
) -> Result<(), AppError> {
    let errors = validate_and_resolve_errors(parsed, base_dir, skip_resource_validation);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(collapse_errors(errors))
    }
}

/// Vec-returning twin of [`validate_and_resolve_with_config`]. `run_validate`
/// uses it to fold findings into its own accumulator; the `Result`-returning
/// wrapper above flushes through `collapse_errors` for the rest of the CLI.
pub fn validate_and_resolve_errors(
    parsed: &mut carina_core::parser::InferredFile,
    base_dir: &Path,
    skip_resource_validation: bool,
) -> Vec<AppError> {
    let (factories, load_errors) = build_factories_from_providers(&parsed.providers, base_dir);
    validate_and_resolve_errors_with_factories(
        parsed,
        base_dir,
        skip_resource_validation,
        factories,
        load_errors,
    )
}

/// Factory-injected variant of [`validate_and_resolve_errors`]. The CLI
/// validation pipeline runs against caller-provided
/// [`Box<dyn ProviderFactory>`] instances rather than building them from
/// the parsed file's `provider` blocks.
///
/// Exposed so e2e tests can drive the full pipeline (parse → resolve →
/// validate) against hand-built schemas without standing up a WASM
/// provider plugin (#2247). The production path goes through the
/// non-injecting wrapper above; this entry point is not used outside
/// tests.
pub fn validate_and_resolve_errors_with_factories(
    parsed: &mut carina_core::parser::InferredFile,
    base_dir: &Path,
    skip_resource_validation: bool,
    factories: Vec<Box<dyn carina_core::provider::ProviderFactory>>,
    load_errors: HashMap<String, String>,
) -> Vec<AppError> {
    let ctx = WiringContext::new(factories);

    let mut errors: Vec<AppError> = Vec::new();

    // `arguments` is a module-input declaration; the CLI only ever feeds
    // root configurations into this function (modules go through
    // `module_resolver::load_module`), so any `arguments` block reaching
    // here is misplaced (#2198).
    if let Err(msg) = carina_core::validation::validate_no_arguments_in_root(parsed) {
        errors.push(AppError::Validation(msg));
    }

    // Check for declared providers whose plugins failed to load.
    // Named instances (`!is_default`) deliberately omit `source` —
    // the parser enforces that `source` is a kind-level property
    // (carina#3023). They inherit the kind default's plugin, so
    // they only matter to this check when the *kind default* could
    // not be loaded, which is already reported via the
    // `factories().iter().any(...)` lookup on the kind's name.
    if !skip_resource_validation {
        for provider in &parsed.providers {
            let loaded = ctx.factories().iter().any(|f| f.name() == provider.name);
            if loaded {
                continue;
            }
            if let Some(reason) = load_errors.get(&provider.name) {
                errors.push(AppError::Validation(reason.clone()));
            } else if provider.is_default && provider.source.is_none() && provider.name != "mock" {
                errors.push(AppError::Validation(missing_provider_source_message(
                    &provider.name,
                )));
            }
        }
    }

    // Validate provider region
    errors.extend(validate_provider_region_with_ctx(&ctx, parsed));

    // Enrich provider context with custom type validators from loaded schemas
    let enriched_context = enrich_provider_context(ctx.schemas(), ctx.factories_arc());

    // carina#3239: reject `arguments { foo: TotallyMadeUpType }`-shaped
    // unknown bare custom-type names in the root parse. The root
    // config was parsed up in `load_configuration_with_config` with
    // the bootstrap context (`customs_loaded = false`) — schemas had
    // not been collected yet, so the parser-side `customs_loaded` gate
    // could not fire there. This post-parse walk re-applies the same
    // predicate against the now-enriched context, closing the
    // standalone-module-validate path so a typo or renamed-then-
    // removed type cannot ride into apply. Imported modules
    // re-parsed below by `resolve_modules_with_config` see the
    // enriched context directly and the parser gate covers them.
    for finding in carina_core::validation::resolve_file_type_exprs(parsed, &enriched_context) {
        errors.push(AppError::Validation(finding));
    }
    for finding in
        carina_core::validation::validate_argument_custom_types(parsed, &enriched_context)
    {
        errors.push(AppError::Validation(finding));
    }

    // Validate module call arguments before expansion (needs enriched
    // context for custom type validators)
    errors.extend(validate_module_calls(parsed, base_dir, &enriched_context));

    // Validate module attribute parameter ref types before expansion
    if !skip_resource_validation {
        errors.extend(validate_module_attribute_param_types(
            &ctx, parsed, base_dir,
        ));
    }

    // Module expansion assumes the checks above succeeded — feeding
    // broken module calls into `resolve_modules_with_config` can
    // surface confusing secondary errors, so gate it here.
    if !errors.is_empty() {
        return errors;
    }

    if let Err(e) =
        module_resolver::resolve_modules_with_config(parsed, base_dir, &enriched_context)
    {
        errors.push(AppError::Config(format!("Module resolution error: {}", e)));
        return errors;
    }

    // Module expansion produces composition resources for module-call
    // bindings (`carina-core/src/module_resolver/expander.rs`). Resolve
    // deferred provider attributes that reference those bindings
    // (`default_tags = mod.tags`), then finalize so the resolved values
    // are promoted into the typed `default_tags` field. See #2717.
    if let Err(e) =
        carina_core::parser::resolve_provider_unresolved_attributes(parsed, &enriched_context)
    {
        errors.push(AppError::Config(format!(
            "Provider attribute resolution error: {}",
            e
        )));
        return errors;
    }
    if let Err(e) = carina_core::parser::finalize_provider_configs(parsed) {
        errors.push(AppError::Config(format!("Finalize error: {}", e)));
        return errors;
    }

    // Resolve names (let bindings -> resource names) — must succeed
    // before per-resource schema checks can look up the renamed
    // attributes, so its failures gate the remaining pipeline.
    errors.extend(resolve_names_with_ctx(&ctx, &mut parsed.resources));
    if !errors.is_empty() {
        return errors;
    }

    if !skip_resource_validation {
        // Mid-edit `${}` is parser-accepted (#2480) so the AST stays
        // intact for LSP diagnostics, but it must not reach a provider —
        // surface it as a hard error before the schema-level checks
        // below try to type-check around the marker. See #2487.
        errors.extend(validate_no_empty_interpolations(parsed));

        errors.extend(validate_resources_with_ctx(&ctx, parsed, &enriched_context));
        errors.extend(validate_depends_on_with_ctx(parsed));
        errors.extend(validate_wait_bindings_with_ctx(&ctx, parsed));
        errors.extend(validate_deferred_populate_refs_with_ctx(&ctx, parsed));
        let mut argument_names: HashSet<String> =
            parsed.arguments.iter().map(|a| a.name.clone()).collect();
        // Upstream state bindings are resolved at plan time, skip type validation
        for us in &parsed.upstream_states {
            argument_names.insert(us.binding.clone());
        }
        errors.extend(validate_resource_ref_types_with_ctx(
            &ctx,
            parsed,
            &argument_names,
        ));
        errors.extend(validate_attribute_param_ref_types_with_ctx(
            &ctx,
            &parsed.attribute_params,
            &parsed.resources,
        ));
        if !errors.is_empty() {
            return errors;
        }
    }

    // Validate export values against their type annotations
    if !skip_resource_validation {
        if let Err(msg) = carina_core::validation::validate_export_params(
            &parsed.export_params,
            &enriched_context,
        ) {
            errors.extend(split_validation_message(&msg));
        }
        let export_bindings =
            carina_core::binding_index::BindingIndex::from_parsed(parsed, ctx.schemas());
        if let Err(msg) = carina_core::validation::validate_export_param_ref_types_with_bindings(
            &parsed.export_params,
            &export_bindings,
        ) {
            errors.extend(split_validation_message(&msg));
        }
    }

    let canonical_resources = carina_core::value::canonicalize_resources_with_schemas(
        &mut parsed.resources,
        ctx.schemas(),
    );

    // Compute anonymous identifiers — downstream plan code assumes
    // every resource has a stable id, so a collision error must stop
    // the pipeline here.
    errors.extend(compute_anonymous_identifiers_with_ctx(
        &ctx,
        canonical_resources,
        &parsed.providers,
    ));
    if !errors.is_empty() {
        return errors;
    }

    // Reject references to `upstream_state` fields that the upstream's
    // `exports { }` block does not declare. Gated like the other
    // validations above so `destroy` / `state` (which pass
    // `skip_resource_validation = true`) can still run when an upstream
    // has drifted — those commands are the recovery path for exactly
    // that situation. No state I/O: we parse the upstream's `.crn`
    // files directly.
    if !skip_resource_validation {
        let (upstream_exports, resolve_errors) =
            carina_core::upstream_exports::resolve_upstream_exports_with_schemas(
                base_dir,
                &parsed.upstream_states,
                &enriched_context,
                Some(ctx.schemas()),
            );
        let field_errors = carina_core::upstream_exports::check_upstream_state_field_references(
            parsed,
            &upstream_exports,
        );
        // Phase 2 of #1992: names are known to exist; now check each
        // reference's declared export type against the consuming
        // attribute's expected type. Skipped when the export has no
        // `: T` annotation (nothing to compare) — see
        // `check_upstream_state_field_types` for the details.
        let type_errors = carina_core::upstream_exports::check_upstream_state_field_types(
            parsed,
            &upstream_exports,
            ctx.schemas(),
        );
        // #1894 (option 2): cross-directory `for`-iterable shape check.
        // Surfaces pending `list ↔ map` migrations in the upstream's
        // `exports.crn` before they hit apply.
        let shape_errors = carina_core::upstream_exports::check_upstream_state_for_iterable_shapes(
            parsed,
            &upstream_exports,
        );
        // #1894 follow-up: cross-directory attribute-access shape check.
        // Catches `orgs.account.bad_field` / `orgs.list_field.foo` where
        // the downstream's `.field` chain doesn't fit the upstream's
        // declared `TypeExpr`.
        let attribute_access_errors =
            carina_core::upstream_exports::check_upstream_state_attribute_access_shapes(
                parsed,
                &upstream_exports,
            );
        let subscript_errors = carina_core::upstream_exports::check_upstream_state_subscript_shapes(
            parsed,
            &upstream_exports,
        );
        errors.extend(
            resolve_errors
                .iter()
                .map(|e| AppError::Validation(e.to_string())),
        );
        // The five upstream-ref checks return distinct concrete types
        // but share `UpstreamRefDiagnostic`. Chain them through the
        // trait (`Display` supertrait gives the canonical
        // `"location: message"` form — the per-type `Display` impl is
        // the single source of truth) so adding a sixth check is one
        // extra `chain(...)`.
        errors.extend(
            field_errors
                .iter()
                .map(|e| e as &dyn UpstreamRefDiagnostic)
                .chain(type_errors.iter().map(|e| e as &dyn UpstreamRefDiagnostic))
                .chain(shape_errors.iter().map(|e| e as &dyn UpstreamRefDiagnostic))
                .chain(
                    attribute_access_errors
                        .iter()
                        .map(|e| e as &dyn UpstreamRefDiagnostic),
                )
                .chain(
                    subscript_errors
                        .iter()
                        .map(|e| e as &dyn UpstreamRefDiagnostic),
                )
                .map(|e| AppError::Validation(e.to_string())),
        );
    }

    errors
}

/// Split the newline-joined error strings that `carina_core::validation::*`
/// helpers return into individual `AppError::Validation` entries. Mirrors
/// the `lift_validation_result` helper in `wiring.rs` for the two
/// `validate_export_*` core functions that are still called inline here.
fn split_validation_message(joined: &str) -> Vec<AppError> {
    joined
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| AppError::Validation(s.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::parser::ProviderConfig;
    use carina_core::resource::{ConcreteValue, Value};
    use indexmap::IndexMap;
    use std::collections::HashMap;

    fn s3_backend_config(bucket: &str, region: &str) -> BackendConfig {
        let mut attributes = HashMap::new();
        attributes.insert(
            "bucket".to_string(),
            Value::Concrete(ConcreteValue::String(bucket.to_string())),
        );
        attributes.insert(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String(region.to_string())),
        );
        BackendConfig {
            backend_type: "s3".to_string(),
            attributes,
        }
    }

    fn local_backend_config(path: &str) -> BackendConfig {
        let mut attributes = HashMap::new();
        attributes.insert(
            "path".to_string(),
            Value::Concrete(ConcreteValue::String(path.to_string())),
        );
        BackendConfig {
            backend_type: "local".to_string(),
            attributes,
        }
    }

    mod inspect_backend_drift {
        use super::*;

        #[test]
        fn inspect_returns_fresh_when_no_lock() {
            let tmp = tempfile::tempdir().unwrap();
            let config = s3_backend_config("my-bucket", "us-east-1");

            let status = crate::commands::inspect_backend_drift(tmp.path(), Some(&config)).unwrap();

            assert_eq!(status, BackendDriftStatus::Fresh);
        }

        #[test]
        fn inspect_returns_unchanged_when_config_matches_lock() {
            let tmp = tempfile::tempdir().unwrap();
            let config = s3_backend_config("my-bucket", "us-east-1");
            ensure_backend_lock(tmp.path(), Some(&config)).unwrap();

            let status = crate::commands::inspect_backend_drift(tmp.path(), Some(&config)).unwrap();

            assert_eq!(status, BackendDriftStatus::Unchanged);
        }

        #[test]
        fn inspect_returns_drifted_when_config_differs() {
            let tmp = tempfile::tempdir().unwrap();
            let old = s3_backend_config("old-bucket", "us-east-1");
            let new = s3_backend_config("new-bucket", "us-east-1");
            ensure_backend_lock(tmp.path(), Some(&old)).unwrap();

            let status = crate::commands::inspect_backend_drift(tmp.path(), Some(&new)).unwrap();
            let existing = BackendLock::for_config(Some(&old)).unwrap();
            let configured = BackendLock::for_config(Some(&new)).unwrap();

            assert_eq!(
                status,
                BackendDriftStatus::Drifted {
                    existing,
                    configured,
                }
            );
        }

        #[test]
        fn inspect_returns_fresh_with_no_backend_and_no_lock() {
            let tmp = tempfile::tempdir().unwrap();

            let status = crate::commands::inspect_backend_drift(tmp.path(), None).unwrap();

            assert_eq!(status, BackendDriftStatus::Fresh);
        }

        #[test]
        fn inspect_returns_drifted_when_lock_is_remote_and_config_is_local() {
            let tmp = tempfile::tempdir().unwrap();
            let remote = s3_backend_config("my-bucket", "us-east-1");
            ensure_backend_lock(tmp.path(), Some(&remote)).unwrap();

            let status = crate::commands::inspect_backend_drift(tmp.path(), None).unwrap();
            let existing = BackendLock::for_config(Some(&remote)).unwrap();
            let configured = BackendLock::for_config(None).unwrap();

            assert_eq!(
                status,
                BackendDriftStatus::Drifted {
                    existing,
                    configured,
                }
            );
        }

        #[test]
        fn drift_warning_names_old_and_new() {
            let old =
                BackendLock::for_config(Some(&local_backend_config("legacy/state.json"))).unwrap();
            let new = BackendLock::for_config(Some(&local_backend_config("state.json"))).unwrap();

            let warning = drift_warning(&old, &new);

            assert!(warning.contains("legacy/state.json"));
            assert!(warning.contains("state.json"));
            assert!(warning.contains("carina init --migrate-state"));
        }

        #[test]
        fn drift_error_names_apply_blocker() {
            let old =
                BackendLock::for_config(Some(&local_backend_config("legacy/state.json"))).unwrap();
            let new = BackendLock::for_config(Some(&local_backend_config("state.json"))).unwrap();

            let error = drift_error_message(DriftCommand::Apply, &old, &new);

            assert!(error.contains("carina init --migrate-state"));
            assert!(error.contains("Cannot apply without first migrating the state"));
        }

        #[test]
        fn drift_error_names_refresh_blocker() {
            let old =
                BackendLock::for_config(Some(&local_backend_config("legacy/state.json"))).unwrap();
            let new = BackendLock::for_config(Some(&local_backend_config("state.json"))).unwrap();

            let error = drift_error_message(DriftCommand::RefreshState, &old, &new);

            assert!(error.contains("carina init --migrate-state"));
            assert!(error.contains("Cannot refresh state without first migrating the state"));
            assert!(!error.contains("Cannot apply without first migrating the state"));
        }
    }

    #[test]
    fn ensure_backend_lock_creates_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let config = s3_backend_config("my-bucket", "us-east-1");
        ensure_backend_lock(tmp.path(), Some(&config)).unwrap();
        assert!(tmp.path().join("carina-backend.lock").exists());
    }

    fn empty_parsed_file() -> carina_core::parser::InferredFile {
        carina_core::parser::InferredFile::default()
    }

    #[test]
    fn test_provider_load_error_shows_actual_reason() {
        // When a provider with source fails to load, the error message should
        // contain the actual failure reason, NOT "Run `carina init`".
        // Use an unsupported source format to trigger a load failure without needing Tokio.
        let mut parsed = empty_parsed_file();
        parsed.providers.push(ProviderConfig {
            name: "fakeprovider".to_string(),
            source: Some("badscheme://not-a-valid-source".to_string()),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
        });
        let base_dir = std::path::Path::new("/tmp/nonexistent-carina-test");
        let result = validate_and_resolve_with_config(&mut parsed, base_dir, false);

        let err = result.unwrap_err();
        let msg = err.to_string();
        // Should NOT suggest "carina init" when the actual problem is a load failure
        assert!(
            !msg.contains("Run `carina init`"),
            "Error should not suggest 'carina init' when the actual failure is known. Got: {}",
            msg
        );
        // Should contain the actual failure reason from build_factories_from_providers
        assert!(
            msg.contains("Unsupported source format for provider 'fakeprovider'"),
            "Error should show actual failure reason. Got: {}",
            msg
        );
    }

    #[test]
    fn test_provider_without_source_shows_source_hint() {
        // A provider without source should tell the user to add source config.
        let mut parsed = empty_parsed_file();
        parsed.providers.push(ProviderConfig {
            name: "awscc".to_string(),
            source: None,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
        });
        let base_dir = std::path::Path::new("/tmp/nonexistent-carina-test");
        let result = validate_and_resolve_with_config(&mut parsed, base_dir, false);

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("has no source configured"),
            "Error should tell user to add source. Got: {}",
            msg
        );
    }

    #[test]
    fn upstream_field_check_honors_skip_resource_validation() {
        // `destroy` and `state` subcommands pass `skip_resource_validation =
        // true` so they can run on projects whose upstream has drifted.
        // The static upstream-exports check must respect that gate;
        // otherwise a broken upstream would block recovery commands.
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        fs::write(
            upstream_dir.join("exports.crn"),
            "exports { accounts: String = \"x\" }\n",
        )
        .unwrap();
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();
        fs::write(
            base.join("main.crn"),
            r#"let orgs = upstream_state { source = "../organizations" }
exports {
    bad: String = orgs.missing
}
"#,
        )
        .unwrap();

        let loaded = carina_core::config_loader::load_configuration_with_config(
            &base,
            &ProviderContext::default(),
            &carina_core::schema::SchemaRegistry::new(),
        )
        .expect("load");
        let mut parsed = loaded.parsed;

        // With full validation, the typo is rejected.
        let err = validate_and_resolve_with_config(&mut parsed.clone(), &base, false)
            .expect_err("full validation must flag the typo");
        assert!(err.to_string().contains("does not export `missing`"));

        // With skip_resource_validation=true, recovery commands pass
        // through despite the typo.
        let result = validate_and_resolve_with_config(&mut parsed, &base, true);
        assert!(
            result.is_ok(),
            "skip_resource_validation=true must bypass upstream field check, got: {:?}",
            result.err()
        );
    }

    struct ResourceCreateOnlyEnumFactory {
        enum_provider: &'static str,
    }

    impl carina_core::provider::ProviderFactory for ResourceCreateOnlyEnumFactory {
        fn name(&self) -> &str {
            "awscc"
        }

        fn display_name(&self) -> &str {
            "AWSCC resource create-only enum test provider"
        }

        fn provider_config_attribute_types(
            &self,
        ) -> HashMap<String, carina_core::schema::AttributeType> {
            HashMap::new()
        }

        fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "ap-northeast-1".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _attributes: &IndexMap<String, Value>,
        ) -> carina_core::provider::BoxFuture<
            '_,
            carina_core::provider::ProviderResult<Box<dyn carina_core::provider::Provider>>,
        > {
            Box::pin(async {
                Ok(Box::new(carina_provider_mock::MockProvider::new())
                    as Box<dyn carina_core::provider::Provider>)
            })
        }

        fn create_normalizer(
            &self,
            _binding: Option<&str>,
            _attributes: &IndexMap<String, Value>,
        ) -> carina_core::provider::BoxFuture<'_, Box<dyn carina_core::provider::ProviderNormalizer>>
        {
            Box::pin(async {
                Box::new(carina_core::provider::NoopNormalizer)
                    as Box<dyn carina_core::provider::ProviderNormalizer>
            })
        }

        fn schemas(&self) -> Vec<carina_core::schema::ResourceSchema> {
            vec![
                carina_core::schema::ResourceSchema::new("ec2.Subnet").attribute(
                    carina_core::schema::AttributeSchema::new(
                        "placement_region",
                        carina_core::schema::AttributeType::enum_(
                            carina_core::schema::TypeIdentity::new(
                                Some(self.enum_provider),
                                Vec::<String>::new(),
                                "Region",
                            ),
                            None,
                            Vec::new(),
                            None,
                            Some(carina_core::schema::DslTransform::HyphenToUnderscore),
                        ),
                    )
                    .create_only(),
                ),
            ]
        }
    }

    fn resource_create_only_enum_file(raw_region: &str) -> carina_core::parser::InferredFile {
        let mut resource =
            carina_core::resource::Resource::with_provider("awscc", "ec2.Subnet", "", None);
        resource.set_attr(
            "placement_region".to_string(),
            Value::Concrete(ConcreteValue::enum_identifier(raw_region)),
        );

        carina_core::parser::InferredFile {
            resources: vec![resource],
            ..carina_core::parser::InferredFile::default()
        }
    }

    #[test]
    fn validate_and_resolve_canonicalizes_resource_create_only_enums_before_anonymous_hash() {
        let base_dir = tempfile::tempdir().unwrap();
        let mut awscc = resource_create_only_enum_file("awscc.Region.ap_northeast_1");
        let mut aws = resource_create_only_enum_file("aws.Region.ap_northeast_1");

        let errors = validate_and_resolve_errors_with_factories(
            &mut awscc,
            base_dir.path(),
            false,
            vec![Box::new(ResourceCreateOnlyEnumFactory {
                enum_provider: "awscc",
            })],
            HashMap::new(),
        );
        assert!(errors.is_empty(), "awscc spelling errors: {errors:?}");

        let errors = validate_and_resolve_errors_with_factories(
            &mut aws,
            base_dir.path(),
            false,
            vec![Box::new(ResourceCreateOnlyEnumFactory {
                enum_provider: "aws",
            })],
            HashMap::new(),
        );
        assert!(errors.is_empty(), "aws spelling errors: {errors:?}");

        assert_eq!(
            awscc.resources[0].id.name_str(),
            aws.resources[0].id.name_str(),
            "resource create-only enum spelling must canonicalize before anonymous hash"
        );
    }
}
