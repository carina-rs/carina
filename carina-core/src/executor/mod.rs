//! Plan executor - Executes a Plan by dispatching Effects to a Provider.
//!
//! This module contains the core execution logic extracted from the CLI apply command.
//! It uses an `ExecutionObserver` trait for UI separation, allowing the CLI to provide
//! colored progress output while keeping the execution logic testable.
//!
//! ## Module structure
//!
//! - `basic`: Single-effect execution (Create/Update/Delete), resource resolution, binding map
//! - `parallel`: Dependency computation and fine-grained parallel scheduling
//! - `deferred_dispatch`: DeferredCreate and DeferredReplace materialization/dispatch
//! - `replace`: Shared update patch helpers

pub(crate) mod basic;
mod deferred_dispatch;
pub mod normalized;
#[cfg(test)]
mod normalized_tests;
mod parallel;
pub mod provider_ready;
mod replace;
pub(super) mod scheduler;
pub(crate) mod wait;

pub use crate::data_source_value::UnresolvedDataSourceInput;
pub use crate::effect::deps::UnresolvedResource;
pub use provider_ready::{
    ModuleConstraintFailure, ModuleConstraintGate, ModuleConstraintGateError,
    ProviderConfigPreparationError, ProviderConfigValidationError, ProviderPreparationContext,
    ProviderPreparationError, ProviderReadyConfig, ProviderValueConstraintErrors,
    prepare_create_request, prepare_provider_ready_config, prepare_provider_ready_data_source,
    prepare_provider_ready_resource, prepare_update_request, provider_custom_type_lookup,
};
/// Phase of an executor scheduler endpoint after deferred replacements have
/// been expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanSchedulerPhase {
    /// The plan effect itself, including a deferred replacement's create gate.
    Primary,
    /// A delete absorbed into a [`Effect::DeferredReplace`].
    DeferredReplaceTeardown,
}

/// One endpoint of an executor dependency edge, projected back to the plan
/// effect that produced it while retaining its execution phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlanSchedulerEndpoint {
    pub effect_index: usize,
    pub phase: PlanSchedulerPhase,
}

/// Build the executor's apply-order edges for a plan, projected back to the
/// original plan effects.
///
/// Deferred replacements are expanded exactly as they are at execution time;
/// endpoint phase preserves whether an edge touches an absorbed teardown.
/// Dependency-analysis failures retain their partial edge graph, matching the
/// scheduler's behavior for unaffected effects.
pub fn build_plan_scheduler_dependency_edges(
    plan_effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, &UnresolvedResource>,
    compositions: &[crate::resource::LiveComposition],
) -> BTreeSet<(PlanSchedulerEndpoint, PlanSchedulerEndpoint)> {
    let parallel::ExpandedEffects {
        effects,
        deferred_replace_delete_deps,
        source_effect_indices,
    } = parallel::expand_deferred_replace_effects(plan_effects);
    let deferred_replace_teardowns: HashSet<_> = deferred_replace_delete_deps
        .iter()
        .map(|(_, delete_idx)| *delete_idx)
        .collect();
    let scheduler::SchedulerDeps { deps_of, .. } = scheduler::build_scheduler_deps_from_resources(
        &effects,
        unresolved_resources,
        compositions,
        &deferred_replace_delete_deps,
    );

    deps_of
        .into_iter()
        .flat_map(|(dependent, dependencies)| {
            let source_effect_indices = &source_effect_indices;
            let deferred_replace_teardowns = &deferred_replace_teardowns;
            dependencies.into_iter().filter_map(move |dependency| {
                let endpoint = |expanded_idx| {
                    Some(PlanSchedulerEndpoint {
                        effect_index: *source_effect_indices.get(expanded_idx)?,
                        phase: if deferred_replace_teardowns.contains(&expanded_idx) {
                            PlanSchedulerPhase::DeferredReplaceTeardown
                        } else {
                            PlanSchedulerPhase::Primary
                        },
                    })
                };
                let from = endpoint(dependency)?;
                let to = endpoint(dependent)?;
                (from != to).then_some((from, to))
            })
        })
        .collect()
}

use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use crate::binding_index::{ResolvedBindings, SecretPreservingAttributes};
use crate::effect::{DeletedInstanceKey, Effect};
use crate::parser::ProviderConfig;
use crate::provider::{
    AppliedIdentifier, CreateFailure, CreateFailureKind, CreateOutcome, CreateRequest,
    InFlightCreate, ObservedCreate, PartialReadDiagnostic, PatchOp, Provider, ProviderError,
    ProviderNormalizer, ProviderReadyDataSource, ResumeCreateRequest, ResumeFailure,
    ResumeFailureKind, ResumeOutcome, UpdateRequest,
};
use crate::resource::{
    ConcreteValue, Current, DataSource, DeferredValue, InterpolationPart, ResolvedResource,
    Resource, ResourceId, State, UnknownReason, Value,
};
use crate::wait::WaitObservation;

use crate::shutdown::ShutdownToken;
use parallel::execute_effects_sequential;

pub const TEST_UNCAPPED: NonZeroUsize = NonZeroUsize::new(usize::MAX).unwrap();

/// Dispatch an already checked create request while minting the unforgeable
/// initial-attempt witness inside core.
pub async fn dispatch_create<P: Provider + ?Sized>(
    provider: &P,
    id: &ResourceId,
    request: CreateRequest,
) -> Result<CreateOutcome, CreateFailure> {
    let request_attributes: BTreeSet<_> = request
        .resource()
        .as_resource()
        .attributes
        .keys()
        .cloned()
        .collect();
    match provider
        .create(id, request, crate::provider::CreateAttempt::new(id.clone()))
        .await
    {
        Ok(outcome) if outcome.id() == id => Ok(outcome),
        Ok(_outcome) => Err(CreateFailure::untracked_at_dispatch(
            ProviderError::internal("provider returned a create outcome for another resource")
                .for_resource(id.clone()),
        )),
        Err(failure) => match failure.into_kind() {
            CreateFailureKind::Created { created, error } => {
                if created.id() != id {
                    return Err(CreateFailure::untracked_at_dispatch(
                        ProviderError::internal_caused_by(
                            "provider returned a created failure for another resource",
                            error,
                        )
                        .for_resource(id.clone()),
                    ));
                }
                let (created, valid) = created.validate_request_sent(&request_attributes);
                let error = if valid {
                    error
                } else {
                    ProviderError::internal_caused_by(
                        "provider returned sent attributes outside the checked create request",
                        error,
                    )
                    .for_resource(id.clone())
                };
                Err(CreateFailure::created(created, error))
            }
            CreateFailureKind::InFlight { pending, error } => {
                if pending.id() != id {
                    return Err(CreateFailure::untracked_at_dispatch(
                        ProviderError::internal_caused_by(
                            "provider returned an in-flight create for another resource",
                            error,
                        )
                        .for_resource(id.clone()),
                    ));
                }
                let (pending, valid) = pending.validate_fresh_sent(&request_attributes);
                let error = if valid {
                    error
                } else {
                    ProviderError::internal_caused_by(
                        "provider returned sent attributes outside the checked create request",
                        error,
                    )
                    .for_resource(id.clone())
                };
                Err(CreateFailure::in_flight(pending, error))
            }
            CreateFailureKind::Untracked(error) => Err(CreateFailure::untracked_at_dispatch(error)),
        },
    }
}

/// Dispatch a checked resume request through the same sent-attribute
/// validation seam as an initial create.
pub async fn dispatch_resume_create<P: Provider + ?Sized>(
    provider: &P,
    request: ResumeCreateRequest,
) -> Result<ResumeOutcome, ResumeFailure> {
    let id = request.id().clone();
    let starting = request.pending_for_dispatch();
    let request_attributes: BTreeSet<_> = request
        .request()
        .resource()
        .as_resource()
        .attributes
        .keys()
        .cloned()
        .collect();
    match provider.resume_create(request).await {
        Ok(outcome) if outcome.id() == &id => Ok(outcome),
        Ok(_outcome) => Err(starting.into_resume_failure(
            ProviderError::internal("provider returned a resume outcome for another resource")
                .for_resource(id),
        )),
        Err(failure) => match failure.into_kind() {
            ResumeFailureKind::Created { created, error } => {
                if created.id() != &id {
                    return Err(starting.into_resume_failure(
                        ProviderError::internal_caused_by(
                            "provider returned a resumed failure for another resource",
                            error,
                        )
                        .for_resource(id),
                    ));
                }
                let (created, valid) = created.validate_request_sent(&request_attributes);
                let error = if valid {
                    error
                } else {
                    ProviderError::internal_caused_by(
                        "provider returned sent attributes outside the checked create request",
                        error,
                    )
                    .for_resource(id)
                };
                Err(ResumeFailure::created(created, error))
            }
            ResumeFailureKind::InFlight { pending, error } => {
                if pending.id() != &id {
                    return Err(starting.into_resume_failure(
                        ProviderError::internal_caused_by(
                            "provider returned a resumed continuation for another resource",
                            error,
                        )
                        .for_resource(id),
                    ));
                }
                let (pending, valid) = pending.validate_fresh_sent(&request_attributes);
                let error = if valid {
                    error
                } else {
                    ProviderError::internal_caused_by(
                        "provider returned sent attributes outside the checked create request",
                        error,
                    )
                    .for_resource(id)
                };
                Err(ResumeFailure::in_flight(pending, error))
            }
            ResumeFailureKind::Ended(error) => Err(ResumeFailure::ended_from_verdict(error)),
        },
    }
}

/// The accepted provider request whose result produced a complete state.
///
/// Update `sent` contains exactly the attributes for which the accepted patch
/// carried a value. Removals stay separate so writeback can drop a previously
/// persisted write-only value without treating the absent desired value as sent.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedRequest {
    kind: AcceptedRequestKind,
    /// Provider-ready desired attributes accepted for this operation. This is
    /// kept separate from the plan-time authored resource so state writeback
    /// masks secrets against the value shape actually sent to the provider.
    resolved_desired_attributes: Option<SecretPreservingAttributes>,
}

/// Read-only shape of the request accepted by the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptedRequestKind {
    Create,
    Update {
        sent: BTreeSet<String>,
        removed: BTreeSet<String>,
    },
}

impl AcceptedRequest {
    /// Record a successful create from the checked executor path.
    pub(crate) fn create() -> Self {
        Self {
            kind: AcceptedRequestKind::Create,
            resolved_desired_attributes: None,
        }
    }

    fn from_update_request(
        request: &UpdateRequest,
        resolved_desired_attributes: SecretPreservingAttributes,
    ) -> Self {
        let mut sent = BTreeSet::new();
        let mut removed = BTreeSet::new();
        for op in &request.patch().ops {
            match op {
                PatchOp::Add { key, .. }
                | PatchOp::Replace { key, .. }
                | PatchOp::Reassert { key, .. } => {
                    sent.insert(key.clone());
                }
                PatchOp::Remove { key } => {
                    removed.insert(key.clone());
                }
            }
        }
        Self {
            kind: AcceptedRequestKind::Update { sent, removed },
            resolved_desired_attributes: Some(resolved_desired_attributes),
        }
    }

    pub(crate) fn with_resolved_desired_attributes(
        mut self,
        attributes: SecretPreservingAttributes,
    ) -> Self {
        self.resolved_desired_attributes = Some(attributes);
        self
    }

    /// Construct create provenance for fixtures and state-boundary tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn create_for_test() -> Self {
        Self::create()
    }

    /// Construct update provenance for fixtures and state-boundary tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn update_for_test(sent: &[&str], removed: &[&str]) -> Self {
        Self {
            kind: AcceptedRequestKind::Update {
                sent: sent.iter().map(|key| (*key).to_string()).collect(),
                removed: removed.iter().map(|key| (*key).to_string()).collect(),
            },
            resolved_desired_attributes: None,
        }
    }

    pub fn kind(&self) -> &AcceptedRequestKind {
        &self.kind
    }

    pub fn resolved_desired_attributes(&self) -> Option<&SecretPreservingAttributes> {
        self.resolved_desired_attributes.as_ref()
    }
}

/// Convert a successful provider create into the state and provenance used by
/// normal apply writeback. Partial-success diagnostics are folded into the
/// state before it is persisted.
pub fn complete_create_for_writeback(
    outcome: CreateOutcome,
) -> (State, AcceptedRequest, Option<PartialReadDiagnostic>) {
    let diagnostic = outcome.diagnostic().cloned();
    let state = outcome.into_state_for_writeback();
    (state, AcceptedRequest::create(), diagnostic)
}

/// Provenance-preserving result of one effect. State writeback must classify
/// every arm explicitly instead of treating every state-shaped value alike.
#[derive(Debug, Clone)]
pub enum Applied {
    Complete {
        state: State,
        request: AcceptedRequest,
    },
    Observed(Arc<ObservedCreate>),
    CreateInFlight(InFlightCreate),
    CreateEnded,
    Imported(State),
    DataSource(State),
    WaitSnapshot(State),
}

/// Input data required to execute a plan.
pub struct LiveExecutionInput<'a> {
    pub plan: &'a crate::plan::Plan,
    /// Explicitly identifies whether provider-bound effects contain authored
    /// values or plan-normalized values paired with their authored origins.
    /// The executor never infers this from a missing map entry.
    pub provider_check_inputs: ProviderCheckInputs<'a>,
    /// Virtual resources (module attribute containers). carina#3181:
    /// compositions are a distinct typestate from managed resources, so the
    /// executor's dependency walk receives them as their own slice. A
    /// managed resource that depends on `<module-instance>.<attr>` —
    /// where the module-instance binding is a composition — has that edge
    /// followed through the composition's own attribute refs (#2543).
    pub compositions: &'a [crate::resource::LiveComposition],
    pub bindings: ResolvedBindings,
    pub managed_current: HashMap<ResourceId, Current>,
    /// One executor-safe outcome for every configured data source. This type
    /// cannot contain a plan-error-blocked read.
    pub data_source_values: crate::data_source_value::ExecutableDataSourceValues,
    /// The same provider normalizer that ran at plan time
    /// (`PlanPreprocessor`). Apply-time reference re-resolution rebuilds
    /// attributes from the un-normalized source, so the executor must
    /// re-apply this after each resolution and before constructing the
    /// provider request — otherwise plan-time normalization is silently
    /// undone (carina#3060).
    pub normalizer: &'a dyn ProviderNormalizer,
    /// Provider configs whose default_tags participate in desired-side
    /// normalization before provider patch construction.
    pub provider_configs: &'a [ProviderConfig],
    /// Provider factories used for provider-defined custom type validation at
    /// the apply boundary. Enum resolution uses the schema registry below and
    /// never delegates spelling conversion to a factory.
    pub factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    /// Schema registry used to re-apply structural and enum canonicalization
    /// after apply-time reference resolution and provider normalization.
    pub schemas: &'a crate::schema::SchemaRegistry,
    /// Maximum concurrent provider operations.
    pub parallelism: NonZeroUsize,
    /// Successful state-only operations completed synchronously by the caller
    /// before provider dispatch.
    pub precompleted_success_count: usize,
    /// Required handoff for accumulated execution snapshots. Production
    /// callers must choose and construct the handoff explicitly; tests that do
    /// not persist effect results may use the test-only no-op constructor.
    pub checkpoint_sink: ExecutionCheckpointSink,
}

/// Saved-plan counterpart of [`LiveExecutionInput`].
///
/// Keeping the composition slice in a distinct input type prevents a
/// deserialized plan from entering the live validation/expansion lifecycle.
pub struct SavedExecutionInput<'a> {
    pub plan: &'a crate::plan::Plan,
    pub provider_check_inputs: ProviderCheckInputs<'a>,
    pub compositions: &'a [crate::resource::SavedComposition],
    pub bindings: ResolvedBindings,
    pub managed_current: HashMap<ResourceId, Current>,
    pub data_source_values: crate::data_source_value::ExecutableDataSourceValues,
    pub normalizer: &'a dyn ProviderNormalizer,
    pub provider_configs: &'a [ProviderConfig],
    pub factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    pub schemas: &'a crate::schema::SchemaRegistry,
    pub parallelism: NonZeroUsize,
    pub precompleted_success_count: usize,
    pub checkpoint_sink: ExecutionCheckpointSink,
}

#[derive(Clone, Copy)]
pub(super) enum RuntimeCompositions<'a> {
    Live(&'a [crate::resource::LiveComposition]),
    Saved(&'a [crate::resource::SavedComposition]),
}

impl RuntimeCompositions<'_> {
    fn module_constraint_gate(self) -> ModuleConstraintGate {
        match self {
            Self::Live(compositions) => ModuleConstraintGate::from_live_compositions(compositions),
            Self::Saved(compositions) => {
                ModuleConstraintGate::from_saved_compositions(compositions)
            }
        }
    }

    fn scheduler_dependencies(
        self,
        effects: &[Effect],
        unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
        deferred_replace_delete_deps: &[(usize, usize)],
    ) -> scheduler::SchedulerDeps {
        match self {
            Self::Live(compositions) => scheduler::build_scheduler_deps(
                effects,
                unresolved_resources,
                compositions,
                deferred_replace_delete_deps,
            ),
            Self::Saved(compositions) => scheduler::build_saved_scheduler_deps(
                effects,
                unresolved_resources,
                compositions,
                deferred_replace_delete_deps,
            ),
        }
    }
}

pub(super) struct RuntimeExecutionInput<'a> {
    pub(super) plan: &'a crate::plan::Plan,
    pub(super) provider_check_inputs: ProviderCheckInputs<'a>,
    pub(super) compositions: RuntimeCompositions<'a>,
    pub(super) bindings: ResolvedBindings,
    pub(super) managed_current: HashMap<ResourceId, Current>,
    pub(super) data_source_values: crate::data_source_value::ExecutableDataSourceValues,
    pub(super) normalizer: &'a dyn ProviderNormalizer,
    pub(super) provider_configs: &'a [ProviderConfig],
    pub(super) factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    pub(super) schemas: &'a crate::schema::SchemaRegistry,
    pub(super) parallelism: NonZeroUsize,
    pub(super) precompleted_success_count: usize,
    pub(super) checkpoint_sink: ExecutionCheckpointSink,
}

impl<'a> From<LiveExecutionInput<'a>> for RuntimeExecutionInput<'a> {
    fn from(input: LiveExecutionInput<'a>) -> Self {
        let LiveExecutionInput {
            plan,
            provider_check_inputs,
            compositions,
            bindings,
            managed_current,
            data_source_values,
            normalizer,
            provider_configs,
            factories,
            schemas,
            parallelism,
            precompleted_success_count,
            checkpoint_sink,
        } = input;
        Self {
            plan,
            provider_check_inputs,
            compositions: RuntimeCompositions::Live(compositions),
            bindings,
            managed_current,
            data_source_values,
            normalizer,
            provider_configs,
            factories,
            schemas,
            parallelism,
            precompleted_success_count,
            checkpoint_sink,
        }
    }
}

impl<'a> From<SavedExecutionInput<'a>> for RuntimeExecutionInput<'a> {
    fn from(input: SavedExecutionInput<'a>) -> Self {
        let SavedExecutionInput {
            plan,
            provider_check_inputs,
            compositions,
            bindings,
            managed_current,
            data_source_values,
            normalizer,
            provider_configs,
            factories,
            schemas,
            parallelism,
            precompleted_success_count,
            checkpoint_sink,
        } = input;
        Self {
            plan,
            provider_check_inputs,
            compositions: RuntimeCompositions::Saved(compositions),
            bindings,
            managed_current,
            data_source_values,
            normalizer,
            provider_configs,
            factories,
            schemas,
            parallelism,
            precompleted_success_count,
            checkpoint_sink,
        }
    }
}

/// Value provenance for provider-bound effects in one execution.
///
/// Runtime-synthesized deferred-for children are authored values and are
/// classified explicitly by the scheduler. Every resource or data-source
/// effect already present in a normalized plan must have an entry in the
/// corresponding origin map; a missing entry is an executor invariant error.
/// A raw origin map cannot implicitly select a validation mode:
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::executor::{ProviderCheckInputs, UnresolvedResource};
/// use carina_core::resource::ResourceId;
///
/// let origins: HashMap<ResourceId, UnresolvedResource> = HashMap::new();
/// let _: ProviderCheckInputs<'_> = (&origins).into();
/// ```
pub enum ProviderCheckInputs<'a> {
    Authored,
    PlanNormalized {
        resource_origins: &'a HashMap<ResourceId, UnresolvedResource>,
        data_source_origins: &'a [DataSource],
    },
}

impl ProviderCheckInputs<'_> {
    fn resource_origins(&self) -> Option<&HashMap<ResourceId, UnresolvedResource>> {
        match self {
            Self::Authored => None,
            Self::PlanNormalized {
                resource_origins, ..
            } => Some(resource_origins),
        }
    }

    pub(in crate::executor) fn resource_input(
        &self,
        resolved: &Resource,
        runtime_authored: bool,
    ) -> Result<provider_ready::CheckInput<Resource>, String> {
        if runtime_authored || matches!(self, Self::Authored) {
            return Ok(provider_ready::CheckInput::Authored(resolved.clone()));
        }
        let Self::PlanNormalized {
            resource_origins, ..
        } = self
        else {
            unreachable!("authored inputs returned above")
        };
        let authored = resource_origins.get(&resolved.id).ok_or_else(|| {
            format!(
                "executor invariant violated: missing authored value origin for plan-normalized resource {}",
                resolved.id
            )
        })?;
        Ok(provider_ready::CheckInput::PlanNormalized {
            resolved: resolved.clone(),
            authored: authored.as_resource().clone(),
        })
    }

    pub(in crate::executor) fn data_source_input(
        &self,
        resolved: &DataSource,
    ) -> Result<provider_ready::CheckInput<DataSource>, String> {
        match self {
            Self::Authored => Ok(provider_ready::CheckInput::Authored(resolved.clone())),
            Self::PlanNormalized {
                data_source_origins,
                ..
            } => {
                let authored = data_source_origins
                    .iter()
                    .find(|origin| origin.id == resolved.id)
                    .ok_or_else(|| {
                    format!(
                        "executor invariant violated: missing authored value origin for plan-normalized data source {}",
                        resolved.id
                    )
                })?;
                Ok(provider_ready::CheckInput::PlanNormalized {
                    resolved: resolved.clone(),
                    authored: authored.clone(),
                })
            }
        }
    }
}

fn visit_unknowns(value: &Value, f: &mut impl FnMut(&UnknownReason)) {
    match value {
        Value::Deferred(DeferredValue::Unknown(reason)) => f(reason),
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                visit_unknowns(item, f);
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for item in map.values() {
                visit_unknowns(item, f);
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let InterpolationPart::Expr(value) = part {
                    visit_unknowns(value, f);
                }
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for arg in args {
                visit_unknowns(arg.walker_value(), f);
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => visit_unknowns(access.base(), f),
        Value::Deferred(DeferredValue::Secret(inner)) => visit_unknowns(inner, f),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            visit_unknowns(value, f);
            visit_unknowns(default, f);
        }
        Value::Concrete(_)
        | Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::RedactedSecret { .. }) => {}
    }
}

pub fn unresolved_data_source_inputs(resource: &DataSource) -> Vec<UnresolvedDataSourceInput> {
    resource
        .attributes
        .iter()
        .filter_map(|(attribute, value)| {
            let mut paths = Vec::new();
            let mut bindings = Vec::new();
            let mut unknowns = Vec::new();
            value.visit_resource_refs(&mut |path| paths.push(path.clone()));
            value.visit_binding_refs(&mut |binding| bindings.push(binding.to_string()));
            visit_unknowns(value, &mut |reason| unknowns.push(reason.clone()));
            (!paths.is_empty() || !bindings.is_empty() || !unknowns.is_empty()).then(|| {
                UnresolvedDataSourceInput {
                    attribute: attribute.clone(),
                    paths,
                    bindings,
                    unknowns,
                }
            })
        })
        .collect()
}

fn is_throttling_error(err: &ProviderError) -> bool {
    let msg = err.to_string();
    msg.contains("ThrottlingException") || msg.contains("Rate exceeded")
}

/// Read a data source through the provider with the same throttling retry
/// policy the CLI refresh path uses.
pub async fn read_data_source_with_retry(
    provider: &dyn Provider,
    resource: &ProviderReadyDataSource,
) -> Result<State, ProviderError> {
    let max_retries = 3;
    for attempt in 0..=max_retries {
        match provider.read_data_source(resource).await {
            Ok(state) => return Ok(state),
            Err(e) if attempt < max_retries && is_throttling_error(&e) => {
                let delay = Duration::from_secs(1 << attempt);
                eprintln!(
                    "  Throttled reading {}, retrying in {}s...",
                    resource.id,
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Non-blocking handoff for accumulated execution snapshots.
///
/// The executor only clones the accumulated result into an `Arc` and invokes
/// this synchronous callback. Implementations must hand the snapshot to a
/// background consumer and must not perform state I/O in the callback.
#[derive(Clone)]
pub struct ExecutionCheckpointSink {
    publish: Arc<dyn Fn(Arc<ExecutionResult>) + Send + Sync>,
}

impl ExecutionCheckpointSink {
    pub fn new(publish: impl Fn(Arc<ExecutionResult>) + Send + Sync + 'static) -> Self {
        Self {
            publish: Arc::new(publish),
        }
    }

    /// Explicit sink for callers that do not persist executor results.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn noop() -> Self {
        Self::new(drop)
    }

    pub fn publish(&self, result: &ExecutionResult) {
        (self.publish)(Arc::new(result.clone()));
    }

    fn publish_owned(&self, result: ExecutionResult) {
        (self.publish)(Arc::new(result));
    }
}

impl std::fmt::Debug for ExecutionCheckpointSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionCheckpointSink")
            .finish_non_exhaustive()
    }
}

/// Result of executing a plan's effects.
#[derive(Clone)]
pub struct ExecutionResult {
    pub success_count: usize,
    pub failure_count: usize,
    pub partial_count: usize,
    pub partial_diagnostics: Vec<(ResourceId, PartialReadDiagnostic)>,
    pub skip_count: usize,
    pub applied_states: std::collections::HashMap<ResourceId, Applied>,
    pub runtime_synthesized_resources: Vec<ResolvedResource>,
    pub successfully_deleted: HashSet<DeletedInstanceKey>,
    pub permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>>,
    pub managed_current: HashMap<ResourceId, Current>,
    pub data_source_states: HashMap<ResourceId, State>,
    pub bindings: ResolvedBindings,
    pub failed_refreshes: HashSet<ResourceId>,
}

/// Mutable execution-result fields other than applied states.
///
/// Keeping `applied_states` out of this mutation view makes it impossible for
/// an executor branch to add a state without explicitly choosing whether its
/// identifier is published to downstream Wait effects.
#[derive(Clone)]
pub(super) struct ExecutionResultFields {
    pub(super) success_count: usize,
    pub(super) failure_count: usize,
    pub(super) partial_count: usize,
    pub(super) partial_diagnostics: Vec<(ResourceId, PartialReadDiagnostic)>,
    pub(super) skip_count: usize,
    pub(super) runtime_synthesized_resources: Vec<ResolvedResource>,
    pub(super) successfully_deleted: HashSet<DeletedInstanceKey>,
    pub(super) permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>>,
    pub(super) managed_current: HashMap<ResourceId, Current>,
    pub(super) data_source_states: HashMap<ResourceId, State>,
    pub(super) bindings: ResolvedBindings,
    pub(super) failed_refreshes: HashSet<ResourceId>,
}

impl ExecutionResultFields {
    fn into_result(
        self,
        applied_states: std::collections::HashMap<ResourceId, Applied>,
    ) -> ExecutionResult {
        let Self {
            success_count,
            failure_count,
            partial_count,
            partial_diagnostics,
            skip_count,
            runtime_synthesized_resources,
            successfully_deleted,
            permanent_name_overrides,
            managed_current,
            data_source_states,
            bindings,
            failed_refreshes,
        } = self;
        ExecutionResult {
            success_count,
            failure_count,
            partial_count,
            partial_diagnostics,
            skip_count,
            applied_states,
            runtime_synthesized_resources,
            successfully_deleted,
            permanent_name_overrides,
            managed_current,
            data_source_states,
            bindings,
            failed_refreshes,
        }
    }
}

/// The executor's single mutable result, with applied-state publication and
/// checkpoint dirtiness attached.
pub(super) struct ExecutionAccumulator {
    fields: ExecutionResultFields,
    applied_states: std::collections::HashMap<ResourceId, Applied>,
    wait_identifiers: wait::SharedWaitIdentifiers,
    dirty: bool,
}

impl ExecutionAccumulator {
    pub(super) fn new(result: ExecutionResult) -> (Self, wait::SharedWaitIdentifiers) {
        let ExecutionResult {
            success_count,
            failure_count,
            partial_count,
            partial_diagnostics,
            skip_count,
            applied_states,
            runtime_synthesized_resources,
            successfully_deleted,
            permanent_name_overrides,
            managed_current,
            data_source_states,
            bindings,
            failed_refreshes,
        } = result;
        assert!(
            applied_states.is_empty(),
            "execution accumulator must start without applied states"
        );
        let wait_identifiers = wait::initial_wait_identifiers(&managed_current);
        let accumulator = Self {
            fields: ExecutionResultFields {
                success_count,
                failure_count,
                partial_count,
                partial_diagnostics,
                skip_count,
                runtime_synthesized_resources,
                successfully_deleted,
                permanent_name_overrides,
                managed_current,
                data_source_states,
                bindings,
                failed_refreshes,
            },
            applied_states,
            wait_identifiers: wait_identifiers.clone(),
            dirty: false,
        };
        (accumulator, wait_identifiers)
    }

    pub(super) fn result(&self) -> &ExecutionResultFields {
        &self.fields
    }

    #[cfg(test)]
    pub(super) fn applied_states(&self) -> &HashMap<ResourceId, Applied> {
        &self.applied_states
    }

    pub(super) fn mutate<T>(
        &mut self,
        mutation: impl FnOnce(&mut ExecutionResultFields) -> T,
    ) -> T {
        self.dirty = true;
        mutation(&mut self.fields)
    }

    pub(super) fn insert_published(&mut self, id: ResourceId, applied: Applied, state: &State) {
        match &applied {
            Applied::Complete {
                state: _,
                request: _,
            }
            | Applied::Observed(_)
            | Applied::CreateInFlight(_)
            | Applied::CreateEnded
            | Applied::Imported(_)
            | Applied::WaitSnapshot(_) => {
                self.wait_identifiers
                    .lock()
                    .expect("wait identifier map poisoned")
                    .insert(
                        id.clone(),
                        state
                            .identifier
                            .as_deref()
                            .and_then(|identifier| AppliedIdentifier::new(identifier).ok()),
                    );
            }
            Applied::DataSource(_) => {}
        }
        self.applied_states.insert(id, applied);
        self.dirty = true;
    }

    pub(super) fn insert_unpublished(&mut self, id: ResourceId, applied: Applied) {
        self.applied_states.insert(id, applied);
        self.dirty = true;
    }

    pub(super) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(super) fn publish_if_dirty(&mut self, sink: &ExecutionCheckpointSink) {
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        // ponytail: this still clones the full cumulative result once per
        // executor iteration; use delta checkpoints if very large plans make
        // that remaining ceiling material.
        sink.publish_owned(self.fields.clone().into_result(self.applied_states.clone()));
    }

    pub(super) fn finish(mut self, sink: &ExecutionCheckpointSink) -> ExecutionResult {
        self.publish_if_dirty(sink);
        self.fields.into_result(self.applied_states)
    }
}

/// Outcome of executing a plan: either it ran to completion, or a
/// cancel request was observed and the run unwound after in-flight
/// effects finished. Both variants carry the `ExecutionResult` of
/// whatever the run produced; callers must destructure to decide
/// whether to surface an `Interrupted` error after persisting state.
///
/// The enum intentionally provides no `?` shortcut and no `From` /
/// `Into` to `Result<ExecutionResult, _>`. A future caller cannot
/// silently drop the `Cancelled` arm — the compiler forces explicit
/// handling. See [`ExecutionOutcomeCannotBeQuestionMarked`] for the
/// compile-fail evidence.
pub enum ExecutionOutcome {
    Completed(ExecutionResult),
    Cancelled(ExecutionResult),
}

/// Marker type whose `compile_fail` doctest is the type-safety evidence
/// that `ExecutionOutcome` cannot be `?`-ed into a `Result`. The marker
/// is hidden from rustdoc — it exists only so `cargo test --doc
/// ExecutionOutcomeCannotBeQuestionMarked` runs the guard.
///
/// ```compile_fail
/// use carina_core::executor::{ExecutionOutcome, ExecutionResult};
///
/// fn must_not_compile(outcome: ExecutionOutcome) -> Result<ExecutionResult, String> {
///     let result = outcome?;
///     Ok(result)
/// }
/// ```
#[doc(hidden)]
pub struct ExecutionOutcomeCannotBeQuestionMarked;

/// Progress information for effect execution.
#[derive(Debug, Clone, Copy)]
pub struct ProgressInfo {
    /// Number of effects completed so far (including this one).
    pub completed: usize,
    /// Total number of actionable effects (excluding Read).
    pub total: usize,
}

/// Events emitted during plan execution.
pub enum ExecutionEvent<'a> {
    /// An effect is waiting for dependencies to complete before it can start.
    Waiting {
        effect: &'a Effect,
        /// Binding names of the dependencies that have not yet completed.
        pending_dependencies: Vec<String>,
    },
    EffectStarted {
        effect: &'a Effect,
    },
    EffectSucceeded {
        effect: &'a Effect,
        state: Option<&'a State>,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectPartiallySucceeded {
        effect: &'a Effect,
        state: &'a State,
        diagnostic: &'a PartialReadDiagnostic,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectFailed {
        effect: &'a Effect,
        error: &'a str,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectSkipped {
        effect: &'a Effect,
        reason: &'a str,
        progress: ProgressInfo,
    },
    /// Heartbeat emitted while a wait poll loop is still alive.
    ///
    /// Emitted at `max(30s, interval * 5)` cadence with the elapsed time and
    /// last observed attributes so operators can see what the wait is reading.
    WaitPolling {
        observation: WaitObservation<'a>,
        elapsed: Duration,
    },
    CascadeUpdateSucceeded {
        id: &'a ResourceId,
    },
    CascadeUpdateFailed {
        id: &'a ResourceId,
        error: &'a str,
    },
    RenameSucceeded {
        id: &'a ResourceId,
        from: &'a str,
        to: &'a str,
    },
    RenameFailed {
        id: &'a ResourceId,
        error: &'a str,
    },
    RefreshStarted,
    RefreshSucceeded {
        id: &'a ResourceId,
    },
    RefreshFailed {
        id: &'a ResourceId,
        error: &'a str,
    },
    /// A terminal module constraint failed without an owning provider effect.
    ModuleConstraintFailed {
        error: &'a str,
    },
}

/// Observer trait for UI separation during plan execution.
///
/// Implementations must handle concurrent calls from parallel effect execution.
/// Use interior mutability (e.g., `Mutex`) if mutable state is needed.
pub trait ExecutionObserver: Send + Sync {
    fn on_event(&self, event: &ExecutionEvent);
}

/// Execute a plan by dispatching effects to a provider.
///
/// This function contains the core execution logic, including:
/// - Reference resolution via the canonical `ResolvedBindings` view
/// - Binding map updates after each effect
/// - Failure propagation (failed_bindings)
/// - Dependency skip
/// - Pending state refreshes
pub async fn execute_live_plan(
    provider: &dyn Provider,
    input: LiveExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    shutdown: ShutdownToken,
) -> ExecutionOutcome {
    execute_runtime_plan(provider, input.into(), observer, shutdown).await
}

#[cfg(test)]
pub(crate) type ExecutionInput<'a> = LiveExecutionInput<'a>;

#[cfg(test)]
async fn execute_plan(
    provider: &dyn Provider,
    input: LiveExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    shutdown: ShutdownToken,
) -> ExecutionOutcome {
    execute_live_plan(provider, input, observer, shutdown).await
}

pub async fn execute_saved_plan(
    provider: &dyn Provider,
    input: SavedExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    shutdown: ShutdownToken,
) -> ExecutionOutcome {
    execute_runtime_plan(provider, input.into(), observer, shutdown).await
}

async fn execute_runtime_plan(
    provider: &dyn Provider,
    mut input: RuntimeExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    shutdown: ShutdownToken,
) -> ExecutionOutcome {
    let (result, was_cancelled) =
        execute_effects_sequential(provider, &mut input, observer, &shutdown).await;
    if was_cancelled {
        ExecutionOutcome::Cancelled(result)
    } else {
        ExecutionOutcome::Completed(result)
    }
}

#[cfg(test)]
mod tests;
