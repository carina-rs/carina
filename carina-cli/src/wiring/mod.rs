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
//! than a context. `carina_core` returns only plain block-name-resolved files;
//! this module alone can mint their canonical pair, and no planning,
//! execution, or recording entry point accepts the plain values instead.
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
//!     let _ = contents.record(&providers, Vec::new());
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
//!     let _ = contents.record(&context, Vec::new());
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
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::cell::Cell;

use indexmap::IndexMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use colored::Colorize;
use futures::stream::{self, StreamExt};

use carina_core::binding_index::{LivePreApplyInputs, ResolvedBindings, WaitAliasSpec};
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
    AddressedMoves, DeferredReplaceDelete, DeferredReplacePayload, Effect, EffectGeneration,
    MoveAuthority, NonEmptyDeletes, OutcomeUnknownImportReplacement, ResolvedExplicitMove,
    ResolvedMove, ResolvedMovePairs,
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
use carina_core::name_override::{NameOverride, NameOverrideSource};
#[cfg(test)]
use carina_core::parser::{MovedAddresses, ParseWarning};
use carina_core::parser::{
    ProviderConfig, ResolvedProviderConfig, ResolvedProviderConfigs, StateBlock, StateBlockAddress,
    WarningKind,
};
use carina_core::plan::{Plan, PlanError, PlanErrorKind};
use carina_core::provider::{
    self as provider_mod, AppliedIdentifier, InitializedProvider, LiftedSavedAttrs,
    LoadedProviderSnapshot, MetadataComparisonResolver, NormalizerRouter, Provider, ProviderError,
    ProviderFactory, ProviderLoadedSchemaRegistry, ProviderNormalizer, ProviderRouter,
    ResourceComparisonResolver,
};
use carina_core::provider_source::ProviderSource;
#[cfg(test)]
use carina_core::resource::ResolvedDataSource;
use carina_core::resource::{
    ConcreteValue, Current, DataSource, DeferredValue, LiveComposition, ModuleSource,
    PartialReadMarker, PersistedDirectives, ResolvedDependencies, ResolvedResource,
    ResolvedResourceId, Resource, ResourceId, ResourceIdentity, ResourceIdentityError, State,
    Value,
};
#[cfg(any(test, feature = "test-utils"))]
use carina_core::schema::ValidatedResourceSchema;
#[cfg(test)]
use carina_core::schema::{AttributeSchema, AttributeType};
use carina_core::schema::{
    OwnerBoundResourceSchema, ResourceSchema, SchemaKind, SchemaRegistry, TypeError,
    ValidatedProviderSchemaWitness, resolve_file_block_names_with_schemas,
};
use carina_core::shutdown::{ShutdownPhase, ShutdownToken};
use carina_core::validation;
use carina_provider_mock::{BUILTIN_PROVIDER_NAME, MockProvider};
use carina_state::{LoadedOrphans, ResourceState, StateFile};

#[cfg(test)]
use crate::commands::shared::progress::RecordedProgressLines;
use crate::commands::shared::progress::{ProgressBackend, ProgressSink, RefreshPhase};
use crate::commands::split_validation_message;
use crate::error::AppError;
use crate::module_walk::ModuleWalk;

mod legacy_migration;
mod override_aware;

pub(crate) use legacy_migration::{
    MaterializedConfiguration, classify_legacy_state, replay_legacy_migration,
};

pub use override_aware::{
    CanonicalResourceIdentityError, CanonicalResourcePair, DeferredForExpansionChoice,
    OverrideAwareBuildMode, OverrideAwareResources, RebuildBindingsInputs,
    ResolvedResourceOriginIndexError,
};

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
    /// Exact classification of the plan-time v20 state. Saved plans retain it
    /// so apply can replay the same move- and scope-aware result.
    pub legacy_dependency_classification: Option<carina_state::LegacyDependencyClassification>,
    /// Graph-classified in-memory view of pre-v21 state. Read-only plan
    /// consumers use this for summaries and saved-plan capture, but never
    /// write it back without the lock-held apply/state-migration path.
    pub migrated_legacy_state: Option<StateFile>,
    /// Pre-value-resolution snapshots with the same resolved identities.
    pub unresolved_resources: Vec<UnresolvedResource>,
    /// Module compositions after plan-time constraint evaluation. Pending
    /// constraints must survive in saved plans for apply-time re-evaluation.
    pub compositions: Vec<LiveComposition>,
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
    pub residual_deferred_for: carina_core::parser::ResolvedDeferredForExpressions,
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
    canonical_resources: PostExpansionConfiguration<E>,
}

impl<E> ExpandedPlanContext<E> {
    fn new(context: PlanContext, canonical_resources: PostExpansionConfiguration<E>) -> Self {
        Self {
            context,
            canonical_resources,
        }
    }

    pub(crate) fn parsed(&self) -> &carina_core::parser::ModuleResolvedFile<E> {
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
    loaded_providers: Arc<LoadedProviderSnapshot>,
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

/// Schema-only provider load used exclusively by `state migrate --check`.
///
/// This type has no transition into [`ValidatedConfig`], so a metadata-only
/// factory can never reach planning or execution. Its sole output is the
/// expanded, provider-I/O-free view consumed by legacy dependency
/// classification.
pub(crate) struct MigrationCheckProviderLoad {
    context: WiringContext,
    load_errors: HashMap<String, String>,
}

pub(crate) struct MigrationCheckConfig {
    context: WiringContext,
    resources: carina_core::parser::ModuleResolvedInferredFile,
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
    canonical_resources:
        Box<ValidationReady<CanonicalResourcePair<carina_core::parser::InferredExportParam>>>,
}

/// Schema and custom-type provider state used by collect-all validation and
/// lint.
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

/// Marker for a configuration that may still contain reusable module-template
/// nodes. Validation reports and editor/lint paths are allowed to operate in
/// this state, but planning and serialization entry points are not.
#[derive(Debug)]
pub struct TemplateConfig;

/// Marker for a configuration that passed the exhaustive root-graph audit.
///
/// The marker is carried by [`ValidatedConfig`] and its value is never exposed;
/// only this module can construct the owner that planning and apply accept.
#[derive(Debug)]
pub struct ExpandedConfig;

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
        parsed: carina_core::parser::ModuleResolvedFile<E>,
        unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
    ) -> Result<CanonicalResourcePair<E>, Vec<AppError>> {
        resolve_config_names_with_ctx(self, ctx, parsed, unresolved)
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

#[derive(Debug)]
enum ValidationReadyOrigin {
    Parsed,
    SavedPlanArtifact,
    #[cfg(any(test, feature = "test-utils"))]
    Fixture,
}

/// Sealed payload accepted by [`ValidatedConfig`].
///
/// Production parsed configurations can enter this wrapper only with the
/// resolved file returned by the core bare-binding authority. Saved plans use
/// a separate private constructor because serialized artifacts contain no
/// parser-authored side data.
pub struct ValidationReady<R> {
    inner: R,
    origin: ValidationReadyOrigin,
}

impl ValidationReady<()> {
    fn from_saved_plan_artifact() -> Self {
        Self {
            inner: (),
            origin: ValidationReadyOrigin::SavedPlanArtifact,
        }
    }
}

impl<R> ValidationReady<R> {
    #[cfg(any(test, feature = "test-utils"))]
    fn for_fixture(inner: R) -> Self {
        Self {
            inner,
            origin: ValidationReadyOrigin::Fixture,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn into_inner(self) -> R {
        self.inner
    }

    fn try_map_with_output_preserving_binding_use_resolution<T, O, E>(
        self,
        transform: impl FnOnce(R) -> Result<(T, O), E>,
    ) -> Result<(ValidationReady<T>, O), E> {
        let (inner, output) = transform(self.inner)?;
        Ok((
            ValidationReady {
                inner,
                origin: self.origin,
            },
            output,
        ))
    }

    async fn try_map_async_preserving_binding_use_resolution<T, E, F, Fut>(
        self,
        transform: F,
    ) -> Result<ValidationReady<T>, E>
    where
        F: FnOnce(R) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        let inner = transform(self.inner).await?;
        Ok(ValidationReady {
            inner,
            origin: self.origin,
        })
    }
}

impl<E> ValidationReady<carina_core::parser::ModuleResolvedFile<E>> {
    fn from_resolved_file(file: carina_core::validation::ResolvedFile<E>) -> Self {
        Self {
            inner: file.into_file(),
            origin: ValidationReadyOrigin::Parsed,
        }
    }
}

impl<E> ValidationReady<CanonicalResourcePair<E>> {
    fn from_resolved_pair(
        parsed: carina_core::validation::ResolvedFile<E>,
        unresolved: Vec<Resource>,
    ) -> Self {
        Self {
            inner: CanonicalResourcePair::from_resolved_file(parsed, unresolved),
            origin: ValidationReadyOrigin::Parsed,
        }
    }
}

impl<R> std::ops::Deref for ValidationReady<R> {
    type Target = R;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Provider wiring and the exact configuration state validated against it.
///
/// Callers outside validation cannot construct this owner by pairing arbitrary
/// resources with unrelated wiring:
///
/// ```compile_fail,E0451
/// use carina_cli::wiring::{CanonicalResourcePair, ValidatedConfig, ValidatedWiring};
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
/// use carina_cli::wiring::CanonicalResourcePair;
/// use carina_core::parser::InferredExportParam;
///
/// fn record_unbound(
///     contents: PlanFileContents,
///     resources: CanonicalResourcePair<InferredExportParam>,
/// ) {
///     let _ = contents.record(&resources, Vec::new());
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
///
/// A raw parsed file is not the payload stored by this owner. The binding-use
/// authority must first consume every parser-side pending slot and mint the
/// sealed validation-ready witness:
///
/// ```compile_fail,E0308
/// use carina_cli::wiring::{ValidatedConfig, ValidatedWiring};
/// use carina_core::parser::InferredFile;
///
/// fn bypass_binding_use_validation(wiring: ValidatedWiring, raw: InferredFile) {
///     let _ = ValidatedConfig::<InferredFile> {
///         wiring,
///         resources: raw,
///     };
/// }
/// ```
///
/// A template-tolerant validated owner is not accepted by a planning boundary
/// that requires the default [`ExpandedConfig`] state:
///
/// ```compile_fail,E0308
/// use carina_cli::wiring::{TemplateConfig, ValidatedConfig};
/// use carina_core::parser::InferredFile;
///
/// fn planning_boundary(_: ValidatedConfig<InferredFile>) {}
///
/// fn cannot_plan_template(template: ValidatedConfig<InferredFile, TemplateConfig>) {
///     planning_boundary(template);
/// }
/// ```
pub struct ValidatedConfig<
    R = CanonicalResourcePair<carina_core::parser::InferredExportParam>,
    C = ExpandedConfig,
> {
    wiring: ValidatedWiring,
    resources: ValidationReady<R>,
    config_state: std::marker::PhantomData<C>,
}

impl<R: fmt::Debug, C> fmt::Debug for ValidatedConfig<R, C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedConfig")
            .field("resources", &self.resources.inner)
            .finish()
    }
}

/// One or more validation diagnostics.
///
/// The inner vector is private and every constructor establishes the
/// non-empty invariant, so a failed validation report cannot carry an empty
/// diagnostic list.
#[derive(Debug)]
pub(crate) struct NonEmptyValidationErrors(Vec<AppError>);

#[derive(Debug)]
pub(crate) struct EmptyValidationErrors;

impl TryFrom<Vec<AppError>> for NonEmptyValidationErrors {
    type Error = EmptyValidationErrors;

    fn try_from(errors: Vec<AppError>) -> Result<Self, Self::Error> {
        if errors.is_empty() {
            Err(EmptyValidationErrors)
        } else {
            Ok(Self(errors))
        }
    }
}

impl NonEmptyValidationErrors {
    pub(crate) fn as_slice(&self) -> &[AppError] {
        &self.0
    }

    pub(crate) fn into_vec(self) -> Vec<AppError> {
        self.0
    }

    fn extend(&mut self, errors: impl IntoIterator<Item = AppError>) {
        self.0.extend(errors);
    }
}

/// Validation outcome plus the read-only schemas used by warning-only checks.
pub(crate) struct ValidationReport<'a> {
    result: Result<carina_core::parser::ModuleResolvedInferredFile, NonEmptyValidationErrors>,
    schemas: &'a SchemaRegistry,
    config_state: std::marker::PhantomData<TemplateConfig>,
}

impl<'a> ValidationReport<'a> {
    #[cfg(test)]
    pub(crate) fn errors(&self) -> &[AppError] {
        match &self.result {
            Ok(_) => &[],
            Err(errors) => errors.as_slice(),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn into_errors(self) -> Vec<AppError> {
        match self.result {
            Ok(_) => Vec::new(),
            Err(errors) => errors.into_vec(),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn into_result(
        self,
    ) -> Result<carina_core::parser::ModuleResolvedInferredFile, NonEmptyValidationErrors> {
        self.result
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Result<carina_core::parser::ModuleResolvedInferredFile, NonEmptyValidationErrors>,
        &'a SchemaRegistry,
    ) {
        (self.result, self.schemas)
    }

    #[cfg(test)]
    pub(crate) fn schemas(&self) -> &SchemaRegistry {
        self.schemas
    }
}

#[allow(clippy::too_many_arguments)]
fn validation_report_from<'a>(
    context: &'a WiringContext,
    load_errors: &HashMap<String, String>,
    parsed: &carina_core::parser::InferredFile,
    base_dir: &Path,
    skip_resource_validation: bool,
    duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
) -> ValidationReport<'a> {
    let result = match crate::commands::validate_and_resolve_errors_inner(
        &ValidationPipelineAccess::new(),
        parsed.clone(),
        base_dir,
        context,
        load_errors,
        skip_resource_validation,
        duplicate_declarations,
        crate::commands::ConfigNameResolution::EvaluatedOnly,
    ) {
        Ok(crate::commands::ResolvedConfig::Evaluated { parsed }) => Ok(*parsed),
        Ok(crate::commands::ResolvedConfig::Paired { .. }) => {
            unreachable!("evaluated-only validation returned pair")
        }
        Err(errors) => Err(errors),
    };
    ValidationReport {
        result,
        schemas: context.schemas(),
        config_state: std::marker::PhantomData,
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
    parsed: carina_core::parser::InferredFile,
    unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
    base_dir: &Path,
    skip_resource_validation: bool,
    duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
) -> Result<CanonicalResourcePair<carina_core::parser::InferredExportParam>, NonEmptyValidationErrors>
{
    match crate::commands::validate_and_resolve_errors_inner(
        &ValidationPipelineAccess::new(),
        parsed,
        base_dir,
        context,
        load_errors,
        skip_resource_validation,
        duplicate_declarations,
        crate::commands::ConfigNameResolution::Paired(Box::new(unresolved)),
    )? {
        crate::commands::ResolvedConfig::Paired { resources } => Ok(*resources),
        crate::commands::ResolvedConfig::Evaluated { .. } => {
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
    duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
) -> Result<CanonicalResourcePair<carina_core::parser::InferredExportParam>, AppError> {
    resolve_config_pair_errors_with_context(
        context,
        load_errors,
        parsed,
        unresolved,
        base_dir,
        skip_resource_validation,
        duplicate_declarations,
    )
    .map_err(|errors| crate::commands::collapse_errors(errors.into_vec()))
}

fn binding_use_report_error(report: carina_core::validation::BindingUseReport) -> AppError {
    AppError::BindingUseReport(Box::new(report))
}

fn require_expanded_config<E, D>(
    file: &carina_core::parser::File<E, D>,
    base_dir: &Path,
) -> Result<(), AppError>
where
    E: carina_core::parser::ExportParamLike,
    D: carina_core::parser::DeferredForPhaseTraversal,
{
    module_resolver::ensure_expanded_root_graph(file)
        .map_err(|error| AppError::Config(error.display_relative_to(base_dir)))
}

fn resolve_file_binding_uses<E>(
    file: carina_core::parser::ModuleResolvedFile<E>,
    base_dir: &Path,
    context: &WiringContext,
) -> Result<carina_core::validation::ResolvedFile<E>, AppError> {
    let provider_context = context.provider_context();
    let module_signatures =
        module_resolver::load_resolved_module_signatures(&file, base_dir, provider_context);
    carina_core::validation::resolve_bare_binding_uses(
        file,
        context.schemas(),
        provider_context,
        &module_signatures,
    )
    .map_err(binding_use_report_error)
}

fn resolve_pair_binding_uses<E>(
    pair: CanonicalResourcePair<E>,
    base_dir: &Path,
    context: &WiringContext,
) -> Result<(carina_core::validation::ResolvedFile<E>, Vec<Resource>), AppError> {
    let (parsed, unresolved) = pair.into_parts();
    let parsed = resolve_file_binding_uses(parsed, base_dir, context)?;
    Ok((parsed, unresolved))
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
        parsed: &carina_core::parser::InferredFile,
        base_dir: &Path,
        skip_resource_validation: bool,
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> ValidationReport<'a> {
        validation_report_from(
            &self.context,
            &self.load_errors,
            parsed,
            base_dir,
            skip_resource_validation,
            duplicate_declarations,
        )
    }

    /// Validate an evaluated config and consume this exact load and file into
    /// one executable owner.
    pub fn validate_evaluated_config(
        self,
        parsed: carina_core::parser::InferredFile,
        base_dir: &Path,
        skip_resource_validation: bool,
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> Result<ValidatedConfig<carina_core::parser::ModuleResolvedInferredFile>, AppError> {
        let resolved = crate::commands::validate_and_resolve_errors_inner(
            &ValidationPipelineAccess::new(),
            parsed,
            base_dir,
            &self.context,
            &self.load_errors,
            skip_resource_validation,
            duplicate_declarations,
            crate::commands::ConfigNameResolution::EvaluatedOnly,
        );
        let parsed = match resolved {
            Ok(crate::commands::ResolvedConfig::Evaluated { parsed }) => *parsed,
            Ok(crate::commands::ResolvedConfig::Paired { .. }) => {
                unreachable!("evaluated-only validation returned pair")
            }
            Err(mut errors) => {
                if skip_resource_validation {
                    errors.extend(sorted_provider_load_errors(&self.load_errors));
                }
                return Err(crate::commands::collapse_errors(errors.into_vec()));
            }
        };
        // The resource-skipping pipeline omits provider-load diagnostics.
        if skip_resource_validation {
            let errors = sorted_provider_load_errors(&self.load_errors);
            if !errors.is_empty() {
                return Err(crate::commands::collapse_errors(errors));
            }
        }
        let parsed = resolve_file_binding_uses(parsed, base_dir, &self.context)?;
        require_expanded_config(parsed.as_file(), base_dir)?;

        Ok(ValidatedConfig {
            wiring: ValidatedWiring {
                load: self.into_complete()?,
            },
            resources: ValidationReady::from_resolved_file(parsed),
            config_state: std::marker::PhantomData,
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
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> Result<ValidatedConfig, AppError> {
        let canonical_resources = match resolve_config_pair_errors_with_context(
            &self.context,
            &self.load_errors,
            parsed,
            unresolved,
            base_dir,
            skip_resource_validation,
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
                return Err(crate::commands::collapse_errors(errors.into_vec()));
            }
        };
        let (parsed, unresolved) =
            resolve_pair_binding_uses(canonical_resources, base_dir, &self.context)?;
        require_expanded_config(parsed.as_file(), base_dir)?;
        Ok(ValidatedConfig {
            wiring: ValidatedWiring {
                load: self.into_complete()?,
            },
            resources: ValidationReady::from_resolved_pair(parsed, unresolved),
            config_state: std::marker::PhantomData,
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

impl MigrationCheckProviderLoad {
    pub(crate) fn load(
        user_dirs: &carina_core::UserDirs,
        providers: &[ProviderConfig],
        base_dir: &Path,
        schema_overrides: &HashMap<String, PathBuf>,
    ) -> Result<Self, AppError> {
        let effective = schema_check_provider_configs(providers, schema_overrides)?;
        let (factories, load_errors) =
            build_schema_only_factories_from_providers(user_dirs, &effective, base_dir)?;
        Ok(Self {
            context: WiringContext::new(factories, &effective),
            load_errors,
        })
    }

    pub(crate) fn validate_evaluated_config(
        self,
        parsed: carina_core::parser::InferredFile,
        base_dir: &Path,
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> Result<MigrationCheckConfig, AppError> {
        let Self {
            context,
            load_errors,
        } = self;
        let load_failures = sorted_provider_load_errors(&load_errors);
        if !load_failures.is_empty() {
            return Err(crate::commands::collapse_errors(load_failures));
        }
        let parsed = match crate::commands::validate_and_resolve_errors_inner(
            &ValidationPipelineAccess::new(),
            parsed,
            base_dir,
            &context,
            &HashMap::new(),
            true,
            duplicate_declarations,
            crate::commands::ConfigNameResolution::EvaluatedOnly,
        ) {
            Ok(crate::commands::ResolvedConfig::Evaluated { parsed }) => *parsed,
            Ok(crate::commands::ResolvedConfig::Paired { .. }) => {
                unreachable!("evaluated-only migration validation returned pair")
            }
            Err(errors) => {
                return Err(crate::commands::collapse_errors(errors.into_vec()));
            }
        };
        let parsed = resolve_file_binding_uses(parsed, base_dir, &context)?.into_file();
        Ok(MigrationCheckConfig {
            context,
            resources: parsed,
        })
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
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> Result<CheckedSavedPlanInputs, AppError> {
        let canonical_resources = resolve_config_pair_with_context(
            &self.context,
            &HashMap::new(),
            parsed,
            unresolved,
            base_dir,
            skip_resource_validation,
            duplicate_declarations,
        )?;
        let (parsed, unresolved) =
            resolve_pair_binding_uses(canonical_resources, base_dir, &self.context)?;
        require_expanded_config(parsed.as_file(), base_dir)?;
        Ok(CheckedSavedPlanInputs {
            load: self,
            canonical_resources: Box::new(ValidationReady::from_resolved_pair(parsed, unresolved)),
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
                resources: ValidationReady::from_saved_plan_artifact(),
                config_state: std::marker::PhantomData,
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
                config_state: std::marker::PhantomData,
            },
        ))
    }
}

impl ReportingProviderLoad {
    pub(crate) fn validation_report<'a>(
        &'a self,
        parsed: &carina_core::parser::InferredFile,
        base_dir: &Path,
        skip_resource_validation: bool,
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> ValidationReport<'a> {
        validation_report_from(
            &self.context,
            &self.load_errors,
            parsed,
            base_dir,
            skip_resource_validation,
            duplicate_declarations,
        )
    }

    pub(crate) fn schemas(&self) -> &SchemaRegistry {
        self.context.schemas()
    }

    pub(crate) fn loaded_provider_context(
        &self,
    ) -> Option<&carina_core::parser::LoadedProviderContext> {
        if self.load_errors.is_empty() {
            Some(self.context.provider_context())
        } else {
            None
        }
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
        &self.resources.inner
    }

    /// Borrow the context and its validated resources together.
    ///
    /// The two borrows originate from one owner, so callers cannot retain the
    /// resources while substituting a context from another provider load.
    pub(crate) fn context_and_resources(&self) -> (&WiringContext, &R) {
        (self.wiring.context(), &self.resources.inner)
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(
        resources: R,
        factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
    ) -> Self {
        Self {
            wiring: ValidatedWiring::for_test(factories, provider_configs),
            resources: ValidationReady::for_fixture(resources),
            config_state: std::marker::PhantomData,
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
        Self {
            wiring,
            resources: ValidationReady::for_fixture(resources),
            config_state: std::marker::PhantomData,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_wiring_ref_for_test(wiring: &ValidatedWiring, resources: R) -> Self {
        Self {
            wiring: ValidatedWiring {
                load: CompleteProviderLoad {
                    context: WiringContext {
                        loaded_providers: Arc::clone(&wiring.load.context.loaded_providers),
                        known_providers: wiring.load.context.known_providers.clone(),
                        uses_builtin_mock: wiring.load.context.uses_builtin_mock,
                        provider_artifacts: wiring.load.context.provider_artifacts.clone(),
                    },
                },
            },
            resources: ValidationReady::for_fixture(resources),
            config_state: std::marker::PhantomData,
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
            resources: ValidationReady::for_fixture(resources),
            config_state: std::marker::PhantomData,
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
            resources: ValidationReady::for_fixture(resources),
            config_state: std::marker::PhantomData,
        }
    }

    /// Fixture-only bridge for the intentionally separate validation and
    /// rendering contexts documented in `fixture_plan`.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn into_resources_for_fixture(self) -> R {
        self.resources.into_inner()
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
        duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
    ) -> Result<ValidatedConfig, AppError> {
        let Self {
            wiring,
            resources: _,
            config_state: _,
        } = self;
        let resources = resolve_config_pair_with_context(
            wiring.context(),
            &HashMap::new(),
            parsed,
            unresolved,
            base_dir,
            skip_resource_validation,
            duplicate_declarations,
        )?;
        let (parsed, unresolved) =
            resolve_pair_binding_uses(resources, base_dir, wiring.context())?;
        require_expanded_config(parsed.as_file(), base_dir)?;
        Ok(ValidatedConfig {
            wiring,
            resources: ValidationReady::from_resolved_pair(parsed, unresolved),
            config_state: std::marker::PhantomData,
        })
    }
}

impl<E> ValidatedConfig<CanonicalResourcePair<E>> {
    /// Apply the state-aware identity reconciliation sequence to this owner's
    /// canonical pair without exposing a mutable pair to the caller.
    pub(crate) fn reconcile_identities_with_state(
        &mut self,
        state_file: &Option<StateFile>,
    ) -> StateBlockResolution {
        let Self {
            wiring, resources, ..
        } = self;
        let ctx = wiring.context();
        reconcile_canonical_prefixed_names(&mut resources.inner, state_file);
        let parsed = resources.parsed();
        let state_blocks = resolve_state_blocks(
            &parsed.state_blocks,
            state_file,
            &parsed.resources,
            ctx.schemas(),
        );
        if let Some(state) = state_file.as_ref() {
            resources.inner.reconcile_anonymous_module_instances(
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
        state_blocks
    }

    /// Fixture-only identity preparation that still mutates through the owner.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn compute_anonymous_identifiers_for_fixture(
        &mut self,
        providers: &[ProviderConfig],
    ) -> Result<Vec<AppError>, AppError> {
        let Self {
            wiring, resources, ..
        } = self;
        compute_canonical_pair_anonymous_identifiers_with_ctx(
            wiring.context(),
            &mut resources.inner,
            providers,
        )
    }
}

impl ValidatedConfig<carina_core::parser::ModuleResolvedInferredFile> {
    /// Integration-test read view of the evaluated configuration. Mutation
    /// remains confined to the owner's concrete transition methods.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn evaluated_resources_for_test(&self) -> &carina_core::parser::ModuleResolvedInferredFile {
        &self.resources.inner
    }

    /// Reconcile the evaluated destroy view against state without releasing a
    /// mutable file from the owner.
    pub(crate) fn reconcile_for_destroy(
        &mut self,
        state_file: &Option<StateFile>,
    ) -> StateBlockClaims {
        self.reconcile_evaluated_identities(state_file, true)
    }

    /// Reconcile the evaluated state-refresh view against state without
    /// applying module-instance reconciliation, preserving its existing
    /// command semantics.
    pub(crate) fn reconcile_for_state_refresh(&mut self, state_file: &Option<StateFile>) {
        self.reconcile_evaluated_identities(state_file, false);
    }

    fn reconcile_evaluated_identities(
        &mut self,
        state_file: &Option<StateFile>,
        reconcile_modules: bool,
    ) -> StateBlockClaims {
        let Self {
            wiring,
            resources,
            config_state: _config_state,
        } = self;
        reconcile_evaluated_identities_with_ctx(
            wiring.context(),
            &mut resources.inner,
            state_file,
            reconcile_modules,
        )
    }
}

fn reconcile_evaluated_identities_with_ctx(
    ctx: &WiringContext,
    resources: &mut carina_core::parser::ModuleResolvedInferredFile,
    state_file: &Option<StateFile>,
    reconcile_modules: bool,
) -> StateBlockClaims {
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
    crate::commands::shared::state_writeback::apply_name_overrides(
        &mut resources.resources,
        state_file,
    );
    state_block_claims
}

impl MigrationCheckConfig {
    /// Reconcile and expand the configuration using only values already
    /// present in the copied state being checked.
    ///
    /// This deliberately does not share the plan/apply deferred-expansion
    /// transition: that transition owns refreshed provider results and needs
    /// a runtime provider for stability classification. A migration check is
    /// offline and read-only, so it projects persisted managed and historical
    /// data-source rows into the same typed [`ResolvedBindings`] view without
    /// ever constructing a provider router.
    fn prepare_from_copied_state(
        &mut self,
        state_file: &Option<StateFile>,
    ) -> Result<StateBlockClaims, AppError> {
        let state_block_claims = reconcile_evaluated_identities_with_ctx(
            &self.context,
            &mut self.resources,
            state_file,
            true,
        );
        self.canonicalize_and_compute_identifiers()?;

        let Some(state_file) = state_file.as_ref() else {
            return Ok(state_block_claims);
        };

        // Expansion can expose a binding consumed by another deferred loop.
        // Iterate to a fixed point; an unavailable upstream declaration stays
        // deferred because no remote state is read on this command path.
        loop {
            let before = self.resources.deferred_for_expressions().len();
            if before == 0 {
                break;
            }

            let managed_current = self
                .resources
                .resources
                .iter()
                .map(|resource| {
                    (
                        resource.id.clone(),
                        state_file.build_state_for_resource(&resource.id),
                    )
                })
                .collect::<HashMap<_, _>>();
            let managed_current = carina_core::resource::into_plan_input_current_map(
                managed_current,
                self.context.schemas(),
                &self.resources.resources,
            );
            let data_source_values =
                DataSourceValues::from_data_sources(&self.resources.data_sources, |data_source| {
                    match state_file.build_state_for_resource(&data_source.id) {
                        Current::Present(present) => DataSourceValue::read(present.state().clone()),
                        Current::MissingIdentifier(state) => {
                            DataSourceValue::read(state.state().clone())
                        }
                        Current::Absent(_)
                        | Current::CreateInFlight(_)
                        | Current::CreateOutcomeUnknown(_) => {
                            DataSourceValue::deferred_to_apply(Vec::new())
                        }
                    }
                });
            let wait_aliases = self
                .resources
                .wait_bindings
                .iter()
                .map(WaitAliasSpec::from)
                .collect::<Vec<_>>();
            let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
                managed: &self.resources.resources,
                compositions: &self.resources.compositions,
                managed_current: &managed_current,
                data_source_values: &data_source_values,
                remote_bindings: &HashMap::new(),
                wait_aliases: &wait_aliases,
            });
            self.resources
                .expand_deferred_for_expressions(&bindings.project_iterable_bindings())
                .map_err(|error| AppError::Validation(error.to_string()))?;

            let after = self.resources.deferred_for_expressions().len();
            if after == before {
                break;
            }
            self.canonicalize_and_compute_identifiers()?;
        }
        Ok(state_block_claims)
    }

    /// Prepare and classify one copied legacy state through the same typed
    /// graph/configuration/move input consumed by the plan path.
    pub(crate) fn migrate_legacy_dependencies_from_copied_state(
        mut self,
        mut state: StateFile,
    ) -> Result<
        legacy_migration::ClassifiedLegacyGraph<carina_core::parser::InferredExportParam>,
        AppError,
    > {
        let state_view = Some(state.clone());
        self.prepare_from_copied_state(&state_view)?;
        state.remove_legacy_data_source_rows(&self.resources.data_sources);

        let moved_pairs = resolve_moved_pairs(
            &self.context,
            &self.resources,
            &self.resources.resources,
            &self.resources.data_sources,
            &state_view,
        )?;
        classify_legacy_state(
            self.resources,
            MaterializedConfiguration::Offline,
            &moved_pairs,
            Some(&state),
        )
    }

    fn canonicalize_and_compute_identifiers(&mut self) -> Result<(), AppError> {
        let providers = self.resources.providers.clone();
        resolve_late_anonymous_resource_ids(
            &self.context,
            &mut self.resources.resources,
            &providers,
        )
    }
}

pub(crate) struct ValidatedLegacyConfiguration<E> {
    pub config: ValidatedConfig<
        carina_core::resolved_graph::ResolvedExpandedGraph<
            E,
            carina_core::parser::ResolvedDeferredForPhase,
        >,
    >,
    pub migrated_state: Option<StateFile>,
}

impl<E> ValidatedConfig<carina_core::parser::ModuleResolvedFile<E>> {
    /// Consume the configuration exactly once, preserving its wiring owner
    /// through classification, warning reporting, and strict graph validation.
    pub(crate) fn resolve_legacy_graph(
        self,
        mode: MaterializedConfiguration,
        moves: &ResolvedMovePairs,
        state: Option<&StateFile>,
    ) -> Result<ValidatedLegacyConfiguration<E>, AppError> {
        let Self {
            wiring,
            resources: validated_resources,
            config_state: _,
        } = self;
        let ValidationReady {
            inner: parsed,
            origin,
        } = validated_resources;
        let validated = classify_legacy_state(parsed, mode, moves, state)?.report_and_validate()?;
        Ok(ValidatedLegacyConfiguration {
            config: ValidatedConfig {
                wiring,
                resources: ValidationReady {
                    inner: validated.graph,
                    origin,
                },
                config_state: std::marker::PhantomData,
            },
            migrated_state: validated.migrated_state,
        })
    }
}

impl<E: Clone> ValidatedConfig<carina_core::parser::ModuleResolvedFile<E>> {
    /// Materialize the migration configuration using the same phase-2 reads,
    /// deferred-loop expansion, and child settlement as live planning. Destroy
    /// and state refresh both preserve persisted addresses through this owner.
    pub(crate) async fn materialize_legacy_configuration(
        &mut self,
        inputs: LegacyConfigurationReadInputs<'_, '_, '_>,
    ) -> Result<MaterializedLegacyConfiguration, AppError> {
        let LegacyConfigurationReadInputs {
            prelude,
            mut state,
            provider,
            state_file,
            refresh_phase,
            remote_bindings,
            cancel,
        } = inputs;
        let (ctx, parsed) = self.context_and_resources();
        hydrate_prior_managed_states(
            &mut state.managed_current,
            &mut state.orphans,
            provider,
            prelude.saved_attrs(),
            state_file.as_ref(),
        )
        .await?;
        let saved_dep_bindings = state_file
            .as_ref()
            .map(|file| {
                prelude
                    .sorted_resources()
                    .iter()
                    .filter_map(|resource| {
                        let row = file.find_resource_for_id(&resource.id)?;
                        Some((
                            resource.id.clone(),
                            row.dependency_bindings.to_resolved_dependencies(),
                        ))
                    })
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let addressed = prelude.preserve_addresses(state);
        let values = addressed
            .read_data_sources_phase2(AddressedDataSourceReadInputs {
                provider,
                refresh_phase: Some(refresh_phase),
                compositions: &parsed.compositions,
                data_sources: &parsed.data_sources,
                remote_bindings,
                schemas: ctx.schemas(),
                saved_dep_bindings: &saved_dep_bindings,
                factories: ctx.factories(),
                cancel: Some(cancel),
            })
            .await?
            .into_values()?;
        if cancel.phase() != carina_core::shutdown::ShutdownPhase::Running {
            return Err(AppError::Interrupted);
        }
        let mut expansion = addressed.seal(values);
        let ExpandedRefreshState { sorted_resources } = self
            .expand_refresh_and_lift_states(ValidatedExpandRefreshAndLiftInputs {
                provider,
                comparison_provider: provider,
                expansion: &mut expansion,
                warning_policy: DeferredForWarningPolicy::Print,
                remote_bindings,
                state_file,
                saved_dep_bindings: &saved_dep_bindings,
                refresh_phase,
            })
            .await?;
        let mut parts = expansion.into_parts();
        let (ctx, parsed) = self.context_and_resources();
        parts.data_source_values = read_expanded_data_sources_phase2(
            parts.data_source_values,
            ExpandedDataSourceReadInputs {
                provider,
                refresh_phase: Some(refresh_phase),
                managed: &sorted_resources,
                compositions: &parsed.compositions,
                data_sources: &parsed.data_sources,
                managed_current: &parts.addressed.managed_current,
                orphans: &parts.addressed.orphans,
                remote_bindings,
                schemas: ctx.schemas(),
                wait_aliases: &parts.wait_aliases,
                saved_dep_bindings: &saved_dep_bindings,
                factories: ctx.factories(),
                cancel: Some(cancel),
            },
        )
        .await?
        .into_values()?;
        if cancel.phase() != carina_core::shutdown::ShutdownPhase::Running {
            return Err(AppError::Interrupted);
        }
        Ok(MaterializedLegacyConfiguration {
            sorted_resources,
            parts,
        })
    }

    /// Run the address-preserving state-refresh expansion through this owner,
    /// keeping the mutated file bound to its validating wiring.
    pub(crate) async fn expand_refresh_and_lift_states<P: Provider + ProviderNormalizer>(
        &mut self,
        inputs: ValidatedExpandRefreshAndLiftInputs<
            '_,
            '_,
            P,
            PreparedAddressPreservingDeferredForExpansion,
        >,
    ) -> Result<ExpandedRefreshState, AppError> {
        let Self {
            wiring, resources, ..
        } = self;
        let ValidatedExpandRefreshAndLiftInputs {
            provider,
            comparison_provider,
            expansion,
            warning_policy,
            remote_bindings,
            state_file,
            saved_dep_bindings,
            refresh_phase,
        } = inputs;
        expand_refresh_and_lift_states(
            &mut resources.inner,
            ExpandRefreshAndLiftInputs {
                provider,
                comparison_provider,
                expansion,
                warning_policy,
                remote_bindings,
                state_file,
                saved_dep_bindings,
                refresh_phase,
                schemas: wiring.context().schemas(),
                identity_context: wiring.context(),
            },
        )
        .await
    }
}

pub(crate) struct LegacyConfigurationReadInputs<'a, 'backend, 'state> {
    pub prelude: DeferredForExpansionPrelude<'state>,
    pub state: PreserveDeferredForExpansionInputs,
    pub provider: &'a ProviderRouter,
    pub state_file: &'a Option<StateFile>,
    pub refresh_phase: &'a RefreshPhase<'backend>,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub cancel: &'a ShutdownToken,
}

pub(crate) struct MaterializedLegacyConfiguration {
    pub sorted_resources: Vec<Resource>,
    pub parts: PreparedDeferredForExpansionParts,
}

pub(crate) fn resources_at_destroy_addresses(
    resources: Vec<Resource>,
    moved_pairs: &ResolvedMovePairs,
) -> Vec<Resource> {
    resources
        .into_iter()
        .map(|mut resource| {
            if let Some((source, _)) = moved_pairs
                .iter()
                .find(|(_, target)| target.as_inner() == &resource.id)
            {
                resource.id = source.as_inner().clone();
            }
            resource
        })
        .collect()
}

impl<E> ValidatedConfig<ExpandedCanonicalResources<E>> {
    /// Normalize the expanded desired side using only the schemas retained by
    /// this owner.
    pub(crate) async fn prepare_desired_side_for_plan(
        &mut self,
        provider_configs: &[ProviderConfig],
        normalizer: &dyn ProviderNormalizer,
    ) -> carina_core::provider::ProviderResult<()> {
        let Self {
            wiring, resources, ..
        } = self;
        resources
            .inner
            .prepare_desired_side_for_plan(provider_configs, normalizer, wiring.context().schemas())
            .await
    }
}

#[cfg(test)]
impl<E> ValidatedConfig<ExpandedPlanContext<E>> {
    pub(crate) fn into_context_for_test(self) -> PlanContext {
        self.resources.into_inner().into_context()
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
        provider: &ProviderRouter,
    ) -> Result<
        (
            ValidatedConfig<ExpandedCanonicalResources<E>>,
            DeferredForExpansion,
        ),
        AppError,
    > {
        let Self {
            wiring, resources, ..
        } = self;
        let (resources, expansion) = resources
            .try_map_with_output_preserving_binding_use_resolution(|resources| {
                prepared.expand(
                    resources,
                    wiring.context(),
                    remote_bindings,
                    warning_policy,
                    provider,
                )
            })?;
        Ok((
            ValidatedConfig {
                wiring,
                resources,
                config_state: std::marker::PhantomData,
            },
            expansion,
        ))
    }

    /// Fixture-plan counterpart of [`Self::expand_deferred_for`] that consumes
    /// the prepared state into the inseparable post-expansion plan view.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn expand_deferred_for_plan(
        self,
        prepared: PreparedDeferredForExpansion,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        warning_policy: DeferredForWarningPolicy,
        provider: &ProviderRouter,
    ) -> Result<ValidatedConfig<PostExpansionPlanView<E, ChildrenPending>>, AppError> {
        let Self {
            wiring, resources, ..
        } = self;
        let (resources, ()) =
            resources.try_map_with_output_preserving_binding_use_resolution(|resources| {
                prepared
                    .expand_for_plan(
                        resources,
                        wiring.context(),
                        remote_bindings,
                        warning_policy,
                        provider,
                    )
                    .map(|expanded| (expanded, ()))
            })?;
        Ok(ValidatedConfig {
            wiring,
            resources,
            config_state: std::marker::PhantomData,
        })
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
        let context = WiringContext::new(factories, providers);
        Ok(Self {
            schemas: context.schemas().clone(),
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
        let context = WiringContext::new(factories, provider_configs);
        Self {
            schemas: context.schemas().clone(),
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
        Self::new_with_schema_registry(factories, provider_configs, SchemaRegistry::new())
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn new_with_additional_schemas(
        loaded_factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
        additional_schemas: impl IntoIterator<Item = (String, ValidatedResourceSchema)>,
    ) -> Self {
        let mut schemas = SchemaRegistry::new();
        for (provider, schema) in additional_schemas {
            schemas.insert(provider, schema);
        }
        Self::new_with_schema_registry(loaded_factories, provider_configs, schemas)
    }

    fn new_with_schema_registry(
        loaded_factories: Vec<LoadedProviderFactory>,
        provider_configs: &[ProviderConfig],
        mut schemas: SchemaRegistry,
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
        if uses_builtin_mock {
            let builtin = carina_provider_mock::builtin_schema_witness();
            let missing = builtin.resources().iter().filter(|schema| {
                let parts = schema.parts();
                let already_registered = if matches!(parts.kind, SchemaKind::DataSource) {
                    schemas.has_data_source(BUILTIN_PROVIDER_NAME, parts.resource_type)
                } else {
                    schemas.has_managed(BUILTIN_PROVIDER_NAME, parts.resource_type)
                };
                !already_registered
            });
            let missing = ValidatedProviderSchemaWitness::new(
                missing.cloned().collect(),
                builtin.config().clone(),
                builtin.reference_forms().clone(),
            )
            .expect("the filtered built-in mock schemas remain a valid aggregate witness");
            schemas
                .insert_provider_witness(BUILTIN_PROVIDER_NAME, &missing)
                .expect("the built-in mock exposes one consistent schema witness");
        }
        let mut known_providers = factories
            .iter()
            .map(|factory| factory.name().to_string())
            .collect::<HashSet<_>>();
        if uses_builtin_mock {
            known_providers.insert(BUILTIN_PROVIDER_NAME.to_string());
        }
        let factories = Arc::new(factories);
        let loaded_providers = Arc::new(provider_mod::loaded_provider_snapshot_from_factories(
            Arc::clone(&factories),
            schemas,
        ));
        Self {
            loaded_providers,
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
        self.loaded_providers.factories()
    }

    pub fn factories_arc(&self) -> Arc<Vec<Box<dyn ProviderFactory>>> {
        self.loaded_providers.factories_arc()
    }

    pub fn custom_type_resolver(&self) -> &dyn carina_core::parser::CustomTypeResolver {
        self.provider_context().custom_type_resolver()
    }

    pub fn provider_context(&self) -> &carina_core::parser::LoadedProviderContext {
        self.loaded_providers.provider_context()
    }

    pub fn schemas(&self) -> &SchemaRegistry {
        self.loaded_providers.schema_registry()
    }

    pub(crate) fn loaded_provider_schemas(&self) -> &ProviderLoadedSchemaRegistry {
        self.loaded_providers.schemas()
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

fn schema_check_provider_configs(
    providers: &[ProviderConfig],
    overrides: &HashMap<String, PathBuf>,
) -> Result<Vec<ProviderConfig>, AppError> {
    let mut effective = providers.to_vec();
    for (name, path) in overrides {
        let Some(config) = effective
            .iter_mut()
            .find(|config| config.name == *name && config.is_default())
        else {
            return Err(AppError::Validation(format!(
                "provider schema override names undeclared default provider '{name}'"
            )));
        };
        let source = format!("file://{}", path.display())
            .parse::<ProviderSource>()
            .map_err(|error| AppError::Validation(error.to_string()))?;
        config.source = Some(source);
        // The explicitly trusted artifact is the complete selection. A
        // source lock's revision/version constraint does not describe it.
        config.version = None;
        config.revision = None;
    }
    Ok(effective)
}

/// Load only provider schema metadata, with no runtime protocol binding or
/// provider initialization.
fn build_schema_only_factories_from_providers(
    user_dirs: &carina_core::UserDirs,
    providers: &[ProviderConfig],
    base_dir: &Path,
) -> Result<ProviderFactoryLoad, AppError> {
    #[cfg(test)]
    record_tracked_provider_load();

    let grant_inputs =
        TRUSTED_LOCAL_GRANT_INPUTS.get_or_init(|| TrustedLocalGrantInputs::new(Vec::new(), None));
    let grants = grant_inputs.get()?;
    let mut factories = Vec::new();
    let mut load_errors = HashMap::new();
    let mut attempted_default_sources = HashSet::new();

    for config in providers {
        let Some(source) = provider_factory_source(config) else {
            continue;
        };
        if config.is_default() && !attempted_default_sources.insert(config.name.clone()) {
            load_errors.insert(
                config.name.clone(),
                format!(
                    "Provider kind '{}' is declared more than once with a source; declare exactly one top-level `provider {}` block.",
                    config.name, config.name
                ),
            );
            continue;
        }

        let installed = match source {
            ProviderSource::File(_) => {
                carina_provider_resolver::find_file_provider_source(base_dir, config)
                    .map_err(carina_provider_resolver::FindInstalledProviderError::from)
            }
            ProviderSource::Remote(_) => {
                carina_provider_resolver::find_installed_provider(base_dir, config)
            }
        };
        let installed = match installed {
            Ok(installed) => installed,
            Err(error) => {
                load_errors.insert(
                    config.name.clone(),
                    format!("Provider '{}' {error}", config.name),
                );
                continue;
            }
        };
        let loaded = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(installed.load_with(
                user_dirs,
                grants,
                |verified| async move {
                    let artifact = verified.artifact().clone();
                    carina_plugin_host::SchemaOnlyWasmProviderFactory::from_bytes(
                        verified, user_dirs,
                    )
                    .await
                    .map(|factory| (factory, artifact))
                },
            ))
        });
        match loaded {
            Ok((factory, artifact)) => {
                if let Some(constraint) = &config.version
                    && let Err(message) = factory.verify_version(&constraint.raw)
                {
                    load_errors.insert(config.name.clone(), message);
                    continue;
                }
                factories.push(LoadedProviderFactory::new(Box::new(factory), artifact));
            }
            Err(error) => {
                let error = match take_unverified_official_load_error(error) {
                    Ok(unverified) => return Err(AppError::from(unverified)),
                    Err(error) => error,
                };
                if let Some(mismatch) =
                    source_provider_kind_mismatch_from_load_error(source, &error)
                {
                    return Err(mismatch);
                }
                load_errors.insert(config.name.clone(), error.to_string());
            }
        }
    }

    Ok((factories, load_errors))
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
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
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
pub fn validate_depends_on_with_ctx<E>(
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
) -> Vec<AppError> {
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
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
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
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
) -> Vec<AppError> {
    carina_core::validation::deferred_populate::validate_deferred_populate_refs(
        parsed,
        ctx.schemas(),
    )
    .into_iter()
    .map(|d| AppError::Validation(d.message))
    .collect()
}

pub fn validate_resource_ref_types_with_ctx<E: carina_core::parser::ExportParamLike>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
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
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
) -> Vec<AppError> {
    let bindings = carina_core::binding_index::BindingIndex::from_parsed(parsed, ctx.schemas());
    validation::validate_composition_ref_types_with_bindings(&parsed.compositions, &bindings)
        .into_iter()
        .map(|error| AppError::Validation(error.to_string()))
        .collect()
}

pub fn validate_attribute_param_ref_types_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
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
pub fn validate_no_empty_interpolations<E>(
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
) -> Vec<AppError>
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
        if let Some(value) = param
            .value
            .as_ref()
            .and_then(carina_core::parser::BoundaryExpression::value)
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
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            value_contains_empty_interpolation(expression.lhs())
                || value_contains_empty_interpolation(expression.rhs())
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
    parsed: &mut carina_core::parser::ModuleResolvedFile<E>,
) -> Vec<AppError> {
    let mut errors = resolve_file_block_names_with_schemas(parsed, ctx.schemas())
        .err()
        .map(block_name_errors)
        .unwrap_or_default();
    errors.extend(resolve_attr_prefixes_with_ctx(ctx, &mut parsed.resources));
    errors
}

fn resolve_config_names_with_ctx<E>(
    access: &ValidationPipelineAccess,
    ctx: &WiringContext,
    parsed: carina_core::parser::ModuleResolvedFile<E>,
    unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
) -> Result<CanonicalResourcePair<E>, Vec<AppError>> {
    CanonicalResourcePair::resolve_block_names(access, parsed, unresolved, ctx.schemas())
        .map_err(block_name_errors)?
        .resolve_attr_prefixes(ctx.schemas())
        .map_err(|error| split_validation_message(&error))
}

/// Resolve block names for tests without running later validation-wiring
/// transformations such as attribute-prefix resolution.
#[cfg(any(test, feature = "test-utils"))]
pub fn resolve_config_block_names_for_test<E>(
    parsed: carina_core::parser::ModuleResolvedFile<E>,
    unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
    registry: &SchemaRegistry,
) -> Result<CanonicalResourcePair<E>, Vec<carina_core::schema::BlockNameConflict>> {
    CanonicalResourcePair::resolve_block_names(
        &ValidationPipelineAccess::new(),
        parsed,
        unresolved,
        registry,
    )
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
        let persisted = state_file.find_resource(provider, resource_type, name)?;
        prefix_state_info(persisted, None)
    });
}

pub(crate) fn reconcile_canonical_prefixed_names<E>(
    resources: &mut CanonicalResourcePair<E>,
    state_file: &Option<StateFile>,
) {
    let Some(state_file) = state_file else {
        return;
    };

    resources.reconcile_prefixed_names(&|provider, resource_type, name| {
        let persisted = state_file.find_resource(provider, resource_type, name)?;
        prefix_state_info(persisted, None)
    });
}

/// Build the one prefix-reconciliation view used before and after refresh.
///
/// `current == None` means no refresh has run yet, so both persisted values
/// and its pending re-read marker are authoritative. Once a refreshed state
/// is supplied, its concrete strings and marker take precedence; persisted
/// concrete strings remain only as the read-omission/out-of-band-deletion
/// fallback, and a persisted marker is deliberately ignored.
fn prefix_state_info(
    persisted: &ResourceState,
    current: Option<&State>,
) -> Option<PrefixStateInfo> {
    let persisted_attributes = persisted.persisted_attributes()?;
    let persisted_values = persisted_attributes.plain_string_values();
    let mut attribute_values = HashMap::new();

    if let Some(current) = current {
        attribute_values.extend(current.attributes.iter().filter_map(|(name, value)| {
            let Value::Concrete(ConcreteValue::String(value)) = value else {
                return None;
            };
            Some((
                name.clone(),
                identifier::PrefixStateValue::Known(value.expose_plaintext().to_string()),
            ))
        }));
        if let Some(marker) = current.partial_read.as_ref() {
            for attribute in marker.missing_attributes() {
                attribute_values
                    .entry(attribute.clone())
                    .or_insert_with(|| {
                        identifier::PrefixStateValue::PendingReRead(marker.pending().clone())
                    });
            }
        }
        for (name, value) in persisted_values {
            attribute_values
                .entry(name)
                .or_insert(identifier::PrefixStateValue::Known(value));
        }
    } else {
        attribute_values.extend(
            persisted_values
                .into_iter()
                .map(|(name, value)| (name, identifier::PrefixStateValue::Known(value))),
        );
        if let Some(marker) = persisted.partial_read() {
            for attribute in marker.missing_attributes() {
                attribute_values
                    .entry(attribute.clone())
                    .or_insert_with(|| {
                        identifier::PrefixStateValue::PendingReRead(marker.pending().clone())
                    });
            }
        }
    }

    Some(PrefixStateInfo {
        prefixes: persisted.prefixes.clone(),
        attribute_values,
    })
}

type PrefixStateKey = (String, String, String);

fn prefix_state_key(provider: &str, resource_type: &str, identity: &str) -> PrefixStateKey {
    (
        provider.to_string(),
        resource_type.to_string(),
        identity.to_string(),
    )
}

/// Prefix inputs proven to have been computed after managed refresh/hydration.
/// The private variants distinguish a legitimate no-state-file path from a
/// computed map and prevent an empty "not computed" map from reaching
/// canonical deferred-for expansion.
#[derive(Clone)]
enum RefreshedPrefixStateInfo {
    NoStateFile,
    Computed(HashMap<PrefixStateKey, PrefixStateInfo>),
}

impl RefreshedPrefixStateInfo {
    fn get(&self, key: &PrefixStateKey) -> Option<&PrefixStateInfo> {
        match self {
            Self::NoStateFile => None,
            Self::Computed(values) => values.get(key),
        }
    }
}

/// Build prefix reconciliation inputs from the hydrated current snapshot.
/// Refreshed values and markers take precedence, while a persisted generated
/// value remains a fallback for keeping the desired name stable. This never
/// writes the fallback into provider-read current state, so an omitted
/// attribute remains observable as drift.
fn refreshed_prefix_state_info(
    resources: &[Resource],
    state_file: &Option<StateFile>,
    managed_current: &HashMap<ResourceId, Current>,
) -> RefreshedPrefixStateInfo {
    let Some(state_file) = state_file.as_ref() else {
        return RefreshedPrefixStateInfo::NoStateFile;
    };
    RefreshedPrefixStateInfo::Computed(
        resources
            .iter()
            .filter(|resource| !resource.prefixes.is_empty())
            .filter_map(|resource| {
                let identity = resource.id.identity_str()?;
                let persisted = state_file.find_resource(
                    &resource.id.provider,
                    &resource.id.resource_type,
                    identity,
                )?;
                let current_state = match managed_current.get(&resource.id)? {
                    Current::Absent(state) => state.state(),
                    Current::Present(present) => present.state(),
                    Current::CreateInFlight(_pending) => return None,
                    Current::CreateOutcomeUnknown(_unknown) => return None,
                    Current::MissingIdentifier(state) => state.state(),
                };
                let state_info = prefix_state_info(persisted, Some(current_state))?;
                Some((
                    prefix_state_key(&resource.id.provider, &resource.id.resource_type, identity),
                    state_info,
                ))
            })
            .collect(),
    )
}

/// Persisted rows indexed at their effective addresses after every move that
/// has already been applied to [`AddressedState`].
struct AnonymousReconciliationStateView {
    by_type: HashMap<(String, String), Vec<AnonymousIdStateInfo>>,
    existence_only_by_type: HashMap<(String, String), HashSet<String>>,
    origins_by_projected_name: HashMap<(String, String), HashMap<String, Vec<ResourceId>>>,
    by_binding: HashMap<String, Vec<AnonymousIdBindingViewEntry>>,
}

struct AnonymousIdBindingViewEntry {
    value: AnonymousIdBindingStateInfo,
    origin: ResourceId,
    effective: ResourceId,
}

impl AnonymousIdBindingViewEntry {
    fn is_visible_to_binding_resolution(&self, claims: &StateBlockClaims) -> bool {
        // A claimed source is normally unavailable to heuristic reads. A
        // moved row is the exception: its matching target claim makes the
        // projected address readable. A removed row has no target claim and
        // therefore remains hidden even if another phase projected it.
        !claims.claims_from(&self.origin) || claims.claims_to(&self.effective)
    }
}

impl AnonymousReconciliationStateView {
    fn new(
        ctx: &WiringContext,
        state_file: &StateFile,
        moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
    ) -> Self {
        let mut by_type = HashMap::<_, Vec<_>>::new();
        let mut existence_only_by_type = HashMap::<_, HashSet<_>>::new();
        let mut origins_by_projected_name = HashMap::<_, HashMap<String, Vec<ResourceId>>>::new();
        let mut by_binding = HashMap::<_, Vec<_>>::new();
        let moved_targets = moved_pairs
            .iter()
            .map(|(from, to)| (from.as_inner().clone(), to.clone()))
            .collect::<HashMap<_, _>>();
        for state_resource in state_file.resources() {
            let state_row_id = resolved_state_row_id(
                state_resource,
                state_resource.directives.provider_instance.clone(),
            );
            let projected_target = moved_targets.get(state_row_id.as_inner());
            let projected_id = projected_target
                .cloned()
                .unwrap_or_else(|| state_row_id.clone());
            let has_persisted_attributes = state_resource.persisted_attributes().is_some();
            let attribute_values = state_resource
                .persisted_attributes()
                .map(|persisted| persisted.to_values())
                .unwrap_or_default();
            let type_key = (
                projected_id.provider().to_string(),
                projected_id.resource_type().to_string(),
            );
            let state_info = if let Some(schema) = ctx.schemas().get(
                projected_id.provider(),
                projected_id.resource_type(),
                carina_core::schema::SchemaKind::Resource,
            ) {
                AnonymousIdStateInfo::try_from_state_attributes(
                    state_row_id.clone(),
                    projected_target.cloned(),
                    schema.schema(),
                    &attribute_values,
                )
            } else {
                AnonymousIdStateInfo::try_without_comparable_attributes(
                    state_row_id.clone(),
                    projected_target.cloned(),
                )
            };
            let projected_name = projected_id.identity_str().to_string();
            if let Some(state_info) = state_info {
                origins_by_projected_name
                    .entry(type_key.clone())
                    .or_default()
                    .entry(projected_name.clone())
                    .or_default()
                    .push(state_row_id.as_inner().clone());
                if !has_persisted_attributes {
                    existence_only_by_type
                        .entry(type_key.clone())
                        .or_default()
                        .insert(projected_name.clone());
                }

                by_type.entry(type_key).or_default().push(state_info);
            }
            let projected = projected_target.is_some();
            let effective_binding = if projected {
                Some(projected_name.clone())
            } else {
                state_resource.binding.clone()
            };
            if let Some(effective_binding) = effective_binding {
                by_binding.entry(effective_binding).or_default().push(
                    AnonymousIdBindingViewEntry {
                        value: AnonymousIdBindingStateInfo {
                            name: projected_name,
                            attribute_values,
                        },
                        origin: state_row_id.as_inner().clone(),
                        effective: projected_id.as_inner().clone(),
                    },
                );
            }
        }

        Self {
            by_type,
            existence_only_by_type,
            origins_by_projected_name,
            by_binding,
        }
    }

    fn entries_by_type(
        &self,
        claims: &StateBlockClaims,
        provider: &str,
        resource_type: &str,
    ) -> identifier::ScreenedStateEntries<AnonymousIdStateInfo> {
        let type_key = (provider.to_string(), resource_type.to_string());
        let entries = self.by_type.get(&type_key).into_iter().flatten().cloned();
        let existence_only = self.existence_only_by_type.get(&type_key);
        let claimed_origin_names = self
            .origins_by_projected_name
            .get(&type_key)
            .into_iter()
            .flat_map(HashMap::iter)
            .filter_map(|(projected_name, origins)| {
                origins
                    .iter()
                    .any(|origin| claims.claims_from(origin))
                    .then_some(projected_name.as_str())
            })
            .collect::<HashSet<_>>();
        claims
            .screen_entries(provider, resource_type, entries, AnonymousIdStateInfo::name)
            .exclude_candidates(|entry| {
                existence_only.is_some_and(|names| names.contains(entry.name()))
                    || claimed_origin_names.contains(entry.name())
            })
    }

    fn entries_by_binding(
        &self,
        claims: &StateBlockClaims,
        binding: &str,
    ) -> Vec<AnonymousIdBindingStateInfo> {
        self.by_binding
            .get(binding)
            .into_iter()
            .flatten()
            .filter(|entry| entry.is_visible_to_binding_resolution(claims))
            .map(|entry| entry.value.clone())
            .collect()
    }
}

fn reconcile_anonymous_identifiers_with_ctx(
    ctx: &WiringContext,
    comparison_provider: &dyn ResourceComparisonResolver,
    resources: &[Resource],
    data_sources: &[DataSource],
    state_view: &AnonymousReconciliationStateView,
    claims: &StateBlockClaims,
) -> Result<Vec<ResolvedMove>, AppError> {
    Ok(identifier::reconcile_anonymous_identifiers(
        resources,
        data_sources,
        ctx.schemas(),
        comparison_provider,
        &|provider, resource_type| state_view.entries_by_type(claims, provider, resource_type),
        &|binding| state_view.entries_by_binding(claims, binding),
        claims,
    )?)
}

fn detect_legacy_loop_moves_with_ctx(
    ctx: &WiringContext,
    comparison_provider: &dyn ResourceComparisonResolver,
    resources: &[Resource],
    state_view: &AnonymousReconciliationStateView,
    claims: &StateBlockClaims,
    reserved_moves: &[ResolvedMove],
) -> Result<Vec<ResolvedMove>, AppError> {
    Ok(identifier::detect_legacy_loop_address_moves(
        resources,
        ctx.schemas(),
        comparison_provider,
        &|provider, resource_type| state_view.entries_by_type(claims, provider, resource_type),
        &|binding| state_view.entries_by_binding(claims, binding),
        claims,
        reserved_moves,
    )?)
}

fn detect_anonymous_to_named_moves_with_ctx(
    ctx: &WiringContext,
    comparison_provider: &dyn ResourceComparisonResolver,
    candidates: identifier::AnonymousToNamedCandidates<'_>,
    providers: &[ProviderConfig],
    state_view: &AnonymousReconciliationStateView,
    claims: &StateBlockClaims,
) -> Result<identifier::AnonymousToNamedRenameDetection, AppError> {
    let canonical_providers = carina_core::value::canonicalize_provider_configs_with_schemas(
        providers,
        &|provider_name| {
            provider_mod::find_factory(ctx.factories(), provider_name)
                .map(ProviderFactory::provider_config_attribute_types)
        },
    )?;
    Ok(identifier::detect_anonymous_to_named_renames(
        candidates,
        ctx.schemas(),
        comparison_provider,
        identifier::AnonymousToNamedStateLookup::new(
            &|provider, resource_type| state_view.entries_by_type(claims, provider, resource_type),
            &|binding| state_view.entries_by_binding(claims, binding),
        ),
        &canonical_providers,
        &|name| {
            provider_mod::find_factory(ctx.factories(), name)
                .map(|factory| {
                    factory
                        .identity_attributes()
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        },
        claims,
    )?)
}

fn detect_anonymous_to_named_moves_to_fixed_point(
    ctx: &WiringContext,
    comparison_provider: &dyn ResourceComparisonResolver,
    resources: &[Resource],
    providers: &[ProviderConfig],
    state_file: &StateFile,
    initial_moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
    claims: &StateBlockClaims,
) -> Result<Vec<ResolvedMove>, AppError> {
    let mut contested_resources = HashSet::new();

    'recompute: loop {
        let mut projected_pairs = initial_moved_pairs.to_vec();
        let mut detected = Vec::new();
        let mut unsatisfied = resources
            .iter()
            .filter(|resource| resource.binding.is_some())
            .map(|resource| resource.id.clone())
            .filter(|resource_id| !contested_resources.contains(resource_id))
            .collect::<HashSet<_>>();

        while !unsatisfied.is_empty() {
            let state_view =
                AnonymousReconciliationStateView::new(ctx, state_file, &projected_pairs);
            let claimed_origins = identifier::AnonymousToNamedClaimedOrigins::from_moves(&detected);
            let detection = detect_anonymous_to_named_moves_with_ctx(
                ctx,
                comparison_provider,
                identifier::AnonymousToNamedCandidates::new(
                    resources,
                    &unsatisfied,
                    &claimed_origins,
                ),
                providers,
                &state_view,
                claims,
            )?;
            let (pass, cross_pass_contests) = detection.into_parts();
            if !cross_pass_contests.is_empty() {
                for contest in cross_pass_contests {
                    let (origin, incumbent, contender) = contest.into_parts();
                    debug_assert!(
                        detected
                            .iter()
                            .any(|resolved_move| resolved_move.from() == &origin)
                    );
                    contested_resources.insert(incumbent);
                    contested_resources.insert(contender);
                }
                continue 'recompute;
            }
            if pass.is_empty() {
                break;
            }
            let satisfied = pass
                .iter()
                .map(|resolved_move| resolved_move.to().as_inner())
                .collect::<HashSet<_>>();
            unsatisfied.retain(|resource_id| !satisfied.contains(resource_id));
            projected_pairs.extend(
                pass.iter().map(|resolved_move| {
                    (resolved_move.from().clone(), resolved_move.to().clone())
                }),
            );
            detected.extend(pass);
        }

        return Ok(detected);
    }
}

pub(crate) struct LateAnonymousIdentityInputs<'a> {
    pub resources: &'a mut OverrideAwareResources,
    pub addressed: &'a mut AddressedState,
    pub data_sources: &'a [DataSource],
    pub state_file: Option<&'a StateFile>,
    pub state_block_claims: &'a StateBlockClaims,
    pub providers: &'a [ProviderConfig],
    pub comparison_provider: &'a ProviderRouter,
}

/// Desired resource identities captured only after late identity resolution.
pub struct ResolvedDesiredIds {
    ids: HashSet<ResolvedResourceId>,
}

impl ResolvedDesiredIds {
    fn from_resources(resources: &[Resource]) -> Result<Self, AppError> {
        let ids = resources
            .iter()
            .map(|resource| {
                ResolvedResourceId::try_new(resource.id.clone()).ok_or_else(|| {
                    carina_core::resource::PendingResourceIdentityError::new(resource.id.clone())
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { ids })
    }
}

struct LateAnonymousMoveDetectionInputs<'a> {
    ctx: &'a WiringContext,
    comparison: &'a dyn ResourceComparisonResolver,
    resources: &'a [Resource],
    data_sources: &'a [DataSource],
    state_file: Option<&'a StateFile>,
    state_block_claims: &'a StateBlockClaims,
    providers: &'a [ProviderConfig],
    initial_moved_pairs: &'a [(ResolvedResourceId, ResolvedResourceId)],
}

fn detect_late_anonymous_moves(
    inputs: LateAnonymousMoveDetectionInputs<'_>,
) -> Result<Vec<ResolvedMove>, AppError> {
    let Some(state_file) = inputs.state_file else {
        return Ok(Vec::new());
    };
    let state_view =
        AnonymousReconciliationStateView::new(inputs.ctx, state_file, inputs.initial_moved_pairs);
    let mut moves = detect_legacy_loop_moves_with_ctx(
        inputs.ctx,
        inputs.comparison,
        inputs.resources,
        &state_view,
        inputs.state_block_claims,
        &[],
    )?;
    let mut projected_pairs = inputs.initial_moved_pairs.to_vec();
    projected_pairs.extend(
        moves
            .iter()
            .map(|migration| (migration.from().clone(), migration.to().clone())),
    );
    let state_view =
        AnonymousReconciliationStateView::new(inputs.ctx, state_file, &projected_pairs);
    moves.extend(reconcile_anonymous_identifiers_with_ctx(
        inputs.ctx,
        inputs.comparison,
        inputs.resources,
        inputs.data_sources,
        &state_view,
        inputs.state_block_claims,
    )?);
    projected_pairs = inputs.initial_moved_pairs.to_vec();
    projected_pairs.extend(
        moves
            .iter()
            .map(|resolved_move| (resolved_move.from().clone(), resolved_move.to().clone())),
    );
    moves.extend(detect_anonymous_to_named_moves_to_fixed_point(
        inputs.ctx,
        inputs.comparison,
        inputs.resources,
        inputs.providers,
        state_file,
        &projected_pairs,
        inputs.state_block_claims,
    )?);
    Ok(moves)
}

pub(crate) fn reconcile_late_anonymous_identities(
    ctx: &WiringContext,
    inputs: LateAnonymousIdentityInputs<'_>,
) -> Result<ResolvedDesiredIds, AppError> {
    resolve_late_anonymous_resource_ids(ctx, inputs.resources.resources_mut(), inputs.providers)?;

    let moves = detect_late_anonymous_moves(LateAnonymousMoveDetectionInputs {
        ctx,
        comparison: inputs.comparison_provider,
        resources: inputs.resources.resources(),
        data_sources: inputs.data_sources,
        state_file: inputs.state_file,
        state_block_claims: inputs.state_block_claims,
        providers: inputs.providers,
        initial_moved_pairs: &inputs.addressed.moved_pairs,
    })?;

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

    let desired = ResolvedDesiredIds::from_resources(inputs.resources.resources())?;
    apply_detected_resource_moves(
        inputs.addressed,
        moves,
        inputs.state_file,
        MoveOverlapPolicy::ComposeForward,
    )?;
    Ok(desired)
}

fn resolve_late_anonymous_resource_ids(
    ctx: &WiringContext,
    resources: &mut [Resource],
    providers: &[ProviderConfig],
) -> Result<(), AppError> {
    let canonical_resources =
        carina_core::value::canonicalize_resources_with_schemas(resources, ctx.schemas())?;
    compute_anonymous_identifiers_result_with_ctx(
        ctx,
        canonical_resources,
        providers,
        identifier::AnonymousIdentityConflictPolicy::RejectAttributeDerived,
    )
}

pub fn compute_anonymous_identifiers_with_ctx(
    ctx: &WiringContext,
    resources: carina_core::value::CanonicalizedResources<'_>,
    providers: &[ProviderConfig],
) -> Vec<AppError> {
    compute_anonymous_identifiers_with_policy_with_ctx(
        ctx,
        resources,
        providers,
        identifier::AnonymousIdentityConflictPolicy::RejectAttributeDerived,
    )
}

pub(crate) fn compute_anonymous_identifiers_with_policy_with_ctx(
    ctx: &WiringContext,
    resources: carina_core::value::CanonicalizedResources<'_>,
    providers: &[ProviderConfig],
    conflict_policy: identifier::AnonymousIdentityConflictPolicy,
) -> Vec<AppError> {
    match compute_anonymous_identifiers_result_with_ctx(ctx, resources, providers, conflict_policy)
    {
        Ok(()) => Vec::new(),
        Err(AppError::AnonymousIdentity(error)) => error
            .into_individual_errors()
            .into_iter()
            .map(AppError::from)
            .collect(),
        Err(error) => vec![error],
    }
}

fn compute_anonymous_identifiers_result_with_ctx(
    ctx: &WiringContext,
    resources: carina_core::value::CanonicalizedResources<'_>,
    providers: &[ProviderConfig],
    conflict_policy: identifier::AnonymousIdentityConflictPolicy,
) -> Result<(), AppError> {
    match identifier::compute_anonymous_identifiers_with_factories(
        resources,
        providers,
        ctx.factories(),
        ctx.schemas(),
        conflict_policy,
    ) {
        Ok(()) => Ok(()),
        Err(identifier::AnonymousIdentityComputationError::ProviderConfig(error)) => {
            Err(error.into())
        }
        Err(identifier::AnonymousIdentityComputationError::Identity(error)) => {
            Err(AppError::AnonymousIdentity(error))
        }
    }
}

fn compute_canonical_pair_anonymous_identifiers_result_with_ctx<E>(
    ctx: &WiringContext,
    resources: &mut CanonicalResourcePair<E>,
    providers: &[ProviderConfig],
    conflict_policy: identifier::AnonymousIdentityConflictPolicy,
) -> Result<(), AppError> {
    match resources.canonicalize_and_compute_anonymous_identifiers(
        ctx.schemas(),
        providers,
        ctx.factories(),
        conflict_policy,
    ) {
        Ok(()) => Ok(()),
        Err(CanonicalResourceIdentityError::EnumCanonicalization(error)) => Err(error.into()),
        Err(CanonicalResourceIdentityError::ProviderConfig(error)) => Err(error.into()),
        Err(CanonicalResourceIdentityError::Identity(error)) => {
            Err(AppError::AnonymousIdentity(error))
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub(crate) fn compute_canonical_pair_anonymous_identifiers_with_ctx<E>(
    ctx: &WiringContext,
    resources: &mut CanonicalResourcePair<E>,
    providers: &[ProviderConfig],
) -> Result<Vec<AppError>, AppError> {
    compute_canonical_pair_anonymous_identifiers_with_policy_with_ctx(
        ctx,
        resources,
        providers,
        identifier::AnonymousIdentityConflictPolicy::RejectAttributeDerived,
    )
}

pub(crate) fn compute_canonical_pair_anonymous_identifiers_with_policy_with_ctx<E>(
    ctx: &WiringContext,
    resources: &mut CanonicalResourcePair<E>,
    providers: &[ProviderConfig],
    conflict_policy: identifier::AnonymousIdentityConflictPolicy,
) -> Result<Vec<AppError>, AppError> {
    match resources.canonicalize_and_compute_anonymous_identifiers(
        ctx.schemas(),
        providers,
        ctx.factories(),
        conflict_policy,
    ) {
        Ok(()) => Ok(Vec::new()),
        Err(CanonicalResourceIdentityError::EnumCanonicalization(error)) => Err(error.into()),
        Err(CanonicalResourceIdentityError::ProviderConfig(error)) => Ok(vec![error.into()]),
        Err(CanonicalResourceIdentityError::Identity(error)) => Ok(error
            .into_individual_errors()
            .into_iter()
            .map(AppError::from)
            .collect()),
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
    state: &ProjectedStateRead<'_>,
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
            state.restore_partial_read_markers(states);
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

/// One or more plan diagnostics.
///
/// The inner vector is private and no mutable access is exposed, so every
/// value permanently retains the non-empty invariant established by
/// [`TryFrom`].
#[derive(Debug, Clone, PartialEq)]
pub struct NonEmptyPlanErrors(Vec<PlanError>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("plan diagnostics cannot be empty")]
pub struct EmptyPlanErrors;

impl TryFrom<Vec<PlanError>> for NonEmptyPlanErrors {
    type Error = EmptyPlanErrors;

    fn try_from(errors: Vec<PlanError>) -> Result<Self, Self::Error> {
        if errors.is_empty() {
            Err(EmptyPlanErrors)
        } else {
            Ok(Self(errors))
        }
    }
}

impl NonEmptyPlanErrors {
    pub fn as_slice(&self) -> &[PlanError] {
        &self.0
    }

    fn into_vec(self) -> Vec<PlanError> {
        self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlanPreparationError {
    #[error("plan validation failed")]
    Plan(NonEmptyPlanErrors),
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

/// Convert newly reported module failures into diagnostics.
///
/// The gate deliberately returns an error with no failures when the same
/// violation was already reported at an earlier provider boundary. That case
/// is not a new diagnostic outcome, so it becomes `None` here.
pub(crate) fn module_constraint_plan_errors(
    error: carina_core::executor::ModuleConstraintGateError,
) -> Option<NonEmptyPlanErrors> {
    let errors: Vec<PlanError> = error
        .failures()
        .iter()
        .cloned()
        .map(|failure| {
            PlanError::new(
                failure.composition_id,
                PlanErrorKind::ModuleConstraint(failure.diagnostic),
            )
        })
        .collect();
    NonEmptyPlanErrors::try_from(errors).ok()
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
        if let Err(error) = module_gate.check(resources.bindings())
            && let Some(module_errors) = module_constraint_plan_errors(error)
        {
            errors.extend(module_errors.into_vec());
        }
        if let Ok(errors) = NonEmptyPlanErrors::try_from(errors) {
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
                Current::CreateOutcomeUnknown(_unknown) => None,
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
    config: &carina_core::parser::LoadedProviderContext,
) -> Vec<AppError> {
    let mut errors = Vec::new();
    for module in module_walk.iter() {
        let mut parsed = module.loaded().parsed.clone();
        let prefix = module.diagnostic_path().display();
        // The recursive module-call checker owns declaration-resolution
        // diagnostics and their source anchors. This pass only needs a
        // successfully resolved declaration set so valid dotted argument
        // types can act as sources before expansion; continuing after an
        // error would manufacture a second, string-prefixed diagnostic.
        if !validation::resolve_file_type_exprs(&mut parsed, config).is_empty() {
            continue;
        }

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
        // The implicit built-in remains compatible with legacy providerless
        // state while also owning the explicit `mock.*` addresses used by new
        // fixtures and configurations.
        progress_sink.line("Using mock provider".cyan());
        router.add_provider(
            BUILTIN_PROVIDER_NAME.to_string(),
            InitializedProvider::unavailable(Box::new(MockProvider::new())),
        );
        router.add_provider(
            String::new(),
            InitializedProvider::unavailable(Box::new(MockProvider::new())),
        );
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
            .create_initialized_provider(binding, &config)
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
            InitializedProvider::unavailable(Box::new(MockProvider::new())),
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
        router.add_provider(
            BUILTIN_PROVIDER_NAME.to_string(),
            InitializedProvider::unavailable(Box::new(MockProvider::new())),
        );
        router.add_provider(
            String::new(),
            InitializedProvider::unavailable(Box::new(MockProvider::new())),
        );
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
) -> Result<NormalizerRouter, AppError> {
    let mut router = NormalizerRouter::new();

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
    pub dependencies: ResolvedDependencies,
    pub template: carina_core::parser::DeferredForExpression,
}

impl DeferredCreateTarget {
    fn from_deferred(deferred: &carina_core::parser::DeferredForExpression) -> Option<Self> {
        let template_id = &deferred.template_resource.id;
        let identity = match ResourceIdentity::try_from(deferred.target.scheduler_key().to_string())
        {
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
            dependencies: ResolvedDependencies::default(),
            template: deferred.clone(),
        })
    }

    fn to_effect(&self) -> Effect {
        Effect::DeferredCreate {
            id: self.id.clone(),
            dependencies: self.dependencies.clone(),
            template: Box::new(self.template.clone()),
        }
    }

    fn to_deferred_replace_effect(&self, deletes: Vec<DeferredReplaceDelete>) -> Effect {
        Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(deletes).expect("planner checked non-empty deletes"),
            id: self.id.clone(),
            dependencies: self.dependencies.clone(),
            template: Box::new(self.template.clone()),
        }))
    }
}

/// State and metadata maps that have crossed the shared address-reconciliation
/// boundary.
#[derive(Clone)]
pub(crate) struct AddressedState {
    managed_current: HashMap<ResourceId, Current>,
    orphans: Orphans,
    saved_attrs: LiftedSavedAttrs,
    prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    moved_pairs: ResolvedMovePairs,
}

pub(crate) struct AddressedStateParts {
    pub managed_current: HashMap<ResourceId, Current>,
    pub orphans: Orphans,
    pub saved_attrs: LiftedSavedAttrs,
}

impl AddressedState {
    #[cfg(test)]
    pub(crate) fn moved_pairs(&self) -> &[(ResolvedResourceId, ResolvedResourceId)] {
        self.moved_pairs.as_slice()
    }

    pub(crate) fn projected_state<'state>(
        &self,
        state_file: Option<&'state StateFile>,
    ) -> ProjectedStateRead<'state> {
        ProjectedStateRead::from_pairs(state_file, &self.moved_pairs)
    }

    pub(crate) fn into_parts(self) -> AddressedStateParts {
        AddressedStateParts {
            managed_current: self.managed_current,
            orphans: self.orphans,
            saved_attrs: self.saved_attrs,
        }
    }

    fn refresh_destroy_metadata_from_state(&mut self, state_file: &StateFile) {
        let source_by_target =
            ProjectedStateRead::from_pairs(Some(state_file), &self.moved_pairs).source_by_target;
        for (desired, current) in &mut self.managed_current {
            let source = source_by_target
                .get(desired)
                .map_or(desired, |id| id.as_inner());
            let Some(row) = state_file.find_resource_for_id(source) else {
                continue;
            };
            current.set_dependency_bindings(row.dependency_bindings.to_resolved_dependencies());
        }
        let orphan_ids = self.orphans.keys().cloned().collect::<Vec<_>>();
        for orphan_id in orphan_ids {
            let Some(row) = state_file.find_resource_for_id(&orphan_id) else {
                continue;
            };
            self.orphans.set_destroy_metadata(
                &orphan_id,
                row.dependency_bindings.to_resolved_dependencies(),
                row.directives.clone(),
            );
        }
    }
}

/// Persisted-state reads projected from each desired address back to the row
/// that owned it before address reconciliation.
///
/// Construction is confined to the detected/addressed move pipeline. Later
/// desired-resource consumers receive this view instead of a raw
/// [`StateFile`], so name overrides and other row metadata cannot accidentally
/// be looked up at a freshly moved target.
pub(crate) struct ProjectedStateRead<'state> {
    state_file: Option<&'state StateFile>,
    source_by_target: HashMap<ResourceId, ResolvedResourceId>,
    target_by_source: HashMap<ResourceId, ResolvedResourceId>,
    #[cfg(test)]
    name_override_source: Option<&'state dyn NameOverrideSource>,
}

impl<'state> ProjectedStateRead<'state> {
    fn from_moves(state_file: Option<&'state StateFile>, moves: &[ResolvedMove]) -> Self {
        Self::from_edges(
            state_file,
            moves
                .iter()
                .map(|resolved_move| (resolved_move.from(), resolved_move.to())),
        )
    }

    fn from_pairs(
        state_file: Option<&'state StateFile>,
        moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
    ) -> Self {
        Self::from_edges(state_file, moved_pairs.iter().map(|(from, to)| (from, to)))
    }

    fn from_edges<'edge>(
        state_file: Option<&'state StateFile>,
        edges: impl IntoIterator<Item = (&'edge ResolvedResourceId, &'edge ResolvedResourceId)>,
    ) -> Self {
        let mut source_by_target = HashMap::new();
        for (from, to) in edges {
            let source = source_by_target
                .remove(from.as_inner())
                .unwrap_or_else(|| from.clone());
            source_by_target.insert(to.as_inner().clone(), source);
        }
        let target_by_source = source_by_target
            .iter()
            .map(|(target, source)| {
                (
                    source.as_inner().clone(),
                    ResolvedResourceId::new(target.clone()),
                )
            })
            .collect();
        Self {
            state_file,
            source_by_target,
            target_by_source,
            #[cfg(test)]
            name_override_source: None,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn for_test_unmoved(state_file: Option<&'state StateFile>) -> Self {
        Self::from_pairs(state_file, &[])
    }

    #[cfg(test)]
    fn for_test_name_overrides(source: &'state dyn NameOverrideSource) -> Self {
        Self {
            state_file: None,
            source_by_target: HashMap::new(),
            target_by_source: HashMap::new(),
            name_override_source: Some(source),
        }
    }

    fn source_for<'a>(&'a self, desired: &'a ResourceId) -> &'a ResourceId {
        self.source_by_target
            .get(desired)
            .map_or(desired, ResolvedResourceId::as_inner)
    }

    fn build_current(&self, desired: &ResourceId) -> (ResourceId, Current) {
        let source = self.source_for(desired).clone();
        let current = self
            .state_file
            .map(|state_file| state_file.build_state_for_resource(&source))
            .unwrap_or_else(|| Current::absent(source.clone()));
        (source, current)
    }

    fn saved_dependency_bindings(
        &self,
        resources: &[Resource],
    ) -> HashMap<ResourceId, ResolvedDependencies> {
        let Some(state_file) = self.state_file else {
            return HashMap::new();
        };
        resources
            .iter()
            .filter_map(|resource| {
                let state = state_file.find_resource_for_id(self.source_for(&resource.id))?;
                (!state.dependency_bindings.is_empty()).then(|| {
                    (
                        resource.id.clone(),
                        state.dependency_bindings.to_resolved_dependencies(),
                    )
                })
            })
            .collect()
    }

    fn persisted_resource_ids(&self, resources: &[Resource]) -> HashSet<ResourceId> {
        resources
            .iter()
            .map(|resource| self.source_for(&resource.id).clone())
            .collect()
    }

    pub(crate) fn build_directives(&self) -> HashMap<ResourceId, PersistedDirectives> {
        let Some(state_file) = self.state_file else {
            return HashMap::new();
        };
        state_file
            .resources()
            .iter()
            .map(|row| {
                let source = resolved_state_row_id(row, row.directives.provider_instance.clone());
                let id = self
                    .target_by_source
                    .get(source.as_inner())
                    .map_or_else(|| source.into_inner(), |target| target.as_inner().clone());
                (id, row.directives.clone())
            })
            .collect()
    }

    fn restore_partial_read_markers(&self, states: &mut provider_mod::StateTransformMap) {
        for (desired, current) in states.iter_mut() {
            if !current.exists {
                current.partial_read = None;
                continue;
            }
            let marker = self
                .state_file
                .and_then(|state_file| state_file.find_resource_for_id(self.source_for(desired)))
                .and_then(carina_state::ResourceState::partial_read)
                .cloned();
            if let Some(marker) = marker {
                let (pending, mut missing_attributes) = marker.into_parts();
                missing_attributes.retain(|attribute| !current.attributes.contains_key(attribute));
                current.partial_read = PartialReadMarker::new(pending, missing_attributes);
            }
        }
    }
}

impl NameOverrideSource for ProjectedStateRead<'_> {
    fn name_overrides_for(
        &self,
        resource_id: &ResourceId,
    ) -> Option<&HashMap<String, NameOverride>> {
        #[cfg(test)]
        if let Some(source) = self.name_override_source {
            return source.name_overrides_for(resource_id);
        }
        self.state_file?
            .name_overrides_for(self.source_for(resource_id))
    }
}

#[derive(Clone, Copy)]
enum MoveOverlapPolicy {
    ComposeForward,
    RejectOverlap,
}

fn updated_move_pairs(
    moved_pairs: &ResolvedMovePairs,
    resolved_move: &ResolvedMove,
    policy: MoveOverlapPolicy,
) -> Result<ResolvedMovePairs, AppError> {
    let mut updated = moved_pairs.clone();
    let result = match policy {
        MoveOverlapPolicy::ComposeForward => updated.compose_forward(resolved_move.clone()),
        MoveOverlapPolicy::RejectOverlap => updated.append_rejecting_overlap(resolved_move.clone()),
    };
    result.map_err(|error| {
        let (from, to, existing_from, existing_to) = error.into_parts();
        AppError::DetectedMoveConflict {
            from: Box::new(from),
            to: Box::new(to),
            existing_from: Box::new(existing_from),
            existing_to: Box::new(existing_to),
        }
    })?;
    Ok(updated)
}

fn apply_detected_resource_moves(
    addressed: &mut AddressedState,
    moves: Vec<ResolvedMove>,
    state_file: Option<&StateFile>,
    overlap_policy: MoveOverlapPolicy,
) -> Result<(), AppError> {
    for resolved_move in moves {
        let updated_pairs =
            updated_move_pairs(&addressed.moved_pairs, &resolved_move, overlap_policy)?;
        let authority = resolved_move.authority();
        let transfer = promote_orphan_or_move_current_state(
            &mut addressed.managed_current,
            &mut addressed.orphans,
            &authority,
            state_file,
        )?;
        match transfer {
            MoveTransferOutcome::Transferred | MoveTransferOutcome::SourceHasNoCurrentInstance => {}
            MoveTransferOutcome::NotLoaded => {
                return Err(AppError::MoveSourceNotLoaded {
                    from: Box::new(resolved_move.from().clone()),
                    to: Box::new(resolved_move.to().clone()),
                });
            }
        }
        if let Some(keys) = addressed.prev_explicit.remove(authority.from()) {
            addressed.prev_explicit.insert(authority.to().clone(), keys);
        }
        addressed
            .saved_attrs
            .remap_resource_id(authority.from(), authority.to().clone());
        addressed.moved_pairs = updated_pairs;
    }
    Ok(())
}

/// Automatic moves detected once from the persisted pre-refresh view.
///
/// Its borrowed projection drives every phase-1 state read. The value itself
/// is then consumed by address reconciliation, which applies those exact
/// moves rather than running the detectors again.
struct DetectedAutomaticMoves {
    moves: Vec<ResolvedMove>,
}

impl DetectedAutomaticMoves {
    fn from_moves(moves: Vec<ResolvedMove>) -> Self {
        Self { moves }
    }

    fn state_read_projection<'state>(
        &self,
        state_file: Option<&'state StateFile>,
    ) -> ProjectedStateRead<'state> {
        ProjectedStateRead::from_moves(state_file, &self.moves)
    }

    fn into_moves(self) -> Vec<ResolvedMove> {
        self.moves
    }

    fn apply_after_explicit(
        self,
        addressed: &mut AddressedState,
        state_file: Option<&StateFile>,
    ) -> Result<(), AppError> {
        // Phase-1 reads were projected before explicit moves were materialized.
        // Refusing overlap here prevents an explicit -> detected chain from
        // applying state different from the source snapshot phase 1 read.
        apply_detected_resource_moves(
            addressed,
            self.into_moves(),
            state_file,
            MoveOverlapPolicy::RejectOverlap,
        )
    }
}

/// I/O-free state derived from the identity-reconciled configuration/state
/// view before managed and data-source refreshes run.
///
/// A caller must choose one of two typed transitions before it can obtain a
/// prepared expansion value:
///
/// - [`DeferredForExpansionPrelude::detect_moves`] followed by
///   [`ProjectedDeferredForExpansionPrelude::reconcile_addresses`] for
///   planning/apply views, which applies explicit moves and detected
///   anonymous-address moves;
/// - [`DeferredForExpansionPrelude::preserve_addresses`] for `state refresh`,
///   which must refresh the rows at their persisted addresses without applying
///   pending configuration transitions.
///
/// The transitions produce distinct prepared types: canonical plan/apply
/// expansion requires refreshed prefix information, while address-preserving
/// state refresh can expand only its raw parsed file. A future caller cannot
/// silently choose the wrong behavior with a flag or an empty metadata map.
pub struct DeferredForExpansionPrelude<'state> {
    preserved_move_scope: AddressedMoves,
    state_file: &'state Option<StateFile>,
    sorted_resources: Vec<Resource>,
    saved_attrs: LiftedSavedAttrs,
    prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    wait_aliases: Vec<WaitAliasSpec>,
}

/// Deferred-expansion prelude after automatic moves have been detected from
/// its owned resource order. Every projected phase-1 read and the eventual
/// address transition use this same value.
pub(crate) struct DetectedDeferredForExpansionPrelude<'state> {
    prelude: DeferredForExpansionPrelude<'state>,
    automatic_moves: DetectedAutomaticMoves,
}

/// Saved-plan move origins that this pre-expansion pipeline will replay.
///
/// A saved Move absent from this set was detected only after plan preparation;
/// its saved current snapshot must remain at the final address because secret
/// verification does not run the late detector.
pub(crate) struct SavedPlanReplayMoves {
    automatic_origins: HashSet<ResourceId>,
    explicit_origins: HashSet<ResourceId>,
}

impl SavedPlanReplayMoves {
    pub(crate) fn replays_origin(&self, origin: &ResourceId) -> bool {
        self.automatic_origins.contains(origin) || self.explicit_origins.contains(origin)
    }

    pub(crate) fn replays_as_explicit_move(&self, origin: &ResourceId) -> bool {
        self.explicit_origins.contains(origin)
    }

    #[cfg(test)]
    pub(crate) fn for_test_automatic(origins: impl IntoIterator<Item = ResourceId>) -> Self {
        Self {
            automatic_origins: origins.into_iter().collect(),
            explicit_origins: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_explicit(origins: impl IntoIterator<Item = ResourceId>) -> Self {
        Self {
            automatic_origins: HashSet::new(),
            explicit_origins: origins.into_iter().collect(),
        }
    }
}

/// Phase-1 state bound by value to the exact detected-move handle that
/// projected its reads and orphan selection.
pub(crate) struct ProjectedDeferredForExpansionPrelude<'state> {
    detected: DetectedDeferredForExpansionPrelude<'state>,
    managed_current: HashMap<ResourceId, Current>,
    orphans: Orphans,
    already_refreshed: HashSet<ResourceId>,
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
    pub(crate) fn new<E>(
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
        parsed: &carina_core::parser::ModuleResolvedFile<E>,
        state_file: &'state Option<StateFile>,
        schemas: &SchemaRegistry,
    ) -> Result<Self, AppError> {
        let sorted_resources =
            sort_resources_by_dependencies(&parsed.resources).map_err(AppError::Validation)?;
        Self::from_sorted_resources(parsed, sorted_resources, state_file, schemas)
    }

    #[cfg(test)]
    fn new_for_test<E>(
        parsed: &carina_core::parser::ModuleResolvedFile<E>,
        state_file: &'state Option<StateFile>,
        schemas: &SchemaRegistry,
    ) -> Result<Self, AppError> {
        Self::new_for_state_refresh(parsed, state_file, schemas)
    }

    fn from_sorted_resources<E>(
        parsed: &carina_core::parser::ModuleResolvedFile<E>,
        sorted_resources: Vec<Resource>,
        state_file: &'state Option<StateFile>,
        schemas: &SchemaRegistry,
    ) -> Result<Self, AppError> {
        Ok(Self {
            preserved_move_scope: AddressedMoves::preserve_addresses(&parsed.state_blocks),
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

    pub(crate) fn detect_moves(
        self,
        ctx: &WiringContext,
        comparison_provider: &ProviderRouter,
        data_sources: &[DataSource],
        providers: &[ProviderConfig],
        claims: &StateBlockClaims,
    ) -> Result<DetectedDeferredForExpansionPrelude<'state>, AppError> {
        let moves = if let Some(state_file) = self.state_file.as_ref() {
            let state_view = AnonymousReconciliationStateView::new(ctx, state_file, &[]);
            let mut moves = detect_legacy_loop_moves_with_ctx(
                ctx,
                comparison_provider,
                &self.sorted_resources,
                &state_view,
                claims,
                &[],
            )?;
            let projected_pairs = moves
                .iter()
                .map(|resolved_move| (resolved_move.from().clone(), resolved_move.to().clone()))
                .collect::<Vec<_>>();
            let state_view =
                AnonymousReconciliationStateView::new(ctx, state_file, &projected_pairs);
            moves.extend(reconcile_anonymous_identifiers_with_ctx(
                ctx,
                comparison_provider,
                &self.sorted_resources,
                data_sources,
                &state_view,
                claims,
            )?);
            let projected_pairs = moves
                .iter()
                .map(|resolved_move| (resolved_move.from().clone(), resolved_move.to().clone()))
                .collect::<Vec<_>>();
            moves.extend(detect_anonymous_to_named_moves_to_fixed_point(
                ctx,
                comparison_provider,
                &self.sorted_resources,
                providers,
                state_file,
                &projected_pairs,
                claims,
            )?);
            moves
        } else {
            Vec::new()
        };
        Ok(DetectedDeferredForExpansionPrelude {
            prelude: self,
            automatic_moves: DetectedAutomaticMoves::from_moves(moves),
        })
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
            wait_aliases: self.wait_aliases,
            already_refreshed: inputs.already_refreshed,
            state: AddressedState {
                managed_current: inputs.managed_current,
                orphans: inputs.orphans,
                saved_attrs: self.saved_attrs,
                prev_explicit: self.prev_explicit,
                moved_pairs: ResolvedMovePairs::from_addressed_moves(self.preserved_move_scope),
            },
        })
    }
}

pub(crate) struct ProjectedPhase1RefreshInputs<'a, 'backend> {
    pub provider: &'a dyn Provider,
    pub refresh_phase: &'a RefreshPhase<'backend>,
    pub schemas: &'a SchemaRegistry,
}

impl<'state> DetectedDeferredForExpansionPrelude<'state> {
    pub(crate) fn sorted_resources(&self) -> &[Resource] {
        &self.prelude.sorted_resources
    }

    pub(crate) fn saved_attrs(&self) -> &LiftedSavedAttrs {
        &self.prelude.saved_attrs
    }

    pub(crate) fn saved_dependency_bindings(&self) -> HashMap<ResourceId, ResolvedDependencies> {
        self.automatic_moves
            .state_read_projection(self.prelude.state_file.as_ref())
            .saved_dependency_bindings(&self.prelude.sorted_resources)
    }

    pub(crate) fn build_current(&self, desired: &ResourceId) -> (ResourceId, Current) {
        self.automatic_moves
            .state_read_projection(self.prelude.state_file.as_ref())
            .build_current(desired)
    }

    pub(crate) fn loaded_orphans(
        &self,
        additionally_reserved: impl IntoIterator<Item = ResourceId>,
    ) -> LoadedOrphans {
        let Some(state_file) = self.prelude.state_file.as_ref() else {
            return LoadedOrphans::default();
        };
        let desired_ids = self
            .automatic_moves
            .state_read_projection(self.prelude.state_file.as_ref())
            .persisted_resource_ids(&self.prelude.sorted_resources)
            .into_iter()
            .chain(additionally_reserved)
            .collect();
        state_file.build_orphans(&desired_ids)
    }

    pub(crate) fn into_destroy_prelude(
        self,
        moved_pairs: &ResolvedMovePairs,
    ) -> DeferredForExpansionPrelude<'state> {
        let Self {
            mut prelude,
            automatic_moves: _,
        } = self;
        prelude.sorted_resources =
            resources_at_destroy_addresses(prelude.sorted_resources, moved_pairs);
        prelude
    }

    pub(crate) fn saved_plan_replay_moves<E>(
        &self,
        parsed: &carina_core::parser::ModuleResolvedFile<E>,
    ) -> SavedPlanReplayMoves {
        let automatic_origins = self
            .automatic_moves
            .moves
            .iter()
            .map(|resolved_move| resolved_move.from().as_inner().clone())
            .collect::<HashSet<_>>();
        let mut explicit_origins = HashSet::new();
        if let Some(state_file) = self.prelude.state_file.as_ref() {
            for block in &parsed.state_blocks {
                let StateBlock::Moved { addresses } = block else {
                    continue;
                };
                if let Some(row) =
                    state_file.find_resource_by_state_block_address(&addresses.from())
                {
                    explicit_origins.insert(
                        resolved_state_row_id(row, row.directives.provider_instance.clone())
                            .into_inner(),
                    );
                }
            }
        }
        SavedPlanReplayMoves {
            automatic_origins,
            explicit_origins,
        }
    }

    pub(crate) async fn refresh_managed_resource_set(
        &self,
        inputs: ProjectedPhase1RefreshInputs<'_, '_>,
        managed_current: &mut HashMap<ResourceId, Current>,
    ) -> Result<(), AppError> {
        let saved_dep_bindings = self.saved_dependency_bindings();
        let projection = self
            .automatic_moves
            .state_read_projection(self.prelude.state_file.as_ref());
        refresh_resource_set_with_state_reads(
            RefreshResourceSetInputs {
                provider: inputs.provider,
                refresh_phase: inputs.refresh_phase,
                state_file: self.prelude.state_file.as_ref(),
                schemas: inputs.schemas,
                saved_dep_bindings: &saved_dep_bindings,
            },
            &projection,
            self.prelude.sorted_resources.iter(),
            managed_current,
        )
        .await
    }

    pub(crate) fn bind_projected_phase1_state(
        self,
        managed_current: HashMap<ResourceId, Current>,
        orphans: Orphans,
        already_refreshed: HashSet<ResourceId>,
    ) -> ProjectedDeferredForExpansionPrelude<'state> {
        ProjectedDeferredForExpansionPrelude {
            detected: self,
            managed_current,
            orphans,
            already_refreshed,
        }
    }
}

impl ProjectedDeferredForExpansionPrelude<'_> {
    /// Apply every address-changing state transition once and retain the exact
    /// metadata/alias/exclusion inputs used by expansion.
    pub(crate) fn reconcile_addresses<E>(
        self,
        parsed: &carina_core::parser::ModuleResolvedFile<E>,
        warning_policy: DeferredForWarningPolicy,
    ) -> Result<ReconciledDeferredForExpansion, AppError> {
        let ProjectedDeferredForExpansionPrelude {
            detected:
                DetectedDeferredForExpansionPrelude {
                    mut prelude,
                    automatic_moves,
                },
            mut managed_current,
            mut orphans,
            already_refreshed,
        } = self;
        let prefix_state_info = refreshed_prefix_state_info(
            &prelude.sorted_resources,
            prelude.state_file,
            &managed_current,
        );
        let mut print_warning = |warning: String| eprintln!("{}", warning.yellow());
        let mut suppress_warning = |_: String| {};
        let warning_sink: &mut dyn FnMut(String) = match warning_policy {
            DeferredForWarningPolicy::Print => &mut print_warning,
            DeferredForWarningPolicy::Suppress => &mut suppress_warning,
        };
        let moved_pairs = materialize_moved_states_with_warning_sink(
            &mut managed_current,
            &mut orphans,
            &mut prelude.prev_explicit,
            &mut prelude.saved_attrs,
            &parsed.state_blocks,
            prelude.state_file,
            warning_sink,
        )?;
        let mut addressed = AddressedState {
            managed_current,
            orphans,
            saved_attrs: prelude.saved_attrs,
            prev_explicit: prelude.prev_explicit,
            moved_pairs,
        };
        automatic_moves.apply_after_explicit(&mut addressed, prelude.state_file.as_ref())?;
        Ok(ReconciledDeferredForExpansion(
            AddressedDeferredForExpansionInputs {
                sorted_resources: prelude.sorted_resources,
                wait_aliases: prelude.wait_aliases,
                already_refreshed,
                state: addressed,
            },
            prefix_state_info,
        ))
    }
}

#[derive(Clone)]
struct AddressedDeferredForExpansionInputs {
    sorted_resources: Vec<Resource>,
    wait_aliases: Vec<WaitAliasSpec>,
    already_refreshed: HashSet<ResourceId>,
    state: AddressedState,
}

struct DeferredForExpansionInputs {
    addressed: AddressedDeferredForExpansionInputs,
    data_source_values: DataSourceValues,
}

/// Address-reconciled state before the plan path's data-source I/O phase has
/// finished. This type deliberately has no expansion method: callers must
/// populate its owned data-source snapshot and seal the complete view first.
pub struct ReconciledDeferredForExpansion(
    AddressedDeferredForExpansionInputs,
    RefreshedPrefixStateInfo,
);

/// Inputs shared by plan, live apply, and state refresh after each path has
/// completed its address transition and before deferred-`for` expansion.
pub(crate) struct AddressedDataSourceReadInputs<'a, 'backend> {
    pub provider: &'a dyn Provider,
    pub refresh_phase: Option<&'a RefreshPhase<'backend>>,
    pub compositions: &'a [LiveComposition],
    pub data_sources: &'a [DataSource],
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub schemas: &'a SchemaRegistry,
    pub saved_dep_bindings: &'a HashMap<ResourceId, ResolvedDependencies>,
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
    for (id, orphan) in &addressed.state.orphans {
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
                &addressed.state.managed_current,
                &addressed.state.orphans,
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
    pub(crate) async fn read_data_sources_phase2(
        &self,
        inputs: AddressedDataSourceReadInputs<'_, '_>,
    ) -> Result<ReadDataSourcesPhase2, AppError> {
        read_addressed_data_sources_phase2(&self.0, inputs).await
    }

    /// Seal the reconciled managed/data-source/orphan snapshots into the only
    /// state accepted by deferred `for` expansion.
    pub fn seal(self, data_source_values: DataSourceValues) -> PreparedDeferredForExpansion {
        PreparedDeferredForExpansion(
            DeferredForExpansionInputs {
                addressed: self.0,
                data_source_values,
            },
            self.1,
        )
    }
}

/// Address-preserving state prepared specifically for `state refresh`.
pub struct AddressPreservingDeferredForExpansion(AddressedDeferredForExpansionInputs);

impl AddressPreservingDeferredForExpansion {
    pub fn sorted_resources(&self) -> &[Resource] {
        &self.0.sorted_resources
    }

    pub fn managed_current(&self) -> &HashMap<ResourceId, Current> {
        &self.0.state.managed_current
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

    pub fn seal(
        self,
        data_source_values: DataSourceValues,
    ) -> PreparedAddressPreservingDeferredForExpansion {
        PreparedAddressPreservingDeferredForExpansion(DeferredForExpansionInputs {
            addressed: self.0,
            data_source_values,
        })
    }
}

/// Canonical expansion inputs paired with prefix information computed from the
/// refreshed current snapshot. Only this type exposes canonical `expand`.
pub struct PreparedDeferredForExpansion(DeferredForExpansionInputs, RefreshedPrefixStateInfo);

/// Address-preserving state-refresh inputs. This distinct type exposes only
/// raw-file expansion, so a preparation without refreshed prefix information
/// cannot reach canonical [`PreparedDeferredForExpansion::expand`].
pub struct PreparedAddressPreservingDeferredForExpansion(DeferredForExpansionInputs);

trait PreparedExpansionInputs {
    fn inputs_mut(&mut self) -> &mut DeferredForExpansionInputs;
}

impl PreparedExpansionInputs for PreparedDeferredForExpansion {
    fn inputs_mut(&mut self) -> &mut DeferredForExpansionInputs {
        &mut self.0
    }
}

impl PreparedExpansionInputs for PreparedAddressPreservingDeferredForExpansion {
    fn inputs_mut(&mut self) -> &mut DeferredForExpansionInputs {
        &mut self.0
    }
}

/// Planning metadata released only after the prepared value has been
/// consumed. This type cannot be converted back into an expansion witness;
/// callers that need both must expand first, then consume the preparation.
pub(crate) struct PreparedDeferredForExpansionParts {
    pub addressed: AddressedState,
    pub data_source_values: DataSourceValues,
    pub wait_aliases: Vec<WaitAliasSpec>,
}

impl PreparedDeferredForExpansion {
    pub(crate) fn into_parts(self) -> PreparedDeferredForExpansionParts {
        PreparedDeferredForExpansionParts {
            addressed: self.0.addressed.state,
            data_source_values: self.0.data_source_values,
            wait_aliases: self.0.addressed.wait_aliases,
        }
    }
}

impl PreparedAddressPreservingDeferredForExpansion {
    pub(crate) fn into_parts(self) -> PreparedDeferredForExpansionParts {
        PreparedDeferredForExpansionParts {
            addressed: self.0.addressed.state,
            data_source_values: self.0.data_source_values,
            wait_aliases: self.0.addressed.wait_aliases,
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
    pub residual_deferred_for: carina_core::parser::ResolvedDeferredForExpressions,
    /// Warnings after resolved/deferred loops have been removed from the
    /// parser view. Retained only by the raw-file test adapter.
    #[cfg(test)]
    residual_warnings: Vec<ParseWarning>,
    /// Complete finalized loop collection after expansion, retained only by
    /// the raw-file test adapter so it can move the collection as a unit.
    #[cfg(test)]
    post_expansion_deferred_for: carina_core::parser::ResolvedDeferredForExpressions,
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
        mut parsed: carina_core::parser::ModuleResolvedFile<E>,
    ) -> carina_core::parser::ModuleResolvedFile<E> {
        parsed.resources = self.sorted_resources;
        // Move the complete post-expansion collection as one finalized value;
        // external code never appends individual expressions.
        parsed.replace_deferred_for_expressions(self.post_expansion_deferred_for);
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

pub(crate) type PostExpansionConfiguration<E> = carina_core::resolved_graph::GraphConfiguration<
    E,
    carina_core::parser::ResolvedDeferredForPhase,
>;

impl<E> ExpandedCanonicalResources<E> {
    pub(crate) fn parsed(&self) -> &carina_core::parser::ModuleResolvedFile<E> {
        self.resources.parsed()
    }

    pub(crate) fn as_pair(&self) -> &CanonicalResourcePair<E> {
        &self.resources
    }

    fn into_configuration(self) -> carina_core::parser::ModuleResolvedFile<E> {
        self.resources.into_parts().0
    }

    fn into_plan_configuration(
        self,
        configured: &OverrideAwareResources,
        data_sources: &[DataSource],
    ) -> carina_core::parser::ModuleResolvedFile<E> {
        let mut parsed = self.into_configuration();
        // The override-aware owner contains configuration resources only and
        // has just reconciled their final anonymous addresses.
        parsed.resources = configured.resources().to_vec();
        parsed.data_sources = data_sources.to_vec();
        parsed
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
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
    resources: &[Resource],
    managed_current: &HashMap<ResourceId, Current>,
    data_source_values: &DataSourceValues,
    schemas: &carina_core::schema::SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    wait_aliases: &[WaitAliasSpec],
    provider: &ProviderRouter,
) -> Result<HashMap<ResourceId, carina_core::binding_index::ManagedBindingStability>, AppError> {
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        schemas,
        resources,
    );
    let optimistic_bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
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
    // Identity derivation and the one dependency-address transition both run
    // later. This early projection resolves only schema-owned lifecycle
    // metadata and keeps authored dependency operands out of resolved types.
    let stability = carina_core::differ::pre_graph_managed_binding_stability(
        &resolved,
        &plan_input_current,
        schemas,
        saved_attrs,
        prev_explicit,
        provider,
    )??;
    Ok(stability)
}

/// Expanded children whose prior-state transition has not run yet.
pub(crate) struct ChildrenPending {
    new_child_ids: HashSet<ResourceId>,
    refreshable_child_ids: RefreshableChildIds,
    printed_warnings: bool,
}

struct LiveChildrenSettled;
struct CachedChildrenSettled;
struct AbsentChildrenSettled;

enum PlanChildrenSettlementKind {
    Live,
    Cached,
    Absent,
}

/// Proof accepted by the plan path. The private kind records which of its
/// three permitted prior-state transitions produced the view.
pub(crate) struct PlanChildrenSettled(PlanChildrenSettlementKind);

enum ApplyChildrenSettlementKind {
    Live,
    Absent,
}

/// Proof accepted by live apply. Cached restoration is unrepresentable.
pub(crate) struct ApplyChildrenSettled(ApplyChildrenSettlementKind);

mod children_settlement {
    pub trait Sealed {}
}

impl children_settlement::Sealed for PlanChildrenSettled {}
impl children_settlement::Sealed for ApplyChildrenSettled {}

/// Canonical resources, state, and expansion metadata produced together by
/// [`PreparedDeferredForExpansion::expand_for_plan`].
///
/// The fields are private so a pre-expansion resource order cannot be paired
/// with a post-expansion canonical resource witness at the plan seam. The
/// `C` marker additionally prevents that seam from accepting expanded loop
/// children before their prior-state transition has completed.
struct PostExpansionPlanData<E> {
    expanded_resources: ExpandedCanonicalResources<E>,
    sorted_resources: Vec<Resource>,
    addressed: AddressedState,
    data_source_values: DataSourceValues,
    wait_aliases: Vec<WaitAliasSpec>,
    residual_deferred_for: carina_core::parser::ResolvedDeferredForExpressions,
    deferred_create_targets: Vec<DeferredCreateTarget>,
}

pub(crate) struct PostExpansionPlanView<E, C> {
    data: PostExpansionPlanData<E>,
    children: C,
}

impl<E, C> PostExpansionPlanView<E, C> {
    fn replace_children<D>(self, children: D) -> PostExpansionPlanView<E, D> {
        PostExpansionPlanView {
            data: self.data,
            children,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl<E, C> PostExpansionPlanView<E, C> {
    pub(crate) fn parsed(&self) -> &carina_core::parser::ModuleResolvedFile<E> {
        self.data.expanded_resources.parsed()
    }
}

struct LiveExpandedChildrenInputs<'a, 'backend> {
    provider: &'a dyn Provider,
    normalizer: &'a dyn ProviderNormalizer,
    state_file: &'a StateFile,
    saved_dep_bindings: &'a HashMap<ResourceId, ResolvedDependencies>,
    refresh_phase: &'a RefreshPhase<'backend>,
    schemas: &'a SchemaRegistry,
}

struct CachedExpandedChildrenInputs<'a> {
    normalizer: &'a dyn ProviderNormalizer,
    state_file: &'a StateFile,
    schemas: &'a SchemaRegistry,
}

impl<E> PostExpansionPlanView<E, ChildrenPending> {
    async fn settle_children_live(
        self,
        inputs: LiveExpandedChildrenInputs<'_, '_>,
    ) -> Result<PostExpansionPlanView<E, LiveChildrenSettled>, AppError> {
        let Self { mut data, children } = self;
        settle_expanded_children_live(&mut data, children, inputs).await?;
        Ok(PostExpansionPlanView {
            data,
            children: LiveChildrenSettled,
        })
    }

    async fn settle_children_from_cache(
        self,
        inputs: CachedExpandedChildrenInputs<'_>,
    ) -> Result<PostExpansionPlanView<E, CachedChildrenSettled>, AppError> {
        let Self { mut data, children } = self;
        settle_expanded_children_from_cache(&mut data, children, inputs).await?;
        Ok(PostExpansionPlanView {
            data,
            children: CachedChildrenSettled,
        })
    }

    fn settle_children_absent(
        self,
        schemas: &SchemaRegistry,
        refresh_phase: Option<&RefreshPhase<'_>>,
    ) -> PostExpansionPlanView<E, AbsentChildrenSettled> {
        let Self { mut data, children } = self;
        settle_expanded_children_absent(&mut data, children, schemas, refresh_phase);
        PostExpansionPlanView {
            data,
            children: AbsentChildrenSettled,
        }
    }
}

impl<E> PostExpansionPlanView<E, LiveChildrenSettled> {
    fn into_plan_settled(self) -> PostExpansionPlanView<E, PlanChildrenSettled> {
        self.replace_children(PlanChildrenSettled(PlanChildrenSettlementKind::Live))
    }

    fn into_apply_settled(self) -> PostExpansionPlanView<E, ApplyChildrenSettled> {
        self.replace_children(ApplyChildrenSettled(ApplyChildrenSettlementKind::Live))
    }
}

impl<E> PostExpansionPlanView<E, CachedChildrenSettled> {
    fn into_plan_settled(self) -> PostExpansionPlanView<E, PlanChildrenSettled> {
        self.replace_children(PlanChildrenSettled(PlanChildrenSettlementKind::Cached))
    }
}

impl<E> PostExpansionPlanView<E, AbsentChildrenSettled> {
    fn into_plan_settled(self) -> PostExpansionPlanView<E, PlanChildrenSettled> {
        self.replace_children(PlanChildrenSettled(PlanChildrenSettlementKind::Absent))
    }

    fn into_apply_settled(self) -> PostExpansionPlanView<E, ApplyChildrenSettled> {
        self.replace_children(ApplyChildrenSettled(ApplyChildrenSettlementKind::Absent))
    }
}

mod post_expansion_mode {
    pub trait Sealed {}
}

/// Preserve unresolved upstream markers for plan display and saved plans.
pub(crate) struct PlanMode;

/// Resolve against the lock-held live state used immediately by apply.
pub(crate) struct ApplyMode;

impl post_expansion_mode::Sealed for PlanMode {}
impl post_expansion_mode::Sealed for ApplyMode {}

pub(crate) struct PostExpansionResourceInputs<'a> {
    override_aware: &'a OverrideAwareResources,
    unresolved_override_aware: &'a OverrideAwareResources,
    resolved: Vec<ResolvedResource>,
    paired_unresolved: Vec<Resource>,
}

/// Consumer-specific semantics for the shared post-expansion seam.
///
/// The trait is sealed so each marker has one statically selected resource
/// output and one complete set of reference-resolution rules.
pub(crate) trait PostExpansionMode: post_expansion_mode::Sealed + Sized {
    type Resources;

    fn override_aware_build_mode<'a>(
        upstream_binding_names: &'a HashSet<&'a str>,
    ) -> OverrideAwareBuildMode<'a>;

    fn data_source_upstream_bindings<'a>(
        upstream_binding_names: &'a HashSet<&'a str>,
    ) -> Option<&'a HashSet<&'a str>>;

    fn state_block_upstream_bindings<'a>(
        upstream_binding_names: &'a HashSet<&'a str>,
        no_unresolved_upstream_bindings: &'a HashSet<&'a str>,
    ) -> &'a HashSet<&'a str>;

    fn build_resources(inputs: PostExpansionResourceInputs<'_>) -> Self::Resources;
}

/// Settlement capability required by one post-expansion consumer mode.
///
/// The sealed implementations below are the only legal pairings: plan accepts
/// all three transitions, while apply can receive only live or absent state.
pub(crate) trait ChildrenSettledFor<M: PostExpansionMode>:
    children_settlement::Sealed
{
    fn consume(self);
}

impl ChildrenSettledFor<PlanMode> for PlanChildrenSettled {
    fn consume(self) {
        match self.0 {
            PlanChildrenSettlementKind::Live
            | PlanChildrenSettlementKind::Cached
            | PlanChildrenSettlementKind::Absent => {}
        }
    }
}

impl ChildrenSettledFor<ApplyMode> for ApplyChildrenSettled {
    fn consume(self) {
        match self.0 {
            ApplyChildrenSettlementKind::Live | ApplyChildrenSettlementKind::Absent => {}
        }
    }
}

/// Inputs available after deferred-`for` expansion and any child refreshes.
///
/// This plan-construction seam is shared by production `plan`, live `apply`,
/// and the provider-less fixture planner. Provider behavior enters through the
/// two traits; this seam itself has no fixture-specific branches.
pub(crate) struct PostExpansionPlanInput<'a, E, S> {
    pub context: &'a WiringContext,
    pub expansion: PostExpansionPlanView<E, S>,
    pub environment: PostExpansionPlanEnvironment<'a>,
}

pub(crate) struct PostExpansionPlanEnvironment<'a> {
    pub data_sources: &'a [DataSource],
    pub provider: &'a ProviderRouter,
    pub normalizer: &'a dyn ProviderNormalizer,
    pub provider_configs: &'a [ProviderConfig],
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub state_file: &'a Option<StateFile>,
    pub state_block_claims: &'a StateBlockClaims,
    pub resolved_state_block_targets: &'a ResolvedStateBlockTargets,
    pub preparation_errors: Option<NonEmptyPlanErrors>,
}

/// Successfully resolved resource views retained for plan consumers.
pub(crate) struct PostExpansionResolvedResources {
    pub resolved: Vec<ResolvedResource>,
    pub unresolved: Vec<UnresolvedResource>,
}

/// Apply-time execution inputs derived by the same seam that built the plan.
pub(crate) struct PostExpansionApplyResources {
    pub resolved: Vec<ResolvedResource>,
    pub paired_unresolved: Vec<Resource>,
    /// Built at the shared seam, but surfaced at the old live-apply execution
    /// boundary so an invalid execution map cannot mask plan diagnostics or
    /// affect the export-only fast path.
    pub unresolved_by_resolved_id:
        Result<HashMap<ResolvedResourceId, UnresolvedResource>, ResolvedResourceOriginIndexError>,
    pub bindings: ResolvedBindings,
}

impl PostExpansionMode for PlanMode {
    type Resources = PostExpansionResolvedResources;

    fn override_aware_build_mode<'a>(
        upstream_binding_names: &'a HashSet<&'a str>,
    ) -> OverrideAwareBuildMode<'a> {
        OverrideAwareBuildMode::Plan {
            unresolved_upstream_bindings: upstream_binding_names,
        }
    }

    fn data_source_upstream_bindings<'a>(
        upstream_binding_names: &'a HashSet<&'a str>,
    ) -> Option<&'a HashSet<&'a str>> {
        Some(upstream_binding_names)
    }

    fn state_block_upstream_bindings<'a>(
        upstream_binding_names: &'a HashSet<&'a str>,
        _no_unresolved_upstream_bindings: &'a HashSet<&'a str>,
    ) -> &'a HashSet<&'a str> {
        upstream_binding_names
    }

    fn build_resources(inputs: PostExpansionResourceInputs<'_>) -> Self::Resources {
        let unresolved = inputs
            .paired_unresolved
            .into_iter()
            .map(UnresolvedResource::from_pre_resolve)
            .collect();
        PostExpansionResolvedResources {
            resolved: inputs.resolved,
            unresolved,
        }
    }
}

impl PostExpansionMode for ApplyMode {
    type Resources = PostExpansionApplyResources;

    fn override_aware_build_mode<'a>(
        _upstream_binding_names: &'a HashSet<&'a str>,
    ) -> OverrideAwareBuildMode<'a> {
        OverrideAwareBuildMode::Apply
    }

    fn data_source_upstream_bindings<'a>(
        _upstream_binding_names: &'a HashSet<&'a str>,
    ) -> Option<&'a HashSet<&'a str>> {
        None
    }

    fn state_block_upstream_bindings<'a>(
        _upstream_binding_names: &'a HashSet<&'a str>,
        no_unresolved_upstream_bindings: &'a HashSet<&'a str>,
    ) -> &'a HashSet<&'a str> {
        no_unresolved_upstream_bindings
    }

    fn build_resources(inputs: PostExpansionResourceInputs<'_>) -> Self::Resources {
        let unresolved_by_resolved_id = inputs
            .override_aware
            .unresolved_by_resolved_id_with_binding_sources(inputs.unresolved_override_aware);
        PostExpansionApplyResources {
            resolved: inputs.resolved,
            paired_unresolved: inputs.paired_unresolved,
            unresolved_by_resolved_id,
            bindings: inputs.override_aware.bindings().clone(),
        }
    }
}

/// Artifacts every post-expansion outcome carries, including the exact
/// canonical resource owner that crossed the seam.
pub(crate) struct PostExpansionArtifacts<R> {
    pub expanded_resources: R,
    pub residual_deferred_for: carina_core::parser::ResolvedDeferredForExpressions,
    pub plan: Plan,
    pub compositions: Vec<LiveComposition>,
    pub data_sources: Vec<DataSource>,
    pub managed_current: HashMap<ResourceId, Current>,
    pub data_source_values: DataSourceValues,
    pub moved_origins: HashMap<ResourceId, ResourceId>,
    pub prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    pub wait_aliases: Vec<WaitAliasSpec>,
    pub legacy_dependency_classification: Option<carina_state::LegacyDependencyClassification>,
    /// Graph-classified replacement for a pre-v21 state. Apply/refresh owners
    /// install this value before any write; read-only plan simply drops it.
    pub migrated_legacy_state: Option<StateFile>,
}

impl<R> PostExpansionArtifacts<R> {
    fn map_expanded_resources<T>(self, map: impl FnOnce(R) -> T) -> PostExpansionArtifacts<T> {
        let Self {
            expanded_resources,
            residual_deferred_for,
            plan,
            compositions,
            data_sources,
            managed_current,
            data_source_values,
            moved_origins,
            prev_explicit,
            wait_aliases,
            legacy_dependency_classification,
            migrated_legacy_state,
        } = self;
        PostExpansionArtifacts {
            expanded_resources: map(expanded_resources),
            residual_deferred_for,
            plan,
            compositions,
            data_sources,
            managed_current,
            data_source_values,
            moved_origins,
            prev_explicit,
            wait_aliases,
            legacy_dependency_classification,
            migrated_legacy_state,
        }
    }
}

/// A successfully constructed plan with the resource view selected by `M`.
pub(crate) struct PostExpansionPlan<M: PostExpansionMode, R> {
    pub artifacts: PostExpansionArtifacts<R>,
    pub resources: M::Resources,
}

/// A preparation failure with a diagnostic plan and the artifacts callers
/// need to construct their command-specific error context.
pub(crate) struct PostExpansionDiagnostics<R> {
    artifacts: PostExpansionArtifacts<R>,
    errors: NonEmptyPlanErrors,
}

impl<R> PostExpansionDiagnostics<R> {
    fn new(artifacts: PostExpansionArtifacts<R>, errors: NonEmptyPlanErrors) -> Self {
        Self { artifacts, errors }
    }

    pub(crate) fn errors(&self) -> &NonEmptyPlanErrors {
        &self.errors
    }

    pub(crate) fn into_artifacts(self) -> PostExpansionArtifacts<R> {
        self.artifacts
    }
}

/// The shared seam either constructs the mode's resource view or returns a
/// non-empty diagnostic plan. A successful outcome always has `M::Resources`.
pub(crate) enum PostExpansionOutcome<M: PostExpansionMode, R> {
    Planned(PostExpansionPlan<M, R>),
    Diagnostics(PostExpansionDiagnostics<R>),
}

impl<M: PostExpansionMode, R> PostExpansionOutcome<M, R> {
    fn map_expanded_resources<T>(self, map: impl FnOnce(R) -> T) -> PostExpansionOutcome<M, T> {
        match self {
            Self::Planned(PostExpansionPlan {
                artifacts,
                resources,
            }) => PostExpansionOutcome::Planned(PostExpansionPlan {
                artifacts: artifacts.map_expanded_resources(map),
                resources,
            }),
            Self::Diagnostics(diagnostics) => {
                let PostExpansionDiagnostics { artifacts, errors } = diagnostics;
                PostExpansionOutcome::Diagnostics(PostExpansionDiagnostics::new(
                    artifacts.map_expanded_resources(map),
                    errors,
                ))
            }
        }
    }
}

/// Build a plan from the exact resource/state view that crossed deferred-`for`
/// expansion.
pub(crate) async fn build_post_expansion_plan<M: PostExpansionMode, E>(
    input: PostExpansionPlanInput<'_, E, impl ChildrenSettledFor<M>>,
) -> Result<PostExpansionOutcome<M, PostExpansionConfiguration<E>>, AppError> {
    let PostExpansionPlanInput {
        context,
        expansion,
        environment,
    } = input;
    let PostExpansionPlanEnvironment {
        data_sources,
        provider,
        normalizer,
        provider_configs,
        remote_bindings,
        state_file,
        state_block_claims,
        resolved_state_block_targets,
        preparation_errors,
    } = environment;
    let PostExpansionPlanView { data, children } = expansion;
    children.consume();
    let PostExpansionPlanData {
        expanded_resources,
        sorted_resources,
        mut addressed,
        mut data_source_values,
        wait_aliases,
        residual_deferred_for,
        mut deferred_create_targets,
    } = data;
    // awscc#251 (follow-up to #3055): data sources do not flow through
    // `into_plan_input_current_map`, so lift them explicitly at this seam.
    lift_data_source_value_enum_leaves(&mut data_source_values, data_sources, context.schemas());
    let upstream_binding_names = expanded_resources.parsed().upstream_binding_names();
    let managed_stability = derive_managed_binding_stability(
        expanded_resources.parsed(),
        &sorted_resources,
        &addressed.managed_current,
        &data_source_values,
        context.schemas(),
        &addressed.saved_attrs,
        &addressed.prev_explicit,
        remote_bindings,
        &wait_aliases,
        provider,
    )?;
    let pre_apply_current = carina_core::resource::into_plan_input_current_map(
        addressed.managed_current.clone(),
        context.schemas(),
        &sorted_resources,
    );
    let projected_state = addressed.projected_state(state_file.as_ref());
    let (mut override_aware_resources, unresolved_override_aware_resources) =
        OverrideAwareResources::build_pair_with_stability(
            expanded_resources.as_pair(),
            &projected_state,
            LivePreApplyInputs {
                managed: &[],
                compositions: &expanded_resources.parsed().compositions,
                managed_current: &pre_apply_current,
                data_source_values: &data_source_values,
                remote_bindings,
                wait_aliases: &wait_aliases,
            },
            &managed_stability,
            M::override_aware_build_mode(&upstream_binding_names),
        )?;
    crate::legacy_name_overrides::emit_legacy_override_warnings(
        &override_aware_resources,
        &projected_state,
    );

    // Refresh-time reads get concrete inputs. Apply-time deferred reads retain
    // structural refs so saved-plan apply can rebuild their dependency graph.
    let mut data_sources_for_plan = prepare_data_sources_for_plan(
        &data_source_values,
        override_aware_resources.bindings(),
        M::data_source_upstream_bindings(&upstream_binding_names),
    )?;
    let constraint_origin_resources = override_aware_resources
        .paired_unresolved_resources_with_binding_sources(&unresolved_override_aware_resources);
    let mut compositions = expanded_resources.parsed().compositions.clone();
    let module_gate =
        carina_core::executor::ModuleConstraintGate::from_live_compositions(&compositions);
    let wait_bindings = expanded_resources.parsed().wait_bindings.clone();
    let state_blocks = expanded_resources.parsed().state_blocks.clone();
    let preparation = if let Some(errors) = preparation_errors {
        Err(PlanPreparationError::Plan(errors))
    } else {
        PlanPreprocessor::new(normalizer, context)
            .prepare(
                &mut override_aware_resources,
                &constraint_origin_resources,
                &module_gate,
                &mut addressed.managed_current,
                &mut data_source_values,
                &mut addressed.orphans,
                provider_configs,
                &mut data_sources_for_plan,
                data_sources,
            )
            .await
    };
    if let Err(error) = preparation {
        let errors = match error {
            PlanPreparationError::Plan(errors) => errors,
            PlanPreparationError::Provider(error) => return Err(AppError::Provider(error)),
        };
        let mut plan = Plan::new();
        for error in errors.as_slice().iter().cloned() {
            plan.add_error(error);
        }
        let moved_origins = addressed
            .moved_pairs
            .iter()
            .map(|(from, to)| (to.as_inner().clone(), from.as_inner().clone()))
            .collect();
        let artifacts = PostExpansionArtifacts {
            expanded_resources: PostExpansionConfiguration::for_diagnostics(
                expanded_resources.into_configuration(),
            ),
            residual_deferred_for,
            plan,
            compositions,
            data_sources: data_sources_for_plan,
            managed_current: into_all_prior_states(addressed.orphans, addressed.managed_current),
            data_source_values,
            moved_origins,
            prev_explicit: addressed.prev_explicit,
            wait_aliases,
            legacy_dependency_classification: None,
            migrated_legacy_state: None,
        };
        return Ok(PostExpansionOutcome::Diagnostics(
            PostExpansionDiagnostics::new(artifacts, errors),
        ));
    }

    // Anonymous identities that depend on normalized ResourceRefs become
    // concrete only here. State transfer and collision validation therefore
    // share this same addressed-state owner.
    let resolved_desired_ids = reconcile_late_anonymous_identities(
        context,
        LateAnonymousIdentityInputs {
            resources: &mut override_aware_resources,
            addressed: &mut addressed,
            data_sources: &data_sources_for_plan,
            state_file: state_file.as_ref(),
            state_block_claims,
            providers: provider_configs,
            comparison_provider: provider,
        },
    )?;
    validate_plan_time_state_block_collisions(
        &resolved_desired_ids,
        &addressed.moved_pairs,
        resolved_state_block_targets,
        state_file,
    )?;

    let validated = classify_legacy_state(
        expanded_resources
            .into_plan_configuration(&override_aware_resources, &data_sources_for_plan),
        MaterializedConfiguration::Planned,
        &addressed.moved_pairs,
        state_file.as_ref(),
    )?
    .report_and_validate()?;
    let legacy_dependency_classification = validated.classification;
    let migrated_legacy_state = validated.migrated_state;
    if let Some(migrated) = migrated_legacy_state.as_ref() {
        addressed.refresh_destroy_metadata_from_state(migrated);
    }
    let state_file = if migrated_legacy_state.is_some() {
        &migrated_legacy_state
    } else {
        state_file
    };
    let (expanded_resources, expanded_graph) = validated.graph.into_parts();
    compositions = expanded_resources.parsed().compositions.clone();
    for target in &mut deferred_create_targets {
        target.dependencies = expanded_graph.resolve_deferred_for_dependencies(&target.template)?;
    }

    let reconciled_provider_states = provider_states_for(
        &addressed.managed_current,
        &addressed.orphans,
        override_aware_resources.resources(),
    );
    let reconciled_input_current = carina_core::resource::into_plan_input_current_map(
        reconciled_provider_states,
        context.schemas(),
        override_aware_resources.resources(),
    );
    let stability_resources = override_aware_resources
        .resources()
        .iter()
        .cloned()
        .map(|resource| expanded_graph.resolve_resource(resource, context.schemas()))
        .collect::<Result<Vec<_>, _>>()?;
    let managed_stability = carina_core::differ::managed_binding_stability(
        &stability_resources,
        &reconciled_input_current,
        context.schemas(),
        &addressed.saved_attrs,
        &addressed.prev_explicit,
        provider,
    )?;
    override_aware_resources.rebuild_bindings_after_identity_reconciliation_with_stability(
        RebuildBindingsInputs {
            compositions: &compositions,
            managed_current: &reconciled_input_current,
            data_source_values: &data_source_values,
            remote_bindings,
            wait_aliases: &wait_aliases,
        },
        &managed_stability,
    );

    // Update ordering looks directives up only for Update effects. Orphan
    // directives stay paired with their typed orphan state for Delete effects.
    let directives_map = addressed
        .projected_state(state_file.as_ref())
        .build_directives();
    let paired_unresolved_resources = override_aware_resources
        .paired_unresolved_resources_with_binding_sources(&unresolved_override_aware_resources);
    let resolved_resources = override_aware_resources
        .resources()
        .iter()
        .cloned()
        .zip(paired_unresolved_resources.iter().cloned())
        .map(|(evaluated, authored)| {
            expanded_graph.resolve_resource_with_dependency_source(
                evaluated,
                authored,
                context.schemas(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let resolved_unresolved_resources = paired_unresolved_resources
        .iter()
        .cloned()
        .map(|resource| expanded_graph.resolve_resource(resource, context.schemas()))
        .collect::<Result<Vec<_>, _>>()?;
    let resolved_data_sources_for_plan = data_sources_for_plan
        .iter()
        .cloned()
        .map(|data_source| expanded_graph.resolve_data_source(data_source))
        .collect::<Result<Vec<_>, _>>()?;
    let resolved_wait_bindings = wait_bindings
        .iter()
        .map(|wait| expanded_graph.resolve_wait_binding(wait))
        .collect::<Result<Vec<_>, _>>()?;
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        addressed.managed_current.clone(),
        context.schemas(),
        override_aware_resources.resources(),
    );
    let outcome_unknown_import_replacements = outcome_unknown_import_replacements(
        &state_blocks,
        state_file,
        override_aware_resources.resources(),
    );
    let mut plan = create_plan_with_cascades(
        &resolved_resources,
        &resolved_unresolved_resources,
        &resolved_data_sources_for_plan,
        provider,
        PlanPriorStates::new(&plan_input_current, &addressed.orphans),
        &directives_map,
        context.schemas(),
        &addressed.saved_attrs,
        &addressed.prev_explicit,
        &resolved_wait_bindings,
        &outcome_unknown_import_replacements,
    )?;
    add_deposed_delete_effects(&mut plan, state_file, &addressed.moved_pairs);
    block_deletes_on_prior_consumer_updates(&mut plan, &directives_map);
    override_aware_resources.apply_plan_stability(&plan_input_current, &plan, context.schemas());

    // State blocks run after differ/deposed identifier errors so `removed`
    // remains the explicit recovery that can suppress those diagnostics.
    let upstream_binding_names = expanded_resources.parsed().upstream_binding_names();
    let no_unresolved_upstream_bindings = HashSet::new();
    let state_block_upstream_bindings =
        M::state_block_upstream_bindings(&upstream_binding_names, &no_unresolved_upstream_bindings);
    add_state_block_effects(
        &mut plan,
        &state_blocks,
        state_file,
        &addressed.moved_pairs,
        override_aware_resources.resources(),
        override_aware_resources.bindings(),
        state_block_upstream_bindings,
    );
    add_deferred_create_effects(&mut plan, &deferred_create_targets);

    let dependency_analysis = carina_core::effect::deps::build_effect_dependency_analysis(
        plan.effects(),
        resolved_desired_ids.ids.iter(),
        carina_core::effect::deps::ScheduleInputs::Apply,
    );
    let dependency_analysis = match dependency_analysis {
        Ok(analysis) => analysis,
        Err(failure) => failure.into_parts().0,
    };
    plan.record_unresolved_delete_ordering_analysis(&dependency_analysis);

    let moved_origins = addressed
        .moved_pairs
        .iter()
        .map(|(from, to)| (to.as_inner().clone(), from.as_inner().clone()))
        .collect();
    let prev_explicit = addressed.prev_explicit;
    let managed_current = into_all_prior_states(addressed.orphans, addressed.managed_current);
    let resources = M::build_resources(PostExpansionResourceInputs {
        override_aware: &override_aware_resources,
        unresolved_override_aware: &unresolved_override_aware_resources,
        resolved: resolved_resources,
        paired_unresolved: paired_unresolved_resources,
    });

    Ok(PostExpansionOutcome::Planned(PostExpansionPlan {
        artifacts: PostExpansionArtifacts {
            expanded_resources,
            residual_deferred_for,
            plan,
            compositions,
            data_sources: data_sources_for_plan,
            managed_current,
            data_source_values,
            moved_origins,
            prev_explicit,
            wait_aliases,
            legacy_dependency_classification,
            migrated_legacy_state,
        },
        resources,
    }))
}

impl<E> ValidatedConfig<PostExpansionPlanView<E, ChildrenPending>> {
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn retain_fixture_deferred_for(
        &mut self,
        managed_bindings: &HashSet<String>,
        non_managed: carina_core::parser::ResolvedDeferredForExpressions,
    ) {
        self.resources
            .inner
            .data
            .deferred_create_targets
            .retain(|target| {
                target
                    .template
                    .root_bindings()
                    .iter()
                    .any(|binding| managed_bindings.contains(binding))
            });
        self.resources
            .inner
            .data
            .residual_deferred_for
            .append_finalized(non_managed);
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) async fn settle_expanded_children_from_cache(
        self,
        normalizer: &dyn ProviderNormalizer,
        state_file: &StateFile,
    ) -> Result<ValidatedConfig<PostExpansionPlanView<E, PlanChildrenSettled>>, AppError> {
        let Self {
            wiring, resources, ..
        } = self;
        let ValidationReady { inner, origin } = resources;
        let settled = inner
            .settle_children_from_cache(CachedExpandedChildrenInputs {
                normalizer,
                state_file,
                schemas: wiring.context().schemas(),
            })
            .await?
            .into_plan_settled();
        Ok(ValidatedConfig {
            wiring,
            resources: ValidationReady {
                inner: settled,
                origin,
            },
            config_state: std::marker::PhantomData,
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn settle_expanded_children_absent(
        self,
    ) -> ValidatedConfig<PostExpansionPlanView<E, PlanChildrenSettled>> {
        let Self {
            wiring, resources, ..
        } = self;
        let ValidationReady { inner, origin } = resources;
        let settled = inner
            .settle_children_absent(wiring.context().schemas(), None)
            .into_plan_settled();
        ValidatedConfig {
            wiring,
            resources: ValidationReady {
                inner: settled,
                origin,
            },
            config_state: std::marker::PhantomData,
        }
    }
}

impl<E, S> ValidatedConfig<PostExpansionPlanView<E, S>> {
    /// Run the shared plan seam while retaining the exact wiring that validated
    /// and expanded the canonical resource pair.
    pub(crate) async fn finish_post_expansion_plan<M: PostExpansionMode>(
        self,
        environment: PostExpansionPlanEnvironment<'_>,
    ) -> Result<PostExpansionOutcome<M, ValidatedConfig<PostExpansionConfiguration<E>>>, AppError>
    where
        S: ChildrenSettledFor<M>,
    {
        let Self {
            wiring, resources, ..
        } = self;
        let ValidationReady {
            inner: expansion,
            origin,
        } = resources;
        let context = wiring.context();
        let outcome = build_post_expansion_plan::<M, E>(PostExpansionPlanInput {
            context,
            expansion,
            environment,
        })
        .await?;
        Ok(
            outcome.map_expanded_resources(|expanded_resources| ValidatedConfig {
                wiring,
                resources: ValidationReady {
                    inner: expanded_resources,
                    origin,
                },
                config_state: std::marker::PhantomData,
            }),
        )
    }
}

impl PreparedDeferredForExpansion {
    /// Expand against the exact addressed state/metadata view sealed by
    /// one of the prelude's typed address transitions, consuming the canonical
    /// pair into an [`ExpandedCanonicalResources`] witness.
    pub(crate) fn expand<E: Clone>(
        &self,
        mut resources: CanonicalResourcePair<E>,
        ctx: &WiringContext,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        warning_policy: DeferredForWarningPolicy,
        provider: &ProviderRouter,
    ) -> Result<(ExpandedCanonicalResources<E>, DeferredForExpansion), AppError> {
        resources.reconcile_refreshed_prefixed_names(&|provider, resource_type, identity| {
            self.1
                .get(&prefix_state_key(provider, resource_type, identity))
                .cloned()
        });
        let sorted_resources = resources.sorted_resources().map_err(AppError::Validation)?;
        let expansion = expand_same_config_deferred_for_target(
            &mut resources,
            &sorted_resources,
            &self.0.addressed.state.managed_current,
            &self.0.data_source_values,
            ctx.schemas(),
            &self.0.addressed.state.saved_attrs,
            &self.0.addressed.state.prev_explicit,
            remote_bindings,
            &self.0.addressed.wait_aliases,
            &self.0.addressed.state.moved_pairs,
            &self.0.addressed.already_refreshed,
            warning_policy,
            provider,
            ctx,
        )?;
        Ok((ExpandedCanonicalResources { resources }, expansion))
    }

    /// Consume the prepared state and canonical pair as one plan-expansion
    /// transition. The returned bundle is the only input accepted by the
    /// shared post-expansion plan builder.
    pub(crate) fn expand_for_plan<E: Clone>(
        self,
        resources: CanonicalResourcePair<E>,
        ctx: &WiringContext,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        warning_policy: DeferredForWarningPolicy,
        provider: &ProviderRouter,
    ) -> Result<PostExpansionPlanView<E, ChildrenPending>, AppError> {
        let (expanded_resources, expansion) =
            self.expand(resources, ctx, remote_bindings, warning_policy, provider)?;
        let PreparedDeferredForExpansionParts {
            addressed,
            data_source_values,
            wait_aliases,
        } = self.into_parts();
        let DeferredForExpansion {
            sorted_resources,
            residual_deferred_for,
            #[cfg(test)]
                residual_warnings: _,
            #[cfg(test)]
                post_expansion_deferred_for: _,
            deferred_create_targets,
            new_child_ids,
            refreshable_child_ids,
            printed_warnings,
            _expanded: _,
        } = expansion;
        Ok(PostExpansionPlanView {
            data: PostExpansionPlanData {
                expanded_resources,
                sorted_resources,
                addressed,
                data_source_values,
                wait_aliases,
                residual_deferred_for,
                deferred_create_targets,
            },
            children: ChildrenPending {
                new_child_ids,
                refreshable_child_ids,
                printed_warnings,
            },
        })
    }
}

impl PreparedAddressPreservingDeferredForExpansion {
    /// Raw-file expansion is reserved for address-preserving state refresh and
    /// focused tests; this type cannot invoke canonical prefix reconciliation.
    pub(crate) fn expand_parsed<E: Clone>(
        &self,
        parsed: &mut carina_core::parser::ModuleResolvedFile<E>,
        schemas: &SchemaRegistry,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        warning_policy: DeferredForWarningPolicy,
        provider: &ProviderRouter,
        identity_context: &WiringContext,
    ) -> Result<DeferredForExpansion, AppError> {
        expand_same_config_deferred_for_target(
            parsed,
            &self.0.addressed.sorted_resources,
            &self.0.addressed.state.managed_current,
            &self.0.data_source_values,
            schemas,
            &self.0.addressed.state.saved_attrs,
            &self.0.addressed.state.prev_explicit,
            remote_bindings,
            &self.0.addressed.wait_aliases,
            &self.0.addressed.state.moved_pairs,
            &self.0.addressed.already_refreshed,
            warning_policy,
            provider,
            identity_context,
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
    fn parsed(&self) -> &carina_core::parser::ModuleResolvedFile<E>;

    fn compute_anonymous_identifiers(
        &mut self,
        ctx: &WiringContext,
        providers: &[ProviderConfig],
    ) -> Result<(), AppError>;

    fn expand_partitioned<F>(
        &mut self,
        bindings: &carina_core::binding_index::IterableBindings<'_>,
        classify: F,
    ) -> Result<
        carina_core::parser::ResolvedDeferredForExpressions,
        carina_core::parser::DeferredForExpansionError,
    >
    where
        F: FnMut(&carina_core::parser::DeferredForExpression) -> DeferredForExpansionChoice;
}

impl<E> DeferredExpansionTarget<E> for CanonicalResourcePair<E> {
    fn parsed(&self) -> &carina_core::parser::ModuleResolvedFile<E> {
        self.parsed()
    }

    fn compute_anonymous_identifiers(
        &mut self,
        ctx: &WiringContext,
        providers: &[ProviderConfig],
    ) -> Result<(), AppError> {
        compute_canonical_pair_anonymous_identifiers_result_with_ctx(
            ctx,
            self,
            providers,
            identifier::AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        )
    }

    fn expand_partitioned<F>(
        &mut self,
        bindings: &carina_core::binding_index::IterableBindings<'_>,
        classify: F,
    ) -> Result<
        carina_core::parser::ResolvedDeferredForExpressions,
        carina_core::parser::DeferredForExpansionError,
    >
    where
        F: FnMut(&carina_core::parser::DeferredForExpression) -> DeferredForExpansionChoice,
    {
        self.expand_deferred_for_expressions(bindings, classify)
    }
}

impl<E> DeferredExpansionTarget<E> for carina_core::parser::ModuleResolvedFile<E> {
    fn parsed(&self) -> &carina_core::parser::ModuleResolvedFile<E> {
        self
    }

    fn compute_anonymous_identifiers(
        &mut self,
        ctx: &WiringContext,
        providers: &[ProviderConfig],
    ) -> Result<(), AppError> {
        let resources = carina_core::value::canonicalize_resources_with_schemas(
            &mut self.resources,
            ctx.schemas(),
        )?;
        compute_anonymous_identifiers_result_with_ctx(
            ctx,
            resources,
            providers,
            identifier::AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        )
    }

    fn expand_partitioned<F>(
        &mut self,
        bindings: &carina_core::binding_index::IterableBindings<'_>,
        mut classify: F,
    ) -> Result<
        carina_core::parser::ResolvedDeferredForExpressions,
        carina_core::parser::DeferredForExpansionError,
    >
    where
        F: FnMut(&carina_core::parser::DeferredForExpression) -> DeferredForExpansionChoice,
    {
        let (selected, retained) = self
            .take_deferred_for_expressions()
            .partition(|deferred| classify(deferred) == DeferredForExpansionChoice::Expand);
        self.warnings.retain(|warning| {
            warning.kind != WarningKind::DeferredFor
                || !retained
                    .iter()
                    .any(|deferred| deferred.file == warning.file && deferred.line == warning.line)
        });
        self.replace_deferred_for_expressions(selected);
        self.expand_deferred_for_expressions(bindings)?;
        let residual = self.deferred_for_expressions().clone();
        self.deferred_for_expressions_mut()
            .append_finalized(retained);
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
    provider: &ProviderRouter,
    identity_context: &WiringContext,
) -> Result<DeferredForExpansion, AppError> {
    let parsed = target.parsed();
    // Common case: no deferred-for at all. Skip the binding projection
    // and the whole-`File` clone entirely (this runs on every plan /
    // validate). Parse warnings still print — with no deferred-for,
    // expansion would remove none of them, so `parsed`'s set is exactly
    // what the post-expansion path would have printed.
    if parsed.deferred_for_expressions().is_empty() {
        let printed_warnings =
            warning_policy == DeferredForWarningPolicy::Print && parsed.print_warnings();
        return Ok(DeferredForExpansion {
            sorted_resources: sorted_resources.to_vec(),
            residual_deferred_for: parsed.deferred_for_expressions().empty_like(),
            #[cfg(test)]
            residual_warnings: parsed.warnings.clone(),
            #[cfg(test)]
            post_expansion_deferred_for: parsed.deferred_for_expressions().clone(),
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
        provider,
    )?;
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        schemas,
        sorted_resources,
    );
    let resolved_bindings = ResolvedBindings::pre_apply_live_with_stability(
        LivePreApplyInputs {
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
    let providers = target.parsed().providers.clone();
    target.compute_anonymous_identifiers(identity_context, &providers)?;
    let printed_warnings =
        warning_policy == DeferredForWarningPolicy::Print && target.parsed().print_warnings();
    #[cfg(test)]
    let residual_warnings = target.parsed().warnings.clone();
    let parsed = target.parsed();
    #[cfg(test)]
    let post_expansion_deferred_for = parsed.deferred_for_expressions().clone();

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
        #[cfg(test)]
        post_expansion_deferred_for,
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
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
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
    // Comparison fixtures may synthesize empty schemas for resource types the
    // focused test did not declare. Those schemas are comparison-only: feeding
    // them into anonymous identity assignment would turn a pre-existing
    // pending id into a generated id during expansion and make the resource
    // look like a newly materialized loop child.
    let identity_context = WiringContext::new_for_test_with_additional_schemas(
        Vec::new(),
        &parsed.providers,
        schemas
            .iter()
            .map(|(provider, _, _, schema)| (provider.to_string(), schema.clone())),
    );
    let (comparison_schemas, comparison_provider) =
        carina_core::provider::comparison_fixture_for_test(
            schemas,
            sorted_resources.iter().map(|resource| &resource.id),
        );
    let mut move_plan: Plan = Plan::default();
    for target in moved_targets {
        let target = ResolvedResourceId::new(target.clone());
        move_plan.add(Effect::Move {
            from: target.clone(),
            to: target,
            origin: None,
        });
    }
    let moved_pairs = ResolvedMovePairs::from_saved_plan(move_plan.move_effects());
    let prepared = PreparedAddressPreservingDeferredForExpansion(DeferredForExpansionInputs {
        addressed: AddressedDeferredForExpansionInputs {
            sorted_resources: sorted_resources.to_vec(),
            wait_aliases: wait_aliases.to_vec(),
            already_refreshed: already_refreshed.clone(),
            state: AddressedState {
                managed_current: managed_current.clone(),
                orphans: Orphans::new(),
                saved_attrs: saved_attrs.clone(),
                prev_explicit: prev_explicit.clone(),
                moved_pairs,
            },
        },
        data_source_values: data_source_values.clone(),
    });
    let mut expanded = parsed.clone();
    prepared.expand_parsed(
        &mut expanded,
        &comparison_schemas,
        remote_bindings,
        DeferredForWarningPolicy::Print,
        &comparison_provider,
        &identity_context,
    )
}

/// Result of the deferred-for expansion, materialised-child read,
/// prior-state hydration, and enum-lift stages shared by every refresh path.
///
/// Closes the bug class behind carina#3266 / #3271 / #3272 at the type
/// level — same closure pattern as
/// [`PostApplyStates`](crate::commands::shared::state_writeback::PostApplyStates).
/// State refresh obtains this through its [`ValidatedConfig`] owner
/// transition. Plan and apply instead use
/// [`PostExpansionPlanView`], whose settlement marker proves the same
/// child-state transition occurred before plan construction. The live
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
/// inside this operation prevents state refresh from transforming only one
/// part of prior state.
pub(crate) struct ExpandedRefreshState {
    pub(crate) sorted_resources: Vec<Resource>,
}

/// Shared inputs for the refresh-and-lift constructors.
///
/// Grouped into a struct because the underlying phases need many of
/// the same inputs (provider, schemas, wait_aliases, …) and a flat
/// arg list would already trip the 7-argument lint here.
pub(crate) struct ExpandRefreshAndLiftInputs<'a, 'backend, P: Provider + ProviderNormalizer, E> {
    pub provider: &'a P,
    pub comparison_provider: &'a ProviderRouter,
    /// Address/metadata parity witness produced by the shared I/O-free
    /// prelude. Expansion cannot be invoked with independently assembled
    /// saved attributes, aliases, or exclusion sets.
    pub expansion: &'a mut E,
    pub warning_policy: DeferredForWarningPolicy,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub state_file: &'a Option<StateFile>,
    pub saved_dep_bindings: &'a HashMap<ResourceId, ResolvedDependencies>,
    pub refresh_phase: &'a RefreshPhase<'backend>,
    pub schemas: &'a carina_core::schema::SchemaRegistry,
    pub identity_context: &'a WiringContext,
}

/// Refresh-and-lift inputs whose schema registry is supplied by the validated
/// configuration owner rather than as an independently selectable argument.
pub(crate) struct ValidatedExpandRefreshAndLiftInputs<
    'a,
    'backend,
    P: Provider + ProviderNormalizer,
    E,
> {
    pub provider: &'a P,
    pub comparison_provider: &'a ProviderRouter,
    pub expansion: &'a mut E,
    pub warning_policy: DeferredForWarningPolicy,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub state_file: &'a Option<StateFile>,
    pub saved_dep_bindings: &'a HashMap<ResourceId, ResolvedDependencies>,
    pub refresh_phase: &'a RefreshPhase<'backend>,
}

/// Canonical apply expansion inputs. The prepared expansion is consumed so its
/// addressed state cannot be paired with a different expanded resource view.
pub(crate) struct ValidatedCanonicalExpandRefreshAndLiftInputs<
    'a,
    'backend,
    P: Provider + ProviderNormalizer,
> {
    pub provider: &'a P,
    pub comparison_provider: &'a ProviderRouter,
    pub expansion: PreparedDeferredForExpansion,
    pub warning_policy: DeferredForWarningPolicy,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub state_file: &'a Option<StateFile>,
    pub saved_dep_bindings: &'a HashMap<ResourceId, ResolvedDependencies>,
    pub refresh_phase: &'a RefreshPhase<'backend>,
}

/// Run the expand → child-refresh → hydrate → lift quartet in
/// lock-step for address-preserving raw-file state refresh.
///
/// The unit resource witness makes the raw-file variant distinct from the
/// canonical plan/apply result at the type level.
async fn expand_refresh_and_lift_states<E: Clone, P: Provider + ProviderNormalizer>(
    parsed: &mut carina_core::parser::ModuleResolvedFile<E>,
    inputs: ExpandRefreshAndLiftInputs<'_, '_, P, PreparedAddressPreservingDeferredForExpansion>,
) -> Result<ExpandedRefreshState, AppError> {
    let expansion = inputs.expansion.expand_parsed(
        parsed,
        inputs.schemas,
        inputs.remote_bindings,
        inputs.warning_policy,
        inputs.comparison_provider,
        inputs.identity_context,
    )?;
    finish_expanded_refresh_state(expansion, inputs).await
}

/// Run the expand → child-refresh → hydrate → lift quartet in lock-step for a
/// canonical live-plan or live-apply resource pair.
///
/// The returned validated owner contains the complete post-expansion plan
/// view, so the refreshed state and resource order cannot be recombined by its
/// caller.
pub(crate) async fn expand_validated_canonical_refresh_and_lift_states<
    E: Clone,
    P: Provider + ProviderNormalizer,
>(
    validated: ValidatedConfig<CanonicalResourcePair<E>>,
    inputs: ValidatedCanonicalExpandRefreshAndLiftInputs<'_, '_, P>,
) -> Result<ValidatedConfig<PostExpansionPlanView<E, ApplyChildrenSettled>>, AppError> {
    let ValidatedConfig {
        wiring,
        resources: canonical_resources,
        config_state: _,
    } = validated;
    let ValidatedCanonicalExpandRefreshAndLiftInputs {
        provider,
        comparison_provider,
        expansion,
        warning_policy,
        remote_bindings,
        state_file,
        saved_dep_bindings,
        refresh_phase,
    } = inputs;
    let resources = {
        let identity_context = wiring.context();
        let schemas = identity_context.schemas();
        canonical_resources
            .try_map_async_preserving_binding_use_resolution(|canonical_resources| async move {
                let pending = expansion.expand_for_plan(
                    canonical_resources,
                    identity_context,
                    remote_bindings,
                    warning_policy,
                    comparison_provider,
                )?;
                let settled = if let Some(state_file) = state_file.as_ref() {
                    pending
                        .settle_children_live(LiveExpandedChildrenInputs {
                            provider,
                            state_file,
                            saved_dep_bindings,
                            refresh_phase,
                            schemas,
                            normalizer: provider,
                        })
                        .await?
                        .into_apply_settled()
                } else {
                    pending
                        .settle_children_absent(schemas, Some(refresh_phase))
                        .into_apply_settled()
                };
                Ok::<_, AppError>(settled)
            })
            .await?
    };
    Ok(ValidatedConfig {
        wiring,
        resources,
        config_state: std::marker::PhantomData,
    })
}

async fn settle_expanded_children_live<E>(
    data: &mut PostExpansionPlanData<E>,
    children: ChildrenPending,
    inputs: LiveExpandedChildrenInputs<'_, '_>,
) -> Result<(), AppError> {
    refresh_expanded_children_live(
        &data.sorted_resources,
        &mut data.addressed,
        &mut data.data_source_values,
        &children,
        inputs,
    )
    .await
}

async fn settle_expanded_children_from_cache<E>(
    data: &mut PostExpansionPlanData<E>,
    children: ChildrenPending,
    inputs: CachedExpandedChildrenInputs<'_>,
) -> Result<(), AppError> {
    restore_expanded_children_from_cache(
        &data.sorted_resources,
        &mut data.addressed,
        &mut data.data_source_values,
        &children,
        inputs,
    )
    .await
}

fn settle_expanded_children_absent<E>(
    data: &mut PostExpansionPlanData<E>,
    children: ChildrenPending,
    schemas: &SchemaRegistry,
    refresh_phase: Option<&RefreshPhase<'_>>,
) {
    mark_expanded_children_absent(
        &data.sorted_resources,
        &mut data.addressed,
        &children,
        schemas,
        refresh_phase,
    );
}

async fn refresh_expanded_children_live(
    sorted_resources: &[Resource],
    addressed: &mut AddressedState,
    data_source_values: &mut DataSourceValues,
    children: &ChildrenPending,
    inputs: LiveExpandedChildrenInputs<'_, '_>,
) -> Result<(), AppError> {
    if children.printed_warnings {
        inputs.refresh_phase.note_external_line();
    }
    if !children.new_child_ids.is_empty() {
        // Phase 2: read materialised children that were neither moved targets
        // nor already refreshed orphan rows.
        let projected_state = addressed.projected_state(Some(inputs.state_file));
        let selected = children
            .refreshable_child_ids
            .select(sorted_resources, &addressed.orphans);
        refresh_resource_set(
            RefreshResourceSetInputs {
                provider: inputs.provider,
                refresh_phase: inputs.refresh_phase,
                state_file: Some(inputs.state_file),
                schemas: inputs.schemas,
                saved_dep_bindings: inputs.saved_dep_bindings,
            },
            selected,
            &mut addressed.managed_current,
        )
        .await?;

        // Phase 2.5: carry saved attributes into newly read children. Keeping
        // hydration in the shared live transition prevents plan and apply from
        // diverging.
        hydrate_prior_data_source_values(
            &mut addressed.managed_current,
            data_source_values,
            &mut addressed.orphans,
            inputs.normalizer,
            &addressed.saved_attrs,
            &projected_state,
        )
        .await?;
    }
    lift_prior_state_enum_leaves(
        &mut addressed.managed_current,
        &mut addressed.orphans,
        sorted_resources,
        inputs.schemas,
    );
    Ok(())
}

async fn restore_expanded_children_from_cache(
    sorted_resources: &[Resource],
    addressed: &mut AddressedState,
    data_source_values: &mut DataSourceValues,
    children: &ChildrenPending,
    inputs: CachedExpandedChildrenInputs<'_>,
) -> Result<(), AppError> {
    if !children.new_child_ids.is_empty() {
        let projected_state = addressed.projected_state(Some(inputs.state_file));
        for resource in children
            .refreshable_child_ids
            .select(sorted_resources, &addressed.orphans)
        {
            let current = inputs.state_file.build_state_for_resource(&resource.id);
            addressed
                .managed_current
                .insert(resource.id.clone(), current);
        }
        hydrate_prior_data_source_values(
            &mut addressed.managed_current,
            data_source_values,
            &mut addressed.orphans,
            inputs.normalizer,
            &addressed.saved_attrs,
            &projected_state,
        )
        .await?;
    }
    lift_prior_state_enum_leaves(
        &mut addressed.managed_current,
        &mut addressed.orphans,
        sorted_resources,
        inputs.schemas,
    );
    Ok(())
}

fn mark_expanded_children_absent(
    sorted_resources: &[Resource],
    addressed: &mut AddressedState,
    children: &ChildrenPending,
    schemas: &SchemaRegistry,
    refresh_phase: Option<&RefreshPhase<'_>>,
) {
    if children.printed_warnings
        && let Some(refresh_phase) = refresh_phase
    {
        refresh_phase.note_external_line();
    }
    for resource in children
        .refreshable_child_ids
        .select(sorted_resources, &addressed.orphans)
    {
        addressed
            .managed_current
            .insert(resource.id.clone(), Current::absent(resource.id.clone()));
    }
    lift_prior_state_enum_leaves(
        &mut addressed.managed_current,
        &mut addressed.orphans,
        sorted_resources,
        schemas,
    );
}

async fn finish_expanded_refresh_state<
    P: Provider + ProviderNormalizer,
    E: PreparedExpansionInputs,
>(
    expansion: DeferredForExpansion,
    inputs: ExpandRefreshAndLiftInputs<'_, '_, P, E>,
) -> Result<ExpandedRefreshState, AppError> {
    let DeferredForExpansion {
        sorted_resources,
        residual_deferred_for: _,
        #[cfg(test)]
            residual_warnings: _,
        #[cfg(test)]
            post_expansion_deferred_for: _,
        deferred_create_targets: _,
        new_child_ids,
        refreshable_child_ids,
        printed_warnings,
        _expanded: _,
    } = expansion;
    let children = ChildrenPending {
        new_child_ids,
        refreshable_child_ids,
        printed_warnings,
    };
    let state_file = inputs.state_file.as_ref();
    let DeferredForExpansionInputs {
        addressed,
        data_source_values,
    } = inputs.expansion.inputs_mut();
    let addressed = &mut addressed.state;
    if let Some(state_file) = state_file {
        refresh_expanded_children_live(
            &sorted_resources,
            addressed,
            data_source_values,
            &children,
            LiveExpandedChildrenInputs {
                provider: inputs.provider,
                normalizer: inputs.provider,
                state_file,
                saved_dep_bindings: inputs.saved_dep_bindings,
                refresh_phase: inputs.refresh_phase,
                schemas: inputs.schemas,
            },
        )
        .await?;
    } else {
        mark_expanded_children_absent(
            &sorted_resources,
            addressed,
            &children,
            inputs.schemas,
            Some(inputs.refresh_phase),
        );
    }

    Ok(ExpandedRefreshState { sorted_resources })
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
        config_state: _,
    } = validated;
    let resources = {
        let context = wiring.context();
        canonical_resources
            .try_map_async_preserving_binding_use_resolution(|canonical_resources| async move {
                create_plan_from_parsed_with_upstream_with_ctx_inner(
                    context,
                    canonical_resources,
                    provider_configs,
                    state_file,
                    refresh,
                    progress_sink,
                    remote_bindings,
                    state_block_claims,
                    resolved_state_block_targets,
                )
                .await
            })
            .await?
    };
    Ok(ValidatedConfig {
        wiring,
        resources,
        config_state: std::marker::PhantomData,
    })
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
    // Initialize the exact provider routes before automatic move detection;
    // create-only matching needs each owning instance's comparison scope.
    let provider = get_provider_with_ctx(ctx, provider_configs, progress_sink).await?;
    let expansion_prelude =
        DeferredForExpansionPrelude::new(&canonical_resources, state_file, ctx.schemas())?
            .detect_moves(
                ctx,
                &provider,
                &parsed.data_sources,
                raw_provider_configs,
                state_block_claims,
            )?;
    // Refresh phases borrow the exact list retained by the typed prelude; no
    // second owned resource graph can diverge before the transition below.
    let sorted_resources = expansion_prelude.sorted_resources();
    let data_sources: Vec<DataSource> = parsed.data_sources.clone();
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

    let progress_backend = ProgressBackend::new(progress_sink);
    let refresh_phase = refresh.then(|| RefreshPhase::start(&progress_backend));
    // Provider reads do not retain Carina-only dependency metadata. Build the
    // restoration map once so both managed refresh and the unconditional
    // data-source phase consume the same snapshot.
    let saved_dep_bindings = expansion_prelude.saved_dependency_bindings();
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
        expansion_prelude
            .refresh_managed_resource_set(
                ProjectedPhase1RefreshInputs {
                    provider: provider_ref,
                    refresh_phase,
                    schemas: ctx.schemas(),
                },
                &mut managed_current,
            )
            .await?;

        // Refresh orphaned resources (#844). These are tracked in state
        // but removed from the .crn config — they're looked up by their
        // *old* address. Must run before automatic address reconciliation so
        // the transfer has the source state entries to move.
        if state_file.is_some() {
            let loaded_orphans = expansion_prelude.loaded_orphans(std::iter::empty());
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
                        Current::CreateOutcomeUnknown(_unknown) => true,
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

        // Transfer state for explicit `moved` blocks and every anonymous
        // address move detectable before phase 2, including anonymous →
        // let-bound moves (#1685). ResourceRef-derived anonymous identities
        // use the typed late-reconciliation pass after plan preparation.
        expansion_prelude
            .bind_projected_phase1_state(managed_current, orphans, orphan_refreshed_ids)
            .reconcile_addresses(parsed, DeferredForWarningPolicy::Print)?
    } else {
        // --refresh=false: use cached state from state file instead of calling provider.read()
        if state_file.is_some() {
            for resource in sorted_resources {
                let (loaded_id, current) = expansion_prelude.build_current(&resource.id);
                managed_current.insert(loaded_id, current);
            }

            // Also include orphaned resources from state file
            orphans = expansion_prelude
                .loaded_orphans(std::iter::empty())
                .into_plan_orphans();

            // Transfer state for explicit and detected anonymous address
            // moves so later ref and data-source-input resolution sees the
            // reconciled state view (#1685).
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
        expansion_prelude
            .bind_projected_phase1_state(managed_current, orphans, orphan_refreshed_ids)
            .reconcile_addresses(parsed, DeferredForWarningPolicy::Print)?
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
        .and_then(module_constraint_plan_errors);
    let data_source_values = phase2.values;
    let prepared_expansion = reconciled_expansion.seal(data_source_values);

    // Expansion yields a pending child-state witness. The plan seam accepts
    // only the settled witness produced by one of the three exhaustive prior-
    // state transitions below.
    let pending_expansion = prepared_expansion.expand_for_plan(
        canonical_resources,
        ctx,
        remote_bindings,
        DeferredForWarningPolicy::Print,
        &provider,
    )?;
    let child_saved_dep_bindings = HashMap::new();
    let plan_expansion = match (refresh_phase.as_ref(), state_file.as_ref()) {
        (Some(refresh_phase), Some(state_file)) => pending_expansion
            .settle_children_live(LiveExpandedChildrenInputs {
                provider: &provider,
                normalizer: &provider,
                state_file,
                saved_dep_bindings: &child_saved_dep_bindings,
                refresh_phase,
                schemas: ctx.schemas(),
            })
            .await?
            .into_plan_settled(),
        (None, Some(state_file)) => pending_expansion
            .settle_children_from_cache(CachedExpandedChildrenInputs {
                normalizer: &provider,
                state_file,
                schemas: ctx.schemas(),
            })
            .await?
            .into_plan_settled(),
        (_, None) => pending_expansion
            .settle_children_absent(ctx.schemas(), refresh_phase.as_ref())
            .into_plan_settled(),
    };

    drop(refresh_phase);
    let outcome = build_post_expansion_plan::<PlanMode, E>(PostExpansionPlanInput {
        context: ctx,
        expansion: plan_expansion,
        environment: PostExpansionPlanEnvironment {
            data_sources: &data_sources,
            provider: &provider,
            normalizer: &provider,
            provider_configs: raw_provider_configs,
            remote_bindings,
            state_file,
            state_block_claims,
            resolved_state_block_targets,
            preparation_errors: refresh_module_constraint_errors,
        },
    })
    .await?;
    let (artifacts, sorted_resources, unresolved_resources) = match outcome {
        PostExpansionOutcome::Planned(PostExpansionPlan {
            artifacts,
            resources,
        }) => (artifacts, resources.resolved, resources.unresolved),
        PostExpansionOutcome::Diagnostics(diagnostics) => {
            (diagnostics.into_artifacts(), Vec::new(), Vec::new())
        }
    };
    let PostExpansionArtifacts {
        expanded_resources,
        residual_deferred_for,
        plan,
        compositions,
        data_sources,
        managed_current,
        data_source_values,
        moved_origins,
        prev_explicit,
        wait_aliases: _,
        legacy_dependency_classification,
        migrated_legacy_state,
    } = artifacts;
    let expansion_trace = expanded_resources.parsed().expansion_trace.clone();
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
            sorted_resources,
            legacy_dependency_classification,
            migrated_legacy_state,
            unresolved_resources,
            compositions,
            data_sources,
            managed_current,
            data_source_values,
            moved_origins,
            upstream_snapshot: remote_bindings.clone(),
            prev_explicit,
            residual_deferred_for,
            expansion_trace,
        },
        expanded_resources,
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

/// One operator-authored move whose state source and desired destination have
/// both been resolved to scheduler addresses.
///
/// Keeping destination membership with the validated move lets the mutating
/// plan path transfer state while read-only migration consumers reuse the
/// exact same address resolution without fabricating a move pair themselves.
fn resolve_explicit_moved_blocks(
    state_blocks: &[StateBlock],
    state_file: &Option<StateFile>,
    mut desired_id: impl FnMut(&StateBlockAddress) -> Option<ResolvedResourceId>,
    warn_missing: &mut dyn FnMut(String),
) -> Result<AddressedMoves, AppError> {
    AddressedMoves::resolve(state_blocks, |addresses| {
        let from = addresses.from();
        let to = addresses.to();
        // `from`/`to` syntax has no provider-instance slot. Recover source
        // routing from state and destination routing from the finalized
        // desired resource set before minting the typed move.
        let resolved_from = state_file.as_ref().and_then(|state| {
            state
                .find_resource_by_state_block_address(&from)
                .map(|row| resolved_state_row_id(row, row.directives.provider_instance.clone()))
        });
        let Some(resolved_from) = resolved_from else {
            let to_exists = state_file
                .as_ref()
                .is_some_and(|state| state.find_resource_by_state_block_address(&to).is_some());
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
            return Ok(None);
        };
        let desired_to = desired_id(&to);
        let target_is_desired = desired_to.is_some();
        let resolved_to = desired_to.unwrap_or_else(|| to.to_unrouted_resource_id());
        Ok(Some((resolved_from, resolved_to, target_is_desired)))
    })
}

/// Resolve every explicit and late anonymous move against one materialized
/// configuration. Migration callers receive the opaque pair set rather than
/// constructing tuple slices at individual command sites.
pub(crate) fn resolve_moved_pairs<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
    resources: &[Resource],
    data_sources: &[DataSource],
    state_file: &Option<StateFile>,
) -> Result<ResolvedMovePairs, AppError> {
    let mut suppress_warning = |_: String| {};
    let explicit = resolve_explicit_moved_blocks(
        &parsed.state_blocks,
        state_file,
        |to| find_desired_id(to, resources.iter().map(|resource| &resource.id)),
        &mut suppress_warning,
    )?;
    let mut moved_pairs = ResolvedMovePairs::from_addressed_moves(explicit);
    let state_block_claims =
        resolve_state_block_claims(&parsed.state_blocks, state_file, resources, ctx.schemas());
    let comparison = MetadataComparisonResolver::new(resources.iter().map(|resource| &resource.id));
    let automatic = detect_late_anonymous_moves(LateAnonymousMoveDetectionInputs {
        ctx,
        comparison: &comparison,
        resources,
        data_sources,
        state_file: state_file.as_ref(),
        state_block_claims: &state_block_claims,
        providers: &parsed.providers,
        initial_moved_pairs: moved_pairs.as_slice(),
    })?;
    for resolved_move in automatic {
        moved_pairs = updated_move_pairs(
            &moved_pairs,
            &resolved_move,
            MoveOverlapPolicy::ComposeForward,
        )?;
    }
    Ok(moved_pairs)
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
) -> Result<ResolvedMovePairs, AppError> {
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
) -> Result<ResolvedMovePairs, AppError> {
    let resolved_moves = resolve_explicit_moved_blocks(
        state_blocks,
        state_file,
        |to| find_desired_id(to, managed_current.keys()),
        warn_missing,
    )?;

    for ResolvedExplicitMove {
        resolved_move,
        target_is_desired,
    } in resolved_moves.iter()
    {
        let authority = resolved_move.authority();

        // Transfer state from the old name to the new name so the
        // differ compares desired(to) against actual(from).
        let transfer = if *target_is_desired {
            promote_orphan_or_move_current_state(
                managed_current,
                orphans,
                &authority,
                state_file.as_ref(),
            )?
        } else if orphans.rekey(&authority) || rekey_current_for_move(managed_current, &authority)?
        {
            MoveTransferOutcome::Transferred
        } else {
            classify_unloaded_move_source(state_file.as_ref(), authority.from())
        };
        match transfer {
            MoveTransferOutcome::Transferred | MoveTransferOutcome::SourceHasNoCurrentInstance => {}
            MoveTransferOutcome::NotLoaded => {
                return Err(AppError::MoveSourceNotLoaded {
                    from: Box::new(resolved_move.from().clone()),
                    to: Box::new(resolved_move.to().clone()),
                });
            }
        }

        // Transfer prev_explicit so the differ detects attribute
        // removals under the new resource name.
        if let Some(keys) = prev_explicit.remove(authority.from()) {
            prev_explicit.insert(authority.to().clone(), keys);
        }

        // Transfer saved_attrs so create_plan can look up saved
        // attributes under the new resource name.
        saved_attrs.remap_resource_id(authority.from(), authority.to().clone());
    }

    Ok(ResolvedMovePairs::from_addressed_moves(resolved_moves))
}

fn promote_orphan_or_move_current_state(
    managed_current: &mut HashMap<ResourceId, Current>,
    orphans: &mut Orphans,
    authority: &MoveAuthority<'_>,
    state_file: Option<&StateFile>,
) -> Result<MoveTransferOutcome, carina_core::differ::CurrentMoveRoutingMismatch> {
    if let Some(current) = orphans.take_rebound(authority) {
        managed_current.insert(authority.to().clone(), current);
        Ok(MoveTransferOutcome::Transferred)
    } else if rekey_current_for_move(managed_current, authority)? {
        Ok(MoveTransferOutcome::Transferred)
    } else {
        Ok(classify_unloaded_move_source(state_file, authority.from()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveTransferOutcome {
    Transferred,
    SourceHasNoCurrentInstance,
    NotLoaded,
}

fn classify_unloaded_move_source(
    state_file: Option<&StateFile>,
    source: &ResourceId,
) -> MoveTransferOutcome {
    match state_file.and_then(|state_file| state_file.find_resource_for_id(source)) {
        Some(row) if !row.has_current_instance() => MoveTransferOutcome::SourceHasNoCurrentInstance,
        Some(_row) => MoveTransferOutcome::NotLoaded,
        None => MoveTransferOutcome::NotLoaded,
    }
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
            StateBlock::Import {
                to: import_to, id, ..
            } => {
                let Some(resource) = import_to.find_resource(desired) else {
                    continue;
                };
                if state_block_import(
                    ResolvedResourceId::new(resource.id.clone()),
                    state_file.as_ref(),
                    || id.clone(),
                )
                .is_some()
                {
                    to.insert(import_to.clone());
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

fn outcome_unknown_import_replacements(
    state_blocks: &[StateBlock],
    state_file: &Option<StateFile>,
    desired_resources: &[Resource],
) -> HashSet<OutcomeUnknownImportReplacement> {
    state_blocks
        .iter()
        .filter_map(|block| {
            let StateBlock::Import { to, id, .. } = block else {
                return None;
            };
            let resource = to.find_resource(desired_resources)?;
            state_block_import(
                ResolvedResourceId::new(resource.id.clone()),
                state_file.as_ref(),
                || id.clone(),
            )?
            .outcome_unknown_replacement
        })
        .collect()
}

struct StateBlockImport {
    effect: Effect,
    outcome_unknown_replacement: Option<OutcomeUnknownImportReplacement>,
}

/// Resolve one import block through the single target-disposition seam.
///
/// A create-outcome-unknown replacement witness can leave this function only
/// beside the `Effect::Import` for the same fully routed resource ID.
fn state_block_import(
    id: ResolvedResourceId,
    state_file: Option<&StateFile>,
    identifier: impl FnOnce() -> Value,
) -> Option<StateBlockImport> {
    let disposition = state_file
        .map(|state| state.import_target_disposition_for_id(id.as_inner()))
        .unwrap_or(carina_state::ImportTargetDisposition::Vacant);
    match disposition {
        carina_state::ImportTargetDisposition::Vacant => Some(StateBlockImport {
            effect: Effect::Import {
                id,
                identifier: identifier(),
            },
            outcome_unknown_replacement: None,
        }),
        carina_state::ImportTargetDisposition::ReplaceCreateOutcomeUnknown => {
            let (effect, replacement) = Effect::import_replacing_outcome_unknown(id, identifier());
            Some(StateBlockImport {
                effect,
                outcome_unknown_replacement: Some(replacement),
            })
        }
        carina_state::ImportTargetDisposition::Occupied => None,
    }
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
pub fn add_state_block_effects<D>(
    plan: &mut Plan,
    state_blocks: &[StateBlock],
    state_file: &Option<StateFile>,
    moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
    desired_resources: &[Resource<ModuleSource, D>],
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
            StateBlock::Import { to, to_span: _, id } => {
                // carina#3329: resolve any `${binding.attr}` segments in `id`
                // against the same binding view as resource attributes.
                // Deferred upstream-state refs remain typed placeholders for
                // display and apply-time resolution.
                let Some(import) = state_block_import(
                    resolve_import_target(to, desired_resources),
                    state_file.as_ref(),
                    || {
                        carina_core::resolver::resolve_value_for_plan(
                            id,
                            bindings,
                            unresolved_upstream_bindings,
                        )
                    },
                ) else {
                    continue;
                };
                let StateBlockImport {
                    effect,
                    outcome_unknown_replacement: _,
                } = import;
                let target_id = effect.resource_id().clone();
                let Effect::Import {
                    identifier: resolved_id,
                    ..
                } = &effect
                else {
                    unreachable!("state_block_import always returns an Import effect")
                };
                suppress_create.insert(target_id.clone());
                if carina_core::effect::resolve_import_identifier(resolved_id)
                    .is_ok_and(|identifier| AppliedIdentifier::new(identifier).is_err())
                {
                    plan.add_error(carina_core::plan::PlanError::new(
                        target_id,
                        carina_core::plan::PlanErrorKind::InvalidImportProviderIdentifier,
                    ));
                    continue;
                }
                new_effects.push(effect);
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
            origin: desired_resources
                .iter()
                .find(|resource| resource.id == *to.as_inner())
                .and_then(|resource| resource.loop_provenance.display_origin())
                .cloned(),
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
            Effect::Read { .. }
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => true,
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
                    binding,
                    dependencies,
                    explicit_dependencies,
                    blocked_by_updates,
                } = effect
                else {
                    return None;
                };

                if deletes_to_absorb.contains(id.as_inner())
                    || id.provider != target.template.template_resource.id.provider
                    || id.resource_type != target.template.template_resource.id.resource_type
                    || id.provider_instance
                        != target.template.template_resource.id.provider_instance
                {
                    return None;
                }
                let matches = binding.as_deref().is_some_and(|binding| {
                    binding_matches_deferred_template(binding, &target.template.target)
                });
                matches.then(|| DeferredReplaceDelete {
                    id: id.clone(),
                    identifier: identifier.clone(),
                    directives: directives.clone(),
                    binding: binding.clone(),
                    dependencies: dependencies.clone(),
                    explicit_dependencies: explicit_dependencies.clone(),
                    blocked_by_updates: blocked_by_updates.clone(),
                })
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
            Effect::Delete {
                generation: EffectGeneration::Deposed(_),
                ..
            }
            | Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => true,
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
    let row_id = resolved_state_row_id(row, row.directives.provider_instance.clone());
    let blocked_by_current_resume = match row.current() {
        carina_state::CurrentInstance::CreateInFlight { .. } => std::iter::once(
            moved_pairs
                .iter()
                .find_map(|(from, to)| (from == &row_id).then(|| to.clone()))
                .unwrap_or_else(|| row_id.clone()),
        )
        .collect(),
        carina_state::CurrentInstance::None
        | carina_state::CurrentInstance::Applied { .. }
        | carina_state::CurrentInstance::CreateOutcomeUnknown { .. }
        | carina_state::CurrentInstance::MissingIdentifier { .. } => {
            ResolvedDependencies::default()
        }
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
                dependencies: deposed.dependency_bindings.to_resolved_dependencies(),
                explicit_dependencies: ResolvedDependencies::default(),
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
pub(crate) fn resolve_import_target<D>(
    to: &StateBlockAddress,
    desired: &[Resource<ModuleSource, D>],
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
        Current::CreateOutcomeUnknown(unknown) => Ok(Current::CreateOutcomeUnknown(unknown)),
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
        Current::CreateOutcomeUnknown(_unknown) => None,
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
                Current::CreateOutcomeUnknown(unknown) => unknown.id(),
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
pub(crate) struct RefreshResourceSetInputs<'a, 'backend> {
    pub provider: &'a dyn Provider,
    pub refresh_phase: &'a RefreshPhase<'backend>,
    pub state_file: Option<&'a StateFile>,
    pub schemas: &'a SchemaRegistry,
    pub saved_dep_bindings: &'a HashMap<ResourceId, ResolvedDependencies>,
}

struct DirectManagedStateReads;

trait ManagedStateReads {
    fn build_current(
        &self,
        state_file: Option<&StateFile>,
        desired: &ResourceId,
    ) -> (ResourceId, Current);
}

impl ManagedStateReads for DirectManagedStateReads {
    fn build_current(
        &self,
        state_file: Option<&StateFile>,
        desired: &ResourceId,
    ) -> (ResourceId, Current) {
        let current = state_file
            .map(|state_file| state_file.build_state_for_resource(desired))
            .unwrap_or_else(|| Current::absent(desired.clone()));
        (desired.clone(), current)
    }
}

impl ManagedStateReads for ProjectedStateRead<'_> {
    fn build_current(
        &self,
        _state_file: Option<&StateFile>,
        desired: &ResourceId,
    ) -> (ResourceId, Current) {
        ProjectedStateRead::build_current(self, desired)
    }
}

/// Refresh resources at their own addresses. Post-expansion children use this
/// shape, which cannot accept an automatic-move projection.
pub(crate) async fn refresh_resource_set<'a>(
    inputs: RefreshResourceSetInputs<'_, '_>,
    resources: impl Iterator<Item = &'a Resource>,
    managed_current: &mut HashMap<ResourceId, Current>,
) -> Result<(), AppError> {
    refresh_resource_set_with_state_reads(
        inputs,
        &DirectManagedStateReads,
        resources,
        managed_current,
    )
    .await
}

async fn refresh_resource_set_with_state_reads<'a>(
    inputs: RefreshResourceSetInputs<'_, '_>,
    state_reads: &impl ManagedStateReads,
    resources: impl Iterator<Item = &'a Resource>,
    managed_current: &mut HashMap<ResourceId, Current>,
) -> Result<(), AppError> {
    let results: Vec<Result<(ResourceId, Current), AppError>> =
        // carina#3181: `resources` is a managed-resource iterator —
        // data sources go through the data-source refresh path and
        // compositions carry no provider state.
        stream::iter(resources)
            .map(|resource| {
                let progress = inputs.refresh_phase.begin(&resource.id);
                let (loaded_id, loaded) =
                    state_reads.build_current(inputs.state_file, &resource.id);
                let dep_bindings = inputs.saved_dep_bindings.get(&resource.id).cloned();
                async move {
                    let mut current = refresh_loaded_resource_with_retry(
                        inputs.provider,
                        loaded,
                        inputs.state_file,
                        inputs.schemas,
                    )
                    .await
                    .map_err(AppError::Provider)?;
                    if let Some(deps) = dep_bindings {
                        current.set_dependency_bindings(deps);
                    }
                    progress.finish();
                    Ok((loaded_id, current))
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
    pub compositions: &'a [LiveComposition],
    pub data_sources: &'a [DataSource],
    managed_current: ManagedCurrentView<'a>,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub schemas: &'a SchemaRegistry,
    pub wait_aliases: &'a [WaitAliasSpec],
    pub saved_dep_bindings: &'a HashMap<ResourceId, ResolvedDependencies>,
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
    pub compositions: &'a [LiveComposition],
    pub data_sources: &'a [DataSource],
    pub managed_current: &'a HashMap<ResourceId, Current>,
    pub orphans: &'a Orphans,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub schemas: &'a SchemaRegistry,
    pub wait_aliases: &'a [WaitAliasSpec],
    pub saved_dep_bindings: &'a HashMap<ResourceId, ResolvedDependencies>,
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
    compositions: &[LiveComposition],
    queries: &[DataSource],
    binding_data_sources: &[DataSource],
) -> Vec<LiveComposition> {
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
        match data_source.module_source.as_ref() {
            Some(ModuleSource::Module(source)) => {
                for (index, composition) in compositions.iter().enumerate() {
                    if composition
                        .call_path()
                        .is_prefix_of(source.module_call_path())
                    {
                        relevant_indices.insert(index);
                        pending.extend(get_composition_dependencies(composition));
                    }
                }
            }
            Some(ModuleSource::Root) | None => {}
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
    let module_gate =
        carina_core::executor::ModuleConstraintGate::from_live_compositions(&relevant_compositions);
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
    compositions: &[carina_core::resource::LiveComposition],
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
    compositions: &[carina_core::resource::LiveComposition],
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
    compositions: &[carina_core::resource::LiveComposition],
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
pub(crate) fn classify_apply_time_data_source_read_inputs<S>(
    data_sources: &[DataSource<S>],
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

fn classify_apply_time_data_source_read_inputs_from_refs<S>(
    data_sources: &[DataSource<S>],
    managed: &[&Resource],
    managed_current: ManagedCurrentView<'_>,
) -> HashMap<ResourceId, Vec<UnresolvedDataSourceInput>> {
    let mut deferred =
        classify_apply_time_data_source_reads(data_sources, managed, managed_current);
    propagate_deferred_data_source_chains(data_sources, &mut deferred);
    deferred
}

fn classify_apply_time_data_source_reads<S>(
    data_sources: &[DataSource<S>],
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
            // Directory reference resolution can fold an input such as
            // `"^${target.name}$"` to a concrete string. The structural
            // dependency is deliberately retained in `dependencies.value_bindings`;
            // consult it so using the expanded parse does not turn an
            // apply-time read into a premature refresh-time read.
            let recorded_dependency_requires_defer =
                resource.dependencies.value_bindings.iter().any(|binding| {
                    managed_by_binding
                        .get(binding.as_str())
                        .is_some_and(|target| {
                            managed_resource_is_missing_from_state(target, managed_current)
                        })
                });
            deferred_data_source_inputs(
                resource,
                &managed_by_binding,
                managed_current,
                recorded_dependency_requires_defer,
            )
            .map(|inputs| (resource.id.clone(), inputs))
        })
        .collect()
}

fn deferred_data_source_inputs<S, D>(
    resource: &DataSource<S, D>,
    managed_by_binding: &HashMap<&str, &Resource>,
    managed_current: ManagedCurrentView<'_>,
    recorded_dependency_requires_defer: bool,
) -> Option<Vec<UnresolvedDataSourceInput>> {
    let input_refs = unresolved_data_source_inputs(resource);
    let path_requires_defer = input_refs
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
    let has_unknown = input_refs.iter().any(|input| !input.unknowns.is_empty());
    (path_requires_defer
        || binding_requires_defer
        || recorded_dependency_requires_defer
        || has_unknown)
        .then_some(input_refs)
}

/// Test adapter for the saved-plan representation. Resolved data sources no
/// longer expose parser-phase binding snapshots, so this path must consume the
/// graph-resolved dependency addresses serialized beside the resource.
#[cfg(test)]
pub(crate) fn classify_apply_time_resolved_data_source_read_inputs<S>(
    data_sources: &[ResolvedDataSource<S>],
    managed: &[Resource],
    managed_current: &HashMap<ResourceId, Current>,
) -> HashMap<ResourceId, Vec<UnresolvedDataSourceInput>> {
    let managed_refs = managed.iter().collect::<Vec<_>>();
    let managed_by_binding = managed_refs
        .iter()
        .filter_map(|resource| {
            resource
                .binding
                .as_deref()
                .map(|binding| (binding, *resource))
        })
        .collect::<HashMap<_, _>>();
    let managed_by_id = managed
        .iter()
        .map(|resource| (&resource.id, resource))
        .collect::<HashMap<_, _>>();
    let managed_current = ManagedCurrentView::base(managed_current);
    let mut deferred = data_sources
        .iter()
        .filter_map(|resource| {
            let recorded_dependency_requires_defer =
                resource.value_dependencies().iter().any(|dependency| {
                    managed_by_id
                        .get(dependency.as_inner())
                        .is_some_and(|target| {
                            managed_resource_is_missing_from_state(target, managed_current)
                        })
                });
            deferred_data_source_inputs(
                resource.as_inner(),
                &managed_by_binding,
                managed_current,
                recorded_dependency_requires_defer,
            )
            .map(|inputs| (resource.id.clone(), inputs))
        })
        .collect::<HashMap<_, _>>();

    loop {
        let mut changed = false;
        for resource in data_sources {
            if deferred.contains_key(&resource.id) {
                continue;
            }
            if resource
                .value_dependencies()
                .iter()
                .any(|dependency| deferred.contains_key(dependency.as_inner()))
            {
                deferred.insert(
                    resource.id.clone(),
                    unresolved_data_source_inputs(resource.as_inner()),
                );
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    deferred
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
        Current::CreateOutcomeUnknown(_unknown) => false,
        Current::MissingIdentifier(_state) => false,
    }
}

fn propagate_deferred_data_source_chains<S>(
    data_sources: &[DataSource<S>],
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
                resource.dependencies.value_bindings.iter().any(|binding| {
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
    let mut parsed = ParsedFile::default();
    parsed.resources = resources.to_vec();
    let parsed = carina_core::module_resolver::resolve_modules_providerless(
        parsed,
        std::path::Path::new("."),
    )
    .expect("test fixture crosses module resolution");
    errors_to_legacy_result(validate_resources_with_ctx(
        &ctx,
        &parsed,
        &carina_core::parser::ProviderContext::default(),
    ))
}

#[cfg(test)]
pub fn resolve_names(resources: &mut [Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new_for_test(vec![], &[]);
    let mut parsed = carina_core::parser::ParsedFile::default();
    parsed.resources = resources.to_vec();
    let mut parsed = carina_core::module_resolver::resolve_modules_providerless(
        parsed,
        std::path::Path::new("."),
    )
    .expect("test fixture crosses module resolution");
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
