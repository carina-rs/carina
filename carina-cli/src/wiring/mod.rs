//! Provider loading and command wiring.
//!
//! Raw loaders and [`WiringContext`] constructors are private to this module.
//! A [`ProviderLoad`] exposes neither its context nor its factories. Concrete
//! config validation or saved-plan artifact verification consumes that load
//! and mints a [`ValidatedConfig`], which owns both the validated resources and
//! their [`ValidatedWiring`]. Identity preparation, planning, execution, and
//! plan recording accept that owner rather than an independently selectable
//! context and resource value. Production code may inspect the owner's
//! resources, but cannot take ownership of, replace, or mutably borrow them;
//! concrete owner methods perform the permitted in-place reconciliation and
//! normalization steps, while consuming transitions carry the same wiring into
//! the next owner state. Validation therefore cannot load independently, and
//! every downstream use observes the exact context and resources that crossed
//! the same minting transition.
//!
//! Calling `ProviderLoad::load` twice remains possible. It cannot create the
//! former divergence: each result must itself be validated or artifact-verified
//! with the resources it owns before it can reach a downstream consumer.
//! Production command paths call the loader once; formatting calls its separate
//! loader once per provider-declaring directory and receives schemas rather
//! than a context. `carina_core` can still construct a raw canonical pair, but
//! no carina-cli planning, execution, or recording entry point accepts one,
//! and a raw pair cannot replace the resources inside an existing owner.
//!
//! ```compile_fail,E0603
//! let providers = Vec::new();
//! let _ = carina_cli::wiring::build_factories_from_providers(
//!     &providers,
//!     std::path::Path::new("."),
//! );
//! ```
//!
//! ```compile_fail,E0624
//! let providers = Vec::new();
//! let _ = carina_cli::wiring::WiringContext::new(Vec::new(), &providers);
//! ```
//!
//! A raw provider load is not accepted where recorded plan evidence requires
//! the owner returned by planning.
//!
//! ```compile_fail,E0308
//! use carina_cli::commands::plan::PlanFileContents;
//! use carina_cli::wiring::ProviderLoad;
//!
//! fn record_raw_load(contents: PlanFileContents, providers: ProviderLoad) {
//!     let _ = contents.record(&providers);
//! }
//! ```
//!
//! A raw wiring context is not accepted either.
//!
//! ```compile_fail,E0308
//! use carina_cli::commands::plan::PlanFileContents;
//! use carina_cli::wiring::WiringContext;
//!
//! fn record_raw_context(contents: PlanFileContents, context: WiringContext) {
//!     let _ = contents.record(&context);
//! }
//! ```
//!
//! Raw loads do not expose their wiring context.
//!
//! ```compile_fail,E0599
//! use carina_cli::wiring::ProviderLoad;
//!
//! fn borrow_raw_context(providers: &ProviderLoad) {
//!     let _ = providers.context();
//! }
//! ```

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::Path;

#[cfg(test)]
use std::cell::Cell;

use indexmap::IndexMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use colored::Colorize;
use futures::stream::{self, StreamExt};

use carina_core::binding_index::{PreApplyInputs, ResolvedBindings, WaitAliasSpec};
use carina_core::data_source_value::{DataSourceValue, DataSourceValues};
use carina_core::deps::{
    get_composition_dependencies, get_data_source_dependencies, get_resource_dependencies,
    sort_resources_by_dependencies,
};
use carina_core::differ::{
    Orphan, Orphans, PlanPriorStates, binding_matches_deferred_template, into_all_prior_states,
    lift_prior_state_enum_leaves, provider_states_for, rekey_current_for_move,
    transform_prior_managed_provider_states, transform_prior_provider_states,
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
    ProviderReadyConfig, UnresolvedDataSourceInput, UnresolvedResource,
    prepare_provider_ready_config, unresolved_data_source_inputs,
};
use carina_core::identifier::{
    self, AnonymousIdBindingStateInfo, AnonymousIdStateInfo, PrefixStateInfo, StateBlockClaims,
};
use carina_core::module_resolver;
use carina_core::override_aware::{
    CanonicalResourcePair, DeferredForExpansionChoice, OverrideAwareBuildMode,
    OverrideAwareResources, RebuildBindingsInputs,
};
#[cfg(test)]
use carina_core::parser::{MovedAddresses, ParseWarning};
use carina_core::parser::{
    ProviderConfig, ResolvedProviderConfig, ResolvedProviderConfigs, StateBlock, StateBlockAddress,
    WarningKind,
};
use carina_core::plan::{Plan, PlanError, PlanErrorKind};
use carina_core::provider::{
    self as provider_mod, AppliedIdentifier, LiftedSavedAttrs, Provider, ProviderError,
    ProviderFactory, ProviderNormalizer, ProviderRouter,
};
use carina_core::provider_source::ProviderSource;
use carina_core::resource::{
    Composition, ConcreteValue, Current, DataSource, DeferredValue, ModuleSource, ResolvedResource,
    ResolvedResourceId, Resource, ResourceId, ResourceIdentity, ResourceIdentityError, State,
    Value,
};
#[cfg(test)]
use carina_core::schema::{AttributeSchema, AttributeType};
use carina_core::schema::{
    OwnerBoundResourceSchema, ResourceSchema, SchemaKind, SchemaRegistry, TypeError,
    ValidatedResourceSchema, resolve_file_block_names_with_schemas,
};
use carina_core::shutdown::{ShutdownPhase, ShutdownToken};
use carina_core::validation;
use carina_provider_mock::{BUILTIN_PROVIDER_NAME, MockProvider};
use carina_state::{BackendError, LoadedOrphans, StateFile};

#[cfg(test)]
use crate::commands::shared::progress::RecordedProgressLines;
use crate::commands::shared::progress::{ProgressBackend, ProgressSink, RefreshPhase};
use crate::commands::split_validation_message;
use crate::error::AppError;
use crate::module_walk::ModuleWalk;

static TRUSTED_LOCAL_GRANT_INPUTS: OnceLock<TrustedLocalGrantInputs> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TRACKED_PROVIDER_LOAD_COUNT: Cell<Option<usize>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct ProviderLoadCountGuard;

#[cfg(test)]
impl ProviderLoadCountGuard {
    pub(crate) fn count(&self) -> usize {
        TRACKED_PROVIDER_LOAD_COUNT.with(|count| {
            count
                .get()
                .expect("provider-load tracking must remain active")
        })
    }
}

#[cfg(test)]
impl Drop for ProviderLoadCountGuard {
    fn drop(&mut self) {
        TRACKED_PROVIDER_LOAD_COUNT.with(|count| count.set(None));
    }
}

#[cfg(test)]
pub(crate) fn track_provider_loads_for_test() -> ProviderLoadCountGuard {
    TRACKED_PROVIDER_LOAD_COUNT.with(|count| {
        assert!(
            count.replace(Some(0)).is_none(),
            "provider-load tracking is already active on this thread"
        );
    });
    ProviderLoadCountGuard
}

#[cfg(test)]
fn record_tracked_provider_load() {
    TRACKED_PROVIDER_LOAD_COUNT.with(|count| {
        if let Some(current) = count.get() {
            count.set(Some(current + 1));
        }
    });
}

#[derive(Debug)]
struct TrustedLocalGrantInputs {
    cli_values: Vec<String>,
    env_value: Option<std::ffi::OsString>,
    parsed: OnceLock<carina_provider_resolver::TrustedLocalGrants>,
}

impl TrustedLocalGrantInputs {
    fn new(cli_values: Vec<String>, env_value: Option<std::ffi::OsString>) -> Self {
        Self {
            cli_values,
            env_value,
            parsed: OnceLock::new(),
        }
    }

    fn get(
        &self,
    ) -> Result<
        &carina_provider_resolver::TrustedLocalGrants,
        carina_provider_resolver::GrantParseError,
    > {
        if let Some(grants) = self.parsed.get() {
            return Ok(grants);
        }
        let grants = carina_provider_resolver::TrustedLocalGrants::parse_os(
            &self.cli_values,
            self.env_value.as_deref(),
        )?;
        Ok(self.parsed.get_or_init(|| grants))
    }
}

/// Error returned when process-local grant inputs were already installed.
#[derive(Debug, thiserror::Error)]
#[error("trusted local provider grant inputs were already installed")]
pub struct TrustedLocalGrantInputsAlreadySetError;

/// Install the raw process-local grant inputs captured by the CLI entry point.
///
/// The grants live only for this process and are never written to project
/// locks, provider caches, saved plans, or state.
pub fn set_trusted_local_grant_inputs(
    cli_values: Vec<String>,
    env_value: Option<std::ffi::OsString>,
) -> Result<(), TrustedLocalGrantInputsAlreadySetError> {
    TRUSTED_LOCAL_GRANT_INPUTS
        .set(TrustedLocalGrantInputs::new(cli_values, env_value))
        .map_err(|_| TrustedLocalGrantInputsAlreadySetError)
}

/// Result of creating a plan, with context needed for saving
pub struct PlanContext {
    pub plan: Plan,
    pub provider: ProviderRouter,
    /// Evaluated desired resources after the identity-resolution boundary.
    pub sorted_resources: Vec<ResolvedResource>,
    /// Pre-value-resolution snapshots with the same resolved identities.
    pub unresolved_resources: Vec<UnresolvedResource>,
    /// Module compositions after plan-time constraint evaluation. Pending
    /// constraints must survive in saved plans for apply-time re-evaluation.
    pub compositions: Vec<Composition>,
    /// Data sources exactly as supplied to the differ. Refresh-time reads
    /// have concrete inputs; apply-time deferred reads keep structural refs so
    /// saved-plan apply can reconstruct the same deferred-read set.
    pub data_sources: Vec<DataSource>,
    pub managed_current: HashMap<ResourceId, Current>,
    pub data_source_values: DataSourceValues,
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

/// A completed plan together with the exact canonical resources consumed by
/// its deferred-`for` expansion. Field access dereferences to [`PlanContext`]
/// so existing plan consumers do not need a second, untyped resource channel.
///
/// The type is public so [`PlanFileContents::record`](crate::commands::plan::PlanFileContents::record)
/// can name the required owner state. Its fields and constructors remain
/// private; production values are returned only by the planning transition.
pub struct ExpandedPlanContext<E> {
    context: PlanContext,
    canonical_resources: ExpandedCanonicalResources<E>,
}

impl<E> ExpandedPlanContext<E> {
    fn new(context: PlanContext, canonical_resources: ExpandedCanonicalResources<E>) -> Self {
        Self {
            context,
            canonical_resources,
        }
    }

    pub(crate) fn parsed(&self) -> &carina_core::parser::File<E> {
        self.canonical_resources.parsed()
    }

    #[cfg(test)]
    pub(crate) fn into_context(self) -> PlanContext {
        self.context
    }
}

impl<E> std::ops::Deref for ExpandedPlanContext<E> {
    type Target = PlanContext;

    fn deref(&self) -> &Self::Target {
        &self.context
    }
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

/// Cached provider factories and schemas owned by provider-load typestate.
///
/// Production commands can borrow this only through a [`ValidatedConfig`]
/// minted by config validation or saved-plan artifact verification. Formatting
/// extracts schemas from its private context and never exposes the context
/// itself.
pub struct WiringContext {
    factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    custom_types: carina_core::parser::ProviderCustomTypes,
    schemas: SchemaRegistry,
    known_providers: HashSet<String>,
    uses_builtin_mock: bool,
    provider_artifacts: BTreeMap<String, LoadedProviderArtifact>,
}

/// A WASM provider factory paired with the resolver-minted identity of the
/// exact artifact bytes that produced it.
pub struct LoadedProviderFactory {
    factory: Box<dyn ProviderFactory>,
    artifact: carina_provider_resolver::LoadedArtifactIdentity,
}

impl LoadedProviderFactory {
    /// Attach resolver-minted test identities to in-process factories.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(factories: Vec<Box<dyn ProviderFactory>>) -> Vec<Self> {
        factories
            .into_iter()
            .map(|factory| {
                let bytes = format!("in-process test factory {}", factory.name());
                Self::for_test_with_bytes(factory, bytes)
            })
            .collect()
    }

    /// Attach a resolver-minted test identity for explicit artifact bytes.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test_with_bytes(factory: Box<dyn ProviderFactory>, bytes: impl AsRef<[u8]>) -> Self {
        let identity = carina_provider_resolver::ProviderLoadIdentity::for_test(
            factory.name(),
            carina_provider_resolver::ProviderTrustClass::Untrusted,
        );
        let verified = carina_provider_resolver::VerifiedLoad::for_test(bytes, identity);
        Self::new(factory, verified.artifact().clone())
    }

    fn new(
        factory: Box<dyn ProviderFactory>,
        artifact: carina_provider_resolver::LoadedArtifactIdentity,
    ) -> Self {
        Self { factory, artifact }
    }

    fn into_factory(self) -> Box<dyn ProviderFactory> {
        self.factory
    }
}

/// Provider-kind artifacts that participate in one CLI wiring context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadedProviderArtifact {
    BuiltIn,
    Wasm(carina_provider_resolver::LoadedArtifactIdentity),
}

/// The opaque context and diagnostics produced by one provider load operation.
///
/// This raw state cannot expose its context. A concrete validation or
/// saved-plan verification transition must consume it before planners and
/// executors can obtain a [`ValidatedConfig`]. The type does not prevent a
/// caller from loading twice; it prevents either result from reaching those
/// consumers with resources validated by the other result.
pub struct ProviderLoad {
    context: WiringContext,
    load_errors: HashMap<String, String>,
}

/// Provider wiring whose loader diagnostics have been checked exactly once.
///
/// This type still exposes no context or factories. Concrete config validation
/// or saved-plan artifact verification must consume it before execution can
/// obtain a [`ValidatedConfig`].
pub(crate) struct CompleteProviderLoad {
    context: WiringContext,
}

/// A complete load bound to the canonical current configuration checked for
/// saved-plan secret verification.
///
/// Both fields are private so the pair cannot be detached from its provider
/// load before saved-plan artifact verification mints the executable owner.
pub(crate) struct CheckedSavedPlanInputs {
    load: CompleteProviderLoad,
    canonical_resources: Box<CanonicalResourcePair<carina_core::parser::InferredExportParam>>,
}

/// Schema-only provider state used by collect-all validation and lint.
///
/// A hard loader failure may produce this value with an empty context. It has
/// no transition into [`CompleteProviderLoad`] or [`ValidatedConfig`].
pub(crate) struct ReportingProviderLoad {
    context: WiringContext,
    load_errors: HashMap<String, String>,
}

/// Capability for entering the raw validation pipeline.
///
/// The type is nameable by `commands`, but its private field means only this
/// module can construct it. That keeps the pipeline's raw canonical-pair
/// result behind the owner-minting transitions.
pub(crate) struct ValidationPipelineAccess {
    _private: (),
}

impl ValidationPipelineAccess {
    fn new() -> Self {
        Self { _private: () }
    }

    /// Resolve both configuration snapshots inside the sealed validation
    /// pipeline. The capability cannot be constructed outside this module, so
    /// no other crate path can obtain an unowned canonical pair from this API.
    pub(crate) fn resolve_config_names<E>(
        &self,
        ctx: &WiringContext,
        parsed: carina_core::parser::File<E>,
        unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
    ) -> Result<CanonicalResourcePair<E>, Vec<AppError>> {
        resolve_config_names_with_ctx(ctx, parsed, unresolved)
    }
}

/// Provider wiring retained inside [`ValidatedConfig`] after a concrete
/// validation or saved-plan artifact-verification transition.
///
/// Its context accessor is crate-private. Production downstream entry points
/// receive the enclosing owner, so wiring cannot be detached from the exact
/// resources checked with it.
pub struct ValidatedWiring {
    load: CompleteProviderLoad,
}

/// Provider wiring and the exact configuration state validated against it.
///
/// Callers outside validation cannot construct this owner by pairing arbitrary
/// resources with unrelated wiring:
///
/// ```compile_fail,E0451
/// use carina_cli::wiring::{ValidatedConfig, ValidatedWiring};
/// use carina_core::override_aware::CanonicalResourcePair;
/// use carina_core::parser::InferredExportParam;
///
/// fn rebind(
///     wiring: ValidatedWiring,
///     resources: CanonicalResourcePair<InferredExportParam>,
/// ) {
///     let _ = ValidatedConfig { wiring, resources };
/// }
/// ```
///
/// A raw canonical pair is not accepted by saved-plan recording, which takes
/// the completed-plan owner that retains its validated wiring:
///
/// ```compile_fail,E0308
/// use carina_cli::commands::plan::PlanFileContents;
/// use carina_core::override_aware::CanonicalResourcePair;
/// use carina_core::parser::InferredExportParam;
///
/// fn record_unbound(
///     contents: PlanFileContents,
///     resources: CanonicalResourcePair<InferredExportParam>,
/// ) {
///     let _ = contents.record(&resources);
/// }
/// ```
///
/// Validated resources cannot be replaced while retaining their original
/// wiring owner:
///
/// ```compile_fail,E0614
/// use carina_cli::wiring::ValidatedConfig;
/// use carina_core::parser::InferredFile;
///
/// fn replace_resources(mut config: ValidatedConfig<InferredFile>, other: InferredFile) {
///     *config = other;
/// }
/// ```
///
/// Two owners cannot swap their validated resources independently of their
/// wiring:
///
/// ```compile_fail,E0614
/// use carina_cli::wiring::ValidatedConfig;
/// use carina_core::parser::InferredFile;
///
/// fn swap_resources(
///     mut first: ValidatedConfig<InferredFile>,
///     mut second: ValidatedConfig<InferredFile>,
/// ) {
///     std::mem::swap(&mut *first, &mut *second);
/// }
/// ```
pub struct ValidatedConfig<R = CanonicalResourcePair<carina_core::parser::InferredExportParam>> {
    wiring: ValidatedWiring,
    resources: R,
}

impl<R: fmt::Debug> fmt::Debug for ValidatedConfig<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedConfig")
            .field("resources", &self.resources)
            .finish()
    }
}

/// Collected validation diagnostics plus the read-only schemas used by
/// warning-only validation checks.
pub(crate) struct ValidationReport<'a> {
    errors: Vec<AppError>,
    schemas: &'a SchemaRegistry,
}

impl ValidationReport<'_> {
    pub(crate) fn errors(&self) -> &[AppError] {
        &self.errors
    }

    pub(crate) fn into_errors(self) -> Vec<AppError> {
        self.errors
    }

    pub(crate) fn schemas(&self) -> &SchemaRegistry {
        self.schemas
    }
}

#[allow(clippy::too_many_arguments)]
fn validation_report_from<'a>(
    context: &'a WiringContext,
    load_errors: &HashMap<String, String>,
    parsed: &mut carina_core::parser::InferredFile,
    base_dir: &Path,
    skip_resource_validation: bool,
    prior_inference_errors: &[carina_core::validation::inference::ExportInferenceError],
    duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
) -> ValidationReport<'a> {
    let errors = match crate::commands::validate_and_resolve_errors_inner(
        &ValidationPipelineAccess::new(),
        parsed,
        base_dir,
        context,
        load_errors,
        skip_resource_validation,
        prior_inference_errors,
        duplicate_declarations,
        crate::commands::ConfigNameResolution::EvaluatedOnly,
    ) {
        Ok(crate::commands::ResolvedConfig::Evaluated(_)) => Vec::new(),
        Ok(crate::commands::ResolvedConfig::Paired(_)) => {
            unreachable!("evaluated-only validation returned pair")
        }
        Err(errors) => errors,
    };
    ValidationReport {
        errors,
        schemas: context.schemas(),
    }
}

fn sorted_provider_load_errors(load_errors: &HashMap<String, String>) -> Vec<AppError> {
    let mut load_errors = load_errors.iter().collect::<Vec<_>>();
    load_errors.sort_unstable_by_key(|(provider, _)| *provider);
    load_errors
        .into_iter()
        .map(|(_provider, reason)| AppError::Validation(reason.clone()))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn resolve_config_pair_errors_with_context(
    context: &WiringContext,
    load_errors: &HashMap<String, String>,
    mut parsed: carina_core::parser::InferredFile,
    unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
    base_dir: &Path,
    skip_resource_validation: bool,
    prior_inference_errors: &[carina_core::validation::inference::ExportInferenceError],
    duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
) -> Result<CanonicalResourcePair<carina_core::parser::InferredExportParam>, Vec<AppError>> {
    match crate::commands::validate_and_resolve_errors_inner(
        &ValidationPipelineAccess::new(),
        &mut parsed,
        base_dir,
        context,
        load_errors,
        skip_resource_validation,
        prior_inference_errors,
        duplicate_declarations,
        crate::commands::ConfigNameResolution::Paired(Box::new(unresolved)),
    )? {
        crate::commands::ResolvedConfig::Paired(pair) => Ok(*pair),
        crate::commands::ResolvedConfig::Evaluated(_) => {
            unreachable!("paired validation returned evaluated-only")
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_config_pair_with_context(
    context: &WiringContext,
    load_errors: &HashMap<String, String>,
    parsed: carina_core::parser::InferredFile,
    unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
    base_dir: &Path,
    skip_resource_validation: bool,
    prior_inference_errors: &[carina_core::validation::inference::ExportInferenceError],
    duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
) -> Result<CanonicalResourcePair<carina_core::parser::InferredExportParam>, AppError> {
    resolve_config_pair_errors_with_context(
        context,
        load_errors,
        parsed,
        unresolved,
        base_dir,
        skip_resource_validation,
        prior_inference_errors,
        duplicate_declarations,
    )
    .map_err(crate::commands::collapse_errors)
}

impl ProviderLoad {
    /// Load the providers declared by one command configuration.
    pub(crate) fn load(
        user_dirs: &carina_core::UserDirs,
        providers: &[ProviderConfig],
        base_dir: &Path,
    ) -> Result<Self, AppError> {
        let (factories, load_errors) =
            build_factories_from_providers(user_dirs, providers, base_dir)?;
        Ok(Self {
            context: WiringContext::new(factories, providers),
            load_errors,
        })
    }

    /// Attempt one provider load while retaining a context for collect-all diagnostics.
    ///
    /// A hard loader error prevents factories from being trusted, so validation and
    /// lint continue with an empty context and report the loader error alongside
    /// schema-independent findings.
    pub(crate) fn load_reporting(
        user_dirs: &carina_core::UserDirs,
        providers: &[ProviderConfig],
        base_dir: &Path,
    ) -> (ReportingProviderLoad, Option<AppError>) {
        match Self::load(user_dirs, providers, base_dir) {
            Ok(Self {
                context,
                load_errors,
            }) => (
                ReportingProviderLoad {
                    context,
                    load_errors,
                },
                None,
            ),
            Err(error) => (
                ReportingProviderLoad {
                    context: WiringContext::new(Vec::new(), providers),
                    load_errors: HashMap::new(),
                },
                Some(error),
            ),
        }
    }

    /// Consume the loader result after proving that every declared provider
    /// loaded successfully.
    pub(crate) fn into_complete(self) -> Result<CompleteProviderLoad, AppError> {
        let Self {
            context,
            load_errors,
        } = self;
        if load_errors.is_empty() {
            return Ok(CompleteProviderLoad { context });
        }

        Err(crate::commands::collapse_errors(
            sorted_provider_load_errors(&load_errors),
        ))
    }

    /// Test-support report that validates without minting executable wiring.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn validation_report<'a>(
        &'a self,
        parsed: &mut carina_core::parser::InferredFile,
        base_dir: &Path,
        skip_resource_validation: bool,
        prior_inference_errors: &[carina_core::validation::inference::ExportInferenceError],
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> ValidationReport<'a> {
        validation_report_from(
            &self.context,
            &self.load_errors,
            parsed,
            base_dir,
            skip_resource_validation,
            prior_inference_errors,
            duplicate_declarations,
        )
    }

    /// Validate an evaluated config and consume this exact load and file into
    /// one executable owner.
    pub fn validate_evaluated_config(
        self,
        mut parsed: carina_core::parser::InferredFile,
        base_dir: &Path,
        skip_resource_validation: bool,
        prior_inference_errors: &[carina_core::validation::inference::ExportInferenceError],
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> Result<ValidatedConfig<carina_core::parser::InferredFile>, AppError> {
        let mut errors = validation_report_from(
            &self.context,
            &self.load_errors,
            &mut parsed,
            base_dir,
            skip_resource_validation,
            prior_inference_errors,
            duplicate_declarations,
        )
        .into_errors();
        // The resource-skipping pipeline omits provider-load diagnostics.
        // Preserve its findings first, then append the load causes by name.
        if skip_resource_validation {
            errors.extend(sorted_provider_load_errors(&self.load_errors));
        }
        if !errors.is_empty() {
            return Err(crate::commands::collapse_errors(errors));
        }

        Ok(ValidatedConfig {
            wiring: ValidatedWiring {
                load: self.into_complete()?,
            },
            resources: parsed,
        })
    }

    /// Validate and canonicalize both snapshots, then bind the pair to this
    /// exact provider load in one executable owner.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn validate_config_pair(
        self,
        parsed: carina_core::parser::InferredFile,
        unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
        base_dir: &Path,
        skip_resource_validation: bool,
        prior_inference_errors: &[carina_core::validation::inference::ExportInferenceError],
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> Result<ValidatedConfig, AppError> {
        let canonical_resources = match resolve_config_pair_errors_with_context(
            &self.context,
            &self.load_errors,
            parsed,
            unresolved,
            base_dir,
            skip_resource_validation,
            prior_inference_errors,
            duplicate_declarations,
        ) {
            Ok(canonical_resources) => {
                let load_errors = if skip_resource_validation {
                    sorted_provider_load_errors(&self.load_errors)
                } else {
                    Vec::new()
                };
                if !load_errors.is_empty() {
                    return Err(crate::commands::collapse_errors(load_errors));
                }
                canonical_resources
            }
            Err(mut errors) => {
                // Keep validation findings ahead of the provider-load causes.
                if skip_resource_validation {
                    errors.extend(sorted_provider_load_errors(&self.load_errors));
                }
                return Err(crate::commands::collapse_errors(errors));
            }
        };
        Ok(ValidatedConfig {
            wiring: ValidatedWiring {
                load: self.into_complete()?,
            },
            resources: canonical_resources,
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(
        factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
    ) -> Self {
        Self {
            context: WiringContext::new(factories, provider_configs),
            load_errors: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_load_errors(
        factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
        load_errors: HashMap<String, String>,
    ) -> Self {
        Self {
            context: WiringContext::new(factories, provider_configs),
            load_errors,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn for_fixture_with_additional_schemas(
        factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
        additional_schemas: impl IntoIterator<Item = (String, ValidatedResourceSchema)>,
    ) -> Self {
        Self {
            context: WiringContext::new_with_additional_schemas(
                factories,
                provider_configs,
                additional_schemas,
            ),
            load_errors: HashMap::new(),
        }
    }
}

impl CompleteProviderLoad {
    /// Validate the current config once and retain its canonical pair with the
    /// exact complete load that will verify and execute a saved plan.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn check_saved_plan_config_pair(
        self,
        parsed: carina_core::parser::InferredFile,
        unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
        base_dir: &Path,
        skip_resource_validation: bool,
        prior_inference_errors: &[carina_core::validation::inference::ExportInferenceError],
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> Result<CheckedSavedPlanInputs, AppError> {
        let canonical_resources = resolve_config_pair_with_context(
            &self.context,
            &HashMap::new(),
            parsed,
            unresolved,
            base_dir,
            skip_resource_validation,
            prior_inference_errors,
            duplicate_declarations,
        )?;
        Ok(CheckedSavedPlanInputs {
            load: self,
            canonical_resources: Box::new(canonical_resources),
        })
    }

    /// Bind a saved-plan artifact proof to this exact complete provider load.
    pub(crate) fn verify_saved_plan_artifacts<T>(
        self,
        check: impl for<'context> FnOnce(
            &'context WiringContext,
        ) -> Result<
            crate::commands::apply::saved_plan::SavedPlanArtifactsChecked<'context, T>,
            AppError,
        >,
    ) -> Result<(T, ValidatedConfig<()>), AppError> {
        let checked = check(&self.context)?;
        let checked_plan = checked.into_plan();
        Ok((
            checked_plan,
            ValidatedConfig {
                wiring: ValidatedWiring { load: self },
                resources: (),
            },
        ))
    }
}

impl CheckedSavedPlanInputs {
    /// Bind saved-plan artifact proof to the load that already checked the
    /// retained current-config pair, returning the checked plan and bound
    /// configuration owner together.
    pub(crate) fn verify_saved_plan_artifacts<T>(
        self,
        check: impl for<'context> FnOnce(
            &'context WiringContext,
        ) -> Result<
            crate::commands::apply::saved_plan::SavedPlanArtifactsChecked<'context, T>,
            AppError,
        >,
    ) -> Result<(T, ValidatedConfig), AppError> {
        let Self {
            load,
            canonical_resources,
        } = self;
        let (plan, validated) = load.verify_saved_plan_artifacts(check)?;
        Ok((
            plan,
            ValidatedConfig {
                wiring: validated.wiring,
                resources: *canonical_resources,
            },
        ))
    }
}

impl ReportingProviderLoad {
    pub(crate) fn validation_report<'a>(
        &'a self,
        parsed: &mut carina_core::parser::InferredFile,
        base_dir: &Path,
        skip_resource_validation: bool,
        prior_inference_errors: &[carina_core::validation::inference::ExportInferenceError],
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> ValidationReport<'a> {
        validation_report_from(
            &self.context,
            &self.load_errors,
            parsed,
            base_dir,
            skip_resource_validation,
            prior_inference_errors,
            duplicate_declarations,
        )
    }

    pub(crate) fn schemas(&self) -> &SchemaRegistry {
        self.context.schemas()
    }
}

impl ValidatedWiring {
    /// Borrow the exact context that passed the minting transition.
    pub(crate) fn context(&self) -> &WiringContext {
        &self.load.context
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(
        factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
    ) -> Self {
        Self {
            load: ProviderLoad::for_test(factories, provider_configs)
                .into_complete()
                .expect("test provider load must be complete"),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_factories_for_test(
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_configs: &[ProviderConfig],
    ) -> Self {
        Self::for_test(LoadedProviderFactory::for_test(factories), provider_configs)
    }

    /// Test-only witness for fixtures that separately exercise the real
    /// loader and the bootstrap revalidation transition.
    #[cfg(test)]
    pub(crate) fn from_load_for_test(load: ProviderLoad) -> Self {
        Self {
            load: load
                .into_complete()
                .expect("test provider load must be complete"),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn for_fixture_with_additional_schemas(
        factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
        additional_schemas: impl IntoIterator<Item = (String, ValidatedResourceSchema)>,
    ) -> Self {
        Self {
            load: ProviderLoad::for_fixture_with_additional_schemas(
                factories,
                provider_configs,
                additional_schemas,
            )
            .into_complete()
            .expect("fixture provider load must be complete"),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_factories_with_additional_schemas_for_test(
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_configs: &[ProviderConfig],
        additional_schemas: impl IntoIterator<Item = (String, ValidatedResourceSchema)>,
    ) -> Self {
        Self::for_fixture_with_additional_schemas(
            LoadedProviderFactory::for_test(factories),
            provider_configs,
            additional_schemas,
        )
    }
}

impl<R> ValidatedConfig<R> {
    /// Borrow the exact provider context that validated this configuration.
    pub fn context(&self) -> &WiringContext {
        self.wiring.context()
    }

    /// Borrow the validated resources without allowing them to be detached or
    /// replaced independently of their wiring owner.
    pub(crate) fn resources(&self) -> &R {
        &self.resources
    }

    /// Borrow the context and its validated resources together.
    ///
    /// The two borrows originate from one owner, so callers cannot retain the
    /// resources while substituting a context from another provider load.
    pub(crate) fn context_and_resources(&self) -> (&WiringContext, &R) {
        (self.wiring.context(), &self.resources)
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(
        resources: R,
        factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
    ) -> Self {
        Self {
            wiring: ValidatedWiring::for_test(factories, provider_configs),
            resources,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_factories_for_test(
        resources: R,
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_configs: &[ProviderConfig],
    ) -> Self {
        Self::for_test(
            resources,
            LoadedProviderFactory::for_test(factories),
            provider_configs,
        )
    }

    #[cfg(test)]
    pub(crate) fn from_wiring_for_test(wiring: ValidatedWiring, resources: R) -> Self {
        Self { wiring, resources }
    }

    #[cfg(test)]
    pub(crate) fn from_wiring_ref_for_test(wiring: &ValidatedWiring, resources: R) -> Self {
        let factories = Arc::clone(&wiring.load.context.factories);
        let custom_types =
            carina_core::provider::provider_custom_types_from_factories(Arc::clone(&factories));
        Self {
            wiring: ValidatedWiring {
                load: CompleteProviderLoad {
                    context: WiringContext {
                        factories,
                        custom_types,
                        schemas: wiring.load.context.schemas.clone(),
                        known_providers: wiring.load.context.known_providers.clone(),
                        uses_builtin_mock: wiring.load.context.uses_builtin_mock,
                        provider_artifacts: wiring.load.context.provider_artifacts.clone(),
                    },
                },
            },
            resources,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_factories_with_additional_schemas_for_test(
        resources: R,
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_configs: &[ProviderConfig],
        additional_schemas: impl IntoIterator<Item = (String, ValidatedResourceSchema)>,
    ) -> Self {
        Self {
            wiring: ValidatedWiring::from_factories_with_additional_schemas_for_test(
                factories,
                provider_configs,
                additional_schemas,
            ),
            resources,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn for_fixture_with_additional_schemas(
        resources: R,
        factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
        additional_schemas: impl IntoIterator<Item = (String, ValidatedResourceSchema)>,
    ) -> Self {
        Self {
            wiring: ValidatedWiring::for_fixture_with_additional_schemas(
                factories,
                provider_configs,
                additional_schemas,
            ),
            resources,
        }
    }

    /// Fixture-only bridge for the intentionally separate validation and
    /// rendering contexts documented in `fixture_plan`.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn into_resources_for_fixture(self) -> R {
        self.resources
    }
}

impl ValidatedConfig<CanonicalResourcePair<carina_core::parser::InferredExportParam>> {
    /// Revalidate the post-bootstrap configuration against this same provider
    /// load, consuming the old canonical pair into its replacement owner.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn revalidate_config_pair(
        self,
        parsed: carina_core::parser::InferredFile,
        unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
        base_dir: &Path,
        skip_resource_validation: bool,
        prior_inference_errors: &[carina_core::validation::inference::ExportInferenceError],
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> Result<ValidatedConfig, AppError> {
        let Self {
            wiring,
            resources: _,
        } = self;
        let resources = resolve_config_pair_with_context(
            wiring.context(),
            &HashMap::new(),
            parsed,
            unresolved,
            base_dir,
            skip_resource_validation,
            prior_inference_errors,
            duplicate_declarations,
        )?;
        Ok(ValidatedConfig { wiring, resources })
    }
}

impl<E> ValidatedConfig<CanonicalResourcePair<E>> {
    /// Apply the state-aware identity reconciliation sequence to this owner's
    /// canonical pair without exposing a mutable pair to the caller.
    pub(crate) fn reconcile_identities_with_state(
        &mut self,
        state_file: &mut Option<StateFile>,
    ) -> Result<StateBlockResolution, AppError> {
        let Self { wiring, resources } = self;
        let ctx = wiring.context();
        reconcile_canonical_prefixed_names(resources, state_file);
        let parsed = resources.parsed();
        let state_blocks = resolve_state_blocks(
            &parsed.state_blocks,
            state_file,
            &parsed.resources,
            ctx.schemas(),
        );
        if let Some(state) = state_file.as_ref() {
            resources.reconcile_anonymous_module_instances(
                &|provider, resource_type| {
                    state
                        .resources_by_type(provider, resource_type)
                        .into_iter()
                        .map(|resource| resource.identity.to_string())
                        .collect()
                },
                &state_blocks.claims,
            );
        }
        if let Some(state) = state_file.as_mut() {
            reconcile_canonical_anonymous_identifiers_with_ctx(
                ctx,
                resources,
                state,
                &state_blocks.claims,
            )?;
        }
        Ok(state_blocks)
    }

    /// Fixture-only identity preparation that still mutates through the owner.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn compute_anonymous_identifiers_for_fixture(
        &mut self,
        providers: &[ProviderConfig],
    ) -> Result<Vec<AppError>, AppError> {
        let Self { wiring, resources } = self;
        compute_canonical_pair_anonymous_identifiers_with_ctx(
            wiring.context(),
            resources,
            providers,
        )
    }
}

impl ValidatedConfig<carina_core::parser::InferredFile> {
    /// Integration-test read view of the evaluated configuration. Mutation
    /// remains confined to the owner's concrete transition methods.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn evaluated_resources_for_test(&self) -> &carina_core::parser::InferredFile {
        &self.resources
    }

    /// Reconcile the evaluated destroy view against state without releasing a
    /// mutable file from the owner.
    pub(crate) fn reconcile_for_destroy(
        &mut self,
        state_file: &mut Option<StateFile>,
    ) -> Result<(), AppError> {
        self.reconcile_evaluated_identities(state_file, true)
    }

    /// Reconcile the evaluated state-refresh view against state without
    /// applying module-instance reconciliation, preserving its existing
    /// command semantics.
    pub(crate) fn reconcile_for_state_refresh(
        &mut self,
        state_file: &mut Option<StateFile>,
    ) -> Result<(), AppError> {
        self.reconcile_evaluated_identities(state_file, false)
    }

    fn reconcile_evaluated_identities(
        &mut self,
        state_file: &mut Option<StateFile>,
        reconcile_modules: bool,
    ) -> Result<(), AppError> {
        let Self { wiring, resources } = self;
        let ctx = wiring.context();
        reconcile_prefixed_names(&mut resources.resources, state_file);
        let state_block_claims = resolve_state_block_claims(
            &resources.state_blocks,
            state_file,
            &resources.resources,
            ctx.schemas(),
        );
        if reconcile_modules && let Some(state) = state_file.as_ref() {
            module_resolver::reconcile_anonymous_module_instances(
                &mut resources.resources,
                &mut resources.compositions,
                &|provider, resource_type| {
                    state
                        .resources_by_type(provider, resource_type)
                        .into_iter()
                        .map(|resource| resource.identity.to_string())
                        .collect()
                },
                &state_block_claims,
            );
        }
        if let Some(state) = state_file.as_mut() {
            reconcile_anonymous_identifiers_with_ctx(
                ctx,
                &mut resources.resources,
                &resources.data_sources,
                state,
                &state_block_claims,
            )?;
        }
        crate::commands::shared::state_writeback::apply_name_overrides(
            &mut resources.resources,
            state_file,
        );
        Ok(())
    }
}

impl<E: Clone> ValidatedConfig<carina_core::parser::File<E>> {
    /// Run the address-preserving state-refresh expansion through this owner,
    /// keeping the mutated file bound to its validating wiring.
    pub(crate) async fn expand_refresh_and_lift_states<P: Provider + ProviderNormalizer>(
        &mut self,
        inputs: ValidatedExpandRefreshAndLiftInputs<'_, '_, P>,
    ) -> Result<ExpandedRefreshState<()>, AppError> {
        let Self { wiring, resources } = self;
        let ValidatedExpandRefreshAndLiftInputs {
            provider,
            expansion,
            warning_policy,
            remote_bindings,
            state_file,
            saved_dep_bindings,
            refresh_phase,
        } = inputs;
        expand_refresh_and_lift_states(
            resources,
            ExpandRefreshAndLiftInputs {
                provider,
                expansion,
                warning_policy,
                remote_bindings,
                state_file,
                saved_dep_bindings,
                refresh_phase,
                schemas: wiring.context().schemas(),
            },
        )
        .await
    }
}

impl<E> ValidatedConfig<ExpandedCanonicalResources<E>> {
    /// Normalize the expanded desired side using only the schemas retained by
    /// this owner.
    pub(crate) async fn prepare_desired_side_for_plan(
        &mut self,
        provider_configs: &[ProviderConfig],
        normalizer: &dyn ProviderNormalizer,
    ) -> carina_core::provider::ProviderResult<()> {
        let Self { wiring, resources } = self;
        resources
            .prepare_desired_side_for_plan(provider_configs, normalizer, wiring.context().schemas())
            .await
    }
}

#[cfg(test)]
impl<E> ValidatedConfig<ExpandedPlanContext<E>> {
    pub(crate) fn into_context_for_test(self) -> PlanContext {
        self.resources.into_context()
    }
}

impl<E: Clone> ValidatedConfig<CanonicalResourcePair<E>> {
    /// Consume this validated pair through the shared deferred-`for`
    /// expansion transition while retaining its validating provider load.
    pub(crate) fn expand_deferred_for(
        self,
        prepared: &PreparedDeferredForExpansion,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        warning_policy: DeferredForWarningPolicy,
    ) -> Result<
        (
            ValidatedConfig<ExpandedCanonicalResources<E>>,
            DeferredForExpansion,
        ),
        AppError,
    > {
        let (resources, expansion) = prepared.expand(
            self.resources,
            self.wiring.context().schemas(),
            remote_bindings,
            warning_policy,
        )?;
        Ok((
            ValidatedConfig {
                wiring: self.wiring,
                resources,
            },
            expansion,
        ))
    }
}

/// Provider factories paired with provider-specific load diagnostics.
type ProviderFactoryLoad = (Vec<LoadedProviderFactory>, HashMap<String, String>);

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

type FormattingFactoryLoad = (
    Vec<LoadedProviderFactory>,
    HashMap<String, ProviderFactoryDiagnostic>,
);

pub(crate) struct FormattingProviderLoad {
    schemas: SchemaRegistry,
    load_errors: HashMap<String, ProviderFactoryDiagnostic>,
}

impl FormattingProviderLoad {
    pub(crate) fn load(
        user_dirs: &carina_core::UserDirs,
        providers: &[ProviderConfig],
        base_dir: &Path,
    ) -> Result<Self, AppError> {
        let (factories, load_errors) =
            build_factories_from_providers_for_formatting(user_dirs, providers, base_dir)?;
        let WiringContext { schemas, .. } = WiringContext::new(factories, providers);
        Ok(Self {
            schemas,
            load_errors,
        })
    }

    pub(crate) fn into_parts(self) -> (SchemaRegistry, HashMap<String, ProviderFactoryDiagnostic>) {
        (self.schemas, self.load_errors)
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
        load_errors: HashMap<String, ProviderFactoryDiagnostic>,
    ) -> Self {
        let WiringContext { schemas, .. } = WiringContext::new(factories, provider_configs);
        Self {
            schemas,
            load_errors,
        }
    }
}

/// Return the source for provider configurations that participate in factory loading.
///
/// A source-less named instance reuses its kind's default factory later during
/// provider routing; it does not cause a factory to be loaded on its own.
pub(crate) fn provider_factory_source(config: &ProviderConfig) -> Option<&ProviderSource> {
    config.source.as_ref()
}

impl WiringContext {
    fn new(factories: Vec<LoadedProviderFactory>, provider_configs: &[ProviderConfig]) -> Self {
        Self::new_with_additional_schemas(factories, provider_configs, std::iter::empty())
    }

    fn new_with_additional_schemas(
        loaded_factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
        additional_schemas: impl IntoIterator<Item = (String, ValidatedResourceSchema)>,
    ) -> Self {
        let uses_builtin_mock =
            carina_provider_mock::uses_builtin_provider(provider_configs, |provider_name| {
                loaded_factories
                    .iter()
                    .any(|loaded| loaded.factory.name() == provider_name)
            });
        let mut provider_artifacts = loaded_factories
            .iter()
            .map(|loaded| {
                (
                    loaded.factory.name().to_string(),
                    LoadedProviderArtifact::Wasm(loaded.artifact.clone()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if uses_builtin_mock {
            provider_artifacts.insert(
                BUILTIN_PROVIDER_NAME.to_string(),
                LoadedProviderArtifact::BuiltIn,
            );
        }
        let factories = loaded_factories
            .into_iter()
            .map(LoadedProviderFactory::into_factory)
            .collect::<Vec<_>>();
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
                let parts = schema.parts();
                let already_registered = if matches!(parts.kind, SchemaKind::DataSource) {
                    schemas.has_data_source(BUILTIN_PROVIDER_NAME, parts.resource_type)
                } else {
                    schemas.has_managed(BUILTIN_PROVIDER_NAME, parts.resource_type)
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
        let factories = Arc::new(factories);
        let custom_types =
            carina_core::provider::provider_custom_types_from_factories(Arc::clone(&factories));
        Self {
            factories,
            custom_types,
            schemas,
            known_providers,
            uses_builtin_mock,
            provider_artifacts,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_configs: &[ProviderConfig],
    ) -> Self {
        Self::new(LoadedProviderFactory::for_test(factories), provider_configs)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_with_additional_schemas(
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_configs: &[ProviderConfig],
        additional_schemas: impl IntoIterator<Item = (String, ValidatedResourceSchema)>,
    ) -> Self {
        Self::new_with_additional_schemas(
            LoadedProviderFactory::for_test(factories),
            provider_configs,
            additional_schemas,
        )
    }

    pub fn factories(&self) -> &[Box<dyn ProviderFactory>] {
        &self.factories
    }

    pub fn factories_arc(&self) -> Arc<Vec<Box<dyn ProviderFactory>>> {
        Arc::clone(&self.factories)
    }

    pub fn custom_type_resolver(&self) -> &dyn carina_core::parser::CustomTypeResolver {
        &self.custom_types
    }

    pub fn schemas(&self) -> &SchemaRegistry {
        &self.schemas
    }

    pub fn known_providers(&self) -> &HashSet<String> {
        &self.known_providers
    }

    pub fn provider_artifacts(&self) -> &BTreeMap<String, LoadedProviderArtifact> {
        &self.provider_artifacts
    }

    fn uses_builtin_mock(&self) -> bool {
        self.uses_builtin_mock
    }
}

/// Build provider factories from provider configs that have a `source` attribute.
///
/// For each provider with a `source`, resolves the WASM component path and creates a
/// `WasmProviderFactory`. Providers without `source` are skipped.
///
/// Returns `(factories, load_errors)` where `load_errors` maps provider names
/// to their failure reasons, so callers can show accurate diagnostics. A stale
/// lock constraint is returned to the supervised command rather than exiting.
fn build_factories_from_providers(
    user_dirs: &carina_core::UserDirs,
    providers: &[ProviderConfig],
    base_dir: &Path,
) -> Result<ProviderFactoryLoad, AppError> {
    carina_provider_resolver::validate_lock_constraints(base_dir, providers)?;

    build_factories_from_providers_impl(user_dirs, providers, base_dir, true)
}

/// Best-effort factory loading for schema-aware formatting.
///
/// Provider resolution is optional on this path: a missing, invalid, or stale
/// provider installation must fall back to plain formatting rather than make
/// `carina fmt` fail or emit a hard provider error.
/// Suppressed provider and lock-constraint failures are returned as load errors
/// so `--check` can report degraded conversion without failing formatting.
fn build_factories_from_providers_for_formatting(
    user_dirs: &carina_core::UserDirs,
    providers: &[ProviderConfig],
    base_dir: &Path,
) -> Result<FormattingFactoryLoad, AppError> {
    if let Err(error) = carina_provider_resolver::validate_lock_constraints(base_dir, providers) {
        return Ok((
            Vec::new(),
            HashMap::from([(
                "carina-providers.lock".to_string(),
                ProviderFactoryDiagnostic::LockConstraint(error),
            )]),
        ));
    }

    let (factories, load_errors) =
        build_factories_from_providers_impl(user_dirs, providers, base_dir, false)?;
    Ok((
        factories,
        load_errors
            .into_iter()
            .map(|(provider, message)| (provider, ProviderFactoryDiagnostic::Message(message)))
            .collect(),
    ))
}

fn build_factories_from_providers_impl(
    user_dirs: &carina_core::UserDirs,
    providers: &[ProviderConfig],
    base_dir: &Path,
    report_errors: bool,
) -> Result<ProviderFactoryLoad, AppError> {
    #[cfg(test)]
    record_tracked_provider_load();

    let grant_inputs =
        TRUSTED_LOCAL_GRANT_INPUTS.get_or_init(|| TrustedLocalGrantInputs::new(Vec::new(), None));
    let loader_user_dirs = user_dirs.clone();
    build_factories_from_providers_impl_using(
        user_dirs,
        providers,
        base_dir,
        report_errors,
        grant_inputs,
        move |verified| {
            let user_dirs = loader_user_dirs.clone();
            async move {
                carina_plugin_host::WasmProviderFactory::from_bytes(verified, &user_dirs).await
            }
        },
    )
}

fn build_factories_from_providers_impl_using<L, Fut>(
    user_dirs: &carina_core::UserDirs,
    providers: &[ProviderConfig],
    base_dir: &Path,
    report_errors: bool,
    grant_inputs: &TrustedLocalGrantInputs,
    mut loader: L,
) -> Result<ProviderFactoryLoad, AppError>
where
    L: FnMut(carina_provider_resolver::VerifiedLoad) -> Fut,
    Fut: std::future::Future<
            Output = Result<
                carina_plugin_host::WasmProviderFactory,
                carina_plugin_host::WasmProviderLoadError,
            >,
        >,
{
    let mut factories = Vec::new();
    let mut load_errors: HashMap<String, String> = HashMap::new();
    let mut attempted_default_sources = HashSet::new();

    for config in providers {
        let Some(source) = provider_factory_source(config) else {
            continue;
        };
        if config.is_default() && !attempted_default_sources.insert(config.name.clone()) {
            let reason = format!(
                "Provider kind '{}' is declared more than once with a source; declare exactly one top-level `provider {}` block.",
                config.name, config.name
            );
            if report_errors {
                return Err(AppError::Validation(reason));
            }
            load_errors.insert(config.name.clone(), reason);
            continue;
        }

        let installed = match carina_provider_resolver::find_installed_provider(base_dir, config) {
            Ok(installed) => installed,
            Err(e) => {
                let reason = format!("Provider '{}' {}", config.name, e);
                if report_errors {
                    eprintln!("{}", reason.red());
                }
                load_errors.insert(config.name.clone(), reason);
                continue;
            }
        };

        let grants = grant_inputs.get()?;
        let factory_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(installed.load_with(
                user_dirs,
                grants,
                |verified| {
                    let artifact = verified.artifact().clone();
                    let loaded = loader(verified);
                    async move { loaded.await.map(|factory| (factory, artifact)) }
                },
            ))
        });

        match factory_result {
            Ok((factory, artifact)) => {
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
                let runtime_artifact = installed.clone();
                let factory = factory.with_runtime_instantiation_error_mapper(move |error| {
                    runtime_instantiation_error_with_provenance(&runtime_artifact, error)
                });
                factories.push(LoadedProviderFactory::new(Box::new(factory), artifact));
            }
            Err(error) => {
                let error = match take_unverified_official_load_error(error) {
                    Ok(unverified) => return Err(AppError::from(unverified)),
                    Err(error) => error,
                };
                if report_errors
                    && let Some(mismatch) =
                        source_provider_kind_mismatch_from_load_error(source, &error)
                {
                    return Err(mismatch);
                }
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

    Ok((factories, load_errors))
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

fn block_name_errors<T: fmt::Display>(conflicts: impl IntoIterator<Item = T>) -> Vec<AppError> {
    conflicts
        .into_iter()
        .map(|conflict| AppError::Validation(conflict.to_string()))
        .collect()
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
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            value_contains_empty_interpolation(value) || value_contains_empty_interpolation(default)
        }
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

/// File-level name resolution also covers residual deferred-for templates.
pub(crate) fn resolve_file_names_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &mut carina_core::parser::File<E>,
) -> Vec<AppError> {
    let mut errors = resolve_file_block_names_with_schemas(parsed, ctx.schemas())
        .err()
        .map(block_name_errors)
        .unwrap_or_default();
    errors.extend(resolve_attr_prefixes_with_ctx(ctx, &mut parsed.resources));
    errors
}

fn resolve_config_names_with_ctx<E>(
    ctx: &WiringContext,
    parsed: carina_core::parser::File<E>,
    unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
) -> Result<CanonicalResourcePair<E>, Vec<AppError>> {
    carina_core::schema::resolve_config_block_names_with_schemas(parsed, unresolved, ctx.schemas())
        .map_err(block_name_errors)?
        .resolve_attr_prefixes(ctx.schemas())
        .map_err(|error| split_validation_message(&error))
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

pub fn reconcile_canonical_prefixed_names<E>(
    resources: &mut CanonicalResourcePair<E>,
    state_file: &Option<StateFile>,
) {
    let Some(state_file) = state_file else {
        return;
    };

    resources.reconcile_prefixed_names(&|provider, resource_type, name| {
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
        Some(identifier::PrefixStateInfo {
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

fn provider_config_schema_for(
    ctx: &WiringContext,
    provider_name: &str,
) -> Option<carina_core::schema::ValidatedConfigSchema> {
    provider_mod::find_factory(ctx.factories(), provider_name)
        .map(|factory| factory.provider_config_attribute_types())
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

    let canonical_providers = carina_core::value::canonicalize_provider_configs_with_schemas(
        providers,
        &|provider_name| provider_config_schema_for(ctx, provider_name),
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
                .map(|schema| schema.schema().as_schema().create_only_attributes())
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
                .map(|schema| schema.schema().as_schema().create_only_attributes())
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

pub fn reconcile_canonical_anonymous_identifiers_with_ctx<E>(
    ctx: &WiringContext,
    resources: &mut CanonicalResourcePair<E>,
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

    let renames = resources.reconcile_anonymous_identifiers(
        ctx.schemas(),
        &|provider, resource_type| {
            let create_only_attrs = ctx
                .schemas()
                .get(
                    provider,
                    resource_type,
                    carina_core::schema::SchemaKind::Resource,
                )
                .map(|schema| schema.schema().as_schema().create_only_attributes())
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
    let canonical_providers = match canonicalized_provider_configs_for_identifiers(ctx, providers) {
        Ok(providers) => providers,
        Err(error) => return vec![error],
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

pub(crate) fn compute_canonical_pair_anonymous_identifiers_with_ctx<E>(
    ctx: &WiringContext,
    resources: &mut CanonicalResourcePair<E>,
    providers: &[ProviderConfig],
) -> Result<Vec<AppError>, AppError> {
    let canonical_providers = match canonicalized_provider_configs_for_identifiers(ctx, providers) {
        Ok(providers) => providers,
        Err(error) => return Ok(vec![error]),
    };

    match resources.canonicalize_and_compute_anonymous_identifiers(
        ctx.schemas(),
        &canonical_providers,
        &|name| identity_attributes_for_provider(ctx, name),
    ) {
        Ok(()) => Ok(Vec::new()),
        Err(carina_core::override_aware::CanonicalResourceIdentityError::EnumCanonicalization(
            error,
        )) => Err(error.into()),
        Err(carina_core::override_aware::CanonicalResourceIdentityError::Identity(message)) => {
            Ok(vec![AppError::Config(message)])
        }
    }
}

fn canonicalized_provider_configs_for_identifiers(
    ctx: &WiringContext,
    providers: &[ProviderConfig],
) -> Result<carina_core::value::CanonicalizedProviderConfigs, AppError> {
    Ok(
        carina_core::value::canonicalize_provider_configs_with_schemas(
            providers,
            &|provider_name| provider_config_schema_for(ctx, provider_name),
        )?,
    )
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

pub(crate) async fn hydrate_prior_managed_states(
    managed_current: &mut HashMap<ResourceId, Current>,
    orphans: &mut Orphans,
    normalizer: &dyn ProviderNormalizer,
    saved_attrs: &LiftedSavedAttrs,
    state_file: Option<&StateFile>,
) -> Result<(), ProviderError> {
    transform_prior_managed_provider_states(managed_current, orphans, async |states| {
        normalizer
            .hydrate_read_state(states, saved_attrs.as_provider_saved_attrs())
            .await?;
        if let Some(state_file) = state_file {
            state_file.restore_partial_read_markers(states);
        }
        Ok(())
    })
    .await
}

pub(crate) async fn hydrate_prior_data_source_values(
    managed_current: &mut HashMap<ResourceId, Current>,
    data_source_values: &mut DataSourceValues,
    orphans: &mut Orphans,
    normalizer: &dyn ProviderNormalizer,
    saved_attrs: &LiftedSavedAttrs,
    state_file: Option<&StateFile>,
) -> Result<(), ProviderError> {
    let mut read_states = data_source_values.read_states_map();
    transform_prior_provider_states(
        managed_current,
        &mut read_states,
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
            let empty_saved_attrs = provider_mod::SavedAttrs::new();
            normalizer
                .hydrate_read_state(states, &empty_saved_attrs)
                .await
        },
    )
    .await?;
    data_source_values
        .merge_read_states(&read_states)
        .map_err(|error| ProviderError::internal(error.to_string()))?;
    Ok(())
}

pub(crate) fn lift_data_source_value_enum_leaves(
    data_source_values: &mut DataSourceValues,
    data_sources: &[DataSource],
    schemas: &SchemaRegistry,
) {
    let schemas_by_id = data_sources
        .iter()
        .filter_map(|data_source| {
            schemas
                .get_for_data_source(data_source)
                .map(|schema| (&data_source.id, schema.schema().as_schema()))
        })
        .collect::<HashMap<_, _>>();
    for (id, state) in data_source_values.iter_read_states_mut() {
        if let Some(schema) = schemas_by_id.get(id) {
            carina_core::utils::lift_state_enum_leaves(&mut state.attributes, schema);
        }
    }
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

async fn normalize_prior_data_source_values(
    managed_current: &mut HashMap<ResourceId, Current>,
    data_source_values: &mut DataSourceValues,
    orphans: &mut Orphans,
    normalizer: &dyn ProviderNormalizer,
) -> Result<(), ProviderError> {
    let mut read_states = data_source_values.read_states_map();
    normalize_prior_states(managed_current, &mut read_states, orphans, normalizer).await?;
    data_source_values
        .merge_read_states(&read_states)
        .map_err(|error| ProviderError::internal(error.to_string()))?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum PlanPreparationError {
    #[error("plan validation failed")]
    Plan(Vec<PlanError>),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

fn resolved_constraint_context(
    error: &TypeError,
    origin_attributes: &IndexMap<String, Value>,
    schema: &ResourceSchema,
) -> (Vec<String>, Vec<String>) {
    let mut origins_by_attribute: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (attribute, value) in origin_attributes {
        if !schema.attributes().contains_key(attribute) {
            continue;
        }
        value.visit_resource_refs(&mut |path| {
            origins_by_attribute
                .entry(attribute.clone())
                .or_default()
                .insert(path.to_plaintext_dot_string());
        });
    }

    let explicit_attribute = error.attribute_name().map(str::to_string);
    let attributes = if let Some(attribute) = explicit_attribute {
        vec![attribute]
    } else if origins_by_attribute.is_empty() {
        let mut known = origin_attributes
            .keys()
            .filter(|attribute| schema.attributes().contains_key(*attribute))
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
            ctx.custom_type_resolver(),
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
            ctx.custom_type_resolver(),
        );
    }

    plan_errors
}

fn append_resolved_value_constraint_errors(
    plan_errors: &mut Vec<PlanError>,
    id: &ResourceId,
    schema: OwnerBoundResourceSchema<'_>,
    attributes: &HashMap<String, Value>,
    quoted_string_attrs: &HashSet<String>,
    origin_attributes: &IndexMap<String, Value>,
    resolver: &dyn carina_core::parser::CustomTypeResolver,
) {
    let is_string_literal = |attribute: &str| quoted_string_attrs.contains(attribute);
    let Err(errors) =
        schema.validate_known_values_with_resolver(attributes, &is_string_literal, resolver)
    else {
        return;
    };

    for error in errors {
        let (attributes, origins) =
            resolved_constraint_context(&error, origin_attributes, schema.schema().as_schema());
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

pub(crate) fn module_constraint_plan_errors(
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
        data_source_values: &mut DataSourceValues,
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
        normalize_prior_data_source_values(
            managed_current,
            data_source_values,
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
        .enable_all()
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

pub(crate) async fn get_provider_with_ctx(
    ctx: &WiringContext,
    configs: &ResolvedProviderConfigs,
    progress_sink: ProgressSink,
) -> Result<ProviderRouter, AppError> {
    let mut router = ProviderRouter::new();

    // Two-pass build so named instances can reuse the kind's factory.
    // Pass 1 handles every default instance (top-level `provider <kind>`
    // blocks). Pass 2 handles each named instance (`let <name> = provider
    // <kind> { ... }`), reusing the same context factory.
    for provider_config in configs.iter().filter(|config| config.is_default()) {
        instantiate_provider_into_router(ctx, &mut router, provider_config, None, progress_sink)
            .await?;
    }

    for provider_config in configs.iter().filter(|config| !config.is_default()) {
        let inherited_source = kind_default_source(configs, provider_config.name());
        instantiate_provider_into_router(
            ctx,
            &mut router,
            provider_config,
            inherited_source,
            progress_sink,
        )
        .await?;
    }

    if router.is_empty() && ctx.uses_builtin_mock() {
        // Use mock provider for other cases.
        // Register the kind's default instance with empty kind to match
        // resources without a provider prefix.
        progress_sink.line("Using mock provider".cyan());
        router.add_provider(String::new(), Box::new(MockProvider::new()));
    }

    Ok(router)
}

/// Look up the `source` declared on the kind's default instance for a
/// given kind name. Named instances inherit `source` from the kind's
/// default — the parser rejects `source` on `let <name> = provider
/// <kind>` — so this is the only way to get the source URL while
/// processing a named instance's wiring.
fn kind_default_source<'a>(
    configs: &'a ResolvedProviderConfigs,
    kind: &str,
) -> Option<&'a ProviderSource> {
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
/// in the codebase that builds this line.
///
/// - `kind` is the kind name (`aws`, `awscc`) — what the user writes
///   in `provider <kind> {}` and routes to via `directives.provider`.
///   Use `ProviderFactory::name()`, not `display_name()`.
/// - `binding = None` is the kind's default instance and is rendered
///   as `instance=default`. Explicit, not omitted: the omission is
///   exactly what made the old shape ambiguous.
/// - `source = None` is omitted from the rendered line. Default instances
///   use their own configured source; named instances inherit the kind
///   default's source.
fn format_provider_using_line(
    kind: &str,
    region: &str,
    binding: Option<&str>,
    source: Option<&ProviderSource>,
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
/// Every instance is constructed from the factory retained by `ctx`; provider
/// artifacts are resolved exactly once before the context is built.
async fn instantiate_provider_into_router(
    ctx: &WiringContext,
    router: &mut ProviderRouter,
    provider_config: &ResolvedProviderConfig,
    inherited_source: Option<&ProviderSource>,
    progress_sink: ProgressSink,
) -> Result<(), AppError> {
    let binding = provider_config.binding();
    if let Some(factory) = provider_mod::find_factory(ctx.factories(), provider_config.name()) {
        let config = prepare_provider_ready_config(factory, provider_config)?;
        let region = factory.extract_region(config.attributes());
        let displayed_source = provider_config.source().or(inherited_source);
        let provider = factory
            .create_provider(binding, &config)
            .await
            .map_err(|e| e.for_provider(provider_config.name().to_string()))?;
        progress_sink.line(
            format_provider_using_line(factory.name(), &region, binding, displayed_source).cyan(),
        );
        router.add_normalizer(
            provider_config.name().to_string(),
            factory
                .create_normalizer(binding, &config)
                .await
                .map_err(|e| e.for_provider(provider_config.name().to_string()))?,
        );
        router.add_provider_config(provider_config.as_provider_config(), provider);
    } else if ctx.uses_builtin_mock() && provider_config.name() == BUILTIN_PROVIDER_NAME {
        progress_sink.line("Using mock provider".cyan());
        router.add_provider_config(
            provider_config.as_provider_config(),
            Box::new(MockProvider::new()),
        );
    } else if let Some(message) = missing_provider_message(provider_config, inherited_source) {
        eprintln!("{}", message.red());
    }
    Ok(())
}

fn missing_provider_message(
    provider_config: &ResolvedProviderConfig,
    inherited_source: Option<&ProviderSource>,
) -> Option<String> {
    if provider_config.name().is_empty() {
        return None;
    }

    Some(match provider_config.binding() {
        Some(name) => format!(
            "Named provider instance '{}' (kind '{}') cannot be loaded \
             because the kind's default instance is unavailable.",
            name,
            provider_config.name()
        ),
        // A source-backed default that failed to load was already reported at
        // the single factory-load boundary, so do not replace that diagnostic.
        None if provider_config.source().is_some() || inherited_source.is_some() => return None,
        None => format!(
            "Provider '{}' requires 'source' and 'version' attributes.",
            provider_config.name()
        ),
    })
}

fn source_provider_kind_mismatch_from_load_error(
    source: &ProviderSource,
    error: &carina_provider_resolver::ProviderArtifactLoadError<
        carina_plugin_host::WasmProviderLoadError,
    >,
) -> Option<AppError> {
    if let Some((configured, expected)) = error
        .identity_error()
        .and_then(|identity| identity.name_mismatch())
    {
        return Some(AppError::SourceProviderTrustMismatch {
            configured: configured.to_string(),
            expected: expected.to_string(),
            provider_source: source.to_string(),
        });
    }

    match error.loader_error()? {
        carina_plugin_host::WasmProviderLoadError::ProviderNameMismatch {
            configured,
            reported,
        } => Some(AppError::SourceProviderKindMismatch {
            configured: configured.clone(),
            loaded: reported.clone(),
            provider_source: source.to_string(),
        }),
        carina_plugin_host::WasmProviderLoadError::PrecompiledDeserialization(_)
        | carina_plugin_host::WasmProviderLoadError::Instantiation(_)
        | carina_plugin_host::WasmProviderLoadError::Metadata { .. }
        | carina_plugin_host::WasmProviderLoadError::Other(_) => None,
    }
}

fn take_unverified_official_load_error<E>(
    error: carina_provider_resolver::ProviderArtifactLoadError<E>,
) -> Result<
    carina_provider_resolver::UnverifiedOfficialProviderError,
    carina_provider_resolver::ProviderArtifactLoadError<E>,
> {
    if error
        .identity_error()
        .and_then(|identity| identity.unverified_official())
        .is_none()
    {
        return Err(error);
    }

    let identity = error
        .into_identity_error()
        .unwrap_or_else(|_| unreachable!("the load error was checked as an identity failure"));
    Ok(identity
        .into_unverified_official()
        .unwrap_or_else(|_| unreachable!("the identity failure was checked as unverified")))
}

/// Build a router exclusively from factories already retained by `ctx`.
pub async fn create_providers_from_configs_with_ctx(
    ctx: &WiringContext,
    configs: &ResolvedProviderConfigs,
) -> Result<ProviderRouter, AppError> {
    let mut router = ProviderRouter::new();

    // Same two-pass shape as `get_provider_with_ctx`: default instances
    // first, then named instances reuse the same context factory.
    for config in configs.iter().filter(|config| config.is_default()) {
        instantiate_provider_into_router(ctx, &mut router, config, None, ProgressSink::Human)
            .await?;
    }
    for config in configs.iter().filter(|config| !config.is_default()) {
        let inherited_source = kind_default_source(configs, config.name());
        instantiate_provider_into_router(
            ctx,
            &mut router,
            config,
            inherited_source,
            ProgressSink::Human,
        )
        .await?;
    }

    if ctx.uses_builtin_mock() {
        router.add_normalizer(BUILTIN_PROVIDER_NAME, Box::new(MockProvider::new()));
    }
    if router.is_empty() && ctx.uses_builtin_mock() {
        ProgressSink::Human.line("Using mock provider".cyan());
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

    if ctx.uses_builtin_mock() {
        router.add_normalizer(BUILTIN_PROVIDER_NAME, Box::new(MockProvider::new()));
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
            template: deferred.clone(),
        })
    }

    fn to_effect(&self) -> Effect {
        Effect::DeferredCreate {
            id: self.id.clone(),
            template: Box::new(self.template.clone()),
        }
    }

    fn to_deferred_replace_effect(&self, deletes: Vec<DeferredReplaceDelete>) -> Effect {
        Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(deletes).expect("planner checked non-empty deletes"),
            id: self.id.clone(),
            template: Box::new(self.template.clone()),
        }))
    }
}

/// I/O-free state derived from the identity-reconciled configuration/state
/// view before managed and data-source refreshes run.
///
/// A caller must choose one of two typed transitions before it can obtain a
/// [`PreparedDeferredForExpansion`]:
///
/// - [`DeferredForExpansionPrelude::reconcile_addresses`] for planning/apply
///   views, which applies explicit moves and anonymous-to-named renames;
/// - [`DeferredForExpansionPrelude::preserve_addresses`] for `state refresh`,
///   which must refresh the rows at their persisted addresses without applying
///   pending configuration transitions.
///
/// Both transitions own the exact state/metadata view accepted by expansion,
/// so a future caller cannot silently choose the wrong behavior with a flag or
/// substitute independently keyed maps.
pub struct DeferredForExpansionPrelude<'state> {
    state_file: &'state Option<StateFile>,
    sorted_resources: Vec<Resource>,
    saved_attrs: LiftedSavedAttrs,
    prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    wait_aliases: Vec<WaitAliasSpec>,
}

/// Inputs for the I/O-free identity/move reconciliation transition.
pub struct ReconcileDeferredForExpansionInputs<'a, E> {
    pub ctx: &'a WiringContext,
    pub parsed: &'a carina_core::parser::File<E>,
    pub provider_configs: &'a [ProviderConfig],
    pub state_block_claims: &'a StateBlockClaims,
    pub managed_current: HashMap<ResourceId, Current>,
    pub orphans: Orphans,
    /// Addresses already read by the pre-expansion refresh pass. The prepared
    /// expansion owns this exact set so no later caller can substitute an
    /// empty/default set at the expansion seam.
    pub already_refreshed: HashSet<ResourceId>,
}

/// State snapshots for the address-preserving `state refresh` transition.
/// Deliberately excludes move/rename inputs: that operation is not authorized
/// to change state-row ownership.
pub struct PreserveDeferredForExpansionInputs {
    pub managed_current: HashMap<ResourceId, Current>,
    pub orphans: Orphans,
    pub already_refreshed: HashSet<ResourceId>,
}

impl<'state> DeferredForExpansionPrelude<'state> {
    /// Build every state/config-derived input used by deferred `for`
    /// expansion. Call this after prefix/module/anonymous identity
    /// reconciliation has produced the state view the plan pipeline uses.
    pub fn new<E>(
        resources: &CanonicalResourcePair<E>,
        state_file: &'state Option<StateFile>,
        schemas: &SchemaRegistry,
    ) -> Result<Self, AppError> {
        Self::from_sorted_resources(
            resources.parsed(),
            resources.sorted_resources().map_err(AppError::Validation)?,
            state_file,
            schemas,
        )
    }

    /// Build the same prelude for `state refresh`, whose parsed configuration
    /// intentionally remains outside the planning-only canonical pair and
    /// crosses the address-preserving transition below.
    pub fn new_for_state_refresh<E>(
        parsed: &carina_core::parser::File<E>,
        state_file: &'state Option<StateFile>,
        schemas: &SchemaRegistry,
    ) -> Result<Self, AppError> {
        let sorted_resources =
            sort_resources_by_dependencies(&parsed.resources).map_err(AppError::Validation)?;
        Self::from_sorted_resources(parsed, sorted_resources, state_file, schemas)
    }

    #[cfg(test)]
    fn new_for_test<E>(
        parsed: &carina_core::parser::File<E>,
        state_file: &'state Option<StateFile>,
        schemas: &SchemaRegistry,
    ) -> Result<Self, AppError> {
        Self::new_for_state_refresh(parsed, state_file, schemas)
    }

    fn from_sorted_resources<E>(
        parsed: &carina_core::parser::File<E>,
        sorted_resources: Vec<Resource>,
        state_file: &'state Option<StateFile>,
        schemas: &SchemaRegistry,
    ) -> Result<Self, AppError> {
        Ok(Self {
            state_file,
            sorted_resources,
            saved_attrs: state_file
                .as_ref()
                .map(StateFile::build_saved_attrs)
                .unwrap_or_default()
                .lift(schemas),
            prev_explicit: state_file
                .as_ref()
                .map(StateFile::build_explicit)
                .unwrap_or_default(),
            wait_aliases: parsed
                .wait_bindings
                .iter()
                .map(WaitAliasSpec::from)
                .collect(),
        })
    }

    pub fn sorted_resources(&self) -> &[Resource] {
        &self.sorted_resources
    }

    pub fn saved_attrs(&self) -> &LiftedSavedAttrs {
        &self.saved_attrs
    }

    pub fn prev_explicit(&self) -> &HashMap<ResourceId, carina_core::explicit::ExplicitFields> {
        &self.prev_explicit
    }

    pub fn wait_aliases(&self) -> &[WaitAliasSpec] {
        &self.wait_aliases
    }

    /// Apply every address-changing state transition once and retain the exact
    /// metadata/alias/exclusion inputs used by expansion.
    pub fn reconcile_addresses<E>(
        mut self,
        mut inputs: ReconcileDeferredForExpansionInputs<'_, E>,
        warning_policy: DeferredForWarningPolicy,
    ) -> Result<ReconciledDeferredForExpansion, AppError> {
        let mut print_warning = |warning: String| eprintln!("{}", warning.yellow());
        let mut suppress_warning = |_: String| {};
        let warning_sink: &mut dyn FnMut(String) = match warning_policy {
            DeferredForWarningPolicy::Print => &mut print_warning,
            DeferredForWarningPolicy::Suppress => &mut suppress_warning,
        };
        let mut moved_pairs = materialize_moved_states_with_warning_sink(
            &mut inputs.managed_current,
            &mut inputs.orphans,
            &mut self.prev_explicit,
            &mut self.saved_attrs,
            &inputs.parsed.state_blocks,
            self.state_file,
            warning_sink,
        )?;
        moved_pairs.extend(apply_anonymous_to_named_renames(
            inputs.ctx,
            &self.sorted_resources,
            inputs.provider_configs,
            &mut inputs.managed_current,
            &mut inputs.orphans,
            &mut self.prev_explicit,
            &mut self.saved_attrs,
            self.state_file,
            inputs.state_block_claims,
        )?);
        Ok(ReconciledDeferredForExpansion(
            AddressedDeferredForExpansionInputs {
                sorted_resources: self.sorted_resources,
                saved_attrs: self.saved_attrs,
                prev_explicit: self.prev_explicit,
                wait_aliases: self.wait_aliases,
                moved_pairs,
                already_refreshed: inputs.already_refreshed,
                managed_current: inputs.managed_current,
                orphans: inputs.orphans,
            },
        ))
    }

    /// Preserve persisted addresses for `state refresh`. This transition is a
    /// different type from [`ReconciledDeferredForExpansion`] so the command
    /// cannot accidentally begin applying pending move/rename declarations.
    pub fn preserve_addresses(
        self,
        inputs: PreserveDeferredForExpansionInputs,
    ) -> AddressPreservingDeferredForExpansion {
        AddressPreservingDeferredForExpansion(AddressedDeferredForExpansionInputs {
            sorted_resources: self.sorted_resources,
            saved_attrs: self.saved_attrs,
            prev_explicit: self.prev_explicit,
            wait_aliases: self.wait_aliases,
            moved_pairs: Vec::new(),
            already_refreshed: inputs.already_refreshed,
            managed_current: inputs.managed_current,
            orphans: inputs.orphans,
        })
    }
}

struct AddressedDeferredForExpansionInputs {
    sorted_resources: Vec<Resource>,
    saved_attrs: LiftedSavedAttrs,
    prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    wait_aliases: Vec<WaitAliasSpec>,
    moved_pairs: Vec<(ResolvedResourceId, ResolvedResourceId)>,
    already_refreshed: HashSet<ResourceId>,
    managed_current: HashMap<ResourceId, Current>,
    orphans: Orphans,
}

struct DeferredForExpansionInputs {
    addressed: AddressedDeferredForExpansionInputs,
    data_source_values: DataSourceValues,
}

/// Address-reconciled state before the plan path's data-source I/O phase has
/// finished. This type deliberately has no expansion method: callers must
/// populate its owned data-source snapshot and seal the complete view first.
pub struct ReconciledDeferredForExpansion(AddressedDeferredForExpansionInputs);

/// Inputs shared by plan, live apply, and state refresh after each path has
/// completed its address transition and before deferred-`for` expansion.
pub(crate) struct AddressedDataSourceReadInputs<'a, 'backend> {
    pub provider: &'a dyn Provider,
    pub refresh_phase: Option<&'a RefreshPhase<'backend>>,
    pub compositions: &'a [Composition],
    pub data_sources: &'a [DataSource],
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub schemas: &'a SchemaRegistry,
    pub saved_dep_bindings: &'a HashMap<ResourceId, BTreeSet<String>>,
    pub factories: &'a [Box<dyn ProviderFactory>],
    pub cancel: Option<&'a ShutdownToken>,
}

async fn read_addressed_data_sources_phase2(
    addressed: &AddressedDeferredForExpansionInputs,
    inputs: AddressedDataSourceReadInputs<'_, '_>,
) -> Result<ReadDataSourcesPhase2, AppError> {
    if inputs.data_sources.is_empty() {
        return Ok(ReadDataSourcesPhase2 {
            values: DataSourceValues::empty(),
            module_constraint_error: None,
        });
    }

    let mut managed_ids = addressed
        .sorted_resources
        .iter()
        .map(|resource| resource.id.clone())
        .collect::<HashSet<_>>();
    let mut occupied_bindings = addressed
        .sorted_resources
        .iter()
        .filter_map(|resource| resource.binding.clone())
        .collect::<HashSet<_>>();
    let mut orphan_resources = Vec::new();
    for (id, orphan) in &addressed.orphans {
        if !managed_ids.insert(id.clone()) {
            continue;
        }
        if let Some(binding) = orphan.binding()
            && !occupied_bindings.insert(binding.to_string())
        {
            continue;
        }
        orphan_resources.push(orphan.binding_resource());
    }
    let managed = addressed
        .sorted_resources
        .iter()
        .chain(orphan_resources.iter())
        .collect::<Vec<_>>();

    read_data_sources_phase2_to_fixpoint(
        ReadDataSourcesInputs {
            provider: inputs.provider,
            refresh_phase: inputs.refresh_phase,
            managed: &managed,
            compositions: inputs.compositions,
            data_sources: inputs.data_sources,
            managed_current: ManagedCurrentView::with_orphans(
                &addressed.managed_current,
                &addressed.orphans,
            ),
            remote_bindings: inputs.remote_bindings,
            schemas: inputs.schemas,
            wait_aliases: &addressed.wait_aliases,
            saved_dep_bindings: inputs.saved_dep_bindings,
            factories: inputs.factories,
            cancel: inputs.cancel,
        },
        Phase2DataSourceStart::Unread,
    )
    .await
}

impl ReconciledDeferredForExpansion {
    pub fn sorted_resources(&self) -> &[Resource] {
        &self.0.sorted_resources
    }

    pub fn saved_attrs(&self) -> &LiftedSavedAttrs {
        &self.0.saved_attrs
    }

    pub fn prev_explicit(&self) -> &HashMap<ResourceId, carina_core::explicit::ExplicitFields> {
        &self.0.prev_explicit
    }

    pub fn managed_current(&self) -> &HashMap<ResourceId, Current> {
        &self.0.managed_current
    }

    pub fn orphans(&self) -> &Orphans {
        &self.0.orphans
    }

    pub fn wait_aliases(&self) -> &[WaitAliasSpec] {
        &self.0.wait_aliases
    }

    pub fn moved_pairs(&self) -> &[(ResolvedResourceId, ResolvedResourceId)] {
        &self.0.moved_pairs
    }

    pub(crate) async fn read_data_sources_phase2(
        &self,
        inputs: AddressedDataSourceReadInputs<'_, '_>,
    ) -> Result<ReadDataSourcesPhase2, AppError> {
        read_addressed_data_sources_phase2(&self.0, inputs).await
    }

    /// Seal the reconciled managed/data-source/orphan snapshots into the only
    /// state accepted by deferred `for` expansion.
    pub fn seal(self, data_source_values: DataSourceValues) -> PreparedDeferredForExpansion {
        PreparedDeferredForExpansion(DeferredForExpansionInputs {
            addressed: self.0,
            data_source_values,
        })
    }
}

/// Address-preserving state prepared specifically for `state refresh`.
pub struct AddressPreservingDeferredForExpansion(AddressedDeferredForExpansionInputs);

impl AddressPreservingDeferredForExpansion {
    pub fn sorted_resources(&self) -> &[Resource] {
        &self.0.sorted_resources
    }

    pub fn managed_current(&self) -> &HashMap<ResourceId, Current> {
        &self.0.managed_current
    }

    pub fn wait_aliases(&self) -> &[WaitAliasSpec] {
        &self.0.wait_aliases
    }

    pub(crate) async fn read_data_sources_phase2(
        &self,
        inputs: AddressedDataSourceReadInputs<'_, '_>,
    ) -> Result<ReadDataSourcesPhase2, AppError> {
        read_addressed_data_sources_phase2(&self.0, inputs).await
    }

    pub fn seal(self, data_source_values: DataSourceValues) -> PreparedDeferredForExpansion {
        PreparedDeferredForExpansion(DeferredForExpansionInputs {
            addressed: self.0,
            data_source_values,
        })
    }
}

/// The single addressed/configured/current-state view accepted by deferred
/// `for` expansion. Its fields are private and it owns the exact snapshots
/// that were reconciled with the persisted metadata, so expansion cannot be
/// invoked with a different state map.
pub struct PreparedDeferredForExpansion(DeferredForExpansionInputs);

/// Planning metadata released only after the prepared value has been
/// consumed. This type cannot be converted back into an expansion witness;
/// callers that need both must expand first, then consume the preparation.
pub(crate) struct PreparedDeferredForExpansionParts {
    pub managed_current: HashMap<ResourceId, Current>,
    pub data_source_values: DataSourceValues,
    pub orphans: Orphans,
    pub saved_attrs: LiftedSavedAttrs,
    pub prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    pub wait_aliases: Vec<WaitAliasSpec>,
    pub moved_pairs: Vec<(ResolvedResourceId, ResolvedResourceId)>,
}

impl PreparedDeferredForExpansion {
    pub(crate) fn into_parts(self) -> PreparedDeferredForExpansionParts {
        PreparedDeferredForExpansionParts {
            managed_current: self.0.addressed.managed_current,
            data_source_values: self.0.data_source_values,
            orphans: self.0.addressed.orphans,
            saved_attrs: self.0.addressed.saved_attrs,
            prev_explicit: self.0.addressed.prev_explicit,
            wait_aliases: self.0.addressed.wait_aliases,
            moved_pairs: self.0.addressed.moved_pairs,
        }
    }
}

/// Controls whether parse/expansion warnings are rendered at this call site.
/// Saved-plan verification must be silent because the normal plan invocation
/// already rendered the same warnings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredForWarningPolicy {
    Print,
    Suppress,
}

/// Private proof that the addressed resource view came from the shared
/// deferred-`for` expansion step.
struct DeferredForExpansionWitness;

/// Outcome of the carina#3132 post-refresh deferred-for expansion.
pub struct DeferredForExpansion {
    /// The augmented, re-sorted resource set: every original resource
    /// plus the materialized loop children, topologically ordered.
    /// Equal in length to the input when no loop resolved.
    pub sorted_resources: Vec<Resource>,
    /// Loops still unresolved (iterable genuinely unknowable at plan
    /// time) — rendered as the carina#3128 validate/plan placeholder.
    pub residual_deferred_for: Vec<carina_core::parser::DeferredForExpression>,
    /// Warnings after resolved/deferred loops have been removed from the
    /// parser view. Retained only by the raw-file test adapter.
    #[cfg(test)]
    residual_warnings: Vec<ParseWarning>,
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
    pub printed_warnings: bool,
    /// Prevent callers from fabricating expansion metadata. Canonical
    /// production consumers additionally require [`ExpandedCanonicalResources`].
    _expanded: DeferredForExpansionWitness,
}

impl DeferredForExpansion {
    /// Consume the witness and apply every expansion-owned parser field as a
    /// unit. This avoids the former resources-only clone/swap that left
    /// resolved loops and warnings in a pre-expansion state.
    #[cfg(test)]
    pub(crate) fn into_expanded_file<E>(
        self,
        mut parsed: carina_core::parser::File<E>,
    ) -> carina_core::parser::File<E> {
        parsed.resources = self.sorted_resources;
        // Concrete loops now live in `resources` and must disappear from the
        // parser view. Genuinely deferred loops still back DeferredCreate /
        // DeferredReplace effects, so retain their fresh templates alongside
        // any residual diagnostic-only loops.
        parsed.deferred_for_expressions = self.residual_deferred_for;
        parsed.deferred_for_expressions.extend(
            self.deferred_create_targets
                .into_iter()
                .map(|target| target.template),
        );
        parsed.warnings = self.residual_warnings;
        parsed
    }
}

/// Canonical resources proven to have crossed the shared deferred-`for`
/// expansion boundary.
///
/// The pair is owned and the field is private, so callers cannot combine an
/// arbitrary canonical pair with expansion metadata. The only production
/// constructor is [`PreparedDeferredForExpansion::expand`].
pub(crate) struct ExpandedCanonicalResources<E> {
    resources: CanonicalResourcePair<E>,
}

impl<E> ExpandedCanonicalResources<E> {
    pub(crate) fn parsed(&self) -> &carina_core::parser::File<E> {
        self.resources.parsed()
    }

    pub(crate) fn as_pair(&self) -> &CanonicalResourcePair<E> {
        &self.resources
    }

    async fn prepare_desired_side_for_plan(
        &mut self,
        provider_configs: &[ProviderConfig],
        normalizer: &dyn ProviderNormalizer,
        schemas: &SchemaRegistry,
    ) -> carina_core::provider::ProviderResult<()> {
        self.resources
            .prepare_desired_side_for_plan(provider_configs, normalizer, schemas)
            .await
    }
}

#[allow(clippy::too_many_arguments)]
pub fn derive_managed_binding_stability<E>(
    parsed: &carina_core::parser::File<E>,
    resources: &[Resource],
    managed_current: &HashMap<ResourceId, Current>,
    data_source_values: &DataSourceValues,
    schemas: &carina_core::schema::SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    wait_aliases: &[WaitAliasSpec],
) -> Result<HashMap<ResourceId, carina_core::binding_index::ManagedBindingStability>, AppError> {
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        schemas,
        resources,
    );
    let optimistic_bindings = ResolvedBindings::pre_apply(PreApplyInputs {
        managed: resources,
        compositions: &parsed.compositions,
        managed_current: &plan_input_current,
        data_source_values,
        remote_bindings,
        wait_aliases,
    });
    let mut resolved = resources.to_vec();
    carina_core::resolver::resolve_refs_for_plan(
        &mut resolved,
        &optimistic_bindings,
        &parsed.upstream_binding_names(),
    )
    .map_err(AppError::Validation)?;
    // Identity derivation runs later in the plan pipeline. Pending resources
    // cannot have a refreshed row addressed to them yet, so they are
    // conservatively unavailable; resolved resources can be diffed normally.
    let mut pending_stability = HashMap::new();
    let resolved = resolved
        .into_iter()
        .filter_map(|resource| {
            let id = resource.id.clone();
            match ResolvedResource::try_new(resource) {
                Some(resource) => Some(resource),
                None => {
                    pending_stability.insert(
                        id,
                        carina_core::binding_index::ManagedBindingStability::Unavailable,
                    );
                    None
                }
            }
        })
        .collect::<Vec<_>>();
    let mut stability = carina_core::differ::managed_binding_stability(
        &resolved,
        &plan_input_current,
        schemas,
        saved_attrs,
        prev_explicit,
    );
    stability.extend(pending_stability);
    Ok(stability)
}

impl PreparedDeferredForExpansion {
    /// Expand against the exact addressed state/metadata view sealed by
    /// one of the prelude's typed address transitions, consuming the canonical
    /// pair into an [`ExpandedCanonicalResources`] witness.
    pub(crate) fn expand<E: Clone>(
        &self,
        mut resources: CanonicalResourcePair<E>,
        schemas: &SchemaRegistry,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        warning_policy: DeferredForWarningPolicy,
    ) -> Result<(ExpandedCanonicalResources<E>, DeferredForExpansion), AppError> {
        let expansion = expand_same_config_deferred_for_target(
            &mut resources,
            &self.0.addressed.sorted_resources,
            &self.0.addressed.managed_current,
            &self.0.data_source_values,
            schemas,
            &self.0.addressed.saved_attrs,
            &self.0.addressed.prev_explicit,
            remote_bindings,
            &self.0.addressed.wait_aliases,
            &self.0.addressed.moved_pairs,
            &self.0.addressed.already_refreshed,
            warning_policy,
        )?;
        Ok((ExpandedCanonicalResources { resources }, expansion))
    }

    /// Raw-file expansion is reserved for address-preserving state refresh
    /// and focused tests. Production plan/apply paths must use [`Self::expand`]
    /// so the canonical pair is consumed into an owned witness.
    pub(crate) fn expand_parsed<E: Clone>(
        &self,
        parsed: &mut carina_core::parser::File<E>,
        schemas: &SchemaRegistry,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        warning_policy: DeferredForWarningPolicy,
    ) -> Result<DeferredForExpansion, AppError> {
        expand_same_config_deferred_for_target(
            parsed,
            &self.0.addressed.sorted_resources,
            &self.0.addressed.managed_current,
            &self.0.data_source_values,
            schemas,
            &self.0.addressed.saved_attrs,
            &self.0.addressed.prev_explicit,
            remote_bindings,
            &self.0.addressed.wait_aliases,
            &self.0.addressed.moved_pairs,
            &self.0.addressed.already_refreshed,
            warning_policy,
        )
    }
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
/// `moved_targets` is derived here from the active `moved_pairs` already
/// materialized on plan/apply paths. Children whose id is in this set are
/// kept out of `refreshable_child_ids` so their migrated state survives
/// (carina#3141). Keeping only the pairs in the prepared value prevents the
/// two representations from diverging.
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
pub(crate) trait DeferredExpansionTarget<E> {
    fn parsed(&self) -> &carina_core::parser::File<E>;

    fn expand_partitioned<F>(
        &mut self,
        bindings: &carina_core::binding_index::IterableBindings<'_>,
        classify: F,
    ) -> Result<
        Vec<carina_core::parser::DeferredForExpression>,
        carina_core::parser::DeferredForExpansionError,
    >
    where
        F: FnMut(&carina_core::parser::DeferredForExpression) -> DeferredForExpansionChoice;
}

impl<E> DeferredExpansionTarget<E> for CanonicalResourcePair<E> {
    fn parsed(&self) -> &carina_core::parser::File<E> {
        self.parsed()
    }

    fn expand_partitioned<F>(
        &mut self,
        bindings: &carina_core::binding_index::IterableBindings<'_>,
        classify: F,
    ) -> Result<
        Vec<carina_core::parser::DeferredForExpression>,
        carina_core::parser::DeferredForExpansionError,
    >
    where
        F: FnMut(&carina_core::parser::DeferredForExpression) -> DeferredForExpansionChoice,
    {
        self.expand_deferred_for_expressions(bindings, classify)
    }
}

impl<E> DeferredExpansionTarget<E> for carina_core::parser::File<E> {
    fn parsed(&self) -> &carina_core::parser::File<E> {
        self
    }

    fn expand_partitioned<F>(
        &mut self,
        bindings: &carina_core::binding_index::IterableBindings<'_>,
        mut classify: F,
    ) -> Result<
        Vec<carina_core::parser::DeferredForExpression>,
        carina_core::parser::DeferredForExpansionError,
    >
    where
        F: FnMut(&carina_core::parser::DeferredForExpression) -> DeferredForExpansionChoice,
    {
        let (selected, retained): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.deferred_for_expressions)
                .into_iter()
                .partition(|deferred| classify(deferred) == DeferredForExpansionChoice::Expand);
        self.warnings.retain(|warning| {
            warning.kind != WarningKind::DeferredFor
                || !retained
                    .iter()
                    .any(|deferred| deferred.file == warning.file && deferred.line == warning.line)
        });
        self.deferred_for_expressions = selected;
        self.expand_deferred_for_expressions(bindings)?;
        let residual = self.deferred_for_expressions.clone();
        self.deferred_for_expressions.extend(retained);
        Ok(residual)
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_same_config_deferred_for_target<E: Clone, T: DeferredExpansionTarget<E>>(
    target: &mut T,
    sorted_resources: &[Resource],
    managed_current: &HashMap<ResourceId, Current>,
    data_source_values: &DataSourceValues,
    schemas: &carina_core::schema::SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    wait_aliases: &[WaitAliasSpec],
    moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
    already_refreshed: &HashSet<ResourceId>,
    warning_policy: DeferredForWarningPolicy,
) -> Result<DeferredForExpansion, AppError> {
    let parsed = target.parsed();
    // Common case: no deferred-for at all. Skip the binding projection
    // and the whole-`File` clone entirely (this runs on every plan /
    // validate). Parse warnings still print — with no deferred-for,
    // expansion would remove none of them, so `parsed`'s set is exactly
    // what the post-expansion path would have printed.
    if parsed.deferred_for_expressions.is_empty() {
        let printed_warnings =
            warning_policy == DeferredForWarningPolicy::Print && parsed.print_warnings();
        return Ok(DeferredForExpansion {
            sorted_resources: sorted_resources.to_vec(),
            residual_deferred_for: Vec::new(),
            #[cfg(test)]
            residual_warnings: parsed.warnings.clone(),
            deferred_create_targets: Vec::new(),
            new_child_ids: HashSet::new(),
            refreshable_child_ids: RefreshableChildIds::default(),
            printed_warnings,
            _expanded: DeferredForExpansionWitness,
        });
    }

    let managed_stability = derive_managed_binding_stability(
        parsed,
        sorted_resources,
        managed_current,
        data_source_values,
        schemas,
        saved_attrs,
        prev_explicit,
        remote_bindings,
        wait_aliases,
    )?;
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        schemas,
        sorted_resources,
    );
    let resolved_bindings = ResolvedBindings::pre_apply_with_stability(
        PreApplyInputs {
            managed: sorted_resources,
            compositions: &parsed.compositions,
            managed_current: &plan_input_current,
            data_source_values,
            remote_bindings,
            wait_aliases,
        },
        &managed_stability,
    );
    let iterable_bindings = resolved_bindings.project_iterable_bindings();

    let mut deferred_create_targets = Vec::new();
    let residual_deferred_for = target
        .expand_partitioned(&iterable_bindings, |deferred| {
            match iterable_bindings.resolve_value(&deferred.iterable) {
                Ok(carina_core::resolver::ValueResolution::Present(value))
                    if is_value_fully_concrete_for_expansion(&value) =>
                {
                    DeferredForExpansionChoice::Expand
                }
                Ok(
                    carina_core::resolver::ValueResolution::Present(_)
                    | carina_core::resolver::ValueResolution::Absent
                    | carina_core::resolver::ValueResolution::Unknown(_),
                ) => {
                    if let Some(target) = DeferredCreateTarget::from_deferred(deferred) {
                        deferred_create_targets.push(target);
                    }
                    DeferredForExpansionChoice::Retain
                }
                // Let the shared expansion path render the path-aware error.
                Err(_) => DeferredForExpansionChoice::Expand,
            }
        })
        .map_err(|error| AppError::Validation(error.to_string()))?;
    let printed_warnings =
        warning_policy == DeferredForWarningPolicy::Print && target.parsed().print_warnings();
    #[cfg(test)]
    let residual_warnings = target.parsed().warnings.clone();
    let parsed = target.parsed();

    let pre_ids: HashSet<ResourceId> = sorted_resources.iter().map(|r| r.id.clone()).collect();

    // A length delta means a loop materialized. Compare against the
    // input slice length (not the deduped `pre_ids` set) so the test is
    // a pure "did expansion add resources" check. Re-sort the augmented
    // set: `topological_sort` preserves declaration order for
    // independent resources (#1071), so already-planned resources keep
    // their relative order (carina#3132 re-sort stability requirement)
    // and the children are slotted in per their refs.
    let materialized = parsed.resources.len() != sorted_resources.len(); // allow: direct — topology (dependency sort)
    let resorted = if materialized {
        sort_resources_by_dependencies(&parsed.resources).map_err(AppError::Validation)?
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
    let moved_targets: HashSet<ResourceId> = moved_pairs
        .iter()
        .map(|(_, to)| to.as_inner().clone())
        .collect();
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
        #[cfg(test)]
        residual_warnings,
        deferred_create_targets,
        new_child_ids,
        refreshable_child_ids,
        printed_warnings,
        _expanded: DeferredForExpansionWitness,
    })
}

/// Unit-test adapter for focused expansion fixtures that predate the
/// production prelude. Production code cannot construct the prepared witness
/// from independently assembled maps.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn expand_same_config_deferred_for<E: Clone>(
    parsed: &carina_core::parser::File<E>,
    sorted_resources: &[Resource],
    managed_current: &HashMap<ResourceId, Current>,
    data_source_values: &DataSourceValues,
    schemas: &SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    wait_aliases: &[WaitAliasSpec],
    moved_targets: &HashSet<ResourceId>,
    already_refreshed: &HashSet<ResourceId>,
) -> Result<DeferredForExpansion, AppError> {
    let moved_pairs = moved_targets
        .iter()
        .map(|target| {
            let target = ResolvedResourceId::new(target.clone());
            (target.clone(), target)
        })
        .collect();
    let prepared = PreparedDeferredForExpansion(DeferredForExpansionInputs {
        addressed: AddressedDeferredForExpansionInputs {
            sorted_resources: sorted_resources.to_vec(),
            saved_attrs: saved_attrs.clone(),
            prev_explicit: prev_explicit.clone(),
            wait_aliases: wait_aliases.to_vec(),
            moved_pairs,
            already_refreshed: already_refreshed.clone(),
            managed_current: managed_current.clone(),
            orphans: Orphans::new(),
        },
        data_source_values: data_source_values.clone(),
    });
    let mut expanded = parsed.clone();
    prepared.expand_parsed(
        &mut expanded,
        schemas,
        remote_bindings,
        DeferredForWarningPolicy::Print,
    )
}

/// Result of the deferred-for expansion, materialised-child read,
/// prior-state hydration, and enum-lift stages shared by every refresh path.
///
/// Closes the bug class behind carina#3266 / #3271 / #3272 at the type
/// level — same closure pattern as
/// [`PostApplyStates`](crate::commands::shared::state_writeback::PostApplyStates).
/// Every call site that needs a post-expansion view of `sorted_resources`
/// (apply `run_apply_locked`, refresh `run_state_refresh_locked`) must obtain it
/// through the corresponding [`ValidatedConfig`] owner transition. Those
/// transitions perform the complete post-expansion sequence:
///
/// 1. `expand_same_config_deferred_for` — synthesise for-loop children
///    and re-sort the augmented set.
/// 2. `refresh_resource_set` — read each materialised child through
///    the provider (filtered through `refreshable_child_ids` so a
///    `moved`-target or orphan-pre-read child is not re-read).
/// 3. Hydrate the managed/orphan and typed data-source snapshots; persisted
///    managed attributes and partial-read markers are restored only to the
///    managed/orphan key space.
/// 4. `lift_prior_state_enum_leaves` on both prior-state owners using the
///    **post-expansion** `sorted_resources`, so enum-typed attrs on new
///    children are not surfaced as phantom case diffs (carina#3272).
///
/// The constructor mutates all three prior-state owners in place. Keeping them
/// inside this operation prevents a refresh path from transforming only one
/// part of prior state.
#[non_exhaustive]
pub struct ExpandedRefreshState<R> {
    /// The resource witness selected by the constructor: `()` for raw-file
    /// state refresh, or [`ExpandedCanonicalResources`] for plan/apply.
    pub(crate) canonical_resources: R,
    pub sorted_resources: Vec<Resource>,
    pub residual_deferred_for: Vec<carina_core::parser::DeferredForExpression>,
    pub deferred_create_targets: Vec<DeferredCreateTarget>,
    pub new_child_ids: HashSet<ResourceId>,
    pub refreshable_child_ids: RefreshableChildIds,
    pub printed_warnings: bool,
}

/// Shared inputs for the refresh-and-lift constructors.
///
/// Grouped into a struct because the underlying phases need many of
/// the same inputs (provider, schemas, wait_aliases, …) and a flat
/// arg list would already trip the 7-argument lint here.
pub(crate) struct ExpandRefreshAndLiftInputs<'a, 'backend, P: Provider + ProviderNormalizer> {
    pub provider: &'a P,
    /// Address/metadata parity witness produced by the shared I/O-free
    /// prelude. Expansion cannot be invoked with independently assembled
    /// saved attributes, aliases, or exclusion sets.
    pub expansion: &'a mut PreparedDeferredForExpansion,
    pub warning_policy: DeferredForWarningPolicy,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub state_file: &'a Option<StateFile>,
    pub saved_dep_bindings: &'a HashMap<ResourceId, BTreeSet<String>>,
    pub refresh_phase: &'a RefreshPhase<'backend>,
    pub schemas: &'a carina_core::schema::SchemaRegistry,
}

/// Refresh-and-lift inputs whose schema registry is supplied by the validated
/// configuration owner rather than as an independently selectable argument.
pub(crate) struct ValidatedExpandRefreshAndLiftInputs<
    'a,
    'backend,
    P: Provider + ProviderNormalizer,
> {
    pub provider: &'a P,
    pub expansion: &'a mut PreparedDeferredForExpansion,
    pub warning_policy: DeferredForWarningPolicy,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub state_file: &'a Option<StateFile>,
    pub saved_dep_bindings: &'a HashMap<ResourceId, BTreeSet<String>>,
    pub refresh_phase: &'a RefreshPhase<'backend>,
}

/// Canonical refresh result retaining the provider wiring that validated the
/// consumed pair.
pub(crate) struct ValidatedExpandedRefreshState<E> {
    pub validated: ValidatedConfig<ExpandedCanonicalResources<E>>,
    pub sorted_resources: Vec<Resource>,
    pub residual_deferred_for: Vec<carina_core::parser::DeferredForExpression>,
    pub deferred_create_targets: Vec<DeferredCreateTarget>,
}

/// Run the expand → child-refresh → hydrate → lift quartet in
/// lock-step for address-preserving raw-file state refresh.
///
/// The unit resource witness makes the raw-file variant distinct from the
/// canonical plan/apply result at the type level.
async fn expand_refresh_and_lift_states<E: Clone, P: Provider + ProviderNormalizer>(
    parsed: &mut carina_core::parser::File<E>,
    inputs: ExpandRefreshAndLiftInputs<'_, '_, P>,
) -> Result<ExpandedRefreshState<()>, AppError> {
    let expansion = inputs.expansion.expand_parsed(
        parsed,
        inputs.schemas,
        inputs.remote_bindings,
        inputs.warning_policy,
    )?;
    finish_expanded_refresh_state((), expansion, inputs).await
}

/// Run the expand → child-refresh → hydrate → lift quartet in lock-step for a
/// canonical plan/apply resource pair.
///
/// The returned state always owns the branded canonical expansion witness; no
/// optional field or variant-dependent assertion is needed by its caller.
pub(crate) async fn expand_validated_canonical_refresh_and_lift_states<
    E: Clone,
    P: Provider + ProviderNormalizer,
>(
    validated: ValidatedConfig<CanonicalResourcePair<E>>,
    inputs: ValidatedExpandRefreshAndLiftInputs<'_, '_, P>,
) -> Result<ValidatedExpandedRefreshState<E>, AppError> {
    let ValidatedConfig {
        wiring,
        resources: canonical_resources,
    } = validated;
    let schemas = wiring.context().schemas();
    let (canonical_resources, expansion) = inputs.expansion.expand(
        canonical_resources,
        schemas,
        inputs.remote_bindings,
        inputs.warning_policy,
    )?;
    let state = finish_expanded_refresh_state(
        canonical_resources,
        expansion,
        ExpandRefreshAndLiftInputs {
            provider: inputs.provider,
            expansion: inputs.expansion,
            warning_policy: inputs.warning_policy,
            remote_bindings: inputs.remote_bindings,
            state_file: inputs.state_file,
            saved_dep_bindings: inputs.saved_dep_bindings,
            refresh_phase: inputs.refresh_phase,
            schemas,
        },
    )
    .await?;
    Ok(ValidatedExpandedRefreshState {
        validated: ValidatedConfig {
            wiring,
            resources: state.canonical_resources,
        },
        sorted_resources: state.sorted_resources,
        residual_deferred_for: state.residual_deferred_for,
        deferred_create_targets: state.deferred_create_targets,
    })
}

async fn finish_expanded_refresh_state<R, P: Provider + ProviderNormalizer>(
    canonical_resources: R,
    expansion: DeferredForExpansion,
    inputs: ExpandRefreshAndLiftInputs<'_, '_, P>,
) -> Result<ExpandedRefreshState<R>, AppError> {
    let DeferredForExpansion {
        sorted_resources,
        residual_deferred_for,
        #[cfg(test)]
            residual_warnings: _,
        deferred_create_targets,
        new_child_ids,
        refreshable_child_ids,
        printed_warnings,
        _expanded: _,
    } = expansion;
    if printed_warnings {
        inputs.refresh_phase.note_external_line();
    }
    // Phase 2: read materialised children that weren't already refreshed.
    // Moved targets are excluded by `refreshable_child_ids`; orphan-owned
    // children are excluded structurally by `select`.
    //
    // Phase 2.5: carry forward saved_attrs to the newly-read children
    // so create-only / API-unreturned fields survive. Mirrors the
    // 2nd `hydrate_read_state` call apply ran by hand
    // (apply/mod.rs ~1098); collapsing it into this constructor stops
    // the next caller from forgetting it.
    let DeferredForExpansionInputs {
        addressed:
            AddressedDeferredForExpansionInputs {
                managed_current,
                orphans,
                saved_attrs,
                ..
            },
        data_source_values,
    } = &mut inputs.expansion.0;
    if !new_child_ids.is_empty() {
        let children = refreshable_child_ids.select(&sorted_resources, orphans);
        refresh_resource_set(
            inputs.provider,
            inputs.refresh_phase,
            children,
            inputs.state_file,
            inputs.schemas,
            inputs.saved_dep_bindings,
            managed_current,
        )
        .await?;
        hydrate_prior_data_source_values(
            managed_current,
            data_source_values,
            orphans,
            inputs.provider,
            saved_attrs,
            inputs.state_file.as_ref(),
        )
        .await?;
    }

    // Phase 3: lift Enums on the post-expansion slice. Both
    // pre-existing managed resources and the new for-loop children
    // need this so enum-typed attrs aren't surfaced as phantom case
    // diffs (carina#3272).
    lift_prior_state_enum_leaves(managed_current, orphans, &sorted_resources, inputs.schemas);

    Ok(ExpandedRefreshState {
        canonical_resources,
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
    validated: ValidatedConfig<CanonicalResourcePair<E>>,
    provider_configs: &ResolvedProviderConfigs,
    state_file: &Option<StateFile>,
    refresh: bool,
    progress_sink: ProgressSink,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    state_block_claims: &StateBlockClaims,
    resolved_state_block_targets: &ResolvedStateBlockTargets,
) -> Result<ValidatedConfig<ExpandedPlanContext<E>>, AppError> {
    let ValidatedConfig {
        wiring,
        resources: canonical_resources,
    } = validated;
    let resources = create_plan_from_parsed_with_upstream_with_ctx_inner(
        wiring.context(),
        canonical_resources,
        provider_configs,
        state_file,
        refresh,
        progress_sink,
        remote_bindings,
        state_block_claims,
        resolved_state_block_targets,
    )
    .await?;
    Ok(ValidatedConfig { wiring, resources })
}

#[allow(clippy::too_many_arguments)]
async fn create_plan_from_parsed_with_upstream_with_ctx_inner<E: Clone>(
    ctx: &WiringContext,
    canonical_resources: CanonicalResourcePair<E>,
    provider_configs: &ResolvedProviderConfigs,
    state_file: &Option<StateFile>,
    refresh: bool,
    progress_sink: ProgressSink,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    state_block_claims: &StateBlockClaims,
    resolved_state_block_targets: &ResolvedStateBlockTargets,
) -> Result<ExpandedPlanContext<E>, AppError> {
    let parsed = canonical_resources.parsed();
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
    let expansion_prelude =
        DeferredForExpansionPrelude::new(&canonical_resources, state_file, ctx.schemas())?;
    // Refresh phases borrow the exact list retained by the typed prelude; no
    // second owned resource graph can diverge before the transition below.
    let sorted_resources = expansion_prelude.sorted_resources();
    let data_sources: Vec<DataSource> = parsed.data_sources.clone();

    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(ctx, provider_configs, progress_sink).await?;

    let mut managed_current: HashMap<ResourceId, Current> = HashMap::new();

    // The prelude owns lifted saved attributes, authoring metadata, wait
    // aliases, and the pre-expansion resource order. It is consumed only
    // after refresh/hydration, when the shared I/O-free transition applies
    // moves/renames and seals the inputs accepted by expansion.
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

    let module_gate = carina_core::executor::ModuleConstraintGate::new(&parsed.compositions);
    let progress_backend = ProgressBackend::new(progress_sink);
    let refresh_phase = refresh.then(|| RefreshPhase::start(&progress_backend));
    // Provider reads do not retain Carina-only dependency metadata. Build the
    // restoration map once so both managed refresh and the unconditional
    // data-source phase consume the same snapshot.
    let saved_dep_bindings: HashMap<ResourceId, BTreeSet<String>> = state_file
        .as_ref()
        .map(|sf| {
            sorted_resources
                .iter()
                .filter_map(|resource| {
                    let identity = resource.id.identity_str()?;
                    let state = sf.find_resource(
                        &resource.id.provider,
                        &resource.id.resource_type,
                        identity,
                    )?;
                    (!state.dependency_bindings.is_empty())
                        .then(|| (resource.id.clone(), state.dependency_bindings.clone()))
                })
                .collect()
        })
        .unwrap_or_default();
    let reconciled_expansion = if let Some(refresh_phase) = refresh_phase.as_ref() {
        // Read states for all resources concurrently using the identifier from
        // state. An address with no row is absent; a persisted row without an
        // identifier remains present and invalid so planning can fail closed.
        // Skip composition resources (module attribute containers) — they have no infrastructure.
        let provider_ref = &provider;
        // Refresh in two phases so data sources can see concrete values
        // from their dependencies (#1683). Phase 1: managed resources in
        // parallel. Phase 2: data sources whose input attributes have
        // been resolved against phase 1's `managed_current`.
        refresh_resource_set(
            provider_ref,
            refresh_phase,
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
            let refreshed = refresh_loaded_orphans(
                provider_ref,
                loaded_orphans,
                refresh_phase,
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
        hydrate_prior_managed_states(
            &mut managed_current,
            &mut orphans,
            &provider,
            expansion_prelude.saved_attrs(),
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
        expansion_prelude.reconcile_addresses(
            ReconcileDeferredForExpansionInputs {
                ctx,
                parsed,
                provider_configs: raw_provider_configs,
                state_block_claims,
                managed_current,
                orphans,
                already_refreshed: orphan_refreshed_ids,
            },
            DeferredForWarningPolicy::Print,
        )?
    } else {
        // --refresh=false: use cached state from state file instead of calling provider.read()
        if let Some(sf) = state_file.as_ref() {
            for resource in sorted_resources {
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
            hydrate_prior_managed_states(
                &mut managed_current,
                &mut orphans,
                &provider,
                expansion_prelude.saved_attrs(),
                state_file.as_ref(),
            )
            .await?;
        } else {
            // No state file: all resources are new (not found)
            for resource in sorted_resources {
                managed_current.insert(resource.id.clone(), Current::absent(resource.id.clone()));
            }
        }
        expansion_prelude.reconcile_addresses(
            ReconcileDeferredForExpansionInputs {
                ctx,
                parsed,
                provider_configs: raw_provider_configs,
                state_block_claims,
                managed_current,
                orphans,
                already_refreshed: orphan_refreshed_ids,
            },
            DeferredForWarningPolicy::Print,
        )?
    };

    // Phase 2 is unconditional: `--refresh=false` skips only managed-resource
    // refresh. Data sources are queries and are always read for this plan.
    let phase2 = reconciled_expansion
        .read_data_sources_phase2(AddressedDataSourceReadInputs {
            provider: &provider,
            refresh_phase: refresh_phase.as_ref(),
            compositions: &parsed.compositions,
            data_sources: &data_sources,
            remote_bindings,
            schemas: ctx.schemas(),
            saved_dep_bindings: &saved_dep_bindings,
            factories: ctx.factories(),
            cancel: None,
        })
        .await?;
    let refresh_module_constraint_errors = phase2
        .module_constraint_error
        .map(module_constraint_plan_errors);
    let data_source_values = phase2.values;
    let prepared_expansion = reconciled_expansion.seal(data_source_values);

    // carina#3132: expand same-config deferred-for loops after refresh,
    // against the same post-refresh `ResolvedBindings` view every
    // non-loop `ResourceRef` resolves against. Pure expand+re-sort is
    // factored into `expand_same_config_deferred_for` (see its doc);
    // here we perform the I/O half — targeted-refresh of the children
    // so a re-plan after they were applied sees their live state
    // instead of a phantom Create.
    let (expanded_canonical_resources, expansion) = prepared_expansion.expand(
        canonical_resources,
        ctx.schemas(),
        remote_bindings,
        DeferredForWarningPolicy::Print,
    )?;
    let DeferredForExpansion {
        sorted_resources: resorted,
        residual_deferred_for,
        #[cfg(test)]
            residual_warnings: _,
        deferred_create_targets,
        new_child_ids,
        refreshable_child_ids,
        printed_warnings,
        _expanded: _,
    } = expansion;
    let sorted_resources = resorted;
    let PreparedDeferredForExpansionParts {
        mut managed_current,
        mut data_source_values,
        mut orphans,
        saved_attrs,
        prev_explicit,
        wait_aliases,
        moved_pairs,
    } = prepared_expansion.into_parts();
    let parsed = expanded_canonical_resources.parsed();

    // A printed `⚠` warning is newline-terminated and is written on top of
    // indicatif's open spinner bar line, so it closes the bar region the
    // refresh phases above left open — the cursor is no longer parked on an
    // unterminated `✓` line. Tell the phase guard so its eventual close does
    // not add a second, spurious blank line on the deferred-for-with-warnings
    // TTY path (#3150 Round-4 finding). A child refresh below may re-open it.
    if printed_warnings && let Some(refresh_phase) = refresh_phase.as_ref() {
        refresh_phase.note_external_line();
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
        if let Some(refresh_phase) = refresh_phase.as_ref() {
            refresh_resource_set(
                &provider,
                refresh_phase,
                children(),
                state_file,
                ctx.schemas(),
                &HashMap::new(),
                &mut managed_current,
            )
            .await?;
            hydrate_prior_data_source_values(
                &mut managed_current,
                &mut data_source_values,
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
            hydrate_prior_data_source_values(
                &mut managed_current,
                &mut data_source_values,
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

    drop(refresh_phase);

    // awscc#251 (follow-up to #3055): data-source lifting stays here
    // because data sources do not flow through `into_plan_input_current_map`.
    lift_data_source_value_enum_leaves(&mut data_source_values, &data_sources, ctx.schemas());
    // Build the unified pre-apply bindings view once (carina#3248):
    // every kind of binding the configuration declares (managed,
    // composition, data source) is in the same view, so a managed
    // attribute referencing `<module_instance>.<attr>` (a composition)
    // chains through to the managed sibling literal instead of
    // surviving as an unresolved `ResourceRef` (carina#3246).
    let upstream_binding_names = parsed.upstream_binding_names();
    let managed_stability = derive_managed_binding_stability(
        parsed,
        &sorted_resources,
        &managed_current,
        &data_source_values,
        ctx.schemas(),
        &saved_attrs,
        &prev_explicit,
        remote_bindings,
        &wait_aliases,
    )?;
    let pre_apply_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        ctx.schemas(),
        &sorted_resources,
    );
    let (mut override_aware_resources, unresolved_override_aware_resources) =
        OverrideAwareResources::build_pair_with_stability(
            expanded_canonical_resources.as_pair(),
            state_file.as_ref(),
            PreApplyInputs {
                managed: &[],
                compositions: &parsed.compositions,
                managed_current: &pre_apply_current,
                data_source_values: &data_source_values,
                remote_bindings,
                wait_aliases: &wait_aliases,
            },
            &managed_stability,
            OverrideAwareBuildMode::Plan {
                unresolved_upstream_bindings: &upstream_binding_names,
            },
        )?;
    crate::legacy_name_overrides::emit_legacy_override_warnings(
        &override_aware_resources,
        state_file.as_ref(),
    );
    // Resolve and canonicalize only refresh-time data sources. Deferred
    // reads keep their original input refs so the plan/executor dependency
    // graph still contains the upstream create edge.
    let mut data_sources_for_plan = prepare_data_sources_for_plan(
        &data_source_values,
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
    let preparation = if let Some(errors) = refresh_module_constraint_errors {
        Err(PlanPreparationError::Plan(errors))
    } else {
        preprocessor
            .prepare(
                &mut override_aware_resources,
                &constraint_origin_resources,
                &module_gate,
                &mut managed_current,
                &mut data_source_values,
                &mut orphans,
                raw_provider_configs,
                &mut data_sources_for_plan,
                &data_sources,
            )
            .await
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
        return Ok(ExpandedPlanContext::new(
            PlanContext {
                plan,
                provider,
                // Error plans are aborted before any resolved-resource consumer,
                // including plan-image rendering. Do not weaken the successful
                // path's type invariant to retain unusable pending resources.
                sorted_resources: Vec::new(),
                unresolved_resources: Vec::new(),
                compositions: prepared_compositions,
                data_sources: data_sources_for_plan,
                managed_current,
                data_source_values,
                moved_origins,
                upstream_snapshot: remote_bindings.clone(),
                prev_explicit,
                residual_deferred_for,
                expansion_trace: parsed.expansion_trace.clone(),
            },
            expanded_canonical_resources,
        ));
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
    let stability_resources =
        ResolvedResource::collect_resolved(override_aware_resources.resources().iter().cloned())?;
    let managed_stability = carina_core::differ::managed_binding_stability(
        &stability_resources,
        &reconciled_input_current,
        ctx.schemas(),
        &saved_attrs,
        &prev_explicit,
    );
    override_aware_resources.rebuild_bindings_after_identity_reconciliation_with_stability(
        RebuildBindingsInputs {
            compositions: &parsed.compositions,
            managed_current: &reconciled_input_current,
            data_source_values: &data_source_values,
            remote_bindings,
            wait_aliases: &wait_aliases,
        },
        &managed_stability,
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
        PlanPriorStates::new(&plan_input_current, &orphans),
        &directives_map,
        ctx.schemas(),
        &saved_attrs,
        &prev_explicit,
        &wait_bindings,
    )?;
    add_deposed_delete_effects(&mut plan, state_file, &moved_pairs);
    block_deletes_on_prior_consumer_updates(&mut plan, &directives_map);
    override_aware_resources.apply_plan_stability(&plan_input_current, &plan, ctx.schemas());

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
    let unresolved_resources = resolved_unresolved_resources
        .into_iter()
        .map(ResolvedResource::into_inner)
        .map(UnresolvedResource::from_pre_resolve)
        .collect();
    Ok(ExpandedPlanContext::new(
        PlanContext {
            plan,
            provider,
            // Keep the evaluated desired view distinct from the authored
            // pre-resolution snapshot. Before carina#3861 this field was filled
            // with `paired_unresolved_resources`; for let-bound resources that
            // pairing deliberately prefers `unresolved_parsed` and therefore
            // restored unevaluated calls such as `secret("...")`. Anonymous
            // resources have no binding and kept this evaluated view, producing
            // the bound/anonymous saved-plan divergence.
            sorted_resources: resolved_resources,
            unresolved_resources,
            compositions: prepared_compositions,
            data_sources: data_sources_for_plan,
            managed_current,
            data_source_values,
            moved_origins,
            upstream_snapshot: remote_bindings.clone(),
            prev_explicit,
            residual_deferred_for,
            expansion_trace: parsed.expansion_trace.clone(),
        },
        expanded_canonical_resources,
    ))
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
#[cfg(test)]
fn materialize_moved_states(
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
                sf.find_resource_by_state_block_address(&from)
                    .map(|rs| resolved_state_row_id(rs, rs.directives.provider_instance.clone()))
            });
            let Some(resolved_from) = resolved_from else {
                let to_exists = state_file
                    .as_ref()
                    .is_some_and(|sf| sf.find_resource_by_state_block_address(&to).is_some());
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
                    sf.find_resource_by_state_block_address(&moved_from)
                        .is_some()
                }) {
                    from.insert(moved_from);
                    to.insert(moved_to);
                }
            }
            StateBlock::Removed { from: removed_from } => {
                if let Some(id) = state_file.as_ref().and_then(|sf| {
                    sf.find_resource_by_state_block_address(removed_from)
                        .map(|rs| {
                            resolved_state_row_id(rs, rs.directives.provider_instance.clone())
                        })
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
                    sf.find_resource_by_state_block_address(&resolved_to)
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
        .find(|resource_id| to.matches_resource_id(resource_id))
        .cloned()
        .and_then(ResolvedResourceId::try_new)
}

fn resolve_import_target_in_desired(
    to: &StateBlockAddress,
    desired: &[Resource],
) -> Option<StateBlockAddress> {
    to.find_resource(desired).and_then(|resource| {
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
                let already_in_state = state_file
                    .as_ref()
                    .is_some_and(|sf| sf.find_resource_by_state_block_address(to).is_some());
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
                    sf.find_resource_by_state_block_address(from).map(|rs| {
                        let id = resolved_state_row_id(rs, rs.directives.provider_instance.clone());
                        let abandoned_deposed = rs
                            .deposed
                            .iter()
                            .filter(|deposed| {
                                AppliedIdentifier::new(deposed.identifier.as_str()).is_err()
                            })
                            .map(|deposed| {
                                (
                                    resolved_state_row_id(rs, deposed.provider_instance.clone()),
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
    to.find_resource(desired)
        .map(|resource| ResolvedResourceId::new(resource.id.clone()))
        .unwrap_or_else(|| to.to_unrouted_resource_id())
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
    refresh_phase: &RefreshPhase<'_>,
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
            let progress = refresh_phase.begin(id);
            async move {
                let refreshed =
                    refresh_loaded_orphan_with_retry(provider, loaded, state_file, schemas)
                        .await
                        .map_err(AppError::Provider)?;
                progress.finish();
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
/// path (`commands::apply`): same `stream::iter → begin →
/// read_with_retry → buffer_unordered(5)` pipeline. `saved_dep_bindings`
/// restores carina-only `dependency_bindings` the provider's `read()`
/// does not return (#1565); pass an empty map when there is nothing to
/// restore (the new loop children have no prior state-file dep
/// bindings).
pub(crate) async fn refresh_resource_set<'a>(
    provider: &dyn Provider,
    refresh_phase: &RefreshPhase<'_>,
    resources: impl Iterator<Item = &'a Resource>,
    state_file: &Option<StateFile>,
    schemas: &SchemaRegistry,
    saved_dep_bindings: &HashMap<ResourceId, BTreeSet<String>>,
    managed_current: &mut HashMap<ResourceId, Current>,
) -> Result<(), AppError> {
    let results: Vec<Result<(ResourceId, Current), AppError>> =
        // carina#3181: `resources` is a managed-resource iterator —
        // data sources go through the data-source refresh path and
        // compositions carry no provider state.
        stream::iter(resources)
            .map(|resource| {
                let progress = refresh_phase.begin(&resource.id);
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
    Ok(())
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

/// Complete inputs for the phase-2 data-source fixpoint shared by plan, apply,
/// and state refresh.
pub(crate) struct ReadDataSourcesInputs<'a, 'backend> {
    pub provider: &'a dyn Provider,
    pub refresh_phase: Option<&'a RefreshPhase<'backend>>,
    pub managed: &'a [&'a Resource],
    pub compositions: &'a [Composition],
    pub data_sources: &'a [DataSource],
    managed_current: ManagedCurrentView<'a>,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub schemas: &'a SchemaRegistry,
    pub wait_aliases: &'a [WaitAliasSpec],
    pub saved_dep_bindings: &'a HashMap<ResourceId, BTreeSet<String>>,
    pub factories: &'a [Box<dyn ProviderFactory>],
    /// When present, stop dispatching new provider reads as soon as the
    /// command enters graceful shutdown. Reads already in flight are drained.
    pub cancel: Option<&'a ShutdownToken>,
}

#[derive(Clone, Copy)]
enum Phase2DataSourceBindings<'a> {
    Unread(&'a [DataSource]),
    Outcomes(&'a DataSourceValues),
}

enum Phase2DataSourceStart {
    Unread,
    Outcomes(DataSourceValues),
}

impl Phase2DataSourceBindings<'_> {
    fn data_sources(&self) -> &[DataSource] {
        match self {
            Self::Unread(data_sources) => data_sources,
            Self::Outcomes(values) => values.data_sources(),
        }
    }
}

#[derive(Clone, Copy)]
struct ManagedCurrentView<'a> {
    base: &'a HashMap<ResourceId, Current>,
    orphans: Option<&'a Orphans>,
}

impl<'a> ManagedCurrentView<'a> {
    #[cfg(test)]
    fn base(base: &'a HashMap<ResourceId, Current>) -> Self {
        Self {
            base,
            orphans: None,
        }
    }

    fn with_orphans(base: &'a HashMap<ResourceId, Current>, orphans: &'a Orphans) -> Self {
        Self {
            base,
            orphans: Some(orphans),
        }
    }

    fn get(&self, id: &ResourceId) -> Option<&'a Current> {
        self.base.get(id).or_else(|| {
            self.orphans
                .and_then(|orphans| orphans.get(id))
                .map(|orphan| orphan.current())
        })
    }

    fn iter(&self) -> impl Iterator<Item = (&'a ResourceId, &'a Current)> {
        self.base
            .iter()
            .chain(self.orphans.into_iter().flat_map(move |orphans| {
                orphans.iter().filter_map(move |(id, orphan)| {
                    (!self.base.contains_key(id)).then_some((id, orphan.current()))
                })
            }))
    }
}

pub(crate) struct ReadDataSourcesPhase2 {
    pub values: DataSourceValues,
    pub module_constraint_error: Option<carina_core::executor::ModuleConstraintGateError>,
}

/// Post-expansion inputs for resuming the shared phase-2 fixpoint. Existing
/// `Read` outcomes are retained; only `DeferredToApply` outcomes are retried.
pub(crate) struct ExpandedDataSourceReadInputs<'a, 'backend> {
    pub provider: &'a dyn Provider,
    pub refresh_phase: Option<&'a RefreshPhase<'backend>>,
    pub managed: &'a [Resource],
    pub compositions: &'a [Composition],
    pub data_sources: &'a [DataSource],
    pub managed_current: &'a HashMap<ResourceId, Current>,
    pub orphans: &'a Orphans,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub schemas: &'a SchemaRegistry,
    pub wait_aliases: &'a [WaitAliasSpec],
    pub saved_dep_bindings: &'a HashMap<ResourceId, BTreeSet<String>>,
    pub factories: &'a [Box<dyn ProviderFactory>],
    pub cancel: Option<&'a ShutdownToken>,
}

pub(crate) async fn read_expanded_data_sources_phase2(
    values: DataSourceValues,
    inputs: ExpandedDataSourceReadInputs<'_, '_>,
) -> Result<ReadDataSourcesPhase2, AppError> {
    if inputs.data_sources.is_empty() {
        return Ok(ReadDataSourcesPhase2 {
            values,
            module_constraint_error: None,
        });
    }
    let managed = inputs.managed.iter().collect::<Vec<_>>();
    read_data_sources_phase2_to_fixpoint(
        ReadDataSourcesInputs {
            provider: inputs.provider,
            refresh_phase: inputs.refresh_phase,
            managed: &managed,
            compositions: inputs.compositions,
            data_sources: inputs.data_sources,
            managed_current: ManagedCurrentView::with_orphans(
                inputs.managed_current,
                inputs.orphans,
            ),
            remote_bindings: inputs.remote_bindings,
            schemas: inputs.schemas,
            wait_aliases: inputs.wait_aliases,
            saved_dep_bindings: inputs.saved_dep_bindings,
            factories: inputs.factories,
            cancel: inputs.cancel,
        },
        Phase2DataSourceStart::Outcomes(values),
    )
    .await
}

impl ReadDataSourcesPhase2 {
    pub fn into_values(self) -> Result<DataSourceValues, AppError> {
        if let Some(error) = self.module_constraint_error {
            return Err(AppError::from(error));
        }
        Ok(self.values)
    }
}

fn collect_data_source_values(
    data_sources: &[DataSource],
    mut values_by_id: HashMap<ResourceId, DataSourceValue>,
) -> Result<DataSourceValues, AppError> {
    DataSourceValues::try_from_data_sources(data_sources, |data_source| {
        values_by_id.remove(&data_source.id).ok_or_else(|| {
            AppError::Config(format!(
                "data-source read phase produced no outcome for `{}`",
                data_source.id
            ))
        })
    })
}

/// Keep the module gate scoped to constraints that govern at least one query.
///
/// A data source can consume a composition directly, through another data
/// source, or through a managed resource. Walk that binding dependency closure
/// and retain only the compositions it reaches. A data source declared inside
/// a module is also governed by that module call even when argument
/// substitution has already made its provider inputs literal.
fn relevant_compositions_for_data_source_reads(
    managed: &[&Resource],
    compositions: &[Composition],
    queries: &[DataSource],
    binding_data_sources: &[DataSource],
) -> Vec<Composition> {
    let managed_by_binding = managed
        .iter()
        .filter_map(|resource| {
            resource
                .binding
                .as_deref()
                .map(|binding| (binding, get_resource_dependencies(resource)))
        })
        .collect::<HashMap<_, _>>();
    let data_sources_by_binding = binding_data_sources
        .iter()
        .filter_map(|data_source| {
            data_source
                .binding
                .as_deref()
                .map(|binding| (binding, get_data_source_dependencies(data_source)))
        })
        .collect::<HashMap<_, _>>();
    let compositions_by_binding = compositions
        .iter()
        .enumerate()
        .filter_map(|(index, composition)| {
            composition
                .binding
                .as_deref()
                .map(|binding| (binding, (index, get_composition_dependencies(composition))))
        })
        .collect::<HashMap<_, _>>();

    let mut pending = queries
        .iter()
        .flat_map(get_data_source_dependencies)
        .collect::<Vec<_>>();
    let mut visited_bindings = HashSet::new();
    let mut relevant_indices = HashSet::new();

    for data_source in queries {
        let Some(ModuleSource::Module { name, instance, .. }) = &data_source.module_source else {
            continue;
        };
        for (index, composition) in compositions.iter().enumerate() {
            if composition.module_name == *name && composition.instance == *instance {
                relevant_indices.insert(index);
                pending.extend(get_composition_dependencies(composition));
            }
        }
    }

    while let Some(binding) = pending.pop() {
        if !visited_bindings.insert(binding.clone()) {
            continue;
        }
        if let Some(dependencies) = managed_by_binding.get(binding.as_str()) {
            pending.extend(dependencies.iter().cloned());
        }
        if let Some(dependencies) = data_sources_by_binding.get(binding.as_str()) {
            pending.extend(dependencies.iter().cloned());
        }
        if let Some((index, dependencies)) = compositions_by_binding.get(binding.as_str()) {
            relevant_indices.insert(*index);
            pending.extend(dependencies.iter().cloned());
        }
    }

    compositions
        .iter()
        .enumerate()
        .filter(|(index, _)| relevant_indices.contains(index))
        .map(|(_, composition)| composition.clone())
        .collect()
}

/// Drive phase-2 data-source reads to a fixpoint.
///
/// The first pass reads every query that is resolvable from managed state.
/// Later passes retain those `Read` outcomes as bindings and reconsider only
/// `DeferredToApply` outcomes. The loop stops when a pass produces no new read
/// state, leaving only queries that depend on genuinely apply-time values.
async fn read_data_sources_phase2_to_fixpoint(
    inputs: ReadDataSourcesInputs<'_, '_>,
    start: Phase2DataSourceStart,
) -> Result<ReadDataSourcesPhase2, AppError> {
    if inputs.data_sources.is_empty() {
        let values = match start {
            Phase2DataSourceStart::Unread => DataSourceValues::empty(),
            Phase2DataSourceStart::Outcomes(values) => values,
        };
        return Ok(ReadDataSourcesPhase2 {
            values,
            module_constraint_error: None,
        });
    }

    let mut values = match start {
        Phase2DataSourceStart::Unread => {
            let initial = read_data_sources_phase2_pass(
                &inputs,
                inputs.data_sources,
                Phase2DataSourceBindings::Unread(inputs.data_sources),
            )
            .await?;
            if initial.module_constraint_error.is_some() {
                return Ok(initial);
            }
            initial.values
        }
        Phase2DataSourceStart::Outcomes(values) => values,
    };

    loop {
        let deferred_ids = values
            .iter()
            .filter_map(|(data_source, value)| {
                matches!(value, DataSourceValue::DeferredToApply(_))
                    .then_some(data_source.id.clone())
            })
            .collect::<HashSet<_>>();
        if deferred_ids.is_empty() {
            return Ok(ReadDataSourcesPhase2 {
                values,
                module_constraint_error: None,
            });
        }
        let retry_data_sources = inputs
            .data_sources
            .iter()
            .filter(|data_source| deferred_ids.contains(&data_source.id))
            .cloned()
            .collect::<Vec<_>>();
        let retry = read_data_sources_phase2_pass(
            &inputs,
            &retry_data_sources,
            Phase2DataSourceBindings::Outcomes(&values),
        )
        .await?;
        if let Some(error) = retry.module_constraint_error {
            return Ok(ReadDataSourcesPhase2 {
                values,
                module_constraint_error: Some(error),
            });
        }
        let read_states = retry.values.read_states_map();
        if read_states.is_empty() {
            return Ok(ReadDataSourcesPhase2 {
                values,
                module_constraint_error: None,
            });
        }
        values
            .merge_read_states(&read_states)
            .map_err(|error| AppError::Config(error.to_string()))?;
    }
}

/// Execute one pass of the shared fixpoint. Callers outside the fixpoint must
/// not bypass [`read_data_sources_phase2_to_fixpoint`].
async fn read_data_sources_phase2_pass(
    inputs: &ReadDataSourcesInputs<'_, '_>,
    data_sources: &[DataSource],
    data_source_bindings: Phase2DataSourceBindings<'_>,
) -> Result<ReadDataSourcesPhase2, AppError> {
    if data_sources.is_empty() {
        return Ok(ReadDataSourcesPhase2 {
            values: DataSourceValues::empty(),
            module_constraint_error: None,
        });
    }
    let bindings = data_source_refresh_bindings_from_refs(
        inputs.managed,
        inputs.compositions,
        data_source_bindings,
        inputs.managed_current,
        inputs.remote_bindings,
        inputs.schemas,
        inputs.wait_aliases,
    );
    let relevant_compositions = relevant_compositions_for_data_source_reads(
        inputs.managed,
        inputs.compositions,
        data_sources,
        data_source_bindings.data_sources(),
    );
    let module_gate = carina_core::executor::ModuleConstraintGate::new(&relevant_compositions);
    let resolutions = resolve_data_source_refs_for_refresh_from_refs(
        inputs.managed,
        inputs.compositions,
        data_sources,
        data_source_bindings,
        inputs.managed_current,
        inputs.remote_bindings,
        inputs.schemas,
        inputs.wait_aliases,
    )?;

    if let Err(error) = module_gate.check(&bindings) {
        let values_by_id = resolutions
            .into_iter()
            .map(|resolution| match resolution {
                DataSourceRefreshResolution::DeferredToApply {
                    resource,
                    unresolved,
                } => (resource.id, DataSourceValue::deferred_to_apply(unresolved)),
                DataSourceRefreshResolution::Resolved(resource) => {
                    (resource.id, DataSourceValue::blocked_by_plan_error())
                }
            })
            .collect();
        return Ok(ReadDataSourcesPhase2 {
            values: collect_data_source_values(data_sources, values_by_id)?,
            module_constraint_error: Some(error),
        });
    }

    let results: Vec<Result<Option<(ResourceId, DataSourceValue)>, AppError>> =
        stream::iter(resolutions)
            .map(|resolution| {
                let bindings = &bindings;
                let module_gate = &module_gate;
                async move {
                    if inputs
                        .cancel
                        .is_some_and(|cancel| !matches!(cancel.phase(), ShutdownPhase::Running))
                    {
                        return Ok(None);
                    }
                    match resolution {
                        DataSourceRefreshResolution::DeferredToApply {
                            resource,
                            unresolved,
                        } => Ok(Some((
                            resource.id,
                            DataSourceValue::deferred_to_apply(unresolved),
                        ))),
                        DataSourceRefreshResolution::Resolved(resource) => {
                            let progress = inputs
                                .refresh_phase
                                .map(|refresh_phase| refresh_phase.begin(&resource.id));
                            let mut state = read_data_source_with_retry(
                                inputs.provider,
                                &resource,
                                bindings,
                                module_gate,
                                inputs.factories,
                                inputs.schemas,
                            )
                            .await?;
                            if let Some(dependencies) =
                                inputs.saved_dep_bindings.get(&resource.id).cloned()
                            {
                                state.dependency_bindings = dependencies;
                            }
                            if let Some(progress) = progress {
                                progress.finish();
                            }
                            Ok(Some((resource.id, DataSourceValue::read(state))))
                        }
                    }
                }
            })
            .buffer_unordered(5)
            .collect()
            .await;
    let mut values_by_id = HashMap::with_capacity(results.len());
    for result in results {
        if let Some((id, value)) = result? {
            values_by_id.insert(id, value);
        }
    }
    if inputs
        .cancel
        .is_some_and(|cancel| !matches!(cancel.phase(), ShutdownPhase::Running))
    {
        return Err(AppError::Interrupted);
    }

    Ok(ReadDataSourcesPhase2 {
        values: collect_data_source_values(data_sources, values_by_id)?,
        module_constraint_error: None,
    })
}

/// Resolve `ResourceRef` values in refreshable data-source input attributes
/// against already-refreshed managed current state, and explicitly classify reads
/// that must wait for apply-time publication.
///
/// The `managed` slice is passed (not just the data sources) because the
/// resolver builds its binding map from every managed resource with a
/// `binding` — data sources reference those.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn resolve_data_source_refs_for_refresh(
    managed: &[Resource],
    compositions: &[carina_core::resource::Composition],
    data_sources: &[DataSource],
    managed_current: &HashMap<ResourceId, Current>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    schemas: &carina_core::schema::SchemaRegistry,
    wait_aliases: &[WaitAliasSpec],
) -> Result<Vec<DataSourceRefreshResolution>, AppError> {
    let managed_refs = managed.iter().collect::<Vec<_>>();
    resolve_data_source_refs_for_refresh_from_refs(
        &managed_refs,
        compositions,
        data_sources,
        Phase2DataSourceBindings::Unread(data_sources),
        ManagedCurrentView::base(managed_current),
        remote_bindings,
        schemas,
        wait_aliases,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_data_source_refs_for_refresh_from_refs(
    managed: &[&Resource],
    compositions: &[carina_core::resource::Composition],
    data_sources: &[DataSource],
    data_source_bindings: Phase2DataSourceBindings<'_>,
    managed_current: ManagedCurrentView<'_>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    schemas: &carina_core::schema::SchemaRegistry,
    wait_aliases: &[WaitAliasSpec],
) -> Result<Vec<DataSourceRefreshResolution>, AppError> {
    // carina#3248: unified pre-apply bindings include compositions so a
    // data-source input referencing `<module_instance>.<attr>` chains
    // through the composition layer to the managed sibling literal.
    let bindings = data_source_refresh_bindings_from_refs(
        managed,
        compositions,
        data_source_bindings,
        managed_current,
        remote_bindings,
        schemas,
        wait_aliases,
    );
    let deferred = classify_apply_time_data_source_read_inputs_from_refs(
        data_sources,
        managed,
        managed_current,
    );

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

fn data_source_refresh_bindings_from_refs(
    managed: &[&Resource],
    compositions: &[carina_core::resource::Composition],
    data_source_bindings: Phase2DataSourceBindings<'_>,
    managed_current: ManagedCurrentView<'_>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    schemas: &carina_core::schema::SchemaRegistry,
    wait_aliases: &[WaitAliasSpec],
) -> ResolvedBindings {
    let managed_current = carina_core::resource::plan_input_current_map_from_refs(
        managed_current.iter(),
        schemas,
        managed.iter().copied(),
    );
    match data_source_bindings {
        Phase2DataSourceBindings::Unread(data_sources) => {
            ResolvedBindings::pre_data_source_read_from_managed(
                managed.iter().copied(),
                compositions,
                data_sources,
                &managed_current,
                remote_bindings,
                wait_aliases,
            )
        }
        Phase2DataSourceBindings::Outcomes(values) => ResolvedBindings::pre_apply_from_managed(
            managed.iter().copied(),
            compositions,
            &managed_current,
            values,
            remote_bindings,
            wait_aliases,
        ),
    }
}

/// Classify data-source reads that must run after apply-time publication.
///
/// Test-facing adapter for the production phase-2 classifier. The returned map
/// is keyed by data-source id; an empty input-details vector is valid when
/// parsing folded the value but retained `dependency_bindings`.
#[cfg(test)]
pub(crate) fn classify_apply_time_data_source_read_inputs(
    data_sources: &[DataSource],
    managed: &[Resource],
    managed_current: &HashMap<ResourceId, Current>,
) -> HashMap<ResourceId, Vec<UnresolvedDataSourceInput>> {
    let managed = managed.iter().collect::<Vec<_>>();
    classify_apply_time_data_source_read_inputs_from_refs(
        data_sources,
        &managed,
        ManagedCurrentView::base(managed_current),
    )
}

fn classify_apply_time_data_source_read_inputs_from_refs(
    data_sources: &[DataSource],
    managed: &[&Resource],
    managed_current: ManagedCurrentView<'_>,
) -> HashMap<ResourceId, Vec<UnresolvedDataSourceInput>> {
    let mut deferred =
        classify_apply_time_data_source_reads(data_sources, managed, managed_current);
    propagate_deferred_data_source_chains(data_sources, &mut deferred);
    deferred
}

fn classify_apply_time_data_source_reads(
    data_sources: &[DataSource],
    managed: &[&Resource],
    managed_current: ManagedCurrentView<'_>,
) -> HashMap<ResourceId, Vec<UnresolvedDataSourceInput>> {
    let managed_by_binding: HashMap<&str, &Resource> = managed
        .iter()
        .filter_map(|resource| {
            resource
                .binding
                .as_deref()
                .map(|binding| (binding, *resource))
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
    managed_current: ManagedCurrentView<'_>,
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
    data_source_values: &DataSourceValues,
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: Option<&std::collections::HashSet<&str>>,
) -> Result<Vec<DataSource>, AppError> {
    let mut prepared = Vec::new();
    for (resource, value) in data_source_values.iter() {
        let mut resource = resource.clone();
        if matches!(value, DataSourceValue::Read(_)) {
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
    let ctx = WiringContext::new_for_test(vec![], &[]);
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
    let ctx = WiringContext::new_for_test(vec![], &[]);
    let mut parsed = carina_core::parser::ParsedFile {
        resources: resources.to_vec(),
        ..Default::default()
    };
    let errors = resolve_file_names_with_ctx(&ctx, &mut parsed);
    resources.clone_from_slice(&parsed.resources);
    errors_to_legacy_result(errors)
}

#[cfg(test)]
pub fn resolve_attr_prefixes(resources: &mut [Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new_for_test(vec![], &[]);
    errors_to_legacy_result(resolve_attr_prefixes_with_ctx(&ctx, resources))
}

#[cfg(test)]
pub fn compute_anonymous_identifiers(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
) -> Result<(), AppError> {
    let ctx = WiringContext::new_for_test(vec![], providers);
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
