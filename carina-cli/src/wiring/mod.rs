use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::io::IsTerminal;
use std::path::Path;

use indexmap::IndexMap;
use std::sync::Arc;
use std::time::Duration;

use colored::Colorize;
use futures::stream::{self, StreamExt};

use carina_core::binding_index::{PreApplyInputs, ResolvedBindings, WaitAliasSpec};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{
    Orphan, Orphans, PlanPriorStates, binding_matches_deferred_template, into_all_prior_states,
    lift_prior_state_enum_leaves, provider_states_for, rekey_current_for_move,
    transform_prior_provider_states,
};
use carina_core::differ::{block_deletes_on_prior_consumer_updates, create_plan_with_cascades};
use carina_core::effect::{
    DeferredReplaceDelete, DeferredReplacePayload, Effect, EffectGeneration, MoveAuthority,
    NonEmptyDeletes, ResolvedMove,
};
use carina_core::executor::normalized::{
    apply_desired_normalization_slice, is_value_fully_concrete_for_expansion,
    states_contain_unknown,
};
use carina_core::executor::{
    ProviderReadyConfig, UnresolvedDataSourceInput, prepare_provider_ready_config,
    unresolved_data_source_inputs,
};
use carina_core::identifier::{
    self, AnonymousIdBindingStateInfo, AnonymousIdStateInfo, PrefixStateInfo, StateBlockClaims,
};
use carina_core::module_resolver;
use carina_core::override_aware::{OverrideAwareResources, RebuildBindingsInputs};
#[cfg(test)]
use carina_core::parser::MovedAddresses;
use carina_core::parser::{
    ProviderConfig, ResolvedProviderConfig, ResolvedProviderConfigs, StateBlock, StateBlockAddress,
    WarningKind,
};
use carina_core::plan::{Plan, PlanError, PlanErrorKind};
use carina_core::provider::{
    self as provider_mod, AppliedIdentifier, LiftedSavedAttrs, Provider, ProviderError,
    ProviderFactory, ProviderNormalizer, ProviderRouter,
};
use carina_core::resource::{
    Composition, ConcreteValue, Current, DataSource, DeferredValue, ResolvedResource,
    ResolvedResourceId, Resource, ResourceId, ResourceIdentity, ResourceIdentityError, State,
    Value,
};
#[cfg(test)]
use carina_core::schema::{AttributeSchema, AttributeType};
use carina_core::schema::{
    CustomTypeLookup, ResourceSchema, SchemaRegistry, TypeError, resolve_block_names,
};
use carina_core::validation;
use carina_provider_mock::{BUILTIN_PROVIDER_NAME, MockProvider};
use carina_state::{BackendError, LoadedOrphans, StateFile};

use crate::commands::shared::progress::{RefreshProgress, refresh_multi_progress};
use crate::commands::split_validation_message;
use crate::error::AppError;
use crate::module_walk::ModuleWalk;

/// Result of creating a plan, with context needed for saving
pub struct PlanContext {
    pub plan: Plan,
    pub provider: ProviderRouter,
    pub sorted_resources: Vec<Resource>,
    pub unresolved_resources: Vec<Resource>,
    /// Module compositions after plan-time constraint evaluation. Pending
    /// constraints must survive in saved plans for apply-time re-evaluation.
    pub compositions: Vec<Composition>,
    /// Data sources exactly as supplied to the differ. Refresh-time reads
    /// have concrete inputs; apply-time deferred reads keep structural refs so
    /// saved-plan apply can reconstruct the same deferred-read set.
    pub data_sources: Vec<DataSource>,
    pub managed_current: HashMap<ResourceId, Current>,
    pub data_source_states: HashMap<ResourceId, State>,
    /// Maps moved-to resource IDs to their original (moved-from) IDs.
    /// Used by display to show "(moved from: ...)" annotations on Update/Replace effects.
    pub moved_origins: HashMap<ResourceId, ResourceId>,
    /// Snapshot of `upstream_state` bindings as resolved at plan time.
    /// Persisted to the plan file (#2303) so apply-from-plan can verify
    /// the upstream values have not drifted before re-using them for
    /// cascade re-resolution. Empty when the configuration declares no
    /// `upstream_state` blocks.
    pub upstream_snapshot: HashMap<String, HashMap<String, carina_core::resource::Value>>,
    /// Per-resource user-authoring trees lifted from the saved state.
    /// Forwarded to the display layer so server-side default fields the
    /// user never wrote do not surface in plan output (refs awscc#206).
    pub prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    /// Deferred-for loops still unresolved after the post-refresh
    /// expansion (carina#3132). The iterable is genuinely unknowable at
    /// plan time (e.g. depends on a not-yet-created resource); the loop
    /// legitimately stays deferred and is rendered as the carina#3128
    /// validate/plan placeholder. Replaces the pre-refresh
    /// `parsed.deferred_for_expressions` the caller used to pass to
    /// `print_plan` — expansion no longer mutates `parsed`.
    pub residual_deferred_for: Vec<carina_core::parser::DeferredForExpression>,
    /// Plan-scoped lineage of leaves back to the composition call sites
    /// that produced them (#3306, #3307). Forwarded to the display
    /// layer so the rendered tree folds leaves under composition rows.
    pub expansion_trace: carina_core::resource::ExpansionTrace,
}

/// State-block targets resolved while constructing [`StateBlockClaims`].
///
/// This keeps plan-time collision checks on the same effectiveness
/// resolution as the heuristic-claim pass. In particular, `removed`
/// targets are resolved here once and then consumed by the RC3 predicate.
#[derive(Debug, Clone, Default)]
pub struct ResolvedStateBlockTargets {
    pub removed_from: Vec<ResourceId>,
}

#[derive(Debug, Clone)]
pub struct StateBlockResolution {
    pub claims: StateBlockClaims,
    pub targets: ResolvedStateBlockTargets,
}

/// Cached provider factories and schemas, constructed once per CLI invocation.
///
/// Instead of calling `provider_factories()` and `get_schemas()` at each call
/// site (which rebuilds the full schema set every time), create a single
/// `WiringContext` and pass it through the command execution path.
pub struct WiringContext {
    factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    schemas: SchemaRegistry,
    known_providers: HashSet<String>,
    uses_builtin_mock: bool,
}

/// Provider factories paired with provider-specific load diagnostics.
pub type ProviderFactoryLoad = (Vec<Box<dyn ProviderFactory>>, HashMap<String, String>);

#[derive(Debug)]
pub(crate) enum ProviderFactoryDiagnostic {
    LockConstraint(carina_provider_resolver::LockConstraintError),
    Message(String),
}

impl fmt::Display for ProviderFactoryDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LockConstraint(error) => error.fmt(f),
            Self::Message(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ProviderFactoryDiagnostic {}

impl From<String> for ProviderFactoryDiagnostic {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

pub(crate) type FormattingFactoryLoad = (
    Vec<Box<dyn ProviderFactory>>,
    HashMap<String, ProviderFactoryDiagnostic>,
);

impl WiringContext {
    pub fn new(
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_configs: &[ProviderConfig],
    ) -> Self {
        Self::new_with_additional_schemas(factories, provider_configs, std::iter::empty())
    }

    pub(crate) fn new_with_additional_schemas(
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_configs: &[ProviderConfig],
        additional_schemas: impl IntoIterator<Item = (String, ResourceSchema)>,
    ) -> Self {
        let uses_builtin_mock =
            carina_provider_mock::uses_builtin_provider(provider_configs, |provider_name| {
                factories
                    .iter()
                    .any(|factory| factory.name() == provider_name)
            });
        let mut schemas = SchemaRegistry::new();
        for (provider, schema) in additional_schemas {
            schemas.insert(provider, schema);
        }
        for factory in &factories {
            for schema in factory.schemas() {
                schemas.insert(factory.name(), schema);
            }
        }
        if uses_builtin_mock {
            for schema in carina_provider_mock::builtin_schemas() {
                let already_registered = if schema.is_data_source() {
                    schemas.has_data_source(BUILTIN_PROVIDER_NAME, &schema.resource_type)
                } else {
                    schemas.has_managed(BUILTIN_PROVIDER_NAME, &schema.resource_type)
                };
                if !already_registered {
                    schemas.insert(BUILTIN_PROVIDER_NAME, schema);
                }
            }
        }
        let mut known_providers = factories
            .iter()
            .map(|factory| factory.name().to_string())
            .collect::<HashSet<_>>();
        if uses_builtin_mock {
            known_providers.insert(BUILTIN_PROVIDER_NAME.to_string());
        }
        Self {
            factories: Arc::new(factories),
            schemas,
            known_providers,
            uses_builtin_mock,
        }
    }

    pub fn factories(&self) -> &[Box<dyn ProviderFactory>] {
        &self.factories
    }

    pub fn factories_arc(&self) -> Arc<Vec<Box<dyn ProviderFactory>>> {
        Arc::clone(&self.factories)
    }

    pub fn schemas(&self) -> &SchemaRegistry {
        &self.schemas
    }

    pub fn known_providers(&self) -> &HashSet<String> {
        &self.known_providers
    }

    fn uses_builtin_mock(&self) -> bool {
        self.uses_builtin_mock
    }
}

/// Build provider factories from provider configs that have a `source` attribute.
///
/// For each provider with a `source`, resolves the WASM component path and creates a
/// `WasmProviderFactory`. Providers without `source` are skipped (handled
/// later in `get_provider_with_ctx`).
///
/// Returns `(factories, load_errors)` where `load_errors` maps provider names
/// to their failure reasons, so callers can show accurate diagnostics. A stale
/// lock constraint is returned to the supervised command rather than exiting.
pub fn build_factories_from_providers(
    providers: &[ProviderConfig],
    base_dir: &Path,
) -> Result<ProviderFactoryLoad, AppError> {
    carina_provider_resolver::validate_lock_constraints(base_dir, providers)?;

    Ok(build_factories_from_providers_impl(
        providers, base_dir, true,
    ))
}

/// Best-effort factory loading for schema-aware formatting.
///
/// Provider resolution is optional on this path: a missing, invalid, or stale
/// provider installation must fall back to plain formatting rather than make
/// `carina fmt` fail or emit a hard provider error.
/// Suppressed provider and lock-constraint failures are returned as load errors
/// so `--check` can report degraded conversion without failing formatting.
pub(crate) fn build_factories_from_providers_for_formatting(
    providers: &[ProviderConfig],
    base_dir: &Path,
) -> FormattingFactoryLoad {
    if let Err(error) = carina_provider_resolver::validate_lock_constraints(base_dir, providers) {
        return (
            Vec::new(),
            HashMap::from([(
                "carina-providers.lock".to_string(),
                ProviderFactoryDiagnostic::LockConstraint(error),
            )]),
        );
    }

    let (factories, load_errors) = build_factories_from_providers_impl(providers, base_dir, false);
    (
        factories,
        load_errors
            .into_iter()
            .map(|(provider, message)| (provider, ProviderFactoryDiagnostic::Message(message)))
            .collect(),
    )
}

fn build_factories_from_providers_impl(
    providers: &[ProviderConfig],
    base_dir: &Path,
    report_errors: bool,
) -> (Vec<Box<dyn ProviderFactory>>, HashMap<String, String>) {
    let mut factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
    let mut load_errors: HashMap<String, String> = HashMap::new();

    for config in providers {
        let source = match &config.source {
            Some(s) => s.clone(),
            None => continue,
        };

        let installed = if source.starts_with("file://") || source.starts_with("github.com/") {
            match carina_provider_resolver::find_installed_provider(base_dir, config) {
                Ok(installed) => installed,
                Err(e) => {
                    let reason = format!("Provider '{}' {}", config.name, e);
                    if report_errors {
                        eprintln!("{}", reason.red());
                    }
                    load_errors.insert(config.name.clone(), reason);
                    continue;
                }
            }
        } else {
            let reason = format!(
                "Unsupported source format for provider '{}': {}. Use file:// or github.com/owner/repo.",
                config.name, source
            );
            if report_errors {
                eprintln!("{}", reason.red());
            }
            load_errors.insert(config.name.clone(), reason);
            continue;
        };

        if !carina_provider_resolver::is_wasm_provider(installed.path()) {
            let reason = format!(
                "Provider '{}': native binaries are no longer supported. Use a .wasm component instead.",
                config.name
            );
            if report_errors {
                eprintln!("{}", reason.red());
            }
            load_errors.insert(config.name.clone(), reason);
            continue;
        }

        let factory_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(load_installed_wasm_provider(&installed))
        });

        match factory_result {
            Ok(factory) => {
                if let Some(constraint) = &config.version
                    && let Err(message) = factory.verify_version(&constraint.raw)
                {
                    let reason = installed
                        .with_load_error(carina_plugin_host::WasmProviderLoadError::Other(message))
                        .to_string();
                    if report_errors {
                        eprintln!("{}", reason.red());
                    }
                    load_errors.insert(config.name.clone(), reason);
                    continue;
                }
                factories.push(Box::new(factory));
            }
            Err(error) => {
                // Final UI boundary: `ProviderArtifactLoadError::Display`
                // renders the structured host chain first, followed by the
                // resolver-owned provenance and mode-accurate hint.
                let reason = error.to_string();
                if report_errors {
                    eprintln!("{}", reason.red());
                }
                load_errors.insert(config.name.clone(), reason);
            }
        }
    }

    (factories, load_errors)
}

async fn load_installed_wasm_provider(
    installed: &carina_provider_resolver::InstalledProvider,
) -> Result<
    carina_plugin_host::WasmProviderFactory,
    carina_provider_resolver::ProviderArtifactLoadError<carina_plugin_host::WasmProviderLoadError>,
> {
    load_installed_wasm_provider_using(installed, carina_plugin_host::WasmProviderFactory::new)
        .await
}

async fn load_installed_wasm_provider_using<F, Fut>(
    installed: &carina_provider_resolver::InstalledProvider,
    loader: F,
) -> Result<
    carina_plugin_host::WasmProviderFactory,
    carina_provider_resolver::ProviderArtifactLoadError<carina_plugin_host::WasmProviderLoadError>,
>
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: std::future::Future<
            Output = Result<
                carina_plugin_host::WasmProviderFactory,
                carina_plugin_host::WasmProviderLoadError,
            >,
        >,
{
    // Injection keeps wiring regression tests out of the user's default precompile cache.
    let factory = installed.load_with(loader).await?;
    let runtime_artifact = installed.clone();
    Ok(
        factory.with_runtime_instantiation_error_mapper(move |error| {
            runtime_instantiation_error_with_provenance(&runtime_artifact, error)
        }),
    )
}

fn runtime_instantiation_error_with_provenance<E>(
    installed: &carina_provider_resolver::InstalledProvider,
    error: E,
) -> ProviderError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ProviderError::internal_caused_by(
        "Failed to instantiate WASM provider runtime instance",
        installed.with_load_error(error),
    )
}

/// Lift a core-validation `Result<(), String>` into per-finding
/// `AppError::Validation` entries. The underlying `validation::*`
/// helpers join every finding with `\n`, so splitting here lets
/// callers surface each finding as its own `AppError`.
fn lift_validation_result(res: Result<(), String>) -> Vec<AppError> {
    let Err(joined) = res else {
        return Vec::new();
    };
    split_validation_message(&joined)
}

fn prefixed_validation_messages(joined: &str, prefix: impl std::fmt::Display) -> Vec<AppError> {
    split_validation_message(joined)
        .into_iter()
        .map(|error| {
            let AppError::Validation(message) = error else {
                unreachable!("split_validation_message only creates validation errors")
            };
            AppError::Validation(format!("{prefix}: {message}"))
        })
        .collect()
}

pub fn validate_resources_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
    provider_context: &carina_core::parser::ProviderContext,
) -> Vec<AppError> {
    lift_validation_result(validation::validate_resources(
        parsed,
        ctx.schemas(),
        ctx.known_providers(),
        provider_context,
    ))
}

/// Surface `directives.depends_on` analysis-pass error diagnostics as
/// `AppError::Validation`. Warnings are emitted to stderr (no
/// AppError::Warning variant exists today) so they don't fail the
/// command but remain visible.
pub fn validate_depends_on_with_ctx<E>(parsed: &carina_core::parser::File<E>) -> Vec<AppError> {
    use carina_core::validation::depends_on::{Severity, validate_depends_on};
    let mut errors = Vec::new();
    for diag in validate_depends_on(parsed) {
        match diag.severity {
            Severity::Error => errors.push(AppError::Validation(diag.message)),
            Severity::Warning => eprintln!("warning: {}", diag.message),
        }
    }
    errors
}

/// Surface `wait <target> { ... }` diagnostics as `AppError::Validation`.
/// Shared with the LSP via the underlying
/// `carina_core::validation::wait::validate_wait_bindings` pass.
pub fn validate_wait_bindings_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
) -> Vec<AppError> {
    carina_core::validation::wait::validate_wait_bindings(parsed, ctx.schemas())
        .into_iter()
        .map(|d| AppError::Validation(d.message))
        .collect()
}

/// Surface deferred-populate-bound chained references that lack a
/// synchronizing `wait` block as `AppError::Validation`. Shared with
/// the LSP via the underlying
/// `carina_core::validation::deferred_populate::validate_deferred_populate_refs`
/// pass. carina#3034.
pub fn validate_deferred_populate_refs_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
) -> Vec<AppError> {
    carina_core::validation::deferred_populate::validate_deferred_populate_refs(
        parsed,
        ctx.schemas(),
    )
    .into_iter()
    .map(|d| AppError::Validation(d.message))
    .collect()
}

pub fn validate_resource_ref_types_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
    argument_names: &HashSet<String>,
) -> Vec<AppError> {
    let bindings = carina_core::binding_index::BindingIndex::from_parsed(parsed, ctx.schemas());
    lift_validation_result(validation::validate_resource_ref_types(
        parsed,
        ctx.schemas(),
        argument_names,
        &bindings,
    ))
}

pub fn validate_composition_ref_types_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
) -> Vec<AppError> {
    let bindings = carina_core::binding_index::BindingIndex::from_parsed(parsed, ctx.schemas());
    validation::validate_composition_ref_types_with_bindings(&parsed.compositions, &bindings)
        .into_iter()
        .map(|error| AppError::Validation(error.to_string()))
        .collect()
}

pub fn validate_attribute_param_ref_types_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
) -> Vec<AppError> {
    let bindings = carina_core::binding_index::BindingIndex::from_parsed(parsed, ctx.schemas());
    lift_validation_result(
        validation::validate_attribute_param_ref_types_with_bindings(
            &parsed.attribute_params,
            &bindings,
        ),
    )
}

/// Reject any resolved value that still carries
/// `Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation))`. The parser
/// accepts mid-edit `${}` to keep the AST intact (#2480) and the LSP
/// surfaces it as a per-location warning, but `carina validate` /
/// `plan` / `apply` must refuse to proceed — letting the marker flow
/// to a provider would render the literal text `${}` (or worse, an
/// empty substitution) into a real API call. See #2487.
pub fn validate_no_empty_interpolations<E>(parsed: &carina_core::parser::File<E>) -> Vec<AppError>
where
    E: carina_core::parser::ExportParamLike,
{
    let mut errors = Vec::new();
    // carina#3181: walk every top-level resource — managed, data source,
    // composition — so an empty interpolation in a `read` resource's
    // attribute is still caught.
    for rref in parsed.iter_top_level_resources() {
        let attrs = rref.attributes();
        for (attr_name, value) in attrs.iter() {
            if value_contains_empty_interpolation(value) {
                errors.push(AppError::Validation(format!(
                    "{}: attribute `{}`: empty interpolation `${{}}` — fill in the expression or remove it",
                    rref.id(), attr_name
                )));
            }
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = export.value()
            && value_contains_empty_interpolation(value)
        {
            errors.push(AppError::Validation(format!(
                "exports `{}`: empty interpolation `${{}}` — fill in the expression or remove it",
                export.name()
            )));
        }
    }
    for param in &parsed.attribute_params {
        if let Some(value) = &param.value
            && value_contains_empty_interpolation(value)
        {
            errors.push(AppError::Validation(format!(
                "attributes `{}` default: empty interpolation `${{}}` — fill in the expression or remove it",
                param.name
            )));
        }
    }
    errors
}

/// Recursively walk a `Value` tree looking for any
/// `Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation))`. Returns `true`
/// when one is found at any depth — inside lists, maps, secrets,
/// function-call arguments, or as the `Expr` segment of an
/// `Interpolation`. Mirrors the variant coverage of the core
/// provider-boundary strip pass to keep them in lockstep when new
/// `Value` variants land.
fn value_contains_empty_interpolation(value: &Value) -> bool {
    use carina_core::resource::{InterpolationPart, UnknownReason};
    match value {
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)) => true,
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|p| match p {
            InterpolationPart::Expr(v) => value_contains_empty_interpolation(v),
            InterpolationPart::Literal(_) => false,
        }),
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(value_contains_empty_interpolation)
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            entries.values().any(value_contains_empty_interpolation)
        }
        Value::Deferred(DeferredValue::Secret(inner)) => value_contains_empty_interpolation(inner),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args.iter().any(|argument| {
            argument
                .value()
                .is_some_and(value_contains_empty_interpolation)
        }),
        Value::Deferred(DeferredValue::Access(access)) => {
            value_contains_empty_interpolation(access.base())
        }
        _ => false,
    }
}

/// Resolve block name aliases and attribute prefixes in one step.
pub fn resolve_names_with_ctx(ctx: &WiringContext, resources: &mut [Resource]) -> Vec<AppError> {
    let mut errors = lift_validation_result(resolve_block_names(resources, ctx.schemas()));
    errors.extend(resolve_attr_prefixes_with_ctx(ctx, resources));
    errors
}

pub fn resolve_attr_prefixes_with_ctx(
    ctx: &WiringContext,
    resources: &mut [Resource],
) -> Vec<AppError> {
    lift_validation_result(identifier::resolve_attr_prefixes(resources, ctx.schemas()))
}

pub fn reconcile_prefixed_names(resources: &mut [Resource], state_file: &Option<StateFile>) {
    let state_file = match state_file {
        Some(sf) => sf,
        None => return,
    };

    identifier::reconcile_prefixed_names(resources, &|provider, resource_type, name| {
        let sr = state_file.find_resource(provider, resource_type, name)?;
        let persisted = sr.persisted_attributes()?;
        let mut attribute_values = persisted
            .plain_string_values()
            .into_iter()
            .map(|(name, value)| (name, identifier::PrefixStateValue::Known(value)))
            .collect::<HashMap<_, _>>();
        if let Some(marker) = sr.partial_read() {
            for attribute in &marker.missing_attributes {
                attribute_values
                    .entry(attribute.clone())
                    .or_insert_with(|| identifier::PrefixStateValue::PostCreateReadIncomplete {
                        detail: marker.detail.clone(),
                    });
            }
        }
        Some(PrefixStateInfo {
            prefixes: sr.prefixes.clone(),
            attribute_values,
        })
    });
}

/// Look up a provider's identity attributes through its factory.
fn identity_attributes_for_provider(ctx: &WiringContext, name: &str) -> Vec<String> {
    provider_mod::find_factory(ctx.factories(), name)
        .map(|f| {
            f.identity_attributes()
                .into_iter()
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn provider_config_attribute_type_for(
    ctx: &WiringContext,
    provider_name: &str,
    attr_name: &str,
) -> Option<carina_core::schema::AttributeType> {
    provider_mod::find_factory(ctx.factories(), provider_name)
        .and_then(|factory| factory.provider_config_attribute_types().remove(attr_name))
}

/// Detect and apply anonymous → let-bound resource renames.
///
/// Mirrors `materialize_moved_states` but for synthetic rename pairs produced
/// by `identifier::detect_anonymous_to_named_renames`. Transfers state,
/// `prev_explicit`, and `saved_attrs` from the old anonymous name to the
/// new binding name so the differ sees the resource under its new identity.
#[allow(clippy::too_many_arguments)]
pub fn apply_anonymous_to_named_renames(
    ctx: &WiringContext,
    resources: &[Resource],
    providers: &[ProviderConfig],
    managed_current: &mut HashMap<ResourceId, Current>,
    orphans: &mut Orphans,
    prev_explicit: &mut HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    saved_attrs: &mut LiftedSavedAttrs,
    state_file: &Option<StateFile>,
    claims: &StateBlockClaims,
) -> Result<Vec<(ResolvedResourceId, ResolvedResourceId)>, AppError> {
    let Some(sf) = state_file.as_ref() else {
        return Ok(Vec::new());
    };

    let canonical_providers =
        carina_core::value::canonicalize_provider_configs_with_attribute_types(
            providers,
            &|provider_name, attr_name| {
                provider_config_attribute_type_for(ctx, provider_name, attr_name)
            },
        )?;

    let renames = identifier::detect_anonymous_to_named_renames(
        resources,
        ctx.schemas(),
        &|provider, resource_type| {
            let create_only_attrs = ctx
                .schemas()
                .get(
                    provider,
                    resource_type,
                    carina_core::schema::SchemaKind::Resource,
                )
                .map(|s| s.create_only_attributes())
                .unwrap_or_default();
            let state_entries = sf
                .resources_by_type(provider, resource_type)
                .into_iter()
                .filter_map(|sr| {
                    let persisted = sr.persisted_attributes()?;
                    let create_only_values = create_only_attrs
                        .iter()
                        .filter_map(|attr| {
                            persisted.get(attr).and_then(|value| {
                                identifier::canonical_create_only_value_string(&value)
                                    .map(|s| (attr.to_string(), s))
                            })
                        })
                        .collect();
                    Some(AnonymousIdStateInfo {
                        name: sr.identity.to_string(),
                        create_only_values,
                    })
                });
            claims.screen_entries(provider, resource_type, state_entries, |entry| {
                entry.name.as_str()
            })
        },
        &canonical_providers,
        &|name| identity_attributes_for_provider(ctx, name),
        claims,
    );

    for resolved_move in &renames {
        let from = resolved_move.from();
        let to = resolved_move.to();
        let authority = resolved_move.authority();
        promote_orphan_or_move_current_state(managed_current, orphans, &authority)?;
        if let Some(keys) = prev_explicit.remove(from.as_inner()) {
            prev_explicit.insert(to.as_inner().clone(), keys);
        }
        saved_attrs.remap_resource_id(from.as_inner(), to.as_inner().clone());
    }

    Ok(renames.into_iter().map(ResolvedMove::into_parts).collect())
}

pub fn reconcile_anonymous_identifiers_with_ctx(
    ctx: &WiringContext,
    resources: &mut [Resource],
    data_sources: &[DataSource],
    state_file: &mut StateFile,
    claims: &StateBlockClaims,
) -> Result<(), BackendError> {
    let state_by_binding: HashMap<String, Vec<AnonymousIdBindingStateInfo>> = state_file
        .resources()
        .iter()
        .filter_map(|sr| {
            let binding = sr.binding.as_ref()?;
            let persisted = sr.persisted_attributes()?;
            Some((
                binding.clone(),
                AnonymousIdBindingStateInfo {
                    name: sr.identity.to_string(),
                    attribute_values: persisted
                        .to_values()
                        .into_iter()
                        .filter_map(|(attr, value)| {
                            identifier::canonical_create_only_value_string(&value)
                                .map(|s| (attr.clone(), s))
                        })
                        .collect(),
                },
            ))
        })
        .fold(HashMap::new(), |mut acc, (binding, info)| {
            acc.entry(binding).or_default().push(info);
            acc
        });

    let renames = identifier::reconcile_anonymous_identifiers(
        resources,
        data_sources,
        ctx.schemas(),
        &|provider, resource_type| {
            let create_only_attrs = ctx
                .schemas()
                .get(
                    provider,
                    resource_type,
                    carina_core::schema::SchemaKind::Resource,
                )
                .map(|s| s.create_only_attributes())
                .unwrap_or_default();

            let state_entries = state_file
                .resources_by_type(provider, resource_type)
                .into_iter()
                .filter_map(|sr| {
                    let persisted = sr.persisted_attributes()?;
                    let create_only_values = create_only_attrs
                        .iter()
                        .filter_map(|attr| {
                            persisted.get(attr).and_then(|value| {
                                identifier::canonical_create_only_value_string(&value)
                                    .map(|s| (attr.to_string(), s))
                            })
                        })
                        .collect();
                    Some(AnonymousIdStateInfo {
                        name: sr.identity.to_string(),
                        create_only_values,
                    })
                });
            claims.screen_entries(provider, resource_type, state_entries, |entry| {
                entry.name.as_str()
            })
        },
        &|binding| state_by_binding.get(binding).cloned().unwrap_or_default(),
        claims,
    );

    state_file.rename_resource_identities(&renames)
}

pub(crate) struct LateAnonymousIdentityInputs<'a> {
    pub resources: &'a mut OverrideAwareResources,
    pub data_sources: &'a [DataSource],
    pub state_file: Option<&'a StateFile>,
    pub state_block_claims: &'a StateBlockClaims,
    pub providers: &'a [ProviderConfig],
}

/// Desired resource identities captured only after late identity resolution.
pub struct ResolvedDesiredIds {
    ids: HashSet<ResolvedResourceId>,
}

impl ResolvedDesiredIds {
    fn from_resources(resources: &[Resource]) -> Result<Self, AppError> {
        let ids = ResolvedResource::collect_resolved(resources.iter().cloned())?
            .into_iter()
            .map(|resource| resource.resolved_id())
            .collect();
        Ok(Self { ids })
    }
}

pub(crate) fn reconcile_late_anonymous_identities(
    ctx: &WiringContext,
    inputs: LateAnonymousIdentityInputs<'_>,
) -> Result<ResolvedDesiredIds, AppError> {
    let canonical_resources = carina_core::value::canonicalize_resources_with_schemas(
        inputs.resources.resources_mut(),
        ctx.schemas(),
    )?;
    let errors = compute_anonymous_identifiers_with_ctx(ctx, canonical_resources, inputs.providers);
    if let Some(error) = errors.into_iter().next() {
        return Err(error);
    }

    if let Some(sf) = inputs.state_file {
        let mut state_for_late_reconcile = sf.clone();
        reconcile_anonymous_identifiers_with_ctx(
            ctx,
            inputs.resources.resources_mut(),
            inputs.data_sources,
            &mut state_for_late_reconcile,
            inputs.state_block_claims,
        )?;
    }

    let unavailable_schema_errors = validation::validate_anonymous_resource_schemas(
        inputs.resources.resources(),
        ctx.schemas(),
        ctx.known_providers(),
    );
    if !unavailable_schema_errors.is_empty() {
        return Err(AppError::Config(
            unavailable_schema_errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }

    // Validate the resolved-key invariant at this seam so two separately
    // scoped module leaves that map to the same state-visible identity return
    // the existing typed error instead of reaching map construction or
    // execution as duplicates.
    inputs.resources.unresolved_by_resolved_id()?;

    ResolvedDesiredIds::from_resources(inputs.resources.resources())
}

pub fn compute_anonymous_identifiers_with_ctx(
    ctx: &WiringContext,
    mut resources: carina_core::value::CanonicalizedResources<'_>,
    providers: &[ProviderConfig],
) -> Vec<AppError> {
    let canonical_providers =
        match carina_core::value::canonicalize_provider_configs_with_attribute_types(
            providers,
            &|provider_name, attr_name| {
                provider_config_attribute_type_for(ctx, provider_name, attr_name)
            },
        ) {
            Ok(providers) => providers,
            Err(error) => return vec![error.into()],
        };

    match identifier::compute_anonymous_identifiers_with_provider_configs(
        resources.as_mut_slice(),
        &canonical_providers,
        ctx.schemas(),
        &|name| identity_attributes_for_provider(ctx, name),
    ) {
        Ok(()) => Vec::new(),
        Err(msg) => vec![AppError::Config(msg)],
    }
}

/// Encapsulates the plan-time normalization pipeline.
///
/// Ensures the correct ordering of normalization steps that must run
/// between reference resolution and plan creation:
///
/// 1. Host schema canonicalization resolves desired enum identifiers.
/// 2. `normalize_desired` runs provider-specific transforms, followed by
///    strict host canonicalization of the returned provider text.
/// 3. Host default-tag merge adds tags from each routed provider instance.
/// 4. `normalize_state` runs provider-specific state transforms; host schema
///    walks own all enum lifting used for comparison.
pub struct PlanPreprocessor<'a> {
    normalizer: &'a dyn ProviderNormalizer,
    ctx: &'a WiringContext,
}

pub(crate) async fn hydrate_prior_states(
    managed_current: &mut HashMap<ResourceId, Current>,
    data_source_states: &mut HashMap<ResourceId, State>,
    orphans: &mut Orphans,
    normalizer: &dyn ProviderNormalizer,
    saved_attrs: &LiftedSavedAttrs,
    state_file: Option<&StateFile>,
) -> Result<(), ProviderError> {
    transform_prior_provider_states(
        managed_current,
        data_source_states,
        orphans,
        async |states| {
            normalizer
                .hydrate_read_state(states, saved_attrs.as_provider_saved_attrs())
                .await?;
            if let Some(state_file) = state_file {
                state_file.restore_partial_read_markers(states);
            }
            Ok(())
        },
        async |states| {
            // Data sources have no persisted managed row of their own. Keep
            // same-ID orphan attributes and partial-read metadata confined to
            // the managed/orphan transform above.
            let empty_saved_attrs = provider_mod::SavedAttrs::new();
            normalizer
                .hydrate_read_state(states, &empty_saved_attrs)
                .await
        },
    )
    .await
}

async fn normalize_prior_states(
    managed_current: &mut HashMap<ResourceId, Current>,
    data_source_states: &mut HashMap<ResourceId, State>,
    orphans: &mut Orphans,
    normalizer: &dyn ProviderNormalizer,
) -> Result<(), ProviderError> {
    transform_prior_provider_states(
        managed_current,
        data_source_states,
        orphans,
        async |states| {
            // Hard assert (not debug_assert): RFC #2371 constraint b says state
            // files never carry `Value::Deferred(DeferredValue::Unknown)`.
            assert!(
                !states_contain_unknown(states),
                "Value::Deferred(DeferredValue::Unknown) found in prior states — RFC #2371 constraint b violated"
            );
            normalizer.normalize_state(states).await?;
            Ok(())
        },
        async |states| {
            // Hard assert (not debug_assert): RFC #2371 constraint b says state
            // files never carry `Value::Deferred(DeferredValue::Unknown)`.
            assert!(
                !states_contain_unknown(states),
                "Value::Deferred(DeferredValue::Unknown) found in prior states — RFC #2371 constraint b violated"
            );
            normalizer.normalize_state(states).await?;
            Ok(())
        },
    )
    .await
}

#[derive(Debug, thiserror::Error)]
pub enum PlanPreparationError {
    #[error("plan validation failed")]
    Plan(Vec<PlanError>),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

fn type_error_attribute(error: &TypeError) -> Option<&str> {
    match error {
        TypeError::InvalidEnumVariant { attribute, .. }
        | TypeError::PatternMismatch { attribute, .. }
        | TypeError::LengthOutOfRange { attribute, .. }
        | TypeError::StringLiteralExpectedEnum { attribute, .. }
        | TypeError::ResourceValidationFailed { attribute, .. } => attribute.as_deref(),
        TypeError::BlockSyntaxNotAllowed { attribute } => Some(attribute),
        TypeError::ListItemError { inner, .. }
        | TypeError::MapKeyError { inner, .. }
        | TypeError::MapValueError { inner, .. }
        | TypeError::StructFieldError { inner, .. } => type_error_attribute(inner),
        TypeError::TypeMismatch { .. }
        | TypeError::ValidationFailed { .. }
        | TypeError::MissingRequired { .. }
        | TypeError::ReadOnlyAttribute { .. }
        | TypeError::UnknownAttribute { .. }
        | TypeError::UnknownStructField { .. }
        | TypeError::UnionStructMismatch { .. } => None,
    }
}

fn resolved_constraint_context(
    error: &TypeError,
    origin_attributes: &IndexMap<String, Value>,
    schema: &ResourceSchema,
) -> (Vec<String>, Vec<String>) {
    let mut origins_by_attribute: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (attribute, value) in origin_attributes {
        if !schema.attributes.contains_key(attribute) {
            continue;
        }
        value.visit_resource_refs(&mut |path| {
            origins_by_attribute
                .entry(attribute.clone())
                .or_default()
                .insert(path.to_plaintext_dot_string());
        });
    }

    let explicit_attribute = type_error_attribute(error).map(str::to_string);
    let attributes = if let Some(attribute) = explicit_attribute {
        vec![attribute]
    } else if origins_by_attribute.is_empty() {
        let mut known = origin_attributes
            .keys()
            .filter(|attribute| schema.attributes.contains_key(*attribute))
            .cloned()
            .collect::<Vec<_>>();
        known.sort();
        known
    } else {
        origins_by_attribute.keys().cloned().collect()
    };

    let mut origins = BTreeSet::new();
    for attribute in &attributes {
        if let Some(paths) = origins_by_attribute.get(attribute) {
            origins.extend(paths.iter().cloned());
        }
    }
    if origins.is_empty() {
        origins.extend(origins_by_attribute.into_values().flatten());
    }

    (attributes, origins.into_iter().collect())
}

fn validate_resolved_value_constraints(
    ctx: &WiringContext,
    resources: &[Resource],
    resource_origins: &[Resource],
    data_sources: &[DataSource],
    data_source_origins: &[DataSource],
) -> Vec<PlanError> {
    assert_eq!(
        resources.len(),
        resource_origins.len(),
        "plan value checking requires paired managed resources"
    );
    assert_eq!(
        data_sources.len(),
        data_source_origins.len(),
        "plan value checking requires paired data sources"
    );

    let lookup = carina_core::executor::provider_custom_type_lookup(ctx.factories());
    let mut plan_errors = Vec::new();
    for (resource, origin) in resources.iter().zip(resource_origins) {
        let Some(schema) = ctx.schemas().get_for(resource) else {
            continue;
        };
        append_resolved_value_constraint_errors(
            &mut plan_errors,
            &resource.id,
            schema,
            &resource.resolved_attributes(),
            &resource.quoted_string_attrs,
            &origin.attributes,
            &lookup,
        );
    }

    for (resource, origin) in data_sources.iter().zip(data_source_origins) {
        let Some(schema) = ctx.schemas().get_for_data_source(resource) else {
            continue;
        };
        append_resolved_value_constraint_errors(
            &mut plan_errors,
            &resource.id,
            schema,
            &carina_core::resource::attrs_to_hashmap(&resource.attributes),
            &resource.quoted_string_attrs,
            &origin.attributes,
            &lookup,
        );
    }

    plan_errors
}

fn append_resolved_value_constraint_errors(
    plan_errors: &mut Vec<PlanError>,
    id: &ResourceId,
    schema: &ResourceSchema,
    attributes: &HashMap<String, Value>,
    quoted_string_attrs: &HashSet<String>,
    origin_attributes: &IndexMap<String, Value>,
    lookup: CustomTypeLookup<'_>,
) {
    let is_string_literal = |attribute: &str| quoted_string_attrs.contains(attribute);
    let Err(errors) = schema.validate_known_values_with_origins_and_lookup(
        attributes,
        &is_string_literal,
        lookup,
    ) else {
        return;
    };

    for error in errors {
        let (attributes, origins) = resolved_constraint_context(&error, origin_attributes, schema);
        plan_errors.push(PlanError::new(
            id.clone(),
            PlanErrorKind::ResolvedValueConstraint {
                attributes,
                origins,
                message: error.to_string(),
            },
        ));
    }
}

fn module_constraint_plan_errors(
    error: carina_core::executor::ModuleConstraintGateError,
) -> Vec<PlanError> {
    error
        .failures()
        .iter()
        .cloned()
        .map(|failure| {
            PlanError::new(
                failure.composition_id,
                PlanErrorKind::ModuleConstraint(failure.diagnostic),
            )
        })
        .collect()
}

impl<'a> PlanPreprocessor<'a> {
    pub fn new(normalizer: &'a dyn ProviderNormalizer, ctx: &'a WiringContext) -> Self {
        Self { normalizer, ctx }
    }

    /// Run the full normalization pipeline on desired resources and all prior
    /// states.
    ///
    /// Call after `resolve_refs_with_state_and_remote()` and before
    /// `create_plan()`. This is the single seam both the plan and apply
    /// pipelines traverse for provider normalization and host-owned enum
    /// canonicalization in resources and prior states. `create_plan()` resolves
    /// typed wait RHS values directly against the selected target schema.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare(
        &self,
        resources: &mut OverrideAwareResources,
        resource_origins: &[Resource],
        module_gate: &carina_core::executor::ModuleConstraintGate,
        managed_current: &mut HashMap<ResourceId, Current>,
        data_source_states: &mut HashMap<ResourceId, State>,
        orphans: &mut Orphans,
        provider_configs: &[ProviderConfig],
        data_sources: &mut [DataSource],
        data_source_origins: &[DataSource],
    ) -> Result<(), PlanPreparationError> {
        let mut errors = validate_resolved_value_constraints(
            self.ctx,
            resources.resources(),
            resource_origins,
            data_sources,
            data_source_origins,
        );
        if let Err(error) = module_gate.check(resources.bindings()) {
            errors.extend(module_constraint_plan_errors(error));
        }
        if !errors.is_empty() {
            return Err(PlanPreparationError::Plan(errors));
        }

        let resources = resources.resources_mut();
        prepare_desired_side_for_plan(
            resources,
            data_sources,
            provider_configs,
            self.normalizer,
            self.ctx,
        )
        .await?;
        normalize_prior_states(
            managed_current,
            data_source_states,
            orphans,
            self.normalizer,
        )
        .await?;
        Ok(())
    }
}

/// The complete desired-side portion of plan preprocessing. Resources retain
/// every desired attribute throughout this host-owned pipeline; any
/// transport-level hiding is internal to the WASM provider boundary.
pub(crate) async fn prepare_desired_side_for_plan(
    resources: &mut [Resource],
    data_sources: &mut [DataSource],
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    ctx: &WiringContext,
) -> Result<(), ProviderError> {
    carina_core::value::canonicalize_data_sources_with_schemas(data_sources, ctx.schemas())?;

    apply_desired_normalization_slice(resources, provider_configs, normalizer, ctx.schemas()).await
}

/// Apply provider-specific state normalization before host schema lifting.
///
/// Enum strings returned by a provider remain API text. The host's later
/// schema-positioned state lift converts recognized members to canonical enum
/// values for comparison.
pub fn normalize_state_with_ctx(
    ctx: &WiringContext,
    provider_configs: &ResolvedProviderConfigs,
    managed_current: &mut HashMap<ResourceId, Current>,
) -> Result<(), ProviderError> {
    // WASM factories bridge their synchronous validation hook to async host
    // calls with `block_in_place` plus `Handle::current`. Preparation must
    // therefore be polled inside a multi-thread runtime, not merely happen
    // before a later async factory call.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .expect("failed to build tokio runtime for normalize_state");
    let mut router = ProviderRouter::new();
    for factory in ctx.factories() {
        let resolved_config = provider_configs.provider_or_implicit(factory.name());
        let config = rt
            .block_on(async {
                prepare_provider_ready_config(factory.as_ref(), resolved_config.as_ref())
            })
            .map_err(|error| {
                ProviderError::internal_caused_by("Failed to prepare provider configuration", error)
            })?;
        router.add_normalizer(
            factory.name(),
            rt.block_on(factory.create_normalizer(None, &config))?,
        );
    }
    // Outermost runtime: not nested.
    let mut provider_states = managed_current
        .iter()
        .filter_map(|(id, current)| {
            let state = match current {
                Current::Absent(state) => Some(state.state()),
                Current::Present(present) => Some(present.state()),
                Current::CreateInFlight(_pending) => None,
                Current::MissingIdentifier(state) => Some(state.state()),
            };
            state.map(|state| (id.clone(), state.clone()))
        })
        .collect();
    let result = rt.block_on(carina_core::provider::normalize_provider_states(
        &router,
        &mut provider_states,
    ));
    for (id, current) in managed_current {
        if let Some(state) = provider_states.remove(id)
            && let Some(target) = current.state_bearing_mut()
        {
            target.apply_transformed_state(state);
        }
    }
    result
}

pub fn check_unused_bindings<E: carina_core::parser::ExportParamLike>(
    parsed: &carina_core::parser::File<E>,
) -> Vec<String> {
    validation::check_unused_bindings(parsed)
}

pub fn validate_provider_region_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
) -> Vec<AppError> {
    lift_validation_result(validation::validate_provider_config(
        parsed,
        ctx.factories(),
    ))
}

pub(crate) fn validate_module_calls_with_imported<E>(
    parsed: &carina_core::parser::File<E>,
    imported_modules: &module_resolver::ResolvedModuleSignatures,
    config: &carina_core::parser::ProviderContext,
) -> Vec<AppError> {
    lift_validation_result(validation::validate_module_calls(
        &parsed.module_calls,
        imported_modules,
        config,
    ))
}

/// Preserve source-local module-output diagnostics when module expansion
/// fails before compositions can be built (for example, on an import cycle).
///
/// Successful validation never calls this fallback: imported module outputs
/// are owned exclusively by the post-expansion composition pass. Without a
/// complete expansion graph, the local [`BindingIndex`] can still validate
/// direct resource/data-source references in each `attributes {}` block, but
/// deliberately does not approximate forwarded composition inference.
pub(crate) fn validate_module_attribute_refs_after_expansion_failure(
    ctx: &WiringContext,
    module_walk: &ModuleWalk,
    config: &carina_core::parser::ProviderContext,
) -> Vec<AppError> {
    let mut errors = Vec::new();
    for module in module_walk.iter() {
        let mut parsed = module.loaded().parsed.clone();
        for finding in validation::resolve_file_type_exprs(&mut parsed, config) {
            errors.push(AppError::Validation(format!(
                "{}: {finding}",
                module.diagnostic_path().display()
            )));
        }
        let bindings =
            carina_core::binding_index::BindingIndex::from_parsed(&parsed, ctx.schemas());
        if let Err(joined) = validation::validate_attribute_param_ref_types_with_bindings(
            &parsed.attribute_params,
            &bindings,
        ) {
            errors.extend(prefixed_validation_messages(
                &joined,
                module.diagnostic_path().display(),
            ));
        }
    }
    errors
}

/// Validate references inside every imported module before substituting call
/// arguments. At this stage `BindingIndex` can use each argument's declaration
/// as the source type, so diagnostics belong to the module definition rather
/// than to whichever caller value happened to be expanded into it.
pub(crate) fn validate_module_internal_ref_types(
    ctx: &WiringContext,
    module_walk: &ModuleWalk,
    config: &carina_core::parser::ProviderContext,
) -> Vec<AppError> {
    let mut errors = Vec::new();
    for module in module_walk.iter() {
        let mut parsed = module.loaded().parsed.clone();
        let prefix = module.diagnostic_path().display();
        // Module expansion retains the established diagnostic for malformed
        // declarations. This pass only needs successful resolution so valid
        // dotted argument types can act as sources before expansion.
        let _ = validation::resolve_file_type_exprs(&mut parsed, config);

        if let Err(joined) = validation::validate_attribute_params(&parsed.attribute_params, config)
        {
            errors.extend(prefixed_validation_messages(&joined, &prefix));
        }

        let imported =
            module_resolver::load_resolved_module_signatures(&parsed, module.module_path(), config);
        let bindings = carina_core::binding_index::BindingIndex::from_parsed_with_module_signatures(
            &parsed,
            ctx.schemas(),
            &imported,
        );
        if let Err(joined) = validation::validate_resource_ref_types(
            &parsed,
            ctx.schemas(),
            &HashSet::new(),
            &bindings,
        ) {
            errors.extend(prefixed_validation_messages(&joined, &prefix));
        }
        if let Err(joined) = validation::validate_attribute_param_ref_types_with_bindings(
            &parsed.attribute_params,
            &bindings,
        ) {
            errors.extend(prefixed_validation_messages(&joined, &prefix));
        }

        errors.extend(
            validation::validate_module_call_argument_ref_types_with_bindings(
                &parsed.module_calls,
                &imported,
                &HashSet::new(),
                &bindings,
            )
            .into_iter()
            .map(|error| AppError::Validation(format!("{prefix}: {error}"))),
        );
    }
    errors
}

/// Reject providers in every recursively loaded module and preserve each
/// module's import path in the resulting user-facing diagnostic. The module
/// directory unit deliberately includes imported-but-uncalled nested modules,
/// matching the duplicate and attribute-type validation policy from
/// #3711/#3714. The resolver retains its own expansion-phase backstop for
/// callers that bypass this validation walk.
pub(crate) fn validate_no_provider_in_modules(module_walk: &ModuleWalk) -> Vec<AppError> {
    let mut errors = Vec::new();
    for module in module_walk.iter() {
        if let Err(message) = validation::validate_no_provider_in_module(&module.loaded().parsed) {
            errors.push(AppError::Validation(format!(
                "{}: {message}",
                module.diagnostic_path().display()
            )));
        }
    }
    errors
}

/// Reject state blocks in every recursively loaded module and preserve each
/// module's import path in the resulting user-facing diagnostic. The module
/// directory unit deliberately includes imported-but-uncalled nested modules,
/// matching the provider validation policy. The resolver retains its own
/// expansion-phase backstop for callers that bypass this validation walk.
pub(crate) fn validate_no_state_blocks_in_modules(module_walk: &ModuleWalk) -> Vec<AppError> {
    let mut errors = Vec::new();
    for module in module_walk.iter() {
        if let Err(message) =
            validation::validate_no_state_blocks_in_module(&module.loaded().parsed)
        {
            errors.push(AppError::Validation(format!(
                "{}: {message}",
                module.diagnostic_path().display()
            )));
        }
    }
    errors
}

/// Reject backend blocks in every recursively loaded module and preserve each
/// module's import path in the resulting user-facing diagnostic. The resolver
/// retains its own expansion-phase backstop for callers that bypass this
/// validation walk.
pub(crate) fn validate_no_backend_in_modules(module_walk: &ModuleWalk) -> Vec<AppError> {
    let mut errors = Vec::new();
    for module in module_walk.iter() {
        if let Err(message) = validation::validate_no_backend_in_module(&module.loaded().parsed) {
            errors.push(AppError::Validation(format!(
                "{}: {message}",
                module.diagnostic_path().display()
            )));
        }
    }
    errors
}

/// Reject upstream state declarations in every recursively loaded module and
/// preserve each module's import path in the resulting user-facing diagnostic.
/// The resolver retains its own expansion-phase backstop for callers that
/// bypass this validation walk.
pub(crate) fn validate_no_upstream_states_in_modules(module_walk: &ModuleWalk) -> Vec<AppError> {
    let mut errors = Vec::new();
    for module in module_walk.iter() {
        if let Err(message) =
            validation::validate_no_upstream_states_in_module(&module.loaded().parsed)
        {
            errors.push(AppError::Validation(format!(
                "{}: {message}",
                module.diagnostic_path().display()
            )));
        }
    }
    errors
}

/// Reject exports blocks in every recursively loaded module and preserve each
/// module's import path in the resulting user-facing diagnostic. The resolver
/// retains its own expansion-phase backstop for callers that bypass this
/// validation walk.
pub(crate) fn validate_no_exports_in_modules(module_walk: &ModuleWalk) -> Vec<AppError> {
    let mut errors = Vec::new();
    for module in module_walk.iter() {
        if let Err(message) = validation::validate_no_exports_in_module(&module.loaded().parsed) {
            errors.push(AppError::Validation(format!(
                "{}: {message}",
                module.diagnostic_path().display()
            )));
        }
    }
    errors
}

pub async fn get_provider_with_ctx(
    ctx: &WiringContext,
    configs: &ResolvedProviderConfigs,
    base_dir: &Path,
) -> Result<ProviderRouter, AppError> {
    let mut router = ProviderRouter::new();

    // Two-pass build so named instances can reuse the kind's factory.
    // Pass 1 handles every default instance (top-level `provider <kind>`
    // blocks) — these are the entries that carry `source`/`version` and
    // may need a WASM plugin to be loaded. Pass 2 handles each named
    // instance (`let <name> = provider <kind> { ... }`), reusing the
    // factory the default instance already brought in.
    for provider_config in configs.iter().filter(|config| config.is_default()) {
        instantiate_provider_into_router(ctx, &mut router, provider_config, base_dir, None).await?;
    }

    for provider_config in configs.iter().filter(|config| !config.is_default()) {
        let inherited_source = kind_default_source(configs, provider_config.name());
        instantiate_provider_into_router(
            ctx,
            &mut router,
            provider_config,
            base_dir,
            inherited_source,
        )
        .await?;
    }

    if router.is_empty() && ctx.uses_builtin_mock() {
        // Use mock provider for other cases.
        // Register the kind's default instance with empty kind to match
        // resources without a provider prefix.
        println!("{}", "Using mock provider".cyan());
        router.add_provider(String::new(), Box::new(MockProvider::new()));
    }

    Ok(router)
}

/// Look up the `source` declared on the kind's default instance for a
/// given kind name. Named instances inherit `source` from the kind's
/// default — the parser rejects `source` on `let <name> = provider
/// <kind>` — so this is the only way to get the source URL while
/// processing a named instance's wiring.
fn kind_default_source<'a>(configs: &'a ResolvedProviderConfigs, kind: &str) -> Option<&'a str> {
    configs
        .iter()
        .find(|config| config.is_default() && config.name() == kind)
        .and_then(ResolvedProviderConfig::source)
}

/// Prepare the default instance used by implicit backend operations.
///
/// Both state-bucket bootstrap and deletion route through this typed seam, so
/// neither can recover an attribute map from `parsed.providers`. Providers
/// without a declared block retain the historical empty-config fallback.
pub(crate) fn prepare_default_provider_ready_config(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    configs: &ResolvedProviderConfigs,
) -> Result<ProviderReadyConfig, carina_core::executor::ProviderConfigPreparationError> {
    let config = configs.provider_or_implicit(provider_name);
    prepare_provider_ready_config(factory, config.as_ref())
}

/// Format the "Using <provider>" announcement emitted by both the
/// default-instance and named-instance wiring paths (carina#3067).
///
/// One helper, one shape — the rule is that there is no other place
/// in the codebase that builds this line. Both code paths
/// (`try_add_source_provider` for the kind's default, the
/// `find_factory` branch of `instantiate_provider_into_router` for
/// named instances) call this function, so a future "third caller"
/// physically cannot emit a divergent shape without touching this
/// function.
///
/// - `kind` is the kind name (`aws`, `awscc`) — what the user writes
///   in `provider <kind> {}` and routes to via `directives.provider`.
///   Use `ProviderFactory::name()`, not `display_name()`.
/// - `binding = None` is the kind's default instance and is rendered
///   as `instance=default`. Explicit, not omitted: the omission is
///   exactly what made the old shape ambiguous.
/// - `source = None` is omitted from the rendered line. In practice
///   both call paths feed a `Some(...)`: the source-loading path has
///   it directly, and the find_factory path threads in the kind
///   default's source. `None` is only reachable if a kind default
///   never declared a `source`, which currently only happens in
///   mock/test wiring that doesn't pass through this helper.
fn format_provider_using_line(
    kind: &str,
    region: &str,
    binding: Option<&str>,
    source: Option<&str>,
) -> String {
    let instance = binding.unwrap_or("default");
    match source {
        Some(src) => format!("Using {kind} (region: {region}, instance={instance}, source: {src})"),
        None => format!("Using {kind} (region: {region}, instance={instance})"),
    }
}

/// Register a single provider instance into `router`. The config's binding
/// determines whether this is the kind's default or a named instance.
///
/// Source-loading (`provider <kind> { source = ... }`) is only invoked
/// when this is the kind's default instance — named instances reuse the
/// factory the default instance already loaded.
async fn instantiate_provider_into_router(
    ctx: &WiringContext,
    router: &mut ProviderRouter,
    provider_config: &ResolvedProviderConfig,
    base_dir: &Path,
    inherited_source: Option<&str>,
) -> Result<(), AppError> {
    let binding = provider_config.binding();
    // Named instances inherit `source`/`version`/`revision` from the
    // kind's default — these fields are rejected by the parser when set
    // on `let <name> = provider <kind> { ... }`. Only the default
    // instance can trigger the source-loading path.
    if provider_config.is_default()
        && let Some(source) = provider_config.source()
    {
        try_add_source_provider(router, source, provider_config, base_dir).await?;
        return Ok(());
    }

    if let Some(factory) = provider_mod::find_factory(ctx.factories(), provider_config.name()) {
        let config = prepare_provider_ready_config(factory, provider_config)?;
        let region = factory.extract_region(config.attributes());
        // Reaching this branch implies `provider_config.source` is None:
        // the early-return above handles a default config with a source,
        // and the parser rejects `source` on named instances. So the only
        // candidate for the log line is the kind default's source, threaded
        // in by the caller as `inherited_source`.
        println!(
            "{}",
            format_provider_using_line(factory.name(), &region, binding, inherited_source).cyan()
        );
        let provider = factory
            .create_provider(binding, &config)
            .await
            .map_err(|e| e.for_provider(provider_config.name().to_string()))?;
        router.add_normalizer(
            provider_config.name().to_string(),
            factory
                .create_normalizer(binding, &config)
                .await
                .map_err(|e| e.for_provider(provider_config.name().to_string()))?,
        );
        router.add_provider_config(provider_config.as_provider_config(), provider);
    } else if ctx.uses_builtin_mock() && provider_config.name() == BUILTIN_PROVIDER_NAME {
        println!("{}", "Using mock provider".cyan());
        router.add_provider_config(
            provider_config.as_provider_config(),
            Box::new(MockProvider::new()),
        );
    } else if !provider_config.name().is_empty() {
        let message = match binding {
            // Named instance whose kind's default did not register a
            // factory — usually the default instance failed to load
            // (a separate error has already been printed for it).
            Some(name) => format!(
                "Named provider instance '{}' (kind '{}') cannot be loaded \
                 because the kind's default instance is unavailable.",
                name,
                provider_config.name()
            ),
            None => format!(
                "Provider '{}' requires 'source' and 'version' attributes.",
                provider_config.name()
            ),
        };
        eprintln!("{}", message.red());
    }
    Ok(())
}

async fn try_add_source_provider(
    router: &mut ProviderRouter,
    source: &str,
    config: &ResolvedProviderConfig,
    base_dir: &Path,
) -> Result<(), AppError> {
    match load_source_provider(source, config, base_dir).await {
        Ok((factory, provider, name, ready_config)) => {
            let region = factory.extract_region(ready_config.attributes());
            println!(
                "{}",
                format_provider_using_line(&name, &region, None, Some(source)).cyan()
            );
            router.add_provider_config(config.as_provider_config(), provider);
            router.add_normalizer(
                config.name().to_string(),
                factory
                    .create_normalizer(None, &ready_config)
                    .await
                    .map_err(|e| AppError::Provider(e.for_provider(config.name().to_string())))?,
            );
            Ok(())
        }
        Err(LoadSourceError::KindMismatch(error)) => Err(error),
        Err(LoadSourceError::Provider(e)) => {
            // Provider init failure (e.g. allowed_account_ids mismatch).
            // Propagate verbatim so the CLI boundary can render it
            // structurally without leaking implementation-detail
            // wrappers like "Failed to load provider '...': ...".
            // Attach provider name so the renderer can label the
            // structured block with the right provider.
            Err(source_provider_initialization_error(config.name(), e))
        }
        Err(LoadSourceError::Config(error)) => Err(AppError::from(error)),
        Err(LoadSourceError::Wasm(error)) => {
            // Terminal display boundary for the structured host error plus
            // resolver provenance. The underlying error remains typed until
            // this colorized stderr rendering.
            eprintln!("{}", error.to_string().red());
            Ok(())
        }
        Err(LoadSourceError::Other(msg)) => {
            eprintln!(
                "{}",
                format!("Failed to load provider '{}': {}", config.name(), msg).red()
            );
            Ok(())
        }
    }
}

fn source_provider_initialization_error(provider_name: &str, error: ProviderError) -> AppError {
    AppError::Provider(error.for_provider(provider_name.to_string()))
}

fn ensure_source_provider_kind_matches(
    configured: &str,
    loaded: &str,
    source: &str,
) -> Result<(), AppError> {
    if configured == loaded {
        return Ok(());
    }

    Err(AppError::SourceProviderKindMismatch {
        configured: configured.to_string(),
        loaded: loaded.to_string(),
        provider_source: source.to_string(),
    })
}

/// Failure mode for `load_source_provider`. Distinguishing between a
/// provider init rejection and other plumbing failures lets the caller
/// surface the former as a structured error without the
/// "Failed to load provider '...': ..." wrapper that obscures the
/// real message (#2407).
enum LoadSourceError {
    /// The loaded artifact reported a provider kind other than the block
    /// that selected it.
    KindMismatch(AppError),
    /// Resolved provider configuration failed the construction gate.
    Config(carina_core::executor::ProviderConfigPreparationError),
    /// The provider's `init` step rejected the configuration (e.g.,
    /// `allowed_account_ids` mismatch). Message is user-facing.
    Provider(ProviderError),
    /// WASM loading failed after resolution. Kept structured until the
    /// terminal-rendering boundary so both the anyhow chain and artifact
    /// provenance remain attached.
    Wasm(
        carina_provider_resolver::ProviderArtifactLoadError<
            carina_plugin_host::WasmProviderLoadError,
        >,
    ),
    /// Plumbing failure: binary not found, unsupported source scheme,
    /// invalid config, etc. Logged and skipped.
    Other(String),
}

async fn load_source_provider(
    source: &str,
    config: &ResolvedProviderConfig,
    base_dir: &Path,
) -> Result<
    (
        Box<dyn ProviderFactory>,
        Box<dyn Provider>,
        String,
        ProviderReadyConfig,
    ),
    LoadSourceError,
> {
    let installed = if source.starts_with("file://") || source.starts_with("github.com/") {
        carina_provider_resolver::find_installed_provider(base_dir, config.as_provider_config())
            .map_err(|e| LoadSourceError::Other(format!("Provider '{}' {}", config.name(), e)))?
    } else {
        return Err(LoadSourceError::Other(format!(
            "Unsupported source format: {source}. Use file:// for local binaries or github.com/owner/repo for remote."
        )));
    };

    if !carina_provider_resolver::is_wasm_provider(installed.path()) {
        return Err(LoadSourceError::Other(format!(
            "Provider '{}': native binaries are no longer supported. Use a .wasm component instead.",
            config.name()
        )));
    }

    let factory: Box<dyn ProviderFactory> = Box::new(
        load_installed_wasm_provider(&installed)
            .await
            .map_err(LoadSourceError::Wasm)?,
    );
    let name = factory.name().to_string();
    ensure_source_provider_kind_matches(config.name(), &name, source)
        .map_err(LoadSourceError::KindMismatch)?;

    let ready_config =
        prepare_provider_ready_config(factory.as_ref(), config).map_err(LoadSourceError::Config)?;

    let provider = factory
        .create_provider(None, &ready_config)
        .await
        .map_err(LoadSourceError::Provider)?;
    Ok((factory, provider, name, ready_config))
}

/// Returns the wired router **and** the `WiringContext` it was built
/// from. The apply-from-saved-plan path needs the context's factories
/// and schemas to re-apply the full normalization pipeline after
/// apply-time reference re-resolution (carina#3063) — without it the
/// from-plan path would silently undo enum-alias / canonicalize stages.
pub async fn create_providers_from_configs_with_ctx(
    ctx: &WiringContext,
    configs: &ResolvedProviderConfigs,
    base_dir: &Path,
) -> Result<ProviderRouter, AppError> {
    let mut router = ProviderRouter::new();

    // Same two-pass shape as `get_provider_with_ctx`: default instances
    // first (they may load the WASM plugin), then named instances reuse
    // the factory that was just loaded.
    for config in configs.iter().filter(|config| config.is_default()) {
        instantiate_provider_into_router(ctx, &mut router, config, base_dir, None).await?;
    }
    for config in configs.iter().filter(|config| !config.is_default()) {
        let inherited_source = kind_default_source(configs, config.name());
        instantiate_provider_into_router(ctx, &mut router, config, base_dir, inherited_source)
            .await?;
    }

    if router.is_empty() && ctx.uses_builtin_mock() {
        println!("{}", "Using mock provider".cyan());
        router.add_provider(String::new(), Box::new(MockProvider::new()));
    }

    Ok(router)
}

/// Build only the desired-state normalizers needed to reproduce the plan-time
/// candidate shape. Runtime providers are intentionally not constructed here:
/// saved-plan secret verification must finish before CRUD-capable objects
/// become available to the apply path.
pub(crate) async fn create_normalizers_from_configs_with_ctx(
    ctx: &WiringContext,
    configs: &ResolvedProviderConfigs,
) -> Result<ProviderRouter, AppError> {
    let mut router = ProviderRouter::new();

    for config in configs.iter() {
        if let Some(factory) = provider_mod::find_factory(ctx.factories(), config.name()) {
            let ready = prepare_provider_ready_config(factory, config)?;
            router.add_normalizer(
                config.name().to_string(),
                factory
                    .create_normalizer(config.binding(), &ready)
                    .await
                    .map_err(|error| AppError::Provider(error.for_provider(config.name())))?,
            );
        }
    }

    Ok(router)
}

/// The expanded children that are safe to re-read from the provider —
/// `new_child_ids` minus any child that is a `moved` block `to` target
/// (carina#3141). [`RefreshableChildIds::select`] also excludes children
/// already owned by the typed orphan collection.
///
/// This is a newtype, not a bare `HashSet<ResourceId>`, on purpose. The
/// plan path and the apply path must refresh *exactly this set* and not
/// the wider `new_child_ids`; if both were `HashSet<ResourceId>` a
/// caller could `new_child_ids.contains(...)` by mistake and the
/// moved-exclusion would silently not apply (the recurring
/// "unit-test path ≠ apply path" parity hazard). The only way to obtain
/// the refresh iterator is [`RefreshableChildIds::select`], which
/// `new_child_ids` does not have — so a path that filters by the wrong
/// set is a *compile error*, not a runtime divergence. See the
/// "Residual structural risk" section of
/// notes/specs/2026-05-18-moved-into-loop-expansion-refresh-design.md.
#[derive(Debug, Clone, Default)]
pub struct RefreshableChildIds(HashSet<ResourceId>);

impl RefreshableChildIds {
    /// From a resource slice, yield exactly the resources that should be
    /// re-read from the provider after a deferred-for expansion: the
    /// expanded children that are not `moved` targets and are not already
    /// owned as typed orphans. Both the plan and apply child-refresh sites
    /// must build their refresh iterator through this method — there is no
    /// other constructor for the refresh set, which is what makes the
    /// plan/apply parity a type-level invariant rather than a reviewer's
    /// responsibility.
    pub fn select<'a>(
        &'a self,
        resources: &'a [Resource],
        orphans: &'a Orphans,
    ) -> impl Iterator<Item = &'a Resource> {
        resources
            .iter()
            .filter(move |r| self.0.contains(&r.id) && !orphans.contains_key(&r.id))
    }

    /// Test/inspection accessor: is this id in the refreshable set?
    pub fn contains(&self, id: &ResourceId) -> bool {
        self.0.contains(id)
    }

    /// Test/inspection accessor: number of refreshable children.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Test/inspection accessor: are there no refreshable children?
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A deferred-for expression whose iterable is not plan-time concrete,
/// but has enough upstream identity for the apply executor to re-expand it.
///
/// This target emits `DeferredCreate` when no matching orphan delete exists,
/// or `DeferredReplace` when `add_deferred_create_effects` absorbs matching
/// delete effects for prior indexed iterations.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredCreateTarget {
    pub id: ResolvedResourceId,
    pub upstream_binding: String,
    pub template: carina_core::parser::DeferredForExpression,
}

impl DeferredCreateTarget {
    fn from_deferred(deferred: &carina_core::parser::DeferredForExpression) -> Option<Self> {
        let template_id = &deferred.template_resource.id;
        let identity = match ResourceIdentity::try_from(deferred.binding_name.clone()) {
            Ok(identity) => identity,
            Err(ResourceIdentityError::Empty) => return None,
        };
        let id = ResolvedResourceId::with_provider_identity(
            template_id.provider.clone(),
            template_id.resource_type.clone(),
            identity,
            template_id.provider_instance.clone(),
        );
        Some(Self {
            id,
            upstream_binding: deferred.iterable_binding.clone(),
            template: deferred.clone(),
        })
    }

    fn to_effect(&self) -> Effect {
        Effect::DeferredCreate {
            id: self.id.clone(),
            upstream_binding: self.upstream_binding.clone(),
            template: Box::new(self.template.clone()),
        }
    }

    fn to_deferred_replace_effect(&self, deletes: Vec<DeferredReplaceDelete>) -> Effect {
        Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(deletes).expect("planner checked non-empty deletes"),
            id: self.id.clone(),
            upstream_binding: self.upstream_binding.clone(),
            template: Box::new(self.template.clone()),
        }))
    }
}

/// Outcome of the carina#3132 post-refresh deferred-for expansion.
pub struct DeferredForExpansion {
    /// The augmented, re-sorted resource set: every original resource
    /// plus the materialized loop children, topologically ordered.
    /// Equal in length to the input when no loop resolved.
    pub sorted_resources: Vec<Resource>,
    /// Loops still unresolved (iterable genuinely unknowable at plan
    /// time) — rendered as the carina#3128 validate/plan placeholder.
    pub residual_deferred_for: Vec<carina_core::parser::DeferredForExpression>,
    /// Loops whose iterable is deferred until apply and should become
    /// state-only `Effect::DeferredCreate` entries in the plan.
    pub deferred_create_targets: Vec<DeferredCreateTarget>,
    /// Ids of the resources materialized by this expansion (empty when
    /// no loop resolved).
    pub new_child_ids: HashSet<ResourceId>,
    /// Expanded children safe to re-read from the provider: the
    /// `moved`-excluded subset of `new_child_ids` (carina#3141). The
    /// plan and apply child-refresh sites build their refresh iterator
    /// via [`RefreshableChildIds::select`]; see that type's doc for why
    /// it is a newtype (compile-time plan/apply parity).
    pub refreshable_child_ids: RefreshableChildIds,
    /// Whether `print_warnings()` emitted any `⚠` line during expansion.
    /// A printed warning is newline-terminated and lands on top of
    /// indicatif's open spinner bar line, so it *closes* the bar region
    /// the refresh phases left open. [`finish_refresh_bar_region`] uses
    /// this to avoid adding a second, spurious blank line on the
    /// deferred-for-with-warnings TTY path (#3150 Round-4 finding).
    pub printed_warnings: bool,
}

/// Pure core of the carina#3132 fix: project the post-refresh
/// `ResolvedBindings` into the typed iterable view, expand every
/// deferred-for whose iterable is now resolvable, and re-sort the
/// augmented set.
///
/// `ResolvedBindings::pre_apply` merges local DSL ⊕ refreshed managed and
/// data-source state ⊕ `upstream_state`/`wait` bindings, so a same-config
/// `cert.domain_validation_options` loop and an `upstream_state` loop
/// resolve through the *identical* view — one resolution point, no
/// upstream-only carve-out.
///
/// Pure (no provider I/O) so it is unit-testable with a hand-built
/// post-refresh state maps; `create_plan_from_parsed_with_upstream_with_ctx`
/// calls exactly this function, then targeted-refreshes
/// `refreshable_child_ids`.
///
/// `moved_targets` is the set of `moved` block `to` ResourceIds (already
/// materialized by `materialize_moved_states` on both the plan and apply
/// paths before this call). Children whose id is in this set are kept
/// out of `refreshable_child_ids` so their migrated state survives
/// (carina#3141).
///
/// `already_refreshed` is the set of ResourceIds the phase-1 orphan pass
/// already performed a live provider read for *this run*. A for-loop
/// child applied on a previous run sits in the state file; because
/// expansion happens *after* refresh, that child is not yet a desired
/// resource when the orphan pass runs, so the orphan pass classifies it
/// as an orphan and live-reads it (keyed by the same state name the
/// post-expansion child resolves to → identical provider identity). Such
/// a child is kept out of `refreshable_child_ids` so the post-expansion
/// child refresh does not read the same address a second time
/// (carina#3145). The decision is made here, once, alongside the
/// moved-target exclusion, and carried in the typed `RefreshableChildIds`
/// so the plan and apply paths cannot diverge.
#[allow(clippy::too_many_arguments)]
pub fn expand_same_config_deferred_for<E: Clone>(
    parsed: &carina_core::parser::File<E>,
    sorted_resources: &[Resource],
    managed_current: &HashMap<ResourceId, Current>,
    data_source_states: &HashMap<ResourceId, State>,
    schemas: &carina_core::schema::SchemaRegistry,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    wait_aliases: &[WaitAliasSpec],
    moved_targets: &HashSet<ResourceId>,
    already_refreshed: &HashSet<ResourceId>,
) -> Result<DeferredForExpansion, AppError> {
    // Common case: no deferred-for at all. Skip the binding projection
    // and the whole-`File` clone entirely (this runs on every plan /
    // validate). Parse warnings still print — with no deferred-for,
    // expansion would remove none of them, so `parsed`'s set is exactly
    // what the post-expansion path would have printed.
    if parsed.deferred_for_expressions.is_empty() {
        let printed_warnings = parsed.print_warnings();
        return Ok(DeferredForExpansion {
            sorted_resources: sorted_resources.to_vec(),
            residual_deferred_for: Vec::new(),
            deferred_create_targets: Vec::new(),
            new_child_ids: HashSet::new(),
            refreshable_child_ids: RefreshableChildIds::default(),
            printed_warnings,
        });
    }

    let iterable_bindings = ResolvedBindings::pre_apply(PreApplyInputs {
        managed: sorted_resources,
        compositions: &parsed.compositions,
        data_sources: &parsed.data_sources,
        managed_current: &carina_core::resource::into_plan_input_current_map(
            managed_current.clone(),
            schemas,
            sorted_resources,
        ),
        data_source_states,
        remote_bindings,
        wait_aliases,
    })
    .project_iterable_bindings();

    let mut deferred_create_targets = Vec::new();
    let pre_expandable_deferred_for: Vec<_> = parsed
        .deferred_for_expressions
        .iter()
        .filter_map(|deferred| {
            let iterable = iterable_bindings
                .get(&deferred.iterable_binding)
                .and_then(|attrs| attrs.get(&deferred.iterable_attr));

            match iterable {
                Some(value) if is_value_fully_concrete_for_expansion(value) => {
                    Some(deferred.clone())
                }
                Some(_) | None => {
                    if let Some(target) = DeferredCreateTarget::from_deferred(deferred) {
                        deferred_create_targets.push(target);
                    }
                    None
                }
            }
        })
        .collect();

    // `expand_deferred_for_expressions` is a `&mut self` method that
    // appends generated resources and drops resolved entries. `parsed`
    // is borrowed immutably here, so expand on a local clone and read
    // the augmented resource set / residual deferred list back out.
    let mut expanded: carina_core::parser::File<E> = (*parsed).clone();
    for target in &deferred_create_targets {
        expanded.warnings.retain(|w| {
            w.kind != WarningKind::DeferredFor
                || w.line != target.template.line
                || w.file != target.template.file
        });
    }
    expanded.deferred_for_expressions = pre_expandable_deferred_for;
    expanded
        .expand_deferred_for_expressions(&iterable_bindings)
        .map_err(|error| AppError::Validation(error.to_string()))?;
    let printed_warnings = expanded.print_warnings();
    let residual_deferred_for = expanded.deferred_for_expressions.clone();

    let pre_ids: HashSet<ResourceId> = sorted_resources.iter().map(|r| r.id.clone()).collect();

    // A length delta means a loop materialized. Compare against the
    // input slice length (not the deduped `pre_ids` set) so the test is
    // a pure "did expansion add resources" check. Re-sort the augmented
    // set: `topological_sort` preserves declaration order for
    // independent resources (#1071), so already-planned resources keep
    // their relative order (carina#3132 re-sort stability requirement)
    // and the children are slotted in per their refs.
    let materialized = expanded.resources.len() != sorted_resources.len();
    let resorted = if materialized {
        sort_resources_by_dependencies(&expanded.resources).map_err(AppError::Validation)?
    } else {
        sorted_resources.to_vec()
    };
    let new_child_ids: HashSet<ResourceId> = resorted
        .iter()
        .map(|r| r.id.clone())
        .filter(|id| !pre_ids.contains(id))
        .collect();

    // Exclude a child from the post-expansion refresh when re-reading it
    // would be wrong or redundant:
    //
    // - carina#3141: a child that is also a `moved` block `to` already
    //   holds the migrated state from `materialize_moved_states`.
    //   Re-reading it would overwrite that state with a `not_found`
    //   provider read (the state file still keys the old name, so no
    //   identifier resolves).
    // - carina#3145: a child applied on a previous run is in the state
    //   file but is not yet a desired resource when the phase-1 orphan
    //   pass runs (expansion is post-refresh), so that pass already
    //   live-read it under the same state name the child resolves to.
    //   Reading it again here is a redundant second provider call for
    //   the same address.
    let refreshable_child_ids = RefreshableChildIds(
        new_child_ids
            .iter()
            .filter(|id| !moved_targets.contains(*id) && !already_refreshed.contains(*id))
            .cloned()
            .collect(),
    );

    Ok(DeferredForExpansion {
        sorted_resources: resorted,
        residual_deferred_for,
        deferred_create_targets,
        new_child_ids,
        refreshable_child_ids,
        printed_warnings,
    })
}

/// Result of the deferred-for expansion, materialised-child read,
/// prior-state hydration, and enum-lift stages shared by every refresh path.
///
/// Closes the bug class behind carina#3266 / #3271 / #3272 at the type
/// level — same closure pattern as
/// [`PostApplyStates`](crate::commands::shared::state_writeback::PostApplyStates).
/// Every call site that needs a post-expansion view of `sorted_resources`
/// (apply `run_apply_locked`, refresh `run_state_refresh_locked`) MUST
/// obtain it via [`expand_refresh_and_lift_states`], and the constructor
/// performs the complete post-expansion sequence:
///
/// 1. `expand_same_config_deferred_for` — synthesise for-loop children
///    and re-sort the augmented set.
/// 2. `refresh_resource_set` — read each materialised child through
///    the provider (filtered through `refreshable_child_ids` so a
///    `moved`-target or orphan-pre-read child is not re-read).
/// 3. `hydrate_prior_states` over `managed_current`, `data_source_states`, and
///    `orphans`; persisted managed attributes and partial-read markers are
///    restored only to the managed/orphan key space.
/// 4. `lift_prior_state_enum_leaves` on both prior-state owners using the
///    **post-expansion** `sorted_resources`, so enum-typed attrs on new
///    children are not surfaced as phantom case diffs (carina#3272).
///
/// The constructor mutates all three prior-state owners in place. Keeping them
/// inside this operation prevents a refresh path from transforming only one
/// part of prior state.
#[non_exhaustive]
pub struct ExpandedRefreshState {
    pub sorted_resources: Vec<Resource>,
    pub residual_deferred_for: Vec<carina_core::parser::DeferredForExpression>,
    pub deferred_create_targets: Vec<DeferredCreateTarget>,
    pub new_child_ids: HashSet<ResourceId>,
    pub refreshable_child_ids: RefreshableChildIds,
    pub printed_warnings: bool,
}

/// Inputs for [`expand_refresh_and_lift_states`].
///
/// Grouped into a struct because the underlying phases need many of
/// the same inputs (provider, schemas, wait_aliases, …) and a flat
/// arg list would already trip the 7-argument lint here.
pub struct ExpandRefreshAndLiftInputs<'a, E: Clone, P: Provider + ProviderNormalizer> {
    pub parsed: &'a carina_core::parser::File<E>,
    pub provider: &'a P,
    pub sorted_resources: &'a [Resource],
    pub managed_current: &'a mut HashMap<ResourceId, Current>,
    pub data_source_states: &'a mut HashMap<ResourceId, State>,
    /// Orphans refreshed before deferred `for` expansion. Persisted loop
    /// children can remain here after materialisation: the differ owns the
    /// late same-id promotion rule, while every provider transform operates
    /// on the joined prior-state view.
    pub orphans: &'a mut Orphans,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub wait_aliases: &'a [WaitAliasSpec],
    pub moved_targets: &'a HashSet<ResourceId>,
    pub already_refreshed: &'a HashSet<ResourceId>,
    pub state_file: &'a Option<StateFile>,
    pub saved_dep_bindings: &'a HashMap<ResourceId, BTreeSet<String>>,
    /// Saved attribute values to carry forward to materialised children
    /// via `provider.hydrate_read_state` after their initial read. Pass
    /// the same lifted saved attributes the caller built once at the head of
    /// its pipeline — `hydrate_read_state` is idempotent, so passing it here
    /// and re-hydrating downstream of this function is safe.
    pub saved_attrs: &'a LiftedSavedAttrs,
    pub multi: &'a indicatif::MultiProgress,
    pub schemas: &'a carina_core::schema::SchemaRegistry,
}

/// Run the expand → child-refresh → hydrate → lift quartet in
/// lock-step.
///
/// See [`ExpandedRefreshState`] for why this is a single function.
pub async fn expand_refresh_and_lift_states<E: Clone, P: Provider + ProviderNormalizer>(
    inputs: ExpandRefreshAndLiftInputs<'_, E, P>,
) -> Result<ExpandedRefreshState, AppError> {
    // Phase 1: expand for-loops.
    let DeferredForExpansion {
        sorted_resources,
        residual_deferred_for,
        deferred_create_targets,
        new_child_ids,
        refreshable_child_ids,
        printed_warnings,
    } = expand_same_config_deferred_for(
        inputs.parsed,
        inputs.sorted_resources,
        inputs.managed_current,
        inputs.data_source_states,
        inputs.schemas,
        inputs.remote_bindings,
        inputs.wait_aliases,
        inputs.moved_targets,
        inputs.already_refreshed,
    )?;

    // Phase 2: read materialised children that weren't already refreshed.
    // Moved targets are excluded by `refreshable_child_ids`; orphan-owned
    // children are excluded structurally by `select`.
    //
    // Phase 2.5: carry forward saved_attrs to the newly-read children
    // so create-only / API-unreturned fields survive. Mirrors the
    // 2nd `hydrate_read_state` call apply ran by hand
    // (apply/mod.rs ~1098); collapsing it into this constructor stops
    // the next caller from forgetting it.
    if !new_child_ids.is_empty() {
        let children = refreshable_child_ids.select(&sorted_resources, inputs.orphans);
        refresh_resource_set(
            inputs.provider,
            inputs.multi,
            children,
            inputs.state_file,
            inputs.schemas,
            inputs.saved_dep_bindings,
            inputs.managed_current,
        )
        .await?;
        hydrate_prior_states(
            inputs.managed_current,
            inputs.data_source_states,
            inputs.orphans,
            inputs.provider,
            inputs.saved_attrs,
            inputs.state_file.as_ref(),
        )
        .await?;
    }

    // Phase 3: lift Enums on the post-expansion slice. Both
    // pre-existing managed resources and the new for-loop children
    // need this so enum-typed attrs aren't surfaced as phantom case
    // diffs (carina#3272).
    lift_prior_state_enum_leaves(
        inputs.managed_current,
        inputs.orphans,
        &sorted_resources,
        inputs.schemas,
    );

    Ok(ExpandedRefreshState {
        sorted_resources,
        residual_deferred_for,
        deferred_create_targets,
        new_child_ids,
        refreshable_child_ids,
        printed_warnings,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_plan_from_parsed_with_upstream_with_ctx<E: Clone>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
    provider_configs: &ResolvedProviderConfigs,
    unresolved_resources: &[Resource],
    state_file: &Option<StateFile>,
    refresh: bool,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    state_block_claims: &StateBlockClaims,
    resolved_state_block_targets: &ResolvedStateBlockTargets,
    base_dir: &Path,
) -> Result<PlanContext, AppError> {
    let raw_provider_configs = provider_configs.as_provider_configs();
    // Mutable: a same-config deferred-for loop is expanded into concrete
    // resources *after* refresh (carina#3132) and the augmented set is
    // re-sorted in place below. Every use up to that point sees the
    // pre-expansion set (the loop's iterable source — e.g. `let cert` —
    // is a normal top-level resource already here and refreshed by the
    // normal phase-1 pass; only the loop's generated children are added).
    // carina#3181: `parsed.resources` is managed-only and
    // `parsed.data_sources` holds the `read`-keyword resources. Only
    // managed resources participate in the dependency sort; data sources
    // are refreshed in a later phase against the already-populated
    // `managed_current`.
    let mut sorted_resources =
        sort_resources_by_dependencies(&parsed.resources).map_err(AppError::Validation)?;
    let data_sources: Vec<DataSource> = parsed.data_sources.clone();

    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(ctx, provider_configs, base_dir).await?;

    let mut managed_current: HashMap<ResourceId, Current> = HashMap::new();
    let mut data_source_states: HashMap<ResourceId, State> = HashMap::new();

    // Build state-file-derived maps up front so anonymous → let-bound
    // rename transfer (#1685) can run between refresh phases 1 and 2.
    // These maps only depend on `state_file`, not on refresh output.
    // awscc#251: state files written before a provider promoted an
    // attribute from `Custom` to `Enum` (e.g. awscc#250 for IAM
    // policy `version`/`effect`) store enum values as plain JSON
    // strings. `build_saved_attrs` bridges those through the
    // schema-blind `PersistedValue` decoder into `ConcreteValue::String`,
    // which the strict carina#2986 Phase 4 validator then rejects at
    // the now-`Enum` position. Lift recognized members to
    // `ConcreteValue::EnumIdentifier` against each resource's current
    // schema before any diff/validation consumes the loaded state.
    // carina-state stays schema-free; the registry only exists here.
    let mut saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default()
        .lift(ctx.schemas());
    let mut prev_explicit = state_file
        .as_ref()
        .map(|sf| sf.build_explicit())
        .unwrap_or_default();
    // `moved_pairs` accumulates explicit `moved` block transfers and
    // detected anonymous → let-bound renames. Populated inside the
    // refresh block so the later plan-building code sees them.
    let mut moved_pairs: Vec<(ResolvedResourceId, ResolvedResourceId)> = Vec::new();
    // Ids the phase-1 orphan pass already performed a live provider read
    // for this run. A for-loop child applied on a previous run is in the
    // state file but not yet a desired resource at orphan time (expansion
    // is post-refresh), so the orphan pass live-reads it under the same
    // state name the child later resolves to. `expand_same_config_deferred_for`
    // excludes these from the post-expansion child refresh so the same
    // address is not read twice (carina#3145). Mirrored by the apply path
    // in `commands/apply/mod.rs`.
    let mut orphan_refreshed_ids: HashSet<ResourceId> = HashSet::new();
    let mut orphans = Orphans::new();

    // Running state: is indicatif's spinner bar region currently *open*
    // (cursor parked on an unterminated `✓` line)? Set true by any refresh
    // phase that draws bars (managed, orphan, data-source, deferred-for
    // children); set back to false when `print_warnings` emits a
    // newline-terminated `⚠` line over the open bar (which closes the
    // region). `finish_refresh_bar_region` reads the final value to close
    // the region exactly once, before the plan is printed (#3150). It is a
    // running flag, not a cumulative OR: a printed warning between phases
    // resets it (Round-4 finding — see the reset after expansion below).
    let mut refresh_printed_bars = false;
    let mut deferred_data_source_ids: HashSet<ResourceId> = HashSet::new();
    let module_gate = carina_core::executor::ModuleConstraintGate::new(&parsed.compositions);
    let mut refresh_module_constraint_errors = Vec::new();

    if refresh {
        RefreshProgress::start_header();
        let multi = refresh_multi_progress();

        // Read states for all resources concurrently using the identifier from
        // state. An address with no row is absent; a persisted row without an
        // identifier remains present and invalid so planning can fail closed.
        // Skip composition resources (module attribute containers) — they have no infrastructure.
        let provider_ref = &provider;
        // Pre-build a map of dependency_bindings from the state file so we can
        // restore them after refresh. Provider.read() returns fresh attributes but
        // doesn't know about dependency_bindings (carina-only metadata).
        let saved_dep_bindings: HashMap<ResourceId, BTreeSet<String>> = state_file
            .as_ref()
            .map(|sf| {
                sorted_resources
                    .iter()
                    .filter_map(|r| {
                        let identity = r.id.identity_str()?;
                        let rs = sf.find_resource(&r.id.provider, &r.id.resource_type, identity)?;
                        if rs.dependency_bindings.is_empty() {
                            None
                        } else {
                            Some((r.id.clone(), rs.dependency_bindings.clone()))
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Refresh in two phases so data sources can see concrete values
        // from their dependencies (#1683). Phase 1: managed resources in
        // parallel. Phase 2: data sources whose input attributes have
        // been resolved against phase 1's `managed_current`.
        refresh_printed_bars |= refresh_resource_set(
            provider_ref,
            &multi,
            sorted_resources.iter(),
            state_file,
            ctx.schemas(),
            &saved_dep_bindings,
            &mut managed_current,
        )
        .await?;

        // Refresh orphaned resources (#844). These are tracked in state
        // but removed from the .crn config — they're looked up by their
        // *old* name, which includes the pre-rename anonymous name of a
        // let-bound resource. Must run before the rename transfer below
        // so that transfer has the old-name state entries to move.
        if let Some(sf) = state_file.as_ref() {
            let desired_ids: HashSet<ResourceId> =
                sorted_resources.iter().map(|r| r.id.clone()).collect();
            let loaded_orphans = sf.build_orphans(&desired_ids);
            refresh_printed_bars |= !loaded_orphans.is_empty();
            let refreshed = refresh_loaded_orphans(
                provider_ref,
                loaded_orphans,
                Some(&multi),
                state_file.as_ref(),
                ctx.schemas(),
            )
            .await?;
            orphan_refreshed_ids.extend(
                refreshed
                    .iter()
                    .filter(|(_id, loaded)| match loaded.current() {
                        Current::Absent(_state) => false,
                        Current::Present(_present) => true,
                        Current::CreateInFlight(_pending) => true,
                        Current::MissingIdentifier(_state) => true,
                    })
                    .map(|(id, _loaded)| id.clone()),
            );
            orphans = refreshed.into_plan_orphans();
        }

        // Hydrate now — before phase 2 resolves data source refs — so
        // any attributes the provider's read() didn't return are
        // available when building the binding map (#1685).
        hydrate_prior_states(
            &mut managed_current,
            &mut data_source_states,
            &mut orphans,
            &provider,
            &saved_attrs,
            state_file.as_ref(),
        )
        .await?;

        // Transfer state for explicit `moved` blocks and anonymous →
        // let-bound renames (#1685). Must run before phase 2 so the ref
        // resolver sees state entries under their *new* binding name.
        // Detection operates on `sorted_resources` (pre-resolved), which
        // is sufficient for the common case of literal create-only
        // attributes; resources with ResourceRef create-only values are
        // an orthogonal edge case that pre-dates this fix.
        moved_pairs.extend(materialize_moved_states(
            &mut managed_current,
            &mut orphans,
            &mut prev_explicit,
            &mut saved_attrs,
            &parsed.state_blocks,
            state_file,
        )?);
        moved_pairs.extend(apply_anonymous_to_named_renames(
            ctx,
            &sorted_resources,
            raw_provider_configs,
            &mut managed_current,
            &mut orphans,
            &mut prev_explicit,
            &mut saved_attrs,
            state_file,
            state_block_claims,
        )?);
        // Phase 2: resolve data source refs against the consolidated
        // managed and data-source state, then refresh each via `read_data_source`.
        let ds_wait_aliases: Vec<WaitAliasSpec> = parsed
            .wait_bindings
            .iter()
            .map(WaitAliasSpec::from)
            .collect();
        let data_source_bindings = data_source_refresh_bindings(
            &sorted_resources,
            &parsed.compositions,
            &data_sources,
            &managed_current,
            &data_source_states,
            remote_bindings,
            ctx.schemas(),
            &ds_wait_aliases,
        );
        let data_source_refreshes = resolve_data_source_refs_for_refresh(
            &sorted_resources,
            &parsed.compositions,
            &data_sources,
            &managed_current,
            &data_source_states,
            remote_bindings,
            ctx.schemas(),
            &ds_wait_aliases,
        )?;
        let mut resolved_data_sources = Vec::new();
        for resolution in data_source_refreshes {
            match resolution {
                DataSourceRefreshResolution::Resolved(resource) => {
                    resolved_data_sources.push(resource);
                }
                DataSourceRefreshResolution::DeferredToApply { resource, .. } => {
                    deferred_data_source_ids.insert(resource.id);
                }
            }
        }
        if let Err(error) = module_gate.check(&data_source_bindings) {
            refresh_module_constraint_errors.extend(module_constraint_plan_errors(error));
        }
        refresh_printed_bars |=
            !resolved_data_sources.is_empty() && refresh_module_constraint_errors.is_empty();
        let phase2_results: Vec<Result<(ResourceId, State), AppError>> =
            if refresh_module_constraint_errors.is_empty() {
                stream::iter(resolved_data_sources.iter())
                    .map(|resource| {
                        let progress = RefreshProgress::begin_multi(&multi, &resource.id);
                        let dep_bindings = saved_dep_bindings.get(&resource.id).cloned();
                        let data_source_bindings = &data_source_bindings;
                        let module_gate = &module_gate;
                        async move {
                            let mut state = read_data_source_with_retry(
                                provider_ref,
                                resource,
                                data_source_bindings,
                                module_gate,
                                ctx.factories(),
                                ctx.schemas(),
                            )
                            .await?;
                            if let Some(deps) = dep_bindings {
                                state.dependency_bindings = deps;
                            }
                            progress.finish();
                            Ok((resource.id.clone(), state))
                        }
                    })
                    .buffer_unordered(5)
                    .collect()
                    .await
            } else {
                Vec::new()
            };
        for result in phase2_results {
            let (id, state) = result?;
            data_source_states.insert(id, state);
        }
    } else {
        // --refresh=false: use cached state from state file instead of calling provider.read()
        if let Some(sf) = state_file.as_ref() {
            for resource in &sorted_resources {
                let current = sf.build_state_for_resource(&resource.id);
                managed_current.insert(resource.id.clone(), current);
            }

            // Also include orphaned resources from state file
            let desired_ids: HashSet<ResourceId> =
                sorted_resources.iter().map(|r| r.id.clone()).collect();
            orphans = sf.build_orphans(&desired_ids).into_plan_orphans();

            // Transfer state for moved blocks and anonymous → let-bound
            // renames so the later `resolve_refs_with_state_and_remote`
            // call (and data-source-input resolution) sees entries
            // under their current binding names (#1685).
            hydrate_prior_states(
                &mut managed_current,
                &mut data_source_states,
                &mut orphans,
                &provider,
                &saved_attrs,
                state_file.as_ref(),
            )
            .await?;
            moved_pairs.extend(materialize_moved_states(
                &mut managed_current,
                &mut orphans,
                &mut prev_explicit,
                &mut saved_attrs,
                &parsed.state_blocks,
                state_file,
            )?);
            moved_pairs.extend(apply_anonymous_to_named_renames(
                ctx,
                &sorted_resources,
                raw_provider_configs,
                &mut managed_current,
                &mut orphans,
                &mut prev_explicit,
                &mut saved_attrs,
                state_file,
                state_block_claims,
            )?);
        } else {
            // No state file: all resources are new (not found)
            for resource in &sorted_resources {
                managed_current.insert(resource.id.clone(), Current::absent(resource.id.clone()));
            }
        }
    }

    let wait_aliases: Vec<WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(WaitAliasSpec::from)
        .collect();

    // carina#3132: expand same-config deferred-for loops after refresh,
    // against the same post-refresh `ResolvedBindings` view every
    // non-loop `ResourceRef` resolves against. Pure expand+re-sort is
    // factored into `expand_same_config_deferred_for` (see its doc);
    // here we perform the I/O half — targeted-refresh of the children
    // so a re-plan after they were applied sees their live state
    // instead of a phantom Create.
    let moved_targets: HashSet<ResourceId> = moved_pairs
        .iter()
        .map(|(_, to)| to.as_inner().clone())
        .collect();
    let DeferredForExpansion {
        sorted_resources: resorted,
        residual_deferred_for,
        deferred_create_targets,
        new_child_ids,
        refreshable_child_ids,
        printed_warnings,
    } = expand_same_config_deferred_for(
        parsed,
        &sorted_resources,
        &managed_current,
        &data_source_states,
        ctx.schemas(),
        remote_bindings,
        &wait_aliases,
        &moved_targets,
        &orphan_refreshed_ids,
    )?;
    sorted_resources = resorted;

    // A printed `⚠` warning is newline-terminated and is written on top of
    // indicatif's open spinner bar line, so it closes the bar region the
    // refresh phases above left open — the cursor is no longer parked on an
    // unterminated `✓` line. Clear the flag so `finish_refresh_bar_region`
    // does not add a second, spurious blank line on the
    // deferred-for-with-warnings TTY path (#3150 Round-4 finding). The
    // child-refresh phase below may re-open the region if it draws bars.
    if printed_warnings {
        refresh_printed_bars = false;
    }

    if !new_child_ids.is_empty() {
        // Refresh only `refreshable_child_ids` (carina#3141): a child
        // that is also a `moved` target keeps the migrated state from
        // `materialize_moved_states`; re-reading it here (any of the
        // three branches below) would clobber that state with
        // `not_found`. `select` is the only constructor for the refresh
        // set — the apply path uses the same one, so the moved-exclusion
        // cannot diverge between the two paths (compile-time parity).
        let children = || refreshable_child_ids.select(&sorted_resources, &orphans);
        if refresh {
            refresh_printed_bars |= refresh_resource_set(
                &provider,
                &refresh_multi_progress(),
                children(),
                state_file,
                ctx.schemas(),
                &HashMap::new(),
                &mut managed_current,
            )
            .await?;
            hydrate_prior_states(
                &mut managed_current,
                &mut data_source_states,
                &mut orphans,
                &provider,
                &saved_attrs,
                state_file.as_ref(),
            )
            .await?;
        } else if let Some(sf) = state_file.as_ref() {
            // --refresh=false: restore children's state from the cached
            // state file, same as the original resources. A child not in
            // cached state stays `not_found` → Create (mirrors the
            // non-loop ref's behavior under --refresh=false).
            for resource in children() {
                let current = sf.build_state_for_resource(&resource.id);
                managed_current.insert(resource.id.clone(), current);
            }
            hydrate_prior_states(
                &mut managed_current,
                &mut data_source_states,
                &mut orphans,
                &provider,
                &saved_attrs,
                state_file.as_ref(),
            )
            .await?;
        } else {
            for resource in children() {
                managed_current.insert(resource.id.clone(), Current::absent(resource.id.clone()));
            }
        }
    }

    // All refresh phases are done. Close indicatif's open bar line so the
    // separator + plan render below it instead of being swallowed (#3150).
    finish_refresh_bar_region(refresh_printed_bars);

    // awscc#251 (follow-up to #3055): data-source lifting stays here
    // because data sources do not flow through `into_plan_input_current_map`.
    carina_core::utils::lift_current_state_enum_leaves_for_data_sources(
        &mut data_source_states,
        &data_sources,
        ctx.schemas(),
    );
    // Build the unified pre-apply bindings view once (carina#3248):
    // every kind of binding the configuration declares (managed,
    // composition, data source) is in the same view, so a managed
    // attribute referencing `<module_instance>.<attr>` (a composition)
    // chains through to the managed sibling literal instead of
    // surviving as an unresolved `ResourceRef` (carina#3246).
    let upstream_binding_names: std::collections::HashSet<&str> =
        remote_bindings.keys().map(String::as_str).collect();
    let pre_apply_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        ctx.schemas(),
        &sorted_resources,
    );
    let mut override_aware_resources = OverrideAwareResources::build_for_plan(
        sorted_resources,
        state_file.as_ref(),
        PreApplyInputs {
            managed: &[],
            compositions: &parsed.compositions,
            data_sources: &data_sources,
            managed_current: &pre_apply_current,
            data_source_states: &data_source_states,
            remote_bindings,
            wait_aliases: &wait_aliases,
        },
        &upstream_binding_names,
    )?;
    crate::legacy_name_overrides::emit_legacy_override_warnings(
        &override_aware_resources,
        state_file.as_ref(),
    );
    let unresolved_override_aware_resources = OverrideAwareResources::build_for_plan(
        unresolved_resources.to_vec(),
        state_file.as_ref(),
        PreApplyInputs {
            managed: &[],
            compositions: &parsed.compositions,
            data_sources: &data_sources,
            managed_current: &pre_apply_current,
            data_source_states: &data_source_states,
            remote_bindings,
            wait_aliases: &wait_aliases,
        },
        &upstream_binding_names,
    )?;
    // Resolve and canonicalize only refresh-time data sources. Deferred
    // reads keep their original input refs so the plan/executor dependency
    // graph still contains the upstream create edge.
    let mut data_sources_for_plan = prepare_data_sources_for_plan(
        &data_sources,
        &deferred_data_source_ids,
        override_aware_resources.bindings(),
        Some(&upstream_binding_names),
    )?;
    let constraint_origin_resources = override_aware_resources
        .paired_unresolved_resources_with_binding_sources(&unresolved_override_aware_resources);
    let prepared_compositions = parsed.compositions.clone();

    // Run the normalization pipeline: host desired canonicalization → guest
    // desired normalization → host provider-output canonicalization/default tags →
    // guest state normalization. Host state lifting follows at the differ
    // seam. `create_plan` resolves typed wait RHS values itself.
    let wait_bindings = parsed.wait_bindings.clone();
    let preprocessor = PlanPreprocessor::new(&provider, ctx);
    let preparation = if refresh_module_constraint_errors.is_empty() {
        preprocessor
            .prepare(
                &mut override_aware_resources,
                &constraint_origin_resources,
                &module_gate,
                &mut managed_current,
                &mut data_source_states,
                &mut orphans,
                raw_provider_configs,
                &mut data_sources_for_plan,
                &data_sources,
            )
            .await
    } else {
        Err(PlanPreparationError::Plan(refresh_module_constraint_errors))
    };
    if let Err(error) = preparation {
        let errors = match error {
            PlanPreparationError::Plan(errors) => errors,
            PlanPreparationError::Provider(error) => return Err(AppError::Provider(error)),
        };
        let mut plan = Plan::new();
        for error in errors {
            plan.add_error(error);
        }
        let moved_origins = moved_pairs
            .iter()
            .map(|(from, to)| (to.as_inner().clone(), from.as_inner().clone()))
            .collect();
        let managed_current = into_all_prior_states(orphans, managed_current);
        return Ok(PlanContext {
            plan,
            provider,
            sorted_resources: constraint_origin_resources.clone(),
            unresolved_resources: constraint_origin_resources,
            compositions: prepared_compositions,
            data_sources: data_sources_for_plan,
            managed_current,
            data_source_states,
            moved_origins,
            upstream_snapshot: remote_bindings.clone(),
            prev_explicit,
            residual_deferred_for,
            expansion_trace: parsed.expansion_trace.clone(),
        });
    }

    // Anonymous resources whose identity attributes include ResourceRefs can
    // only be named and matched after plan-time refs and provider normalization
    // have made those create-only values concrete. Keep this late pass local to
    // the desired resources; the earlier mutable-state reconciliation still
    // owns state-file rename migration.
    let resolved_desired_ids = reconcile_late_anonymous_identities(
        ctx,
        LateAnonymousIdentityInputs {
            resources: &mut override_aware_resources,
            data_sources: &data_sources_for_plan,
            state_file: state_file.as_ref(),
            state_block_claims,
            providers: raw_provider_configs,
        },
    )?;
    validate_plan_time_state_block_collisions(
        &resolved_desired_ids,
        &moved_pairs,
        resolved_state_block_targets,
        state_file,
    )?;

    let reconciled_provider_states = provider_states_for(
        &managed_current,
        &orphans,
        override_aware_resources.resources(),
    );
    let reconciled_input_current = carina_core::resource::into_plan_input_current_map(
        reconciled_provider_states,
        ctx.schemas(),
        override_aware_resources.resources(),
    );
    override_aware_resources.rebuild_bindings_after_identity_reconciliation(
        RebuildBindingsInputs {
            compositions: &parsed.compositions,
            data_sources: &data_sources,
            managed_current: &reconciled_input_current,
            data_source_states: &data_source_states,
            remote_bindings,
            wait_aliases: &wait_aliases,
        },
    );

    // Update ordering looks directives up only for Update effects. Orphan
    // directives stay paired with their typed orphan state for Delete effects.
    let directives_map = state_file
        .as_ref()
        .map(|sf| sf.build_directives())
        .unwrap_or_default();
    let paired_unresolved_resources = override_aware_resources
        .paired_unresolved_resources_with_binding_sources(&unresolved_override_aware_resources);
    let resolved_resources =
        ResolvedResource::collect_resolved(override_aware_resources.resources().iter().cloned())?;
    let resolved_unresolved_resources =
        ResolvedResource::collect_resolved(paired_unresolved_resources.iter().cloned())?;
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        ctx.schemas(),
        override_aware_resources.resources(),
    );
    let mut plan = create_plan_with_cascades(
        &resolved_resources,
        &resolved_unresolved_resources,
        &data_sources_for_plan,
        &provider,
        PlanPriorStates::new(&plan_input_current, &data_source_states, &orphans),
        &directives_map,
        ctx.schemas(),
        &saved_attrs,
        &prev_explicit,
        &wait_bindings,
    )?;
    add_deposed_delete_effects(&mut plan, state_file, &moved_pairs);
    block_deletes_on_prior_consumer_updates(&mut plan, &directives_map);

    // Add state block effects (import/removed/moved) only after both the
    // differ and deposed-generation pass have emitted identifier errors.
    // `removed` is the explicit recovery that suppresses those errors; the
    // missing-provider-identifier E2E pins this ordering for live planning.
    // carina#3329: pass the same override-aware bindings + upstream_binding_names
    // the resource-attribute resolver uses so an `import { id = "${…}|…" }`
    // expression is folded into a concrete cloud identifier (when the
    // referenced binding is in scope) or stamped for display as
    // `(known after upstream apply: …)` when the referenced upstream
    // state has not been published yet.
    add_state_block_effects(
        &mut plan,
        &parsed.state_blocks,
        state_file,
        &moved_pairs,
        override_aware_resources.resources(),
        override_aware_resources.bindings(),
        &upstream_binding_names,
    );
    add_deferred_create_effects(&mut plan, &deferred_create_targets);

    let managed_current = into_all_prior_states(orphans, managed_current);

    let moved_origins: HashMap<ResourceId, ResourceId> = moved_pairs
        .iter()
        .map(|(from, to)| (to.as_inner().clone(), from.as_inner().clone()))
        .collect();
    Ok(PlanContext {
        plan,
        provider,
        // Keep the evaluated desired view distinct from the authored
        // pre-resolution snapshot. Before carina#3861 this field was filled
        // with `paired_unresolved_resources`; for let-bound resources that
        // pairing deliberately prefers `unresolved_parsed` and therefore
        // restored unevaluated calls such as `secret("...")`. Anonymous
        // resources have no binding and kept this evaluated view, producing
        // the bound/anonymous saved-plan divergence.
        sorted_resources: override_aware_resources.resources().to_vec(),
        unresolved_resources: paired_unresolved_resources,
        compositions: prepared_compositions,
        data_sources: data_sources_for_plan,
        managed_current,
        data_source_states,
        moved_origins,
        upstream_snapshot: remote_bindings.clone(),
        prev_explicit,
        residual_deferred_for,
        expansion_trace: parsed.expansion_trace.clone(),
    })
}

fn resolved_state_row_id(
    row: &carina_state::ResourceState,
    provider_instance: Option<String>,
) -> ResolvedResourceId {
    ResolvedResourceId::with_provider_identity(
        row.provider.clone(),
        row.resource_type.clone(),
        row.identity.clone(),
        provider_instance,
    )
}

/// Pre-process moved blocks by transferring the typed orphan/current state,
/// `prev_explicit`, and `saved_attrs` from the old resource name to the new
/// name.
///
/// This must be called BEFORE `create_plan()` so the differ sees the moved
/// resource's state under its new name and can produce Update/Replace effects
/// if attributes differ between state and desired. An undeclared destination
/// remains an orphan and is re-keyed as one value so its binding/directives
/// travel with its provider state. Transferring
/// `prev_explicit` ensures attribute removals are detected; transferring
/// `saved_attrs` ensures hydrated attributes are found under the new name.
///
/// Returns a list of active Move pairs (from, to) where the `from` resource
/// existed in state. Callers use this to add Move effects to the plan.
pub fn materialize_moved_states(
    managed_current: &mut HashMap<ResourceId, Current>,
    orphans: &mut Orphans,
    prev_explicit: &mut HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    saved_attrs: &mut LiftedSavedAttrs,
    state_blocks: &[StateBlock],
    state_file: &Option<StateFile>,
) -> Result<Vec<(ResolvedResourceId, ResolvedResourceId)>, AppError> {
    materialize_moved_states_with_warning_sink(
        managed_current,
        orphans,
        prev_explicit,
        saved_attrs,
        state_blocks,
        state_file,
        &mut |warning| eprintln!("{}", warning.yellow()),
    )
}

fn materialize_moved_states_with_warning_sink(
    managed_current: &mut HashMap<ResourceId, Current>,
    orphans: &mut Orphans,
    prev_explicit: &mut HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    saved_attrs: &mut LiftedSavedAttrs,
    state_blocks: &[StateBlock],
    state_file: &Option<StateFile>,
    warn_missing: &mut dyn FnMut(String),
) -> Result<Vec<(ResolvedResourceId, ResolvedResourceId)>, AppError> {
    let mut moved_pairs = Vec::new();

    for block in state_blocks {
        if let StateBlock::Moved { addresses } = block {
            let from = addresses.from();
            let to = addresses.to();
            // carina#3324: `from`/`to` are parsed from
            // `moved { from = X 'a', to = X 'b' }`, which has no
            // syntax for `provider_instance`. The state-resident
            // `from` may carry routing from its original
            // `directives { provider = ... }`, and the desired-state
            // `to` may similarly carry routing from the new let-bound
            // resource's directives. Resolve both to their full ids
            // so map lookups (which include `provider_instance` in
            // `Hash`/`Eq`) hit the right keys and the emitted
            // `(from, to)` pair matches every downstream consumer's
            // address shape.
            let resolved_from = state_file.as_ref().and_then(|sf| {
                sf.find_resource(&from.provider, &from.resource_type, from.name_str())
                    .map(|rs| resolved_state_row_id(rs, rs.directives.provider_instance.clone()))
            });
            let Some(resolved_from) = resolved_from else {
                let to_exists = state_file.as_ref().is_some_and(|sf| {
                    sf.find_resource(&to.provider, &to.resource_type, to.name_str())
                        .is_some()
                });
                if !to_exists {
                    warn_missing(format!(
                        "warning: moved block from {} '{}' to {} '{}' was not applied: {} not found in state",
                        from.display_type(),
                        from.name_str(),
                        to.display_type(),
                        to.name_str(),
                        from.name_str()
                    ));
                }
                continue;
            };
            // For `to`, look up the destination key by
            // `(provider, resource_type, name)` in the desired-state map.
            // Phase 1 inserts an entry (including `not_found`) for every
            // desired resource, so this both resolves provider routing and
            // tells us whether to promote an orphan into desired state or
            // re-key the complete orphan value for a still-undeclared target.
            let desired_to = find_desired_id(&to, managed_current.keys());
            let target_is_desired = desired_to.is_some();
            let resolved_to = desired_to.unwrap_or_else(|| to.to_unrouted_resource_id());
            let resolved_move =
                ResolvedMove::from_moved_block(addresses, resolved_from, resolved_to)?;
            let authority = resolved_move.authority();

            // Transfer state from the old name to the new name so the
            // differ compares desired(to) against actual(from).
            if target_is_desired {
                promote_orphan_or_move_current_state(managed_current, orphans, &authority)?;
            } else if orphans.rekey(&authority) {
            } else {
                rekey_current_for_move(managed_current, &authority)?;
            }

            // Transfer prev_explicit so the differ detects attribute
            // removals under the new resource name.
            if let Some(keys) = prev_explicit.remove(authority.from()) {
                prev_explicit.insert(authority.to().clone(), keys);
            }

            // Transfer saved_attrs so create_plan can look up saved
            // attributes under the new resource name.
            saved_attrs.remap_resource_id(authority.from(), authority.to().clone());

            moved_pairs.push(resolved_move.into_parts());
        }
    }

    Ok(moved_pairs)
}

fn promote_orphan_or_move_current_state(
    managed_current: &mut HashMap<ResourceId, Current>,
    orphans: &mut Orphans,
    authority: &MoveAuthority<'_>,
) -> Result<(), carina_core::differ::CurrentMoveRoutingMismatch> {
    if let Some(current) = orphans.take_rebound(authority) {
        managed_current.insert(authority.to().clone(), current);
    } else {
        rekey_current_for_move(managed_current, authority)?;
    }
    Ok(())
}

pub fn resolve_state_block_claims(
    blocks: &[StateBlock],
    state_file: &Option<StateFile>,
    desired: &[Resource],
    registry: &SchemaRegistry,
) -> StateBlockClaims {
    resolve_state_blocks(blocks, state_file, desired, registry).claims
}

pub fn resolve_state_blocks(
    blocks: &[StateBlock],
    state_file: &Option<StateFile>,
    desired: &[Resource],
    _registry: &SchemaRegistry,
) -> StateBlockResolution {
    let mut from = HashSet::new();
    let mut to = HashSet::new();
    let mut resolved_removed_from = Vec::new();

    for block in blocks {
        match block {
            StateBlock::Moved { addresses } => {
                let moved_from = addresses.from();
                let moved_to = addresses.to();
                if state_file.as_ref().is_some_and(|sf| {
                    sf.find_resource(
                        &moved_from.provider,
                        &moved_from.resource_type,
                        moved_from.name_str(),
                    )
                    .is_some()
                }) {
                    from.insert(moved_from);
                    to.insert(moved_to);
                }
            }
            StateBlock::Removed { from: removed_from } => {
                if let Some(id) = state_file.as_ref().and_then(|sf| {
                    sf.find_resource(
                        &removed_from.provider,
                        &removed_from.resource_type,
                        removed_from.name_str(),
                    )
                    .map(|rs| resolved_state_row_id(rs, rs.directives.provider_instance.clone()))
                }) {
                    from.insert(removed_from.clone());
                    resolved_removed_from.push(id.into_inner());
                }
            }
            StateBlock::Import { to: import_to, .. } => {
                let Some(resolved_to) = resolve_import_target_in_desired(import_to, desired) else {
                    continue;
                };
                let already_in_state = state_file.as_ref().is_some_and(|sf| {
                    sf.find_resource(
                        &resolved_to.provider,
                        &resolved_to.resource_type,
                        resolved_to.name_str(),
                    )
                    .is_some()
                });
                if !already_in_state {
                    to.insert(resolved_to);
                }
            }
        }
    }

    StateBlockResolution {
        claims: StateBlockClaims::new(from, to),
        targets: ResolvedStateBlockTargets {
            removed_from: resolved_removed_from,
        },
    }
}

pub fn validate_plan_time_state_block_collisions(
    desired: &ResolvedDesiredIds,
    moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
    resolved_targets: &ResolvedStateBlockTargets,
    state_file: &Option<StateFile>,
) -> Result<(), AppError> {
    for (from, _to) in moved_pairs {
        if desired.ids.contains(from) {
            return Err(AppError::Validation(format!(
                "moved/rename pair from {} collides with a desired resource: applying this plan would both upsert and clean up the same resource id",
                from.human()
            )));
        }
    }

    for from in &resolved_targets.removed_from {
        if desired.ids.contains(from) {
            return Err(AppError::Validation(format!(
                "removed block from {} collides with desired resource {}: applying this plan would both upsert and clean up the same resource id",
                from.human(),
                from.human()
            )));
        }
    }

    let mut seen_to: HashMap<&ResolvedResourceId, &ResolvedResourceId> = HashMap::new();
    let mut seen_from: HashMap<&ResolvedResourceId, &ResolvedResourceId> = HashMap::new();
    for (from, to) in moved_pairs {
        match seen_to.entry(to) {
            Entry::Occupied(first) => {
                return Err(AppError::Validation(format!(
                    "two moved/rename pairs resolve to the same target {}: {} -> {} and {} -> {} would overwrite state during plan application",
                    to.human(),
                    first.get().human(),
                    to.human(),
                    from.human(),
                    to.human()
                )));
            }
            Entry::Vacant(slot) => {
                slot.insert(from);
            }
        }

        match seen_from.entry(from) {
            Entry::Occupied(first_to) => {
                return Err(AppError::Validation(format!(
                    "two moved/rename pairs share the same source {}: {} -> {} and {} -> {} cannot both transfer the same state entry",
                    from.human(),
                    from.human(),
                    first_to.get().human(),
                    from.human(),
                    to.human()
                )));
            }
            Entry::Vacant(slot) => {
                slot.insert(to);
            }
        }
    }

    let Some(sf) = state_file.as_ref() else {
        return Ok(());
    };

    for (from, to) in moved_pairs {
        let from_exists = sf
            .find_resource(&from.provider, &from.resource_type, from.identity_str())
            .is_some();
        if from == to && from_exists {
            return Err(AppError::Validation(format!(
                "moved block from and to name the same address {}: a self-move cannot transfer state",
                from.human()
            )));
        }
        let to_exists = sf
            .find_resource(&to.provider, &to.resource_type, to.identity_str())
            .is_some();
        if from_exists && to_exists {
            return Err(AppError::Validation(format!(
                "moved/rename pair {} -> {} would overwrite an existing state entry at {}: applying this plan would drop the occupied target state row",
                from.human(),
                to.human(),
                to.human()
            )));
        }
    }

    Ok(())
}

/// Look up `to`'s full id (with any routed `provider_instance`) in
/// `desired` by matching a [`StateBlockAddress`] against the routed
/// `ResourceId` keys. `StateBlockAddress` is routing-agnostic by
/// construction, so this is the only place the routing for the
/// destination of a `moved { ... }` block can be derived. Returns
/// `None` when no matching key exists, so callers can chain across
/// multiple maps with `.or_else(...)` (carina#3324).
fn find_desired_id<'a>(
    to: &StateBlockAddress,
    mut desired: impl Iterator<Item = &'a ResourceId>,
) -> Option<ResolvedResourceId> {
    desired
        .find(|k| {
            k.provider == to.provider
                && k.resource_type == to.resource_type
                && k.identity_str() == Some(to.name_str())
        })
        .cloned()
        .and_then(ResolvedResourceId::try_new)
}

fn resolve_import_target_in_desired(
    to: &StateBlockAddress,
    desired: &[Resource],
) -> Option<StateBlockAddress> {
    match_import_target(to, desired.iter()).and_then(|resource| {
        let identity = resource.id.identity()?.clone();
        Some(StateBlockAddress::new(
            resource.id.provider.clone(),
            resource.id.resource_type.clone(),
            identity,
        ))
    })
}

/// Add state block effects (import/removed/moved) to the plan.
///
/// State blocks become no-ops on subsequent runs when:
/// - Import: the resource already exists in state
/// - Removed: the resource does not exist in state
/// - Moved: the old resource does not exist in state (already moved)
///
/// For `moved` blocks, `materialize_moved_states()` must be called before
/// `create_plan()` to transfer state entries. The pre-computed `moved_pairs`
/// are passed here to add Move effects without re-checking state.
///
/// This function also removes Delete effects for resources covered by
/// `removed` blocks, since those operations manage state without
/// destroying infrastructure.
pub fn add_state_block_effects(
    plan: &mut Plan,
    state_blocks: &[StateBlock],
    state_file: &Option<StateFile>,
    moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
    desired_resources: &[Resource],
    // carina#3329: resolved bindings + the set of upstream-state
    // binding names whose surviving refs are stamped as
    // `Value::Deferred(DeferredValue::Unknown(UpstreamExport { … }))`.
    // The `id = "${X.attr}|…"` interpolation inside an `import` block
    // is resolved here against the same view that the differ uses for
    // resource attributes (`resolve_refs_for_plan`), so a deferred
    // upstream-state ref carries through as a `(known after upstream
    // apply: …)` placeholder instead of being silently substituted to
    // empty.
    bindings: &carina_core::binding_index::ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
) {
    // Collect resource IDs that are covered by removed blocks
    // to suppress orphan Delete effects
    let mut suppress_delete: std::collections::HashSet<ResourceId> =
        std::collections::HashSet::new();
    let mut suppress_create: std::collections::HashSet<ResourceId> =
        std::collections::HashSet::new();
    let mut suppress_identifier_errors: std::collections::HashSet<StateBlockAddress> =
        std::collections::HashSet::new();
    let desired_ids: std::collections::HashSet<ResourceId> = desired_resources
        .iter()
        .map(|resource| resource.id.clone())
        .collect();

    let mut new_effects: Vec<Effect> = Vec::new();

    for block in state_blocks {
        match block {
            StateBlock::Import { to, id } => {
                let effective_to = resolve_import_target(to, desired_resources);

                // Skip if resource already exists in state
                let already_in_state = state_file.as_ref().is_some_and(|sf| {
                    sf.find_resource(
                        &effective_to.provider,
                        &effective_to.resource_type,
                        effective_to.identity_str(),
                    )
                    .is_some()
                });
                if !already_in_state {
                    // carina#3329: resolve any `${binding.attr}` segments
                    // in `id` against the same binding view the resource
                    // attribute resolver uses. If the referenced binding
                    // is a same-stack `let`, this folds the interpolation
                    // to a `Value::Concrete(ConcreteValue::String)`; if
                    // it is an `upstream_state` binding whose state has
                    // not been published yet, it is stamped as
                    // `Value::Deferred(DeferredValue::Unknown(UpstreamExport))`
                    // for display. The pre-#3329 path stored a partially-
                    // substituted `String` and dropped the deferred-ref
                    // signal entirely.
                    let resolved_id = carina_core::resolver::resolve_value_for_plan(
                        id,
                        bindings,
                        unresolved_upstream_bindings,
                    );
                    suppress_create.insert(effective_to.as_inner().clone());
                    if carina_core::effect::resolve_import_identifier(&resolved_id)
                        .is_ok_and(|identifier| AppliedIdentifier::new(identifier).is_err())
                    {
                        plan.add_error(carina_core::plan::PlanError::new(
                            effective_to.as_inner().clone(),
                            carina_core::plan::PlanErrorKind::InvalidImportProviderIdentifier,
                        ));
                        continue;
                    }
                    new_effects.push(Effect::Import {
                        id: effective_to,
                        identifier: resolved_id,
                    });
                }
            }
            StateBlock::Removed { from } => {
                // `from` is parsed from `removed { from = X 'addr' }`,
                // which has no syntax for `provider_instance`. The
                // resource in state may still carry a routed instance
                // from its original `directives { provider = ... }`.
                // Resolve to the full state id (inheriting the
                // routing) so the emitted Remove effect and the
                // `suppress_delete` HashSet key match the orphan
                // Delete effect's id exactly — same root fix as the
                // Import arm above (carina#3324).
                let resolved_from = state_file.as_ref().and_then(|sf| {
                    sf.find_resource(&from.provider, &from.resource_type, from.name_str())
                        .map(|rs| {
                            let id =
                                resolved_state_row_id(rs, rs.directives.provider_instance.clone());
                            let abandoned_deposed = rs
                                .deposed
                                .iter()
                                .filter(|deposed| {
                                    AppliedIdentifier::new(deposed.identifier.as_str()).is_err()
                                })
                                .map(|deposed| {
                                    (
                                        resolved_state_row_id(
                                            rs,
                                            deposed.provider_instance.clone(),
                                        ),
                                        deposed.key.clone(),
                                    )
                                })
                                .collect::<Vec<_>>();
                            (id, abandoned_deposed)
                        })
                });
                if let Some((id, abandoned_deposed)) = resolved_from {
                    suppress_delete.insert(id.as_inner().clone());
                    suppress_identifier_errors.insert(from.clone());
                    for (deposed_id, key) in abandoned_deposed {
                        plan.abandon_deposed_generation(deposed_id, key);
                    }
                    new_effects.push(Effect::Remove { id });
                }
            }
            StateBlock::Moved { .. } => {
                // Moved blocks are handled by materialize_moved_states() + moved_pairs below
            }
        }
    }

    // Add Move effects from pre-computed moved pairs.
    // Move effects are always added to the plan (for summary counting),
    // but display skips the Move line when an Update/Replace with
    // "(moved from: ...)" annotation already conveys the information.
    // Also suppress orphan Delete for `to` when there is no desired resource
    // for the target (the moved state entry would otherwise appear as an orphan).
    for (from, to) in moved_pairs {
        if !desired_ids.contains(to.as_inner()) {
            suppress_delete.insert(to.as_inner().clone());
        }
        new_effects.push(Effect::Move {
            from: from.clone(),
            to: to.clone(),
        });
    }

    // Remove Delete effects for resources covered by removed blocks,
    // and Create effects for import targets (they will be imported, not created)
    if !suppress_delete.is_empty() || !suppress_create.is_empty() {
        plan.retain(|effect| match effect {
            Effect::Delete {
                id,
                generation: EffectGeneration::Current,
                ..
            } => !suppress_delete.contains(id),
            Effect::Delete {
                generation: EffectGeneration::Deposed(_),
                ..
            } => true,
            Effect::Create(resource) => !suppress_create.contains(&resource.id),
            _ => true,
        });
    }

    // A removed block abandons the whole persisted row. Suppress identifier
    // errors for both its current instance and its deposed generations (whose
    // provider-instance routing can differ), allowing Remove to be applied.
    if !suppress_identifier_errors.is_empty() {
        plan.retain_errors(|error| {
            if !matches!(
                error.kind,
                carina_core::plan::PlanErrorKind::MissingProviderIdentifier
                    | carina_core::plan::PlanErrorKind::CreateInFlightOrphan
                    | carina_core::plan::PlanErrorKind::MissingDeposedProviderIdentifier { .. }
            ) {
                return true;
            }
            let Some(identity) = error.resource_id.identity() else {
                return true;
            };
            !suppress_identifier_errors.contains(&StateBlockAddress::new(
                error.resource_id.provider.clone(),
                error.resource_id.resource_type.clone(),
                identity.clone(),
            ))
        });
    }

    // Add the new state block effects
    for effect in new_effects {
        plan.add(effect);
    }
}

/// Add apply-time deferred-for re-expansion effects to the plan.
///
/// Matching orphan `Delete` effects are absorbed into `DeferredReplace`; targets
/// with no matching deletes remain plain `DeferredCreate` effects.
pub fn add_deferred_create_effects(plan: &mut Plan, targets: &[DeferredCreateTarget]) {
    let mut deletes_to_absorb: HashSet<ResourceId> = HashSet::new();
    let mut deferred_effects = Vec::new();

    for target in targets {
        let deletes: Vec<DeferredReplaceDelete> = plan
            .effects()
            .iter()
            .filter_map(|effect| {
                let Effect::Delete {
                    id,
                    identifier,
                    generation: EffectGeneration::Current,
                    directives,
                    binding: Some(binding),
                    dependencies,
                    explicit_dependencies,
                    blocked_by_updates,
                } = effect
                else {
                    return None;
                };

                if id.resource_type == target.template.template_resource.id.resource_type
                    && binding_matches_deferred_template(binding, &target.template.binding_name)
                {
                    Some(DeferredReplaceDelete {
                        id: id.clone(),
                        identifier: identifier.clone(),
                        directives: directives.clone(),
                        binding: Some(binding.clone()),
                        dependencies: dependencies.clone(),
                        explicit_dependencies: explicit_dependencies.clone(),
                        blocked_by_updates: blocked_by_updates.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();

        if deletes.is_empty() {
            deferred_effects.push(target.to_effect());
        } else {
            deletes_to_absorb.extend(deletes.iter().map(|delete| delete.id.clone().into_inner()));
            deferred_effects.push(target.to_deferred_replace_effect(deletes));
        }
    }

    if !deletes_to_absorb.is_empty() {
        plan.retain(|effect| match effect {
            Effect::Delete {
                id,
                generation: EffectGeneration::Current,
                ..
            } => !deletes_to_absorb.contains(id),
            _ => true,
        });
    }

    for effect in deferred_effects {
        plan.add(effect);
    }
}

/// Add delete-pending deposed generations from state to the plan.
///
/// Each entry uses its own provider-instance routing and dependency bindings,
/// not the row's current-instance routing.
pub fn add_deposed_delete_effects(
    plan: &mut Plan,
    state_file: &Option<StateFile>,
    moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
) {
    let Some(state_file) = state_file.as_ref() else {
        return;
    };

    for row in state_file.resources() {
        let (effects, errors) = deposed_delete_effects_for_row(row, moved_pairs);
        for effect in effects {
            plan.add(effect);
        }
        for error in errors {
            plan.add_error(error);
        }
    }
}

/// Build top-level delete effects for every deposed generation on a state row.
///
/// Each deposed entry owns its provider-instance routing and dependency
/// bindings; callers must not substitute the current row's routing here.
pub(crate) fn deposed_delete_effects_for_row(
    row: &carina_state::ResourceState,
    moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
) -> (Vec<Effect>, Vec<PlanError>) {
    let mut effects = Vec::with_capacity(row.deposed.len());
    let mut errors = Vec::new();
    let blocked_by_current_resume = match row.current() {
        carina_state::CurrentInstance::CreateInFlight { .. } => HashSet::from([moved_pairs
            .iter()
            .find_map(|(from, to)| {
                let row_id = resolved_state_row_id(row, row.directives.provider_instance.clone());
                (from.as_inner() == row_id.as_inner()).then(|| to.identity().clone())
            })
            .unwrap_or_else(|| row.identity.clone())]),
        carina_state::CurrentInstance::None
        | carina_state::CurrentInstance::Applied { .. }
        | carina_state::CurrentInstance::MissingIdentifier { .. } => HashSet::new(),
    };
    for deposed in &row.deposed {
        let id = resolved_state_row_id(row, deposed.provider_instance.clone());
        match PlanError::require_deposed_provider_identifier(
            id.as_inner(),
            &deposed.key,
            deposed.identifier.as_str(),
            row.identifier().map(AppliedIdentifier::as_str),
        ) {
            Ok(identifier) => effects.push(Effect::Delete {
                id,
                identifier,
                generation: EffectGeneration::Deposed(deposed.key.clone()),
                directives: Default::default(),
                binding: row.binding.clone(),
                dependencies: deposed.dependency_bindings.iter().cloned().collect(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: blocked_by_current_resume.clone(),
            }),
            Err(error) => errors.push(*error),
        }
    }
    (effects, errors)
}

/// Resolve an import block's `to` address to a matching resource in the plan or state.
///
/// Match keys on `(provider, resource_type, name)`. **`provider_instance`
/// is intentionally not part of the address** — `to = X 'addr'` in DSL
/// has no syntax for specifying a routed instance, so the parsed `to`
/// always has `provider_instance = None`. The let-bound resource it
/// targets may carry `directives { provider = <name> }`, which the
/// parser stamps as `provider_instance = Some(<name>)`. Treating the
/// instance as part of the address would falsely make these addresses
/// non-equal and silently emit both an Import and a Create for the
/// same infrastructure (carina#3324).
///
/// The function returns the matched resource's full id (inheriting
/// its `provider_instance`), so:
/// - The Import effect carries the routing the operator's directives
///   selected — apply-time routing sends the import to the right
///   provider instance.
/// - The `suppress_create` set in [`add_state_block_effects`] keys on
///   exactly the same id as the Create's resource, so the Create is
///   suppressed.
///
/// Match precedence:
/// 1. Exact `(provider, resource_type, name)` against the prepared desired
///    resources, inheriting its routed provider instance.
/// 2. The routing-agnostic address from the state block.
pub(crate) fn resolve_import_target(
    to: &StateBlockAddress,
    desired: &[Resource],
) -> ResolvedResourceId {
    match_import_target(to, desired.iter())
        .map(|resource| ResolvedResourceId::new(resource.id.clone()))
        .unwrap_or_else(|| to.to_unrouted_resource_id())
}

pub(crate) fn match_import_target<'a>(
    to: &StateBlockAddress,
    resources: impl Iterator<Item = &'a Resource>,
) -> Option<&'a Resource> {
    for resource in resources {
        if resource.id.provider != to.provider || resource.id.resource_type != to.resource_type {
            continue;
        }
        if resource.id.identity_str() == Some(to.name_str()) {
            return Some(resource);
        }
    }
    None
}

/// Check whether a `ProviderError` is an AWS throttling error that should be retried.
fn is_throttling_error(err: &ProviderError) -> bool {
    let msg = err.to_string();
    msg.contains("ThrottlingException") || msg.contains("Rate exceeded")
}

/// Read a resource via the provider with retry and exponential backoff for throttling errors.
///
/// Retries up to 3 times with delays of 1s, 2s, 4s when the error looks like an
/// AWS throttling / rate-limit response.
pub async fn read_with_retry(
    provider: &dyn Provider,
    id: &ResourceId,
    identifier: &AppliedIdentifier,
) -> Result<State, ProviderError> {
    let max_retries = 3;
    for attempt in 0..=max_retries {
        match provider
            .read(id, identifier, carina_core::provider::ReadRequest)
            .await
        {
            Ok(outcome) => return Ok(outcome.into_state()),
            Err(ProviderError::NotFound(_)) => {
                // carina-rs/carina-provider-aws#462: ProviderError::NotFound is
                // the canonical drift signal — the resource is recorded in state
                // but no longer exists in the cloud. Translate it into
                // State::not_found(id) so the planner emits a Create-this-back
                // plan instead of aborting.
                return Ok(State::not_found(id.clone()));
            }
            Err(e) if attempt < max_retries && is_throttling_error(&e) => {
                let delay = Duration::from_secs(1 << attempt); // 1s, 2s, 4s
                eprintln!(
                    "  Throttled reading {}, retrying in {}s...",
                    id,
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Read one managed instance and restore host-owned write-only comparison
/// values from the persisted row when the provider omits them.
pub(crate) async fn read_managed_state_with_retry(
    provider: &dyn Provider,
    id: &ResourceId,
    identifier: &AppliedIdentifier,
    state_file: Option<&StateFile>,
    schemas: &SchemaRegistry,
) -> Result<State, ProviderError> {
    let mut state = read_with_retry(provider, id, identifier).await?;
    if let Some(state_file) = state_file {
        state_file.restore_recorded_write_only_attributes_in_state(&mut state, schemas);
    }
    Ok(state)
}

/// Refresh one managed state-file entry without erasing a malformed row.
///
/// Only [`Current::Present`] may cross the provider read boundary.
/// A missing-identifier row stays present in cached state so planning can emit
/// its actionable error, while an absent address remains `not_found`.
pub async fn refresh_loaded_resource_with_retry(
    provider: &dyn Provider,
    loaded: Current,
    state_file: Option<&StateFile>,
    schemas: &SchemaRegistry,
) -> Result<Current, ProviderError> {
    match loaded {
        Current::Present(present) => {
            let (state, identifier) = present.into_parts();
            read_managed_state_with_retry(provider, &state.id, &identifier, state_file, schemas)
                .await
                .map(Current::from_state)
        }
        Current::Absent(state) => Ok(Current::Absent(state)),
        Current::CreateInFlight(pending) => Ok(Current::CreateInFlight(pending)),
        Current::MissingIdentifier(state) => Ok(Current::MissingIdentifier(state)),
    }
}

/// Refresh one typed orphan without allowing a missing identifier to cross the
/// provider boundary.
///
/// The missing condition stays attached to the returned orphan so a later
/// planning conversion still fails closed.
pub async fn refresh_loaded_orphan_with_retry(
    provider: &dyn Provider,
    loaded: Orphan,
    state_file: Option<&StateFile>,
    schemas: &SchemaRegistry,
) -> Result<Orphan, ProviderError> {
    let provider_read = match loaded.current() {
        Current::Absent(_state) => None,
        Current::Present(present) => {
            Some((present.state().id.clone(), present.identifier().clone()))
        }
        Current::CreateInFlight(_pending) => None,
        Current::MissingIdentifier(_state) => None,
    };
    match provider_read {
        Some((id, identifier)) => {
            let fresh =
                read_managed_state_with_retry(provider, &id, &identifier, state_file, schemas)
                    .await?;
            Ok(loaded.with_refreshed_state(fresh))
        }
        None => Ok(loaded),
    }
}

async fn refresh_loaded_orphans(
    provider: &dyn Provider,
    loaded: LoadedOrphans,
    multi: Option<&indicatif::MultiProgress>,
    state_file: Option<&StateFile>,
    schemas: &SchemaRegistry,
) -> Result<LoadedOrphans, AppError> {
    let results: Vec<Result<Orphan, AppError>> = stream::iter(loaded)
        .map(|(_id, loaded)| {
            let id = match loaded.current() {
                Current::Absent(state) => &state.state().id,
                Current::Present(present) => &present.state().id,
                Current::CreateInFlight(pending) => pending.id(),
                Current::MissingIdentifier(state) => &state.state().id,
            };
            let progress = multi.map(|multi| RefreshProgress::begin_multi(multi, id));
            async move {
                let refreshed =
                    refresh_loaded_orphan_with_retry(provider, loaded, state_file, schemas)
                        .await
                        .map_err(AppError::Provider)?;
                if let Some(progress) = progress {
                    progress.finish();
                }
                Ok(refreshed)
            }
        })
        .buffer_unordered(5)
        .collect()
        .await;
    let mut refreshed = LoadedOrphans::default();
    for result in results {
        refreshed.insert(result?);
    }
    Ok(refreshed)
}

/// Refresh a set of managed resources concurrently and merge the
/// results into `managed_current`.
///
/// Shared by the phase-1 refresh and the carina#3132 post-expansion
/// child refresh on **both** the plan path (this module) and the apply
/// path (`commands::apply`): same `stream::iter → begin_multi →
/// read_with_retry → buffer_unordered(5)` pipeline. `saved_dep_bindings`
/// restores carina-only `dependency_bindings` the provider's `read()`
/// does not return (#1565); pass an empty map when there is nothing to
/// restore (the new loop children have no prior state-file dep
/// bindings).
/// Returns `true` iff at least one refresh spinner bar was started (i.e. the
/// filtered iterator was non-empty). The caller uses this to decide whether
/// the indicatif bar region needs an explicit terminating newline before the
/// plan is printed — see [`finish_refresh_bar_region`].
pub(crate) async fn refresh_resource_set<'a>(
    provider: &dyn Provider,
    multi: &indicatif::MultiProgress,
    resources: impl Iterator<Item = &'a Resource>,
    state_file: &Option<StateFile>,
    schemas: &SchemaRegistry,
    saved_dep_bindings: &HashMap<ResourceId, BTreeSet<String>>,
    managed_current: &mut HashMap<ResourceId, Current>,
) -> Result<bool, AppError> {
    let mut started_bar = false;
    let results: Vec<Result<(ResourceId, Current), AppError>> =
        // carina#3181: `resources` is a managed-resource iterator —
        // data sources go through the data-source refresh path and
        // compositions carry no provider state.
        stream::iter(resources)
            .map(|resource| {
                started_bar = true;
                let progress = RefreshProgress::begin_multi(multi, &resource.id);
                let loaded = state_file
                    .as_ref()
                    .map(|sf| sf.build_state_for_resource(&resource.id))
                    .unwrap_or_else(|| Current::absent(resource.id.clone()));
                let dep_bindings = saved_dep_bindings.get(&resource.id).cloned();
                async move {
                    let mut current = refresh_loaded_resource_with_retry(
                        provider,
                        loaded,
                        state_file.as_ref(),
                        schemas,
                    )
                    .await
                    .map_err(AppError::Provider)?;
                    if let Some(deps) = dep_bindings {
                        current.set_dependency_bindings(deps);
                    }
                    progress.finish();
                    Ok((resource.id.clone(), current))
                }
            })
            .buffer_unordered(5)
            .collect()
            .await;
    for result in results {
        let (id, current) = result?;
        managed_current.insert(id, current);
    }
    Ok(started_bar)
}

/// Terminate indicatif's spinner-bar region so a following `print!` starts on
/// a fresh line.
///
/// Root cause of #3150: indicatif draws the refresh spinners to **stderr**
/// (`MultiProgress::new()`'s default draw target in indicatif 0.17) and
/// leaves the **last** finished bar line *without* a terminating newline
/// (the cursor is parked at the end of `✓ <name> [<elapsed>s]`). Under a
/// TTY stdout and stderr are the *same terminal device*, so the next thing
/// written to stdout — the `\n` separator from
/// [`crate::display::refresh_plan_separator`] — lands right after that open
/// bar line and is consumed just closing it, so no blank line appears
/// before `Execution Plan:` / `No changes.`. This emits the one newline
/// that closes the bar region; the separator then renders as the single
/// intended blank line.
///
/// Only needed when the bar region is actually open on the terminal the
/// plan prints to. `started_bar` is the running "region open" state
/// (false for the empty-config header-only case, and reset when a
/// `print_warnings` `⚠` line already closed the region). The
/// `stdout().is_terminal()` gate excludes the piped path: there
/// `refresh_multi_progress` redirects bars to stderr-only (keyed on
/// exactly `!stdout().is_terminal()`) while the plan goes to a piped
/// stdout whose `Refreshing state...` header is already newline-terminated
/// by its `println!`, so no close is needed. On a TTY both fds are the
/// same device, so `stdout().is_terminal()` correctly proxies "the open
/// bar line is on the device we are about to print the plan to".
pub(crate) fn finish_refresh_bar_region(started_bar: bool) {
    if started_bar && std::io::stdout().is_terminal() {
        println!();
    }
}

/// Read a data source resource via the provider with retry on throttling errors.
///
/// Same backoff policy as [`read_with_retry`] but uses [`Provider::read_data_source`]
/// so the provider receives the full [`DataSource`](carina_core::resource::DataSource)
/// (including user-supplied input attributes) rather than just the identifier.
pub async fn read_data_source_with_retry(
    provider: &dyn Provider,
    resource: &carina_core::resource::DataSource,
    bindings: &ResolvedBindings,
    module_gate: &carina_core::executor::ModuleConstraintGate,
    factories: &[Box<dyn carina_core::provider::ProviderFactory>],
    schemas: &carina_core::schema::SchemaRegistry,
) -> Result<State, AppError> {
    let preparation = carina_core::executor::ProviderPreparationContext::new(
        bindings,
        module_gate,
        &[],
        &carina_core::provider::NoopNormalizer,
        factories,
        schemas,
    );
    let ready =
        carina_core::executor::prepare_provider_ready_data_source(resource.clone(), &preparation)
            .map_err(|source| AppError::from_resource_preparation(resource.id.clone(), source))?;
    carina_core::executor::read_data_source_with_retry(provider, &ready)
        .await
        .map_err(AppError::Provider)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DataSourceRefreshResolution {
    Resolved(DataSource),
    DeferredToApply {
        resource: DataSource,
        unresolved: Vec<UnresolvedDataSourceInput>,
    },
}

/// Resolve `ResourceRef` values in refreshable data-source input attributes
/// against already-refreshed managed current state, and explicitly classify reads
/// that must wait for apply-time publication.
///
/// The `managed` slice is passed (not just the data sources) because the
/// resolver builds its binding map from every managed resource with a
/// `binding` — data sources reference those.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_data_source_refs_for_refresh(
    managed: &[Resource],
    compositions: &[carina_core::resource::Composition],
    data_sources: &[DataSource],
    managed_current: &HashMap<ResourceId, Current>,
    data_source_states: &HashMap<ResourceId, State>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    schemas: &carina_core::schema::SchemaRegistry,
    wait_aliases: &[WaitAliasSpec],
) -> Result<Vec<DataSourceRefreshResolution>, AppError> {
    // carina#3248: unified pre-apply bindings include compositions so a
    // data-source input referencing `<module_instance>.<attr>` chains
    // through the composition layer to the managed sibling literal.
    let bindings = data_source_refresh_bindings(
        managed,
        compositions,
        data_sources,
        managed_current,
        data_source_states,
        remote_bindings,
        schemas,
        wait_aliases,
    );
    let deferred =
        classify_apply_time_data_source_read_inputs(data_sources, managed, managed_current);

    let mut resolutions = Vec::with_capacity(data_sources.len());
    for resource in data_sources {
        if let Some(unresolved) = deferred.get(&resource.id) {
            resolutions.push(DataSourceRefreshResolution::DeferredToApply {
                resource: resource.clone(),
                unresolved: unresolved.clone(),
            });
            continue;
        }

        let mut resolved = resource.clone();
        carina_core::resolver::resolve_data_source_refs(
            std::slice::from_mut(&mut resolved),
            &bindings,
        )
        .map_err(AppError::Validation)?;
        carina_core::value::canonicalize_data_sources_with_schemas(
            std::slice::from_mut(&mut resolved),
            schemas,
        )?;
        let unresolved = unresolved_data_source_inputs(&resolved);
        if unresolved.is_empty() {
            resolutions.push(DataSourceRefreshResolution::Resolved(resolved));
        } else {
            resolutions.push(DataSourceRefreshResolution::DeferredToApply {
                resource: resource.clone(),
                unresolved,
            });
        }
    }

    Ok(resolutions)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn data_source_refresh_bindings(
    managed: &[Resource],
    compositions: &[carina_core::resource::Composition],
    data_sources: &[DataSource],
    managed_current: &HashMap<ResourceId, Current>,
    data_source_states: &HashMap<ResourceId, State>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    schemas: &carina_core::schema::SchemaRegistry,
    wait_aliases: &[WaitAliasSpec],
) -> ResolvedBindings {
    ResolvedBindings::pre_apply(PreApplyInputs {
        managed,
        compositions,
        data_sources,
        managed_current: &carina_core::resource::into_plan_input_current_map(
            managed_current.clone(),
            schemas,
            managed,
        ),
        data_source_states,
        remote_bindings,
        wait_aliases,
    })
}

/// Classify data-source reads that must run after apply-time publication.
///
/// This is shared with saved-plan apply so both live and persisted plans use
/// the expanded data-source set plus its preserved dependency metadata. The
/// returned map is keyed by data-source id; an empty input-details vector is
/// valid when parsing folded the value but retained `dependency_bindings`.
pub(crate) fn classify_apply_time_data_source_read_inputs(
    data_sources: &[DataSource],
    managed: &[Resource],
    managed_current: &HashMap<ResourceId, Current>,
) -> HashMap<ResourceId, Vec<UnresolvedDataSourceInput>> {
    let mut deferred =
        classify_apply_time_data_source_reads(data_sources, managed, managed_current);
    propagate_deferred_data_source_chains(data_sources, &mut deferred);
    deferred
}

fn classify_apply_time_data_source_reads(
    data_sources: &[DataSource],
    managed: &[Resource],
    managed_current: &HashMap<ResourceId, Current>,
) -> HashMap<ResourceId, Vec<UnresolvedDataSourceInput>> {
    let managed_by_binding: HashMap<&str, &Resource> = managed
        .iter()
        .filter_map(|resource| {
            resource
                .binding
                .as_deref()
                .map(|binding| (binding, resource))
        })
        .collect();

    data_sources
        .iter()
        .filter_map(|resource| {
            let input_refs = unresolved_data_source_inputs(resource);
            let path_requires_defer =
                input_refs
                    .iter()
                    .flat_map(|input| &input.paths)
                    .any(|path| {
                        managed_by_binding
                            .get(path.binding())
                            .is_some_and(|target| {
                                managed_resource_is_missing_from_state(target, managed_current)
                            })
                    });
            let binding_requires_defer =
                input_refs
                    .iter()
                    .flat_map(|input| &input.bindings)
                    .any(|binding| {
                        managed_by_binding
                            .get(binding.as_str())
                            .is_some_and(|target| {
                                managed_resource_is_missing_from_state(target, managed_current)
                            })
                    });
            // Directory reference resolution can fold an input such as
            // `"^${target.name}$"` to a concrete string. The structural
            // dependency is deliberately retained in `dependency_bindings`;
            // consult it so using the expanded parse does not turn an
            // apply-time read into a premature refresh-time read.
            let recorded_dependency_requires_defer =
                resource.dependency_bindings.iter().any(|binding| {
                    managed_by_binding
                        .get(binding.as_str())
                        .is_some_and(|target| {
                            managed_resource_is_missing_from_state(target, managed_current)
                        })
                });
            let has_unknown = input_refs.iter().any(|input| !input.unknowns.is_empty());
            let must_defer = path_requires_defer
                || binding_requires_defer
                || recorded_dependency_requires_defer
                || has_unknown;
            must_defer.then(|| (resource.id.clone(), input_refs))
        })
        .collect()
}

/// Data-source refresh classification is intentionally based on existence,
/// not predicted plan mutations. If a referenced managed binding already has
/// an existing state row, refresh may read it even when the current plan later
/// replaces that resource; that pre-replace staleness is normal plan-time
/// staleness and apply-time dependency execution can re-read when a read was
/// deferred for a genuinely missing upstream. Re-deriving replacement decisions
/// here duplicates differ semantics such as provider defaults and removable
/// attributes, and has caused false deferrals after successful applies.
fn managed_resource_is_missing_from_state(
    resource: &Resource,
    managed_current: &HashMap<ResourceId, Current>,
) -> bool {
    let Some(current) = managed_current.get(&resource.id) else {
        return true;
    };
    match current {
        Current::Absent(_state) => true,
        Current::Present(_present) => false,
        Current::CreateInFlight(_pending) => false,
        Current::MissingIdentifier(_state) => false,
    }
}

fn propagate_deferred_data_source_chains(
    data_sources: &[DataSource],
    deferred: &mut HashMap<ResourceId, Vec<UnresolvedDataSourceInput>>,
) {
    let data_source_id_by_binding: HashMap<&str, &ResourceId> = data_sources
        .iter()
        .filter_map(|resource| {
            resource
                .binding
                .as_deref()
                .map(|binding| (binding, &resource.id))
        })
        .collect();
    let refs_by_id: HashMap<ResourceId, Vec<UnresolvedDataSourceInput>> = data_sources
        .iter()
        .map(|resource| (resource.id.clone(), unresolved_data_source_inputs(resource)))
        .collect();

    loop {
        let mut changed = false;
        for resource in data_sources {
            if deferred.contains_key(&resource.id) {
                continue;
            }
            let refs = refs_by_id.get(&resource.id).cloned().unwrap_or_default();
            let path_depends_on_deferred_read =
                refs.iter().flat_map(|input| &input.paths).any(|path| {
                    data_source_id_by_binding
                        .get(path.binding())
                        .is_some_and(|id| deferred.contains_key(*id))
                });
            let binding_depends_on_deferred_read = refs
                .iter()
                .flat_map(|input| &input.bindings)
                .any(|binding| {
                    data_source_id_by_binding
                        .get(binding.as_str())
                        .is_some_and(|id| deferred.contains_key(*id))
                });
            let recorded_dependency_depends_on_deferred_read =
                resource.dependency_bindings.iter().any(|binding| {
                    data_source_id_by_binding
                        .get(binding.as_str())
                        .is_some_and(|id| deferred.contains_key(*id))
                });
            if path_depends_on_deferred_read
                || binding_depends_on_deferred_read
                || recorded_dependency_depends_on_deferred_read
            {
                deferred.insert(resource.id.clone(), refs);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

pub(crate) fn prepare_data_sources_for_plan(
    data_sources: &[DataSource],
    deferred_data_source_ids: &HashSet<ResourceId>,
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: Option<&std::collections::HashSet<&str>>,
) -> Result<Vec<DataSource>, AppError> {
    let mut prepared = Vec::with_capacity(data_sources.len());
    for resource in data_sources {
        let mut resource = resource.clone();
        if !deferred_data_source_ids.contains(&resource.id) {
            if let Some(upstream_bindings) = unresolved_upstream_bindings {
                carina_core::resolver::resolve_data_source_refs_for_plan(
                    std::slice::from_mut(&mut resource),
                    bindings,
                    upstream_bindings,
                )
                .map_err(AppError::Validation)?;
            } else {
                carina_core::resolver::resolve_data_source_refs(
                    std::slice::from_mut(&mut resource),
                    bindings,
                )
                .map_err(AppError::Validation)?;
            }
        }
        prepared.push(resource);
    }

    Ok(prepared)
}

/// Convenience wrappers for tests. Each creates a fresh `WiringContext` internally,
/// which is acceptable in test code where the overhead is negligible.
#[cfg(test)]
pub fn validate_resources(resources: &[Resource]) -> Result<(), AppError> {
    use carina_core::parser::ParsedFile;
    let ctx = WiringContext::new(vec![], &[]);
    let parsed = ParsedFile {
        resources: resources.to_vec(),
        ..ParsedFile::default()
    };
    errors_to_legacy_result(validate_resources_with_ctx(
        &ctx,
        &parsed,
        &carina_core::parser::ProviderContext::default(),
    ))
}

#[cfg(test)]
pub fn resolve_names(resources: &mut [Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new(vec![], &[]);
    errors_to_legacy_result(resolve_names_with_ctx(&ctx, resources))
}

#[cfg(test)]
pub fn resolve_attr_prefixes(resources: &mut [Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new(vec![], &[]);
    errors_to_legacy_result(resolve_attr_prefixes_with_ctx(&ctx, resources))
}

#[cfg(test)]
pub fn compute_anonymous_identifiers(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
) -> Result<(), AppError> {
    let ctx = WiringContext::new(vec![], providers);
    let canonical_resources =
        carina_core::value::canonicalize_resources_with_schemas(resources, ctx.schemas())?;
    let errors = compute_anonymous_identifiers_with_ctx(&ctx, canonical_resources, providers);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(crate::commands::collapse_errors(errors))
    }
}

/// Test-only adapter: collapse a `Vec<AppError>` back into the
/// `Result<(), AppError>` shape pre-#2105 callers expect. Always
/// joins as `AppError::Validation` — the three wrappers that use it
/// only return validation-kind errors.
#[cfg(test)]
fn errors_to_legacy_result(errors: Vec<AppError>) -> Result<(), AppError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Validation(
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

#[cfg(test)]
mod tests;
