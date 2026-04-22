pub mod apply;
pub mod destroy;
pub mod docs;
pub mod export;
pub mod fmt;
pub mod init;
pub mod lint;
pub mod module;
pub mod plan;
pub mod skills;
pub mod state;
pub mod validate;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use carina_core::module_resolver;
use carina_core::parser::{BackendConfig, ParsedFile, ProviderContext};
use carina_state::BackendLock;
use carina_state::backend::BackendConfig as StateBackendConfig;

use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, compute_anonymous_identifiers_with_ctx,
    resolve_names_with_ctx, validate_attribute_param_ref_types_with_ctx,
    validate_module_attribute_param_types, validate_module_calls,
    validate_provider_region_with_ctx, validate_resource_ref_types_with_ctx,
    validate_resources_with_ctx,
};

/// Detect whether the `backend` block in the current configuration has
/// changed since the last run, by comparing against `carina-backend.lock`
/// under `base_dir`.
///
/// - If no lock exists, returns an error asking user to run `carina init`.
/// - If the lock matches, nothing happens.
/// - If the lock differs and `reconfigure` is `false`, returns an error
///   explaining the change and asking the user to re-run with `--reconfigure`.
/// - If `reconfigure` is `true`, overwrites the lock with the new config.
pub fn check_backend_lock(
    base_dir: &Path,
    backend_config: Option<&BackendConfig>,
    reconfigure: bool,
) -> Result<(), AppError> {
    let current = match backend_config {
        Some(config) => {
            let state_config = StateBackendConfig::from(config);
            BackendLock::from_config(&state_config)
        }
        None => BackendLock::local_default(),
    };
    let existing = BackendLock::load(base_dir).map_err(AppError::Backend)?;

    match existing {
        Some(existing) if existing != current => {
            if reconfigure {
                current.save(base_dir).map_err(AppError::Backend)?;
                Ok(())
            } else {
                Err(AppError::Config(format!(
                    "Backend configuration has changed since the last run:\n\n{}\n\n\
                     Changing backend settings can silently redirect Carina at a \
                     different state file, which may cause state loss or drift.\n\n\
                     To preserve existing state, run `carina state migrate` — it \
                     will copy state from the old backend to the new one and \
                     update the backend lock.\n\n\
                     To discard the old state and start fresh with the new backend, \
                     re-run with --reconfigure.",
                    existing.describe_diff(&current)
                )))
            }
        }
        Some(_) => Ok(()),
        // No lock file — user must run `carina init` first
        None => Err(AppError::Config(
            "Backend lock file not found. Run 'carina init' to initialize the project.".to_string(),
        )),
    }
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
    let lock = match backend_config {
        Some(config) => {
            let state_config = StateBackendConfig::from(config);
            BackendLock::from_config(&state_config)
        }
        None => BackendLock::local_default(),
    };
    lock.save(base_dir).map_err(AppError::Backend)
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
    parsed: &mut ParsedFile,
    base_dir: &Path,
    skip_resource_validation: bool,
) -> Result<(), AppError> {
    validate_and_resolve_with_config(parsed, base_dir, skip_resource_validation)
}

/// Create a `ProviderContext` with custom type validators extracted from
/// the already-collected schema map and factory-based validation for WASM providers.
fn enrich_provider_context(
    schemas: &std::collections::HashMap<String, carina_core::schema::ResourceSchema>,
    factories: Arc<Vec<Box<dyn carina_core::provider::ProviderFactory>>>,
) -> ProviderContext {
    ProviderContext {
        decryptor: None,
        validators: carina_core::provider::collect_custom_type_validators(schemas),
        custom_type_validator: Some(Box::new(move |type_name: &str, value: &str| {
            for factory in factories.iter() {
                factory.validate_custom_type(type_name, value)?;
            }
            Ok(())
        })),
        schema_types: Default::default(),
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
    parsed: &mut ParsedFile,
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
    parsed: &mut ParsedFile,
    base_dir: &Path,
    skip_resource_validation: bool,
) -> Vec<AppError> {
    let (factories, load_errors) = build_factories_from_providers(&parsed.providers, base_dir);
    let ctx = WiringContext::new(factories);

    let mut errors: Vec<AppError> = Vec::new();

    // Check for declared providers whose plugins failed to load.
    if !skip_resource_validation {
        for provider in &parsed.providers {
            let loaded = ctx.factories().iter().any(|f| f.name() == provider.name);
            if loaded {
                continue;
            }
            if let Some(reason) = load_errors.get(&provider.name) {
                errors.push(AppError::Validation(reason.clone()));
            } else if provider.source.is_none() {
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

    // Resolve names (let bindings -> resource names) — must succeed
    // before per-resource schema checks can look up the renamed
    // attributes, so its failures gate the remaining pipeline.
    errors.extend(resolve_names_with_ctx(&ctx, &mut parsed.resources));
    if !errors.is_empty() {
        return errors;
    }

    if !skip_resource_validation {
        errors.extend(validate_resources_with_ctx(&ctx, parsed));
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
        if let Err(msg) = carina_core::validation::validate_export_param_ref_types(
            &parsed.export_params,
            &parsed.resources,
            ctx.schemas(),
            &|r: &carina_core::resource::Resource| {
                carina_core::provider::schema_key_for_resource(ctx.factories(), r)
            },
        ) {
            errors.extend(split_validation_message(&msg));
        }
    }

    // Compute anonymous identifiers — downstream plan code assumes
    // every resource has a stable id, so a collision error must stop
    // the pipeline here.
    errors.extend(compute_anonymous_identifiers_with_ctx(
        &ctx,
        &mut parsed.resources,
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
            carina_core::upstream_exports::resolve_upstream_exports(
                base_dir,
                &parsed.upstream_states,
                &enriched_context,
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
            &|r: &carina_core::resource::Resource| {
                carina_core::provider::schema_key_for_resource(ctx.factories(), r)
            },
        );
        errors.extend(
            resolve_errors
                .iter()
                .map(|e| AppError::Validation(e.to_string())),
        );
        errors.extend(
            field_errors
                .iter()
                .map(|e| AppError::Validation(e.to_string())),
        );
        errors.extend(
            type_errors
                .iter()
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
    use carina_core::resource::Value;
    use std::collections::{HashMap, HashSet};

    fn s3_backend_config(bucket: &str, region: &str) -> BackendConfig {
        let mut attributes = HashMap::new();
        attributes.insert("bucket".to_string(), Value::String(bucket.to_string()));
        attributes.insert("region".to_string(), Value::String(region.to_string()));
        BackendConfig {
            backend_type: "s3".to_string(),
            attributes,
        }
    }

    #[test]
    fn check_backend_lock_errors_when_lock_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let config = s3_backend_config("my-bucket", "us-east-1");
        let err = check_backend_lock(tmp.path(), Some(&config), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("carina init"));
    }

    #[test]
    fn ensure_backend_lock_creates_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let config = s3_backend_config("my-bucket", "us-east-1");
        ensure_backend_lock(tmp.path(), Some(&config)).unwrap();
        assert!(tmp.path().join("carina-backend.lock").exists());
    }

    #[test]
    fn check_backend_lock_passes_when_config_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let config = s3_backend_config("my-bucket", "us-east-1");
        ensure_backend_lock(tmp.path(), Some(&config)).unwrap();
        let result = check_backend_lock(tmp.path(), Some(&config), false);
        assert!(result.is_ok());
    }

    #[test]
    fn check_backend_lock_blocks_on_bucket_change() {
        let tmp = tempfile::tempdir().unwrap();
        let old = s3_backend_config("old-bucket", "us-east-1");
        let new = s3_backend_config("new-bucket", "us-east-1");
        ensure_backend_lock(tmp.path(), Some(&old)).unwrap();
        let err = check_backend_lock(tmp.path(), Some(&new), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Backend configuration has changed"));
        assert!(msg.contains("bucket"));
        assert!(msg.contains("old-bucket"));
        assert!(msg.contains("new-bucket"));
        assert!(msg.contains("carina state migrate"));
        assert!(msg.contains("--reconfigure"));
    }

    #[test]
    fn check_backend_lock_accepts_change_with_reconfigure() {
        let tmp = tempfile::tempdir().unwrap();
        let old = s3_backend_config("old-bucket", "us-east-1");
        let new = s3_backend_config("new-bucket", "us-east-1");
        ensure_backend_lock(tmp.path(), Some(&old)).unwrap();
        let result = check_backend_lock(tmp.path(), Some(&new), true);
        assert!(result.is_ok());
        let result2 = check_backend_lock(tmp.path(), Some(&new), false);
        assert!(result2.is_ok());
    }

    #[test]
    fn check_backend_lock_errors_when_lock_missing_no_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let err = check_backend_lock(tmp.path(), None, false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("carina init"));
    }

    #[test]
    fn check_backend_lock_blocks_local_to_remote_transition() {
        let tmp = tempfile::tempdir().unwrap();
        // Simulate first apply with no backend (local default)
        ensure_backend_lock(tmp.path(), None).unwrap();
        // Second run: user adds an S3 backend
        let new = s3_backend_config("my-bucket", "us-east-1");
        let err = check_backend_lock(tmp.path(), Some(&new), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Backend configuration has changed"));
        assert!(msg.contains("local"));
        assert!(msg.contains("s3"));
        assert!(msg.contains("--reconfigure"));
    }

    #[test]
    fn check_backend_lock_allows_local_to_remote_with_reconfigure() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_backend_lock(tmp.path(), None).unwrap();
        let new = s3_backend_config("my-bucket", "us-east-1");
        let result = check_backend_lock(tmp.path(), Some(&new), true);
        assert!(result.is_ok());
        // Subsequent run with the new backend should now pass
        let result2 = check_backend_lock(tmp.path(), Some(&new), false);
        assert!(result2.is_ok());
    }

    fn empty_parsed_file() -> ParsedFile {
        ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            upstream_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
            string_literal_paths: std::collections::HashSet::new(),
        }
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
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
            version: None,
            revision: None,
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
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
            version: None,
            revision: None,
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
}
