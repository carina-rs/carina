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

pub use deferred_dispatch::{AnonymousIdentityCollisionSource, DeferredCreateFailure};
pub use parallel::classify_stalled_effects;
pub use wait::StartMarker;

pub use crate::data_source_value::UnresolvedDataSourceInput;
pub use crate::effect::deps::UnresolvedResource;
pub use provider_ready::{
    ModuleConstraintFailure, ModuleConstraintGate, ModuleConstraintGateError,
    ProviderConfigPreparationError, ProviderConfigValidationError, ProviderPreparationContext,
    ProviderPreparationError, ProviderReadyConfig, ProviderValueConstraintErrors,
    prepare_create_request, prepare_provider_ready_config, prepare_provider_ready_data_source,
    prepare_provider_ready_resource, prepare_update_request,
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
    unresolved_resources: &HashMap<ResolvedResourceId, &UnresolvedResource>,
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

use crate::resource::ResolvedResourceId;
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
    ResumeFailureKind, ResumeOutcome, SentAttributes, SentDesiredAttributes, UnknownCreateOutcome,
    UpdateRequest,
};
use crate::resource::{
    ApplyTimeUnknown, ConcreteValue, Current, DataSource, DeferredValue, InterpolationPart,
    ModuleSource, ResolvedResource, Resource, ResourceId, ResourceIdentity, SavedModuleSource,
    State, Value,
};
use crate::wait::WaitObservation;

use crate::shutdown::ShutdownToken;
use parallel::execute_effects_sequential;

pub const TEST_UNCAPPED: NonZeroUsize = NonZeroUsize::new(usize::MAX).unwrap();

/// Prepare dispatch of an already checked create request while minting the
/// unforgeable initial-attempt witness and its cancellation receiver inside
/// core. The supervising caller must retain the receiver until the returned
/// future settles. Executor-owned abandonment paths drain it before saving
/// the cancellation classification.
pub fn dispatch_create<'a, P: Provider + ?Sized>(
    provider: &'a P,
    id: &'a ResourceId,
    request: CreateRequest,
) -> (
    impl std::future::Future<Output = Result<CreateOutcome, CreateFailure>> + 'a,
    crate::provider::CreateCallCancellationReceiver,
) {
    let (attempt, cancellation) = crate::provider::CreateAttempt::new(id.clone());
    (
        dispatch_create_with_attempt(provider, id, request, attempt),
        cancellation,
    )
}

async fn dispatch_create_with_attempt<P: Provider + ?Sized>(
    provider: &P,
    id: &ResourceId,
    request: CreateRequest,
    attempt: crate::provider::CreateAttempt,
) -> Result<CreateOutcome, CreateFailure> {
    let request_attributes: BTreeSet<_> = request
        .resource()
        .as_resource()
        .attributes
        .keys()
        .cloned()
        .collect();
    match provider.create(id, request, attempt).await {
        Ok(outcome) => Ok(outcome),
        Err(failure) => match failure.into_kind() {
            CreateFailureKind::OutcomeUnknown { unknown, error } => {
                Err(CreateFailure::outcome_unknown_at_dispatch(unknown, error))
            }
            CreateFailureKind::Created { created, error } => {
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
    sent_attributes: SentAttributes,
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
    /// Record a successful create from the checked request that was dispatched.
    pub fn from_create_request(request: &CreateRequest) -> Self {
        Self {
            kind: AcceptedRequestKind::Create,
            sent_attributes: request.sent_attributes(),
            resolved_desired_attributes: None,
        }
    }

    fn from_update_request(
        request: &UpdateRequest,
        resolved_desired_attributes: SecretPreservingAttributes,
    ) -> Self {
        let sent_attributes = request.patch().sent_attributes();
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
            sent_attributes,
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
        Self {
            kind: AcceptedRequestKind::Create,
            sent_attributes: SentAttributes::all_for_test(),
            resolved_desired_attributes: None,
        }
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
            sent_attributes: SentAttributes::only(
                sent.iter().map(|key| (*key).to_string()).collect(),
            ),
            resolved_desired_attributes: None,
        }
    }

    pub fn kind(&self) -> &AcceptedRequestKind {
        &self.kind
    }

    pub fn sent_attributes(&self) -> &SentAttributes {
        &self.sent_attributes
    }

    /// Return the desired values and sent-attribute membership proven by this
    /// accepted request. Production executor paths retain resolved values;
    /// provider-ready bootstrap and test paths may use the supplied resource
    /// as their already-resolved fallback.
    pub fn sent_desired_attributes<'a, S, D>(
        &'a self,
        fallback_resource: &'a Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> SentDesiredAttributes<'a> {
        match self.resolved_desired_attributes.as_ref() {
            Some(attributes) => {
                SentDesiredAttributes::new(attributes.values().iter(), self.sent_attributes.clone())
            }
            None => SentDesiredAttributes::new(
                fallback_resource.attributes.iter(),
                self.sent_attributes.clone(),
            ),
        }
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
    request: AcceptedRequest,
) -> (
    State,
    AcceptedRequest,
    Option<PartialReadDiagnostic>,
    crate::provider::ReadBackEnd,
) {
    let diagnostic = outcome.diagnostic().cloned();
    let read_back_end = outcome.read_back_end();
    let state = outcome.into_state_for_writeback();
    (state, request, diagnostic, read_back_end)
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
    CreateOutcomeUnknown(UnknownCreateOutcome),
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
    pub provider_check_inputs: ProviderCheckInputs<'a, ModuleSource>,
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
    pub data_source_values: crate::data_source_value::ExecutableDataSourceValues<ModuleSource>,
    /// The same provider normalizer that ran at plan time
    /// (`PlanPreprocessor`). Apply-time reference re-resolution rebuilds
    /// attributes from the un-normalized source, so the executor must
    /// re-apply this after each resolution and before constructing the
    /// provider request — otherwise plan-time normalization is silently
    /// undone (carina#3060).
    pub normalizer: &'a dyn ProviderNormalizer,
    /// Provider configs whose default_tags participate in desired-side
    /// normalization before provider patch construction.
    pub provider_configs: &'a [ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
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
    pub checkpoint_sink: ExecutionCheckpointSink<ModuleSource>,
}

/// Saved-plan counterpart of [`LiveExecutionInput`].
///
/// Keeping the composition slice in a distinct input type prevents a
/// deserialized plan from entering the live validation/expansion lifecycle.
pub struct SavedExecutionInput<'a> {
    pub plan: &'a crate::plan::SavedPlan,
    pub provider_check_inputs: ProviderCheckInputs<'a, SavedModuleSource>,
    pub compositions: &'a [crate::resource::SavedComposition],
    pub bindings: ResolvedBindings,
    pub managed_current: HashMap<ResourceId, Current>,
    pub data_source_values: crate::data_source_value::ExecutableDataSourceValues<SavedModuleSource>,
    pub normalizer: &'a dyn ProviderNormalizer,
    pub provider_configs: &'a [ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    pub factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    pub schemas: &'a crate::schema::SchemaRegistry,
    pub parallelism: NonZeroUsize,
    pub precompleted_success_count: usize,
    pub checkpoint_sink: ExecutionCheckpointSink<SavedModuleSource>,
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

    fn scheduler_dependencies<S>(
        self,
        effects: &[Effect<S>],
        unresolved_resources: &HashMap<ResolvedResourceId, UnresolvedResource<S>>,
        deferred_replace_delete_deps: &[(usize, usize)],
    ) -> scheduler::SchedulerDeps {
        let _ = self;
        scheduler::build_scheduler_deps(effects, unresolved_resources, deferred_replace_delete_deps)
    }
}

pub(super) struct RuntimeExecutionInput<'a, S = ModuleSource> {
    pub(super) plan: &'a crate::plan::Plan<S>,
    pub(super) provider_check_inputs: ProviderCheckInputs<'a, S>,
    pub(super) compositions: RuntimeCompositions<'a>,
    pub(super) bindings: ResolvedBindings,
    pub(super) managed_current: HashMap<ResourceId, Current>,
    pub(super) data_source_values: crate::data_source_value::ExecutableDataSourceValues<S>,
    pub(super) normalizer: &'a dyn ProviderNormalizer,
    pub(super) provider_configs: &'a [ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    pub(super) factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    pub(super) schemas: &'a crate::schema::SchemaRegistry,
    pub(super) parallelism: NonZeroUsize,
    pub(super) precompleted_success_count: usize,
    pub(super) checkpoint_sink: ExecutionCheckpointSink<S>,
}

impl<'a> From<LiveExecutionInput<'a>> for RuntimeExecutionInput<'a, ModuleSource> {
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

impl<'a> From<SavedExecutionInput<'a>> for RuntimeExecutionInput<'a, SavedModuleSource> {
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
pub enum ProviderCheckInputs<'a, S = ModuleSource> {
    Authored,
    PlanNormalized {
        resource_origins: &'a HashMap<ResolvedResourceId, UnresolvedResource<S>>,
        data_source_origins: &'a [DataSource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            (),
        >],
    },
}

impl<S: Clone> ProviderCheckInputs<'_, S> {
    fn resource_origins(&self) -> Option<&HashMap<ResolvedResourceId, UnresolvedResource<S>>> {
        match self {
            Self::Authored => None,
            Self::PlanNormalized {
                resource_origins, ..
            } => Some(resource_origins),
        }
    }

    pub(in crate::executor) fn resource_input(
        &self,
        resolved: &Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            (),
        >,
        runtime_authored: bool,
    ) -> Result<
        provider_ready::CheckInput<
            Resource<
                S,
                crate::resource::NoPlaceholder,
                crate::resource::AbsoluteBindingAddress,
                (),
            >,
            Resource<
                S,
                crate::resource::NoPlaceholder,
                crate::resource::AbsoluteBindingAddress,
                (),
            >,
        >,
        String,
    > {
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
        resolved: &DataSource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            (),
        >,
    ) -> Result<
        provider_ready::CheckInput<
            DataSource<
                S,
                crate::resource::NoPlaceholder,
                crate::resource::AbsoluteBindingAddress,
                (),
            >,
            DataSource<
                S,
                crate::resource::NoPlaceholder,
                crate::resource::AbsoluteBindingAddress,
                (),
            >,
        >,
        String,
    > {
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

fn visit_unknowns(value: &Value, f: &mut impl FnMut(&ApplyTimeUnknown)) {
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
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred
                .values()
                .into_iter()
                .for_each(|value| visit_unknowns(value, f));
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            visit_unknowns(expression.lhs(), f);
            visit_unknowns(expression.rhs(), f);
        }
        Value::Concrete(_)
        | Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::RedactedSecret { .. }) => {}
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
    }
}

pub fn unresolved_data_source_inputs<S, D>(
    resource: &DataSource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
) -> Vec<UnresolvedDataSourceInput> {
    resource
        .attributes
        .iter()
        .filter_map(|(attribute, value)| {
            let mut paths = Vec::new();
            let mut bindings = Vec::new();
            let mut unknowns = Vec::new();
            value.visit_resource_refs(&mut |path| paths.push(path.clone()));
            value.visit_binding_refs(&mut |binding| bindings.push(binding.clone()));
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
pub struct ExecutionCheckpointSink<S = ModuleSource> {
    publish: Arc<dyn Fn(Arc<ExecutionResult<S>>) + Send + Sync>,
}

impl<S: Clone> ExecutionCheckpointSink<S> {
    pub fn new(publish: impl Fn(Arc<ExecutionResult<S>>) + Send + Sync + 'static) -> Self {
        Self {
            publish: Arc::new(publish),
        }
    }

    /// Explicit sink for callers that do not persist executor results.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn noop() -> Self
    where
        S: 'static,
    {
        Self::new(drop)
    }

    pub fn publish(&self, result: &ExecutionResult<S>) {
        (self.publish)(Arc::new(result.clone()));
    }

    fn publish_owned(&self, result: ExecutionResult<S>) {
        (self.publish)(Arc::new(result));
    }
}

impl<S> std::fmt::Debug for ExecutionCheckpointSink<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionCheckpointSink")
            .finish_non_exhaustive()
    }
}

/// Cloneable ownership wrapper for a typed execution error.
#[derive(Clone)]
pub struct ReportedError {
    inner: Arc<dyn std::error::Error + Send + Sync>,
}

impl ReportedError {
    pub fn new(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(error),
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::new(MessageError(message.into()))
    }

    pub fn context(
        context: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::new(source).with_context(context)
    }

    pub fn with_context(self, context: impl Into<String>) -> Self {
        Self::new(ContextError {
            context: context.into(),
            source: self,
        })
    }

    pub fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self.inner.as_ref()
    }
}

impl std::fmt::Debug for ReportedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ReportedError")
            .field(&self.inner.to_string())
            .finish()
    }
}

impl std::fmt::Display for ReportedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(formatter)
    }
}

/// Render a diagnostic and each typed cause without flattening its ownership.
pub fn render_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        rendered.push_str("\ncaused by: ");
        rendered.push_str(&cause.to_string());
        source = cause.source();
    }
    rendered
}

#[derive(Debug)]
struct MessageError(String);

impl std::fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MessageError {}

#[derive(Debug)]
struct ContextError {
    context: String,
    source: ReportedError,
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.context)
    }
}

impl std::error::Error for ContextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_error())
    }
}

/// Operation component of an owned effect endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectOperation {
    Read,
    Create,
    ResumeCreate,
    Update,
    Delete,
    Import,
    Remove,
    Move,
    ReleaseListKeys,
    Wait,
    DeferredCreate,
    DeferredReplace,
}

impl std::fmt::Display for EffectOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Read => "read",
            Self::Create => "create",
            Self::ResumeCreate => "resume create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Import => "import",
            Self::Remove => "remove",
            Self::Move => "move",
            Self::ReleaseListKeys => "release list keys",
            Self::Wait => "wait",
            Self::DeferredCreate => "deferred create",
            Self::DeferredReplace => "deferred replace",
        })
    }
}

/// Owned identity of an effect that can outlive the borrowed execution event.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EffectEndpoint {
    resource_id: ResourceId,
    operation: EffectOperation,
}

impl EffectEndpoint {
    pub fn from_effect<S>(effect: &Effect<S>) -> Self {
        let operation = match effect {
            Effect::Read { .. } => EffectOperation::Read,
            Effect::Create(_) => EffectOperation::Create,
            Effect::ResumeCreate { .. } => EffectOperation::ResumeCreate,
            Effect::Update { .. } => EffectOperation::Update,
            Effect::Delete { .. } => EffectOperation::Delete,
            Effect::Import { .. } => EffectOperation::Import,
            Effect::Remove { .. } => EffectOperation::Remove,
            Effect::Move { .. } => EffectOperation::Move,
            Effect::ReleaseListKeys { .. } => EffectOperation::ReleaseListKeys,
            Effect::Wait { .. } => EffectOperation::Wait,
            Effect::DeferredCreate(_) => EffectOperation::DeferredCreate,
            Effect::DeferredReplace(_) => EffectOperation::DeferredReplace,
        };
        Self {
            resource_id: effect.resource_id().clone(),
            operation,
        }
    }

    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }

    pub fn operation(&self) -> EffectOperation {
        self.operation
    }
}

impl std::fmt::Display for EffectEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}", self.operation, self.resource_id)
    }
}

/// A strongly connected component that prevented scheduler progress.
#[derive(Debug, Clone)]
pub struct DependencyCycle {
    members: Vec<EffectEndpoint>,
}

impl DependencyCycle {
    pub fn new(members: Vec<EffectEndpoint>) -> Self {
        Self { members }
    }

    pub fn members(&self) -> &[EffectEndpoint] {
        &self.members
    }
}

impl std::fmt::Display for DependencyCycle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let members = self
            .members
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        write!(formatter, "dependency cycle among {{{members}}}")
    }
}

/// An unfinished scheduling dependency, including a malformed missing index.
#[derive(Debug, Clone)]
pub enum UnsatisfiedDependency {
    Effect(EffectEndpoint),
    MissingEffect { index: usize },
}

impl std::fmt::Display for UnsatisfiedDependency {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Effect(endpoint) => endpoint.fmt(formatter),
            Self::MissingEffect { index } => write!(formatter, "missing effect #{index}"),
        }
    }
}

/// Typed reason an effect did not complete successfully.
#[derive(Debug, Clone)]
pub enum EffectFailureKind {
    Provider {
        error: ReportedError,
        note: Option<String>,
    },
    Execution {
        error: ReportedError,
    },
    DeferredMaterialization {
        failure: Arc<DeferredCreateFailure>,
    },
    MissingDependency {
        identity: ResourceIdentity,
    },
    DependencyCycle {
        cycle: Arc<DependencyCycle>,
    },
    BlockedByDependencyCycle {
        cycle: Arc<DependencyCycle>,
    },
    NoProgress {
        waiting_on: Vec<UnsatisfiedDependency>,
    },
    ModuleConstraint {
        failure: Box<ModuleConstraintFailure>,
    },
}

impl EffectFailureKind {
    /// Short final-summary label. The observer renders the full cause once.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Provider { .. } => "provider error",
            Self::Execution { .. } => "execution error",
            Self::DeferredMaterialization { .. } => "deferred materialization failure",
            Self::MissingDependency { .. } => "missing dependency",
            Self::DependencyCycle { .. } => "dependency cycle",
            Self::BlockedByDependencyCycle { .. } => "blocked by dependency cycle",
            Self::NoProgress { .. } => "no scheduler progress",
            Self::ModuleConstraint { .. } => "module constraint failure",
        }
    }

    pub fn provider(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Provider {
            error: ReportedError::new(error),
            note: None,
        }
    }

    pub fn provider_with_context(
        context: impl Into<String>,
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Provider {
            error: ReportedError::context(context, error),
            note: None,
        }
    }

    pub fn provider_with_note(
        error: impl std::error::Error + Send + Sync + 'static,
        note: impl Into<String>,
    ) -> Self {
        Self::Provider {
            error: ReportedError::new(error),
            note: Some(note.into()),
        }
    }

    pub fn execution(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Execution {
            error: ReportedError::new(error),
        }
    }

    pub fn execution_with_context(
        context: impl Into<String>,
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Execution {
            error: ReportedError::context(context, error),
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::Execution {
            error: ReportedError::message(message),
        }
    }

    pub fn deferred(failure: DeferredCreateFailure) -> Self {
        Self::DeferredMaterialization {
            failure: Arc::new(failure),
        }
    }

    fn render(&self) -> String {
        let Some(error) = self.reported_error() else {
            return self.to_string();
        };
        let mut rendered = render_error_chain(error);
        if let Self::Provider {
            note: Some(note), ..
        } = self
        {
            rendered.push('\n');
            rendered.push_str(note);
        }
        rendered
    }

    fn reported_error(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Provider { error, .. } | Self::Execution { error } => Some(error.as_error()),
            Self::DeferredMaterialization { failure } => Some(failure.as_ref()),
            Self::MissingDependency { .. }
            | Self::DependencyCycle { .. }
            | Self::BlockedByDependencyCycle { .. }
            | Self::NoProgress { .. }
            | Self::ModuleConstraint { .. } => None,
        }
    }
}

impl std::fmt::Display for EffectFailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider { error, note } => {
                error.fmt(formatter)?;
                if let Some(note) = note {
                    write!(formatter, "\n{note}")?;
                }
                Ok(())
            }
            Self::Execution { error } => error.fmt(formatter),
            Self::DeferredMaterialization { failure } => failure.fmt(formatter),
            Self::MissingDependency { identity } => {
                write!(
                    formatter,
                    "required scheduling target `{identity}` is missing"
                )
            }
            Self::DependencyCycle { cycle } => cycle.fmt(formatter),
            Self::BlockedByDependencyCycle { cycle } => match cycle.members().first() {
                Some(member) => write!(
                    formatter,
                    "blocked by the dependency cycle containing {member}"
                ),
                None => write!(formatter, "blocked by {cycle}"),
            },
            Self::NoProgress { waiting_on } => {
                if waiting_on.is_empty() {
                    formatter.write_str("no progress: effect was not dispatched despite all dependencies completing")
                } else {
                    write!(
                        formatter,
                        "no progress: waiting on {}",
                        waiting_on
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            }
            Self::ModuleConstraint { failure } => failure.fmt(formatter),
        }
    }
}

/// One failed effect and its retained typed cause.
#[derive(Debug, Clone)]
pub struct EffectFailure {
    endpoint: Option<EffectEndpoint>,
    kind: EffectFailureKind,
}

impl EffectFailure {
    pub fn new<S>(effect: &Effect<S>, kind: EffectFailureKind) -> Self {
        Self {
            endpoint: Some(EffectEndpoint::from_effect(effect)),
            kind,
        }
    }

    pub fn module_constraint(failure: impl Into<Box<ModuleConstraintFailure>>) -> Self {
        Self {
            endpoint: None,
            kind: EffectFailureKind::ModuleConstraint {
                failure: failure.into(),
            },
        }
    }

    pub fn endpoint(&self) -> Option<&EffectEndpoint> {
        self.endpoint.as_ref()
    }

    pub fn kind(&self) -> &EffectFailureKind {
        &self.kind
    }
}

impl std::fmt::Display for EffectFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.kind.render())
    }
}

/// Proof that a failure has been retained by a [`FailureSink`]. Only the sink
/// can issue this borrowed token, and it only lends it to the observer.
pub struct RecordedFailure<'a> {
    failure: &'a EffectFailure,
}

impl std::ops::Deref for RecordedFailure<'_> {
    type Target = EffectFailure;

    fn deref(&self) -> &Self::Target {
        self.failure
    }
}

impl std::fmt::Display for RecordedFailure<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.failure.fmt(formatter)
    }
}

/// Private-storage failure ledger. Recording retains the failure and emits
/// its typed event together; there is no observer-free insertion API.
///
/// An unretained effect failure cannot be reported by constructing an event:
///
/// ```compile_fail
/// use carina_core::effect::Effect;
/// use carina_core::executor::{EffectFailure, ExecutionEvent, ProgressInfo};
/// fn unretained<'a>(effect: &'a Effect, failure: &'a EffectFailure) -> ExecutionEvent<'a> {
///     ExecutionEvent::EffectFailed {
///         effect, failure, recovery: None, duration: std::time::Duration::ZERO,
///         progress: ProgressInfo { completed: 1, total: 1 },
///     }
/// }
/// ```
///
/// Module constraint failures must use the same ledger:
///
/// ```compile_fail
/// use carina_core::executor::{EffectFailure, ExecutionEvent};
/// fn unretained(failure: &EffectFailure) -> ExecutionEvent<'_> {
///     ExecutionEvent::ModuleConstraintFailed { failure }
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct FailureSink {
    failures: Vec<EffectFailure>,
}

pub enum FailureReport<'a, S> {
    Effect {
        effect: &'a Effect<S>,
        kind: EffectFailureKind,
        context: FailureEventContext<'a>,
    },
    ModuleConstraint(Box<ModuleConstraintFailure>),
}

impl FailureSink {
    pub fn failures(&self) -> &[EffectFailure] {
        &self.failures
    }

    pub fn record<S>(&mut self, observer: &dyn ExecutionObserver<S>, report: FailureReport<'_, S>) {
        let (failure, effect_context) = match report {
            FailureReport::Effect {
                effect,
                kind,
                context,
            } => (EffectFailure::new(effect, kind), Some((effect, context))),
            FailureReport::ModuleConstraint(failure) => {
                (EffectFailure::module_constraint(failure), None)
            }
        };
        self.failures.push(failure);
        let recorded = RecordedFailure {
            failure: self.failures.last().expect("just retained a failure"),
        };
        if let Some((effect, context)) = effect_context {
            observer.on_event(&ExecutionEvent::EffectFailed {
                effect,
                failure: &recorded,
                recovery: context.recovery,
                duration: context.duration,
                progress: context.progress,
            });
        } else {
            observer.on_event(&ExecutionEvent::ModuleConstraintFailed { failure: &recorded });
        }
    }
}

/// Result of executing a plan's effects.
#[derive(Clone)]
pub struct ExecutionResult<S = ModuleSource> {
    pub success_count: usize,
    failures: FailureSink,
    pub partial_count: usize,
    pub partial_diagnostics: Vec<(ResourceId, PartialReadDiagnostic)>,
    pub skip_count: usize,
    pub interrupted_count: usize,
    pub applied_states: std::collections::HashMap<ResourceId, Applied>,
    pub runtime_synthesized_resources: Vec<ResolvedResource<S>>,
    pub successfully_deleted: HashSet<DeletedInstanceKey>,
    pub permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>>,
    pub managed_current: HashMap<ResourceId, Current>,
    pub data_source_states: HashMap<ResourceId, State>,
    pub bindings: ResolvedBindings,
    pub failed_refreshes: HashSet<ResourceId>,
}

impl<S> ExecutionResult<S> {
    pub fn empty() -> Self {
        Self {
            success_count: 0,
            failures: FailureSink::default(),
            partial_count: 0,
            partial_diagnostics: Vec::new(),
            skip_count: 0,
            interrupted_count: 0,
            applied_states: HashMap::new(),
            runtime_synthesized_resources: Vec::new(),
            successfully_deleted: HashSet::new(),
            permanent_name_overrides: HashMap::new(),
            managed_current: HashMap::new(),
            data_source_states: HashMap::new(),
            bindings: ResolvedBindings::default(),
            failed_refreshes: HashSet::new(),
        }
    }

    pub fn failure_count(&self) -> usize {
        self.failures().len()
    }

    pub fn failures(&self) -> &[EffectFailure] {
        self.failures.failures()
    }

    pub fn failure_sink(&mut self) -> &mut FailureSink {
        &mut self.failures
    }
}

/// Mutable execution-result fields other than applied states.
///
/// Keeping `applied_states` out of this mutation view makes it impossible for
/// an executor branch to add a state without explicitly choosing whether its
/// identifier is published to downstream Wait effects.
#[derive(Clone)]
pub(super) struct ExecutionResultFields<S = ModuleSource> {
    pub(super) success_count: usize,
    failures: FailureSink,
    pub(super) partial_count: usize,
    pub(super) partial_diagnostics: Vec<(ResourceId, PartialReadDiagnostic)>,
    pub(super) skip_count: usize,
    pub(super) interrupted_count: usize,
    pub(super) runtime_synthesized_resources: Vec<ResolvedResource<S>>,
    pub(super) successfully_deleted: HashSet<DeletedInstanceKey>,
    pub(super) permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>>,
    pub(super) managed_current: HashMap<ResourceId, Current>,
    pub(super) data_source_states: HashMap<ResourceId, State>,
    pub(super) bindings: ResolvedBindings,
    pub(super) failed_refreshes: HashSet<ResourceId>,
}

impl<S> ExecutionResultFields<S> {
    pub(super) fn failure_count(&self) -> usize {
        self.failures.failures().len()
    }

    fn into_result(
        self,
        applied_states: std::collections::HashMap<ResourceId, Applied>,
    ) -> ExecutionResult<S> {
        let Self {
            success_count,
            failures,
            partial_count,
            partial_diagnostics,
            skip_count,
            interrupted_count,
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
            failures,
            partial_count,
            partial_diagnostics,
            skip_count,
            interrupted_count,
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
pub(super) struct ExecutionAccumulator<S = ModuleSource> {
    fields: ExecutionResultFields<S>,
    applied_states: std::collections::HashMap<ResourceId, Applied>,
    wait_identifiers: wait::SharedWaitIdentifiers,
    dirty: bool,
}

impl<S: Clone> ExecutionAccumulator<S> {
    pub(super) fn new(result: ExecutionResult<S>) -> (Self, wait::SharedWaitIdentifiers) {
        let ExecutionResult {
            success_count,
            failures,
            partial_count,
            partial_diagnostics,
            skip_count,
            interrupted_count,
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
                failures,
                partial_count,
                partial_diagnostics,
                skip_count,
                interrupted_count,
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

    pub(super) fn result(&self) -> &ExecutionResultFields<S> {
        &self.fields
    }

    #[cfg(test)]
    pub(super) fn applied_states(&self) -> &HashMap<ResourceId, Applied> {
        &self.applied_states
    }

    pub(super) fn mutate<T>(
        &mut self,
        mutation: impl FnOnce(&mut ExecutionResultFields<S>) -> T,
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
            | Applied::CreateOutcomeUnknown(_)
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

    pub(super) fn publish_if_dirty(&mut self, sink: &ExecutionCheckpointSink<S>) {
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        // ponytail: this still clones the full cumulative result once per
        // executor iteration; use delta checkpoints if very large plans make
        // that remaining ceiling material.
        sink.publish_owned(self.fields.clone().into_result(self.applied_states.clone()));
    }

    pub(super) fn finish(mut self, sink: &ExecutionCheckpointSink<S>) -> ExecutionResult<S> {
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
pub enum ExecutionOutcome<S = ModuleSource> {
    Completed(ExecutionResult<S>),
    Cancelled(ExecutionResult<S>),
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
pub enum ExecutionEvent<'a, S = ModuleSource> {
    /// An effect is waiting for dependencies to complete before it can start.
    Waiting {
        effect: &'a Effect<S>,
        /// Binding names of the dependencies that have not yet completed.
        pending_dependencies: Vec<String>,
    },
    EffectStarted {
        effect: &'a Effect<S>,
        start: StartMarker,
    },
    /// The effect was started and then abandoned under cleanup priority.
    ///
    /// Its provider-side outcome is unknown because its in-flight future was
    /// dropped before a terminal result could be observed.
    EffectInterrupted {
        effect: &'a Effect<S>,
        progress: ProgressInfo,
    },
    EffectSucceeded {
        effect: &'a Effect<S>,
        state: Option<&'a State>,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectPartiallySucceeded {
        effect: &'a Effect<S>,
        state: &'a State,
        diagnostic: &'a PartialReadDiagnostic,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectFailed {
        effect: &'a Effect<S>,
        failure: &'a RecordedFailure<'a>,
        recovery: Option<&'a crate::plan::PlanError>,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectSkipped {
        effect: &'a Effect<S>,
        reason: &'a str,
        progress: ProgressInfo,
    },
    /// Heartbeat emitted while a wait poll loop is still alive.
    ///
    /// Emitted at `max(30s, interval * 5)` cadence with the elapsed time and
    /// last observed attributes so operators can see what the wait is reading.
    WaitPolling {
        effect: &'a Effect<S>,
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
        error: &'a ProviderError,
    },
    /// A terminal module constraint failed without an owning provider effect.
    ModuleConstraintFailed {
        failure: &'a RecordedFailure<'a>,
    },
}

pub struct FailureEventContext<'a> {
    pub recovery: Option<&'a crate::plan::PlanError>,
    pub duration: Duration,
    pub progress: ProgressInfo,
}

/// The only executor path that both publishes and retains an effect failure.
pub(super) fn record_effect_failure<S: Clone>(
    accumulator: &mut ExecutionAccumulator<S>,
    observer: &dyn ExecutionObserver<S>,
    effect: &Effect<S>,
    kind: EffectFailureKind,
    context: FailureEventContext<'_>,
) {
    accumulator.mutate(|result| {
        result.failures.record(
            observer,
            FailureReport::Effect {
                effect,
                kind,
                context,
            },
        );
    });
}

fn record_module_constraint_failure<S: Clone>(
    accumulator: &mut ExecutionAccumulator<S>,
    observer: &dyn ExecutionObserver<S>,
    failure: ModuleConstraintFailure,
) {
    accumulator.mutate(|result| {
        result
            .failures
            .record(observer, FailureReport::ModuleConstraint(Box::new(failure)))
    });
}

impl<'a, S> ExecutionEvent<'a, S> {
    /// Construct a start event for observer-focused tests without exposing the
    /// production start marker constructor.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn effect_started_for_test(effect: &'a Effect<S>) -> Self {
        wait::effect_started_for_test(effect)
    }
}

/// Observer trait for UI separation during plan execution.
///
/// Implementations must handle concurrent calls from parallel effect execution.
/// Use interior mutability (e.g., `Mutex`) if mutable state is needed.
pub trait ExecutionObserver<S = ModuleSource>: Send + Sync {
    fn on_event(&self, event: &ExecutionEvent<S>);
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub struct NoopExecutionObserver;

#[cfg(any(test, feature = "test-utils"))]
impl<S> ExecutionObserver<S> for NoopExecutionObserver {
    fn on_event(&self, _event: &ExecutionEvent<S>) {}
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
    provider: &crate::provider::ProviderRouter,
    input: LiveExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    shutdown: ShutdownToken,
) -> ExecutionOutcome {
    execute_runtime_plan(provider, provider, input.into(), observer, shutdown).await
}

/// Test-only execution entry point for focused provider fixtures without an
/// initialized provider instance. Comparisons use an unavailable scope.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub async fn execute_live_plan_for_test(
    provider: &dyn Provider,
    input: LiveExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    shutdown: ShutdownToken,
) -> ExecutionOutcome {
    execute_test_runtime_plan(provider, input, observer, shutdown).await
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
    execute_test_runtime_plan(provider, input, observer, shutdown).await
}

pub async fn execute_saved_plan(
    provider: &crate::provider::ProviderRouter,
    input: SavedExecutionInput<'_>,
    observer: &dyn ExecutionObserver<SavedModuleSource>,
    shutdown: ShutdownToken,
) -> ExecutionOutcome<SavedModuleSource> {
    execute_runtime_plan(provider, provider, input.into(), observer, shutdown).await
}

async fn execute_runtime_plan<S: Clone + crate::resource::AnonymousIdentityModuleSource>(
    provider: &dyn Provider,
    comparison_provider: &crate::provider::ProviderRouter,
    mut input: RuntimeExecutionInput<'_, S>,
    observer: &dyn ExecutionObserver<S>,
    shutdown: ShutdownToken,
) -> ExecutionOutcome<S> {
    let (result, was_cancelled) = execute_effects_sequential(
        provider,
        comparison_provider,
        &mut input,
        observer,
        &shutdown,
    )
    .await;
    if was_cancelled {
        ExecutionOutcome::Cancelled(result)
    } else {
        ExecutionOutcome::Completed(result)
    }
}

#[cfg(any(test, feature = "test-utils"))]
async fn execute_test_runtime_plan(
    provider: &dyn Provider,
    input: LiveExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    shutdown: ShutdownToken,
) -> ExecutionOutcome {
    let (schemas, comparison_provider) = crate::provider::comparison_fixture_for_test(
        input.schemas,
        input.plan.effects().iter().map(Effect::resource_id),
    );
    let mut input: RuntimeExecutionInput<'_> = input.into();
    input.schemas = &schemas;
    execute_runtime_plan(provider, &comparison_provider, input, observer, shutdown).await
}

#[cfg(test)]
mod tests;
