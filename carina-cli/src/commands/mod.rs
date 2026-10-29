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
pub mod providers;
pub(crate) mod shared;
pub mod skills;
pub mod state;
pub mod validate;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use carina_core::config_loader::{DuplicateDeclaration, UnresolvedConfigSnapshot};
use carina_core::module_resolver;
use carina_core::parser::{BackendConfig, ProviderContext};
use carina_core::upstream_exports::UpstreamRefDiagnostic;
use carina_core::validation::inference::ExportInferenceError;
use carina_state::{
    BackendConfig as StateBackendConfig, BackendDrift, BackendLock, BackendSelection,
    MutationBackendSelection, ResolvedReadBackend, VerifiedMutationBackendSelection,
};
#[cfg(test)]
use carina_state::{BackendError, StateBackend};
use colored::Colorize;

pub use carina_state::{BackendDriftStatus, inspect_backend_drift};

use crate::error::AppError;
use crate::module_walk::ModuleWalk;
#[cfg(any(test, feature = "test-utils"))]
use crate::wiring::ProviderLoad;
use crate::wiring::{
    CanonicalResourcePair, WiringContext, compute_anonymous_identifiers_with_ctx,
    compute_canonical_pair_anonymous_identifiers_with_ctx, resolve_file_names_with_ctx,
    validate_deferred_populate_refs_with_ctx, validate_depends_on_with_ctx,
    validate_module_attribute_refs_after_expansion_failure, validate_module_calls_with_imported,
    validate_module_internal_ref_types, validate_no_backend_in_modules,
    validate_no_empty_interpolations, validate_no_exports_in_modules,
    validate_no_provider_in_modules, validate_no_state_blocks_in_modules,
    validate_no_upstream_states_in_modules, validate_provider_region_with_ctx,
    validate_resources_with_ctx, validate_wait_bindings_with_ctx,
};
use carina_core::hint::ProjectCommand;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendLockRequirement {
    AllowMissing,
    RequirePresent,
}

/// Required warning behavior for a lock-aware backend read.
///
/// Making this an argument to [`resolve_backend_for_read`] prevents callers
/// from obtaining a readable backend before choosing how pending-migration
/// drift is surfaced.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ReadBackendWarningPolicy<'a> {
    /// A top-level command reading the current project's state.
    CommandBoundary { project_dir: &'a Path },
    /// An `upstream_state` read, deduplicated by canonical source directory.
    UpstreamSource { source_dir: &'a Path },
}

/// Resolve the backend for a read-only state operation.
///
/// Every project-backed read goes through this function. If the configured
/// backend differs from `carina-backend.lock`, the returned
/// [`ResolvedReadBackend`] points at the locked old backend. A warning policy
/// is mandatory and is applied before the readable backend is returned.
pub(crate) async fn resolve_backend_for_read(
    base_dir: &Path,
    command_path: &Path,
    backend_config: Option<&BackendConfig>,
    lock_requirement: BackendLockRequirement,
    warning_policy: ReadBackendWarningPolicy<'_>,
) -> Result<ResolvedReadBackend, AppError> {
    let selection = BackendSelection::for_read(base_dir, backend_config)?;
    if lock_requirement == BackendLockRequirement::RequirePresent && !selection.lock_present() {
        return Err(AppError::Config(format!(
            "Backend lock file not found. Run `{}` to initialize the project.",
            ProjectCommand::new("init", command_path)
        )));
    }
    let backend = selection
        .resolve_with_drift_warning(|drift| {
            emit_read_backend_drift_warning(drift, warning_policy);
        })
        .await?;
    Ok(backend)
}

pub fn verify_for_mutation(
    project_dir: &Path,
    backend_config: Option<&BackendConfig>,
    command: DriftCommand,
) -> Result<VerifiedMutationBackendSelection, AppError> {
    match BackendSelection::verified_for_mutation(project_dir, backend_config)? {
        MutationBackendSelection::MissingLock { path } => Err(AppError::Config(format!(
            "Backend lock file not found at {}. Run `{}` to initialize the project.",
            path.display(),
            ProjectCommand::new("init", project_dir)
        ))),
        MutationBackendSelection::Verified(selection) => Ok(selection),
        MutationBackendSelection::Drifted(drift) => Err(AppError::Config(drift_error_message(
            command,
            drift.existing(),
            drift.configured(),
            project_dir,
        ))),
    }
}

static WARNED_UPSTREAM_BACKEND_DRIFT: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn emit_read_backend_drift_warning(drift: &BackendDrift, policy: ReadBackendWarningPolicy<'_>) {
    let warning = match policy {
        ReadBackendWarningPolicy::CommandBoundary { project_dir } => {
            drift_warning(drift.existing(), drift.configured(), project_dir)
        }
        ReadBackendWarningPolicy::UpstreamSource { source_dir } => {
            let warned = WARNED_UPSTREAM_BACKEND_DRIFT.get_or_init(|| Mutex::new(HashSet::new()));
            let mut warned = warned
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !warned.insert(source_dir.to_path_buf()) {
                return;
            }
            drop(warned);

            upstream_backend_drift_warning(drift, source_dir)
        }
    };

    eprintln!("{}", warning.yellow());
}

fn upstream_backend_drift_warning(drift: &BackendDrift, source: &Path) -> String {
    format!(
        "upstream_state source {} has a pending backend migration; reading its OLD backend \
         recorded in carina-backend.lock.\n{}\nRun `{}` to migrate that upstream state.",
        source.display(),
        drift.existing().describe_diff(drift.configured()),
        ProjectCommand::new("init --migrate-state", source),
    )
}

/// Build a local backend through carina-state's feature-gated test utility.
#[cfg(test)]
pub(crate) fn local_backend_for_test(path: impl AsRef<Path>) -> Box<dyn StateBackend> {
    carina_state::local_backend_for_test(path.as_ref().to_path_buf())
}

fn backend_drift_header(existing: &BackendLock, configured: &BackendLock) -> String {
    format!(
        "Backend configuration changed since the last state migration:\n{}",
        existing.describe_diff(configured)
    )
}

pub fn drift_warning(
    existing: &BackendLock,
    configured: &BackendLock,
    project_dir: &Path,
) -> String {
    format!(
        "{}\n\n    Until migration completes, Carina reads state from the OLD backend recorded in carina-backend.lock.\n    \
         Before running apply, destroy, or state refresh, run `{}`\n    \
         to migrate state from the OLD backend to the new one.\n\n    \
         To revert instead, restore the backend block to match the lock.",
        backend_drift_header(existing, configured),
        ProjectCommand::new("init --migrate-state", project_dir)
    )
}

pub fn drift_error_message(
    command: DriftCommand,
    existing: &BackendLock,
    configured: &BackendLock,
    project_dir: &Path,
) -> String {
    format!(
        "{}\n\n{} without first migrating the state. State migration is an\n\
         explicit, named operation:\n\n    {}\n\n\
         Or revert the `backend` block to match the lock if the change was unintended.",
        backend_drift_header(existing, configured),
        command.verb_phrase(),
        ProjectCommand::new("init --migrate-state", project_dir)
    )
}

/// Error message for a provider block that declares no `source` attribute.
/// Shared between `init` (pre-resolution check) and validation (post-load check)
/// so both surfaces report the same text.
pub fn missing_provider_source_message(name: &str) -> String {
    format!(
        "Provider '{}' has no source configured. Add `source = 'carina-rs/aws'` for a registry provider, or `source = 'github.com/owner/repo'` for a direct GitHub source.",
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

/// Create a `ProviderContext` with custom type validators extracted from
/// the already-collected schema map and factory-based validation for WASM providers.
fn enrich_provider_context(
    schemas: &carina_core::schema::SchemaRegistry,
    factories: Arc<Vec<Box<dyn carina_core::provider::ProviderFactory>>>,
) -> ProviderContext {
    ProviderContext {
        decryptor: None,
        custom_types: carina_core::provider::provider_custom_types_from_factories(factories),
        resource_types: ProviderContext::resource_types_from_schema_registry(schemas),
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

pub(crate) fn duplicate_declaration_errors(
    duplicate_declarations: &[DuplicateDeclaration],
    module_walk: &ModuleWalk,
) -> Vec<AppError> {
    let mut errors: Vec<AppError> = duplicate_declarations
        .iter()
        .map(|duplicate| AppError::Validation(duplicate.to_string()))
        .collect();
    for module in module_walk.iter() {
        let prefix = module.diagnostic_path().display();
        errors.extend(
            module
                .loaded()
                .duplicate_declarations
                .iter()
                .map(|duplicate| AppError::Validation(format!("{prefix}: {duplicate}"))),
        );
    }
    errors
}

/// Test-support wrapper around the non-minting validation report.
///
/// Both diagnostic channels from the same
/// [`LoadedConfig`](carina_core::config_loader::LoadedConfig) remain mandatory,
/// so fixtures exercise directory-level uniqueness validation as well.
#[cfg(any(test, feature = "test-utils"))]
pub fn validate_and_resolve_with_config(
    parsed: &mut carina_core::parser::InferredFile,
    base_dir: &Path,
    providers: &ProviderLoad,
    skip_resource_validation: bool,
    prior_inference_errors: &[ExportInferenceError],
    duplicate_declarations: &[DuplicateDeclaration],
) -> Result<(), AppError> {
    let errors = providers
        .validation_report(
            parsed,
            base_dir,
            skip_resource_validation,
            prior_inference_errors,
            duplicate_declarations,
        )
        .into_errors();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(collapse_errors(errors))
    }
}

/// Test-support entry point that exercises the real command-scoped provider loader.
#[cfg(any(test, feature = "test-utils"))]
pub fn validate_and_resolve_with_provider_load_for_test(
    user_dirs: &carina_core::UserDirs,
    parsed: &mut carina_core::parser::InferredFile,
    base_dir: &Path,
    skip_resource_validation: bool,
    prior_inference_errors: &[ExportInferenceError],
    duplicate_declarations: &[DuplicateDeclaration],
) -> Result<(), AppError> {
    let providers = ProviderLoad::load(user_dirs, &parsed.providers, base_dir)?;
    validate_and_resolve_with_config(
        parsed,
        base_dir,
        &providers,
        skip_resource_validation,
        prior_inference_errors,
        duplicate_declarations,
    )
}

/// Test-support collecting, non-minting validation entry point.
///
/// The `&ProviderLoad` privately owns both the factory context and
/// per-provider diagnostics used to build the report. This helper neither
/// performs another load nor exposes a context or mints a
/// [`ValidatedConfig`](crate::wiring::ValidatedConfig).
/// Production commands use the consuming validation methods on
/// [`ProviderLoad`] or the report-only loader instead.
#[cfg(any(test, feature = "test-utils"))]
pub fn validate_and_resolve_errors(
    parsed: &mut carina_core::parser::InferredFile,
    base_dir: &Path,
    providers: &ProviderLoad,
    skip_resource_validation: bool,
    prior_inference_errors: &[ExportInferenceError],
    duplicate_declarations: &[DuplicateDeclaration],
) -> Vec<AppError> {
    providers
        .validation_report(
            parsed,
            base_dir,
            skip_resource_validation,
            prior_inference_errors,
            duplicate_declarations,
        )
        .into_errors()
}

pub(crate) enum ConfigNameResolution {
    EvaluatedOnly,
    Paired(Box<UnresolvedConfigSnapshot>),
}

/// Internal result of the sealed validation pipeline.
///
/// The paired form is built only through [`crate::wiring::ValidationPipelineAccess`]
/// and is immediately captured by the consuming provider-load transition.
/// After that owner is minted, production code can change its resources only
/// through concrete [`crate::wiring::ValidatedConfig`] methods or consuming
/// owner transitions.
pub(crate) enum ResolvedConfig<'a> {
    Evaluated {
        parsed: &'a mut carina_core::parser::InferredFile,
        binding_uses: carina_core::validation::ResolvedBindingUses,
    },
    Paired {
        resources: Box<CanonicalResourcePair<carina_core::parser::InferredExportParam>>,
        binding_uses: carina_core::validation::ResolvedBindingUses,
    },
}

impl ResolvedConfig<'_> {
    fn parsed(&self) -> &carina_core::parser::InferredFile {
        match self {
            Self::Evaluated { parsed, .. } => parsed,
            Self::Paired { resources, .. } => resources.parsed(),
        }
    }

    fn compute_anonymous_identifiers(
        &mut self,
        ctx: &WiringContext,
    ) -> Result<Vec<AppError>, AppError> {
        let providers = self.parsed().providers.clone();
        match self {
            Self::Evaluated { parsed, .. } => {
                let canonical_resources = carina_core::value::canonicalize_resources_with_schemas(
                    &mut parsed.resources,
                    ctx.schemas(),
                )?;
                Ok(compute_anonymous_identifiers_with_ctx(
                    ctx,
                    canonical_resources,
                    &providers,
                ))
            }
            Self::Paired { resources, .. } => {
                compute_canonical_pair_anonymous_identifiers_with_ctx(ctx, resources, &providers)
            }
        }
    }
}

fn extend_binding_use_errors(
    errors: &mut Vec<AppError>,
    report: &carina_core::validation::BindingUseReport,
) {
    errors.extend(
        report
            .findings()
            .iter()
            .map(|finding| AppError::Validation(finding.error.to_string())),
    );
    if !report.is_ready() && report.findings().is_empty() {
        errors.push(AppError::Validation(
            "bare binding validation left an unconsumed pending value slot".to_string(),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_and_resolve_errors_inner<'a>(
    access: &crate::wiring::ValidationPipelineAccess,
    parsed: &'a mut carina_core::parser::InferredFile,
    base_dir: &Path,
    ctx: &WiringContext,
    load_errors: &HashMap<String, String>,
    skip_resource_validation: bool,
    prior_inference_errors: &[ExportInferenceError],
    duplicate_declarations: &[DuplicateDeclaration],
    name_resolution: ConfigNameResolution,
) -> Result<ResolvedConfig<'a>, Vec<AppError>> {
    // Binding-use validation is a production-readiness invariant, not a
    // resource-validation preference. Keep the recursive module snapshots
    // available even on destroy/state recovery paths; the later optional
    // policy checks remain gated by `skip_resource_validation`.
    let module_walk = ModuleWalk::load(parsed, base_dir);

    let mut errors: Vec<AppError> = Vec::new();

    if !skip_resource_validation {
        errors.extend(duplicate_declaration_errors(
            duplicate_declarations,
            &module_walk,
        ));
    }

    let retry_inference_errors = if skip_resource_validation {
        Vec::new()
    } else {
        carina_core::validation::inference::retry_unknown_export_inference(
            parsed,
            ctx.schemas(),
            prior_inference_errors,
        )
    };
    // A later Unknown-type existence walk suppresses precisely the export
    // parameters already reported by either inference pass. The vector index
    // is the identity: export names are user supplied and may collide across
    // sibling files.
    let reported_inference_error_indices: HashSet<usize> = prior_inference_errors
        .iter()
        .chain(&retry_inference_errors)
        .map(|error| error.index)
        .collect();
    errors.extend(retry_inference_errors.iter().map(|inference_error| {
        AppError::Validation(carina_core::validation::inference::format_inference_error(
            &inference_error.name,
            &inference_error.error,
        ))
    }));

    // A backend in a module-definition directory is the structural error: the
    // backend is misplaced, so do not also classify the module's arguments as
    // misplaced root declarations. Provider + arguments retains the existing
    // root-arguments rule (#2198).
    if let Err(msg) = carina_core::validation::validate_no_backend_in_module(parsed) {
        errors.push(AppError::Validation(msg));
    } else if let Err(msg) = carina_core::validation::validate_no_arguments_in_root(parsed) {
        errors.push(AppError::Validation(msg));
    }

    // Check for declared providers whose plugins failed to load.
    // Named instances (`binding.is_some()`) deliberately omit `source` —
    // the parser enforces that `source` is a kind-level property
    // (carina#3023). They inherit the kind default's plugin, so
    // they only matter to this check when the *kind default* could
    // not be loaded, which is already reported via the
    // `factories().iter().any(...)` lookup on the kind's name.
    if !skip_resource_validation {
        for provider in &parsed.providers {
            let loaded = ctx.factories().iter().any(|f| f.name() == provider.name);
            let uses_builtin_provider = carina_provider_mock::uses_builtin_provider(
                std::iter::once(provider),
                |provider_name| {
                    ctx.factories()
                        .iter()
                        .any(|factory| factory.name() == provider_name)
                },
            );
            if loaded {
                continue;
            }
            if let Some(reason) = load_errors.get(&provider.name) {
                errors.push(AppError::Validation(reason.clone()));
            } else if provider.is_default() && provider.source.is_none() && !uses_builtin_provider {
                errors.push(AppError::Validation(missing_provider_source_message(
                    &provider.name,
                )));
            }
        }
    }

    // Validate provider region
    errors.extend(validate_provider_region_with_ctx(ctx, parsed));

    if !carina_core::validation::is_module_definition(parsed)
        && let Some(backend) = &parsed.backend
        && let Err(error) = StateBackendConfig::try_from(backend)
    {
        errors.push(AppError::Backend(error.into()));
    }

    // Enrich provider context with custom type validators from loaded schemas
    let enriched_context = enrich_provider_context(ctx.schemas(), ctx.factories_arc());

    // carina#3239: reject `arguments { foo: TotallyMadeUpType }`-shaped
    // unknown bare custom-type names in the root parse. The root
    // config was parsed up in `load_configuration_with_config` with
    // the bootstrap `ProviderCustomTypes::Unloaded` context — schemas had
    // not been collected yet, so the parser-side loaded-state gate
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
    if let Err(error) =
        carina_core::parser::resolve_resource_refs_with_config(parsed, &enriched_context)
    {
        errors.push(AppError::Config(error.to_string()));
    }
    if !skip_resource_validation {
        errors.extend(validate_module_internal_ref_types(
            ctx,
            &module_walk,
            &enriched_context,
        ));
    }

    // Load root module signatures once before expansion for concrete argument,
    // missing-argument, and unknown-argument validation.
    let imported_module_signatures =
        module_resolver::load_resolved_module_signatures(parsed, base_dir, &enriched_context);

    // Consume caller-authored uses before the legacy module-argument checker
    // or expansion can turn a typed pending slot into a secondary missing-
    // argument error. This authority is mandatory even for destroy/state
    // recovery paths (`skip_resource_validation`): the flag may skip provider
    // resource checks, but it cannot mint validation readiness.
    let binding_use_report = carina_core::validation::resolve_bare_binding_uses(
        parsed,
        &carina_core::validation::ValidationTypeContext::new(
            parsed,
            ctx.schemas(),
            &enriched_context,
            &imported_module_signatures,
        ),
    );
    extend_binding_use_errors(&mut errors, &binding_use_report);
    for module in module_walk.iter() {
        let module_parsed = &module.loaded().parsed;
        let module_signatures = module_resolver::load_resolved_module_signatures(
            module_parsed,
            module.module_path(),
            &enriched_context,
        );
        let report = carina_core::validation::resolve_bare_binding_uses(
            module_parsed,
            &carina_core::validation::ValidationTypeContext::new(
                module_parsed,
                ctx.schemas(),
                &enriched_context,
                &module_signatures,
            ),
        );
        let prefix = module.diagnostic_path().display();
        errors.extend(
            report
                .findings()
                .iter()
                .map(|finding| AppError::Validation(format!("{prefix}: {}", finding.error))),
        );
        if !report.is_ready() && report.findings().is_empty() {
            errors.push(AppError::Validation(format!(
                "{prefix}: bare binding validation left an unconsumed pending value slot"
            )));
        }
    }

    // Validate module call arguments before expansion (needs enriched
    // context for custom type validators).
    errors.extend(validate_module_calls_with_imported(
        parsed,
        &imported_module_signatures,
        &enriched_context,
    ));

    // User-facing checks for root-owned blocks inside modules run on the
    // recursive module walk so validate/plan/apply can collect path-prefixed
    // findings alongside other validation errors. As with the duplicate
    // checks established in #3711, the directory unit includes every imported
    // nested module, even when that import is never called. The `errors`
    // return immediately below happens before
    // `resolve_modules_with_config`, so the same run cannot also emit the
    // resolver's expansion-phase backstops. Destroy and state refresh set
    // `skip_resource_validation = true`, bypass this walk, and rely on those
    // backstops while still expanding modules.
    if !skip_resource_validation {
        errors.extend(validate_no_provider_in_modules(&module_walk));
        errors.extend(validate_no_state_blocks_in_modules(&module_walk));
        errors.extend(validate_no_backend_in_modules(&module_walk));
        errors.extend(validate_no_upstream_states_in_modules(&module_walk));
        errors.extend(validate_no_exports_in_modules(&module_walk));
    }

    // Module expansion assumes the checks above succeeded — feeding
    // broken module calls into `resolve_modules_with_config` can
    // surface confusing secondary errors, so gate it here.
    if !errors.is_empty() {
        return Err(errors);
    }

    if let Err(e) = module_resolver::resolve_modules_with_config(
        parsed,
        base_dir,
        &enriched_context,
        ctx.schemas(),
    ) {
        // A cycle or other expansion failure leaves no complete composition
        // graph for the unified boundary pass. Preserve source-local
        // `attributes {}` diagnostics that do not depend on expansion; this
        // fallback never runs on the successful path, so it cannot duplicate
        // composition diagnostics.
        errors.extend(validate_module_attribute_refs_after_expansion_failure(
            ctx,
            &module_walk,
            &enriched_context,
        ));
        let message = match e.root_cause() {
            module_resolver::ModuleError::Constraint(_) => e.to_string(),
            _ => format!(
                "Module resolution error: {}",
                e.display_relative_to(base_dir)
            ),
        };
        errors.push(AppError::Config(message));
        return Err(errors);
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
        return Err(errors);
    }
    if let Err(e) = carina_core::parser::finalize_provider_configs(parsed) {
        errors.push(AppError::Config(format!("Finalize error: {}", e)));
        return Err(errors);
    }

    // Expansion carries module-internal side data in parallel with each
    // expanded resource and remaps its owner. Re-run the same authority over
    // that final graph, then retain its sealed success proof for the
    // ValidatedConfig transition. This is not a second interpretation path:
    // both passes use resolve_bare_binding_uses and the same mandatory
    // provider context.
    let binding_use_report = carina_core::validation::resolve_bare_binding_uses(
        parsed,
        &carina_core::validation::ValidationTypeContext::new(
            parsed,
            ctx.schemas(),
            &enriched_context,
            &imported_module_signatures,
        ),
    );
    extend_binding_use_errors(&mut errors, &binding_use_report);
    let binding_uses = match binding_use_report.into_resolved() {
        Ok(binding_uses) => binding_uses,
        Err(_) => return Err(errors),
    };

    let parsed: &mut carina_core::parser::InferredFile = &mut *parsed;

    // Resolve names (let bindings -> resource names) — must succeed
    // before per-resource schema checks can look up the renamed
    // attributes, so its failures gate the remaining pipeline.
    let mut resolved_config = match name_resolution {
        ConfigNameResolution::EvaluatedOnly => {
            let name_errors = resolve_file_names_with_ctx(ctx, parsed);
            if !name_errors.is_empty() {
                errors.extend(name_errors);
                return Err(errors);
            }
            ResolvedConfig::Evaluated {
                parsed,
                binding_uses,
            }
        }
        ConfigNameResolution::Paired(unresolved) => {
            match access.resolve_config_names(ctx, std::mem::take(parsed), *unresolved) {
                Ok(resources) => ResolvedConfig::Paired {
                    resources: Box::new(resources),
                    binding_uses,
                },
                Err(name_errors) => {
                    errors.extend(name_errors);
                    return Err(errors);
                }
            }
        }
    };
    let parsed = resolved_config.parsed();

    let (upstream_exports, upstream_resolve_errors) = if skip_resource_validation {
        (Default::default(), Vec::new())
    } else {
        carina_core::upstream_exports::resolve_upstream_exports_with_schemas(
            base_dir,
            &parsed.upstream_states,
            &enriched_context,
            Some(ctx.schemas()),
        )
    };

    if !skip_resource_validation {
        // Mid-edit `${}` is parser-accepted (#2480) so the AST stays
        // intact for LSP diagnostics, but it must not reach a provider —
        // surface it as a hard error before the schema-level checks
        // below try to type-check around the marker. See #2487.
        errors.extend(validate_no_empty_interpolations(parsed));

        errors.extend(validate_resources_with_ctx(ctx, parsed, &enriched_context));
        errors.extend(validate_depends_on_with_ctx(parsed));
        errors.extend(validate_wait_bindings_with_ctx(ctx, parsed));
        errors.extend(validate_deferred_populate_refs_with_ctx(ctx, parsed));
        errors.extend(
            carina_core::identifier::check_attribute_derived_anonymous_resource_conflicts(
                &parsed.resources,
                ctx.schemas(),
            )
            .into_iter()
            .map(|conflict| AppError::Validation(conflict.to_string())),
        );
        errors.extend(
            upstream_resolve_errors
                .iter()
                .map(|error| AppError::Validation(error.to_string())),
        );

        let export_value_bindings =
            carina_core::binding_index::ResolvedBindings::for_validation(parsed);
        let checked_export_values = carina_core::validation::check_export_values_pre_apply(
            &parsed.export_params,
            &export_value_bindings,
            &enriched_context,
        );
        let bindings = carina_core::binding_index::BindingIndex::from_parsed_with_upstream_exports(
            parsed,
            ctx.schemas(),
            &upstream_exports,
        );
        // The mandatory pre-expansion graph check has already validated every
        // declared module-output boundary. Keep the expanded walk for
        // unannotated forwarded outputs, whose inferred source type exists
        // only on compositions; it uses the same BindingIndex + TypeInSchema
        // relation rather than a second assignability authority.
        errors.extend(
            carina_core::validation::validate_composition_ref_types_with_bindings(
                &parsed.compositions,
                &bindings,
            )
            .into_iter()
            .map(|error| AppError::Validation(error.to_string())),
        );
        if let Err(message) = carina_core::validation::validate_resource_ref_types(
            parsed,
            ctx.schemas(),
            &HashSet::new(),
            &bindings,
        ) {
            errors.extend(split_validation_message(&message));
        }
        if let Err(message) =
            carina_core::validation::validate_attribute_param_ref_types_with_bindings(
                &parsed.attribute_params,
                &bindings,
            )
        {
            errors.extend(split_validation_message(&message));
        }
        if let Err(message) = carina_core::validation::validate_attribute_params(
            &parsed.attribute_params,
            &enriched_context,
        ) {
            errors.extend(split_validation_message(&message));
        }
        if let Err(msg) = checked_export_values {
            errors.extend(split_validation_message(&msg));
        }
        if let Err(msg) = carina_core::validation::validate_export_param_ref_types_with_bindings(
            &parsed.export_params,
            &bindings,
            &reported_inference_error_indices,
        ) {
            errors.extend(split_validation_message(&msg));
        }
        if !errors.is_empty() {
            return Err(errors);
        }
    }

    let anonymous_identifier_errors = match resolved_config.compute_anonymous_identifiers(ctx) {
        Ok(errors) => errors,
        Err(error) => {
            errors.push(error);
            return Err(errors);
        }
    };

    // Compute anonymous identifiers — downstream plan code assumes
    // every resource has a stable id, so a collision error must stop
    // the pipeline here.
    errors.extend(anonymous_identifier_errors);
    if !errors.is_empty() {
        return Err(errors);
    }
    let parsed = resolved_config.parsed();

    // Reject references to `upstream_state` fields that the upstream's
    // `exports { }` block does not declare. Gated like the other
    // validations above so `destroy` / `state` (which pass
    // `skip_resource_validation = true`) can still run when an upstream
    // has drifted — those commands are the recovery path for exactly
    // that situation. No state I/O: we parse the upstream's `.crn`
    // files directly.
    if !skip_resource_validation {
        let field_errors =
            carina_core::upstream_exports::check_upstream_state_untyped_field_references(
                parsed,
                &upstream_exports,
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
            carina_core::upstream_exports::check_upstream_state_attribute_access_shape_fallbacks(
                parsed,
                &upstream_exports,
            );
        let subscript_errors = carina_core::upstream_exports::check_upstream_state_subscript_shapes(
            parsed,
            &upstream_exports,
        );
        // The four upstream-ref shape/existence checks return distinct concrete types
        // but share `UpstreamRefDiagnostic`. Chain them through the
        // trait (`Display` supertrait gives the canonical
        // `"location: message"` form — the per-type `Display` impl is
        // the single source of truth) so adding another check is one
        // extra `chain(...)`.
        errors.extend(
            field_errors
                .iter()
                .map(|e| e as &dyn UpstreamRefDiagnostic)
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

    if errors.is_empty() {
        Ok(resolved_config)
    } else {
        Err(errors)
    }
}

/// Split the newline-joined error strings that `carina_core::validation::*`
/// helpers return into individual `AppError::Validation` entries. Command and
/// wiring validation paths share this renderer so multi-error results retain
/// identical splitting behavior.
pub(crate) fn split_validation_message(joined: &str) -> Vec<AppError> {
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
            Value::Concrete(ConcreteValue::String(bucket.to_string().into())),
        );
        attributes.insert(
            "key".to_string(),
            Value::Concrete(ConcreteValue::String("state.json".into())),
        );
        attributes.insert(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String(region.to_string().into())),
        );
        BackendConfig {
            backend_type: "s3".to_string(),
            attributes,
            attribute_spans: HashMap::new(),
        }
    }

    fn local_backend_config(path: &str) -> BackendConfig {
        let mut attributes = HashMap::new();
        attributes.insert(
            "path".to_string(),
            Value::Concrete(ConcreteValue::String(path.to_string().into())),
        );
        BackendConfig {
            backend_type: "local".to_string(),
            attributes,
            attribute_spans: HashMap::new(),
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

            let BackendDriftStatus::Drifted(drift) = status else {
                panic!("expected backend drift")
            };
            assert_eq!(drift.existing(), &existing);
            assert_eq!(drift.configured(), &configured);
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

            let BackendDriftStatus::Drifted(drift) = status else {
                panic!("expected backend drift")
            };
            assert_eq!(drift.existing(), &existing);
            assert_eq!(drift.configured(), &configured);
        }

        #[test]
        fn drift_warning_names_old_and_new() {
            let old =
                BackendLock::for_config(Some(&local_backend_config("legacy/state.json"))).unwrap();
            let new = BackendLock::for_config(Some(&local_backend_config("state.json"))).unwrap();

            let warning = drift_warning(&old, &new, Path::new("."));

            assert!(warning.contains("legacy/state.json"));
            assert!(warning.contains("state.json"));
            assert!(warning.contains("carina init --migrate-state"));
            assert!(warning.contains("Before running apply, destroy, or state refresh"));
        }

        #[test]
        fn drift_error_names_apply_blocker() {
            let old =
                BackendLock::for_config(Some(&local_backend_config("legacy/state.json"))).unwrap();
            let new = BackendLock::for_config(Some(&local_backend_config("state.json"))).unwrap();

            let error = drift_error_message(DriftCommand::Apply, &old, &new, Path::new("."));

            assert!(error.contains("carina init --migrate-state"));
            assert!(!error.contains("carina init --migrate-state ."));
            assert!(error.contains("Cannot apply without first migrating the state"));
        }

        #[test]
        fn drift_error_names_refresh_blocker() {
            let old =
                BackendLock::for_config(Some(&local_backend_config("legacy/state.json"))).unwrap();
            let new = BackendLock::for_config(Some(&local_backend_config("state.json"))).unwrap();

            let error = drift_error_message(DriftCommand::RefreshState, &old, &new, Path::new("."));

            assert!(error.contains("carina init --migrate-state"));
            assert!(error.contains("Cannot refresh state without first migrating the state"));
            assert!(!error.contains("Cannot apply without first migrating the state"));
        }

        #[test]
        fn drift_error_renders_copy_pasteable_indented_command() {
            let old =
                BackendLock::for_config(Some(&local_backend_config("legacy/state.json"))).unwrap();
            let new = BackendLock::for_config(Some(&local_backend_config("state.json"))).unwrap();

            let error =
                drift_error_message(DriftCommand::Apply, &old, &new, Path::new("infra/foo"));
            let command = error
                .lines()
                .find(|line| line.trim_start().starts_with("carina init --migrate-state"))
                .unwrap();

            assert_eq!(command, "    carina init --migrate-state infra/foo");
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
    fn backend_validation_keeps_the_typed_error() {
        let user_dirs = carina_core::TestUserDirs::new();
        let raw = carina_core::parser::parse_file(
            r#"backend s3 {
                bucket = "state"
                key = "state.json"
                region = true
            }"#,
            std::path::Path::new("backend.crn"),
            &ProviderContext::default(),
        )
        .unwrap();
        let mut parsed = empty_parsed_file();
        parsed.backend = raw.backend;
        let base = tempfile::tempdir().unwrap();
        let providers = ProviderLoad::load(&user_dirs, &parsed.providers, base.path()).unwrap();

        let errors =
            validate_and_resolve_errors(&mut parsed, base.path(), &providers, false, &[], &[]);

        assert!(errors.iter().any(|error| {
            matches!(
                error,
                AppError::Backend(BackendError::InvalidConfig(
                    carina_state::BackendConfigError::Invalid(errors)
                )) if errors.errors().iter().any(|error| matches!(
                    error,
                    carina_state::BackendAttributeError::InvalidAttribute { attribute, .. }
                        if attribute == "region"
                ))
            )
        }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_provider_load_error_shows_actual_reason() {
        let user_dirs = carina_core::TestUserDirs::new();
        let base = tempfile::tempdir().unwrap();
        let source_path = base.path().join("local-provider.wasm");
        let installed_path = base
            .path()
            .join(".carina/providers/file/local-provider/local-provider.wasm");
        std::fs::create_dir_all(installed_path.parent().unwrap()).unwrap();
        std::fs::write(&installed_path, b"not a wasm component").unwrap();

        let mut parsed = empty_parsed_file();
        parsed.providers.push(ProviderConfig {
            name: "fakeprovider".to_string(),
            source: Some(format!("file://{}", source_path.display()).parse().unwrap()),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        });

        let providers = ProviderLoad::load(&user_dirs, &parsed.providers, base.path()).unwrap();
        let error =
            validate_and_resolve_with_config(&mut parsed, base.path(), &providers, false, &[], &[])
                .unwrap_err();
        let message = error.to_string();

        assert!(
            message.contains("Failed to load WASM component"),
            "{message}"
        );
        assert!(!message.contains("Run `carina init`"), "{message}");
    }

    #[test]
    fn missing_provider_source_hint_advertises_registry_and_github_forms() {
        assert_eq!(
            missing_provider_source_message("aws"),
            "Provider 'aws' has no source configured. Add `source = 'carina-rs/aws'` for a registry provider, or `source = 'github.com/owner/repo'` for a direct GitHub source."
        );
    }

    #[test]
    fn test_provider_without_source_shows_source_hint() {
        let user_dirs = carina_core::TestUserDirs::new();
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
        });
        let base_dir = std::path::Path::new("/tmp/nonexistent-carina-test");
        let providers = ProviderLoad::load(&user_dirs, &parsed.providers, base_dir).unwrap();
        let result =
            validate_and_resolve_with_config(&mut parsed, base_dir, &providers, false, &[], &[]);

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
        let user_dirs = carina_core::TestUserDirs::new();
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
        let inference_errors = loaded.inference_errors;
        let duplicate_declarations = loaded.duplicate_declarations;
        let mut parsed = loaded.parsed;
        let providers = ProviderLoad::load(&user_dirs, &parsed.providers, &base).unwrap();

        // With full validation, the typo is rejected.
        let err = validate_and_resolve_with_config(
            &mut parsed.clone(),
            &base,
            &providers,
            false,
            &inference_errors,
            &duplicate_declarations,
        )
        .expect_err("full validation must flag the typo");
        assert!(err.to_string().contains("does not export `missing`"));

        // With skip_resource_validation=true, recovery commands pass
        // through despite the typo.
        let result = validate_and_resolve_with_config(
            &mut parsed,
            &base,
            &providers,
            true,
            &inference_errors,
            &duplicate_declarations,
        );
        assert!(
            result.is_ok(),
            "skip_resource_validation=true must bypass upstream field check, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn duplicate_declaration_check_honors_skip_resource_validation() {
        let user_dirs = carina_core::TestUserDirs::new();
        let base = tempfile::tempdir().unwrap();
        std::fs::write(
            base.path().join("a.crn"),
            "exports {\n  x = \"from-a\"\n}\n",
        )
        .unwrap();
        std::fs::write(
            base.path().join("b.crn"),
            "exports {\n  x = \"from-b\"\n}\n",
        )
        .unwrap();

        let loaded = carina_core::config_loader::load_configuration_with_config(
            base.path(),
            &ProviderContext::default(),
            &carina_core::schema::SchemaRegistry::new(),
        )
        .expect("load");
        let inference_errors = loaded.inference_errors;
        let duplicate_declarations = loaded.duplicate_declarations;
        let mut parsed = loaded.parsed;
        let providers = ProviderLoad::load(&user_dirs, &parsed.providers, base.path()).unwrap();

        let err = validate_and_resolve_with_config(
            &mut parsed.clone(),
            base.path(),
            &providers,
            false,
            &inference_errors,
            &duplicate_declarations,
        )
        .expect_err("full validation must reject duplicate declarations");
        assert!(err.to_string().contains("duplicate export 'x'"));

        let result = validate_and_resolve_with_config(
            &mut parsed,
            base.path(),
            &providers,
            true,
            &inference_errors,
            &duplicate_declarations,
        );
        assert!(
            result.is_ok(),
            "skip_resource_validation=true must bypass duplicate declarations, got: {:?}",
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

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "ap-northeast-1".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
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
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> carina_core::provider::BoxFuture<
            '_,
            carina_core::provider::ProviderResult<
                Box<dyn carina_core::provider::ProviderNormalizer>,
            >,
        > {
            Box::pin(async {
                Ok(Box::new(carina_core::provider::NoopNormalizer)
                    as Box<dyn carina_core::provider::ProviderNormalizer>)
            })
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
                vec![
                    carina_core::schema::ResourceSchema::new("ec2.Subnet").attribute(
                        carina_core::schema::AttributeSchema::optional(
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
                        .create_only()
                        .build(),
                    ),
                ]
            };
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    fn resource_create_only_enum_file(raw_region: &str) -> carina_core::parser::InferredFile {
        let mut resource =
            carina_core::resource::Resource::pending_with_provider("awscc", "ec2.Subnet", None);
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

        let providers = ProviderLoad::for_test(
            crate::wiring::LoadedProviderFactory::for_test(vec![Box::new(
                ResourceCreateOnlyEnumFactory {
                    enum_provider: "awscc",
                },
            )]),
            &awscc.providers,
        );
        let errors =
            validate_and_resolve_errors(&mut awscc, base_dir.path(), &providers, false, &[], &[]);
        assert!(errors.is_empty(), "awscc spelling errors: {errors:?}");

        let providers = ProviderLoad::for_test(
            crate::wiring::LoadedProviderFactory::for_test(vec![Box::new(
                ResourceCreateOnlyEnumFactory {
                    enum_provider: "aws",
                },
            )]),
            &aws.providers,
        );
        let errors =
            validate_and_resolve_errors(&mut aws, base_dir.path(), &providers, false, &[], &[]);
        assert!(errors.is_empty(), "aws spelling errors: {errors:?}");

        assert_eq!(
            awscc.resources[0]
                .id
                .identity_str()
                .expect("resolved identity"),
            aws.resources[0]
                .id
                .identity_str()
                .expect("resolved identity"),
            "resource create-only enum spelling must canonicalize before anonymous hash"
        );
    }
}
