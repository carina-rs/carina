use std::collections::{HashMap, HashSet};

use crate::binding_index::ResolvedBindings;
use crate::effect::{Effect, SchedulerKey};
use crate::parser::CompiledDeferredForExpression as DeferredForExpression;
use crate::resource::ResolvedResourceId;

use super::deferred_dispatch::{PureMetaCtx, dispatch_deferred_create};
use crate::effect::deps::UnresolvedRequiredDependency;

/// Permission to send exactly one admitted, dependency-ready prepared request.
/// The request is immutable and owned by the permit; dispatch accepts no substitute.
///
/// ```compile_fail,E0599
/// use carina_core::replacement::CreatePermit;
/// fn history(permit: &CreatePermit) { permit.old_instances(); }
/// ```
///
/// ```compile_fail,E0451
/// use carina_core::replacement::CreatePermit;
/// let permit = CreatePermit { request: todo!() };
/// ```
pub struct CreatePermit {
    request: Box<super::PreparedWrite<crate::provider::CreateRequest>>,
}

impl std::fmt::Debug for CreatePermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CreatePermit")
            .field("target", self.target())
            .finish_non_exhaustive()
    }
}

impl CreatePermit {
    pub fn target(&self) -> &crate::resource::ResourceId {
        &self.request.request().resource().id
    }
    pub fn request(&self) -> &crate::provider::CreateRequest {
        self.request.request()
    }
    pub(crate) fn into_request(self) -> crate::provider::CreateRequest {
        (*self.request).into_parts().0
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn create_permit_for_test(request: crate::provider::CreateRequest) -> CreatePermit {
    create_permit_with_schemas_for_test(request, &crate::schema::SchemaRegistry::new())
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn create_permit_with_schemas_for_test(
    request: crate::provider::CreateRequest,
    schemas: &crate::schema::SchemaRegistry,
) -> CreatePermit {
    let (ready, check) = fixture_write_authority(
        Effect::Create(request.resource().as_inner().clone()),
        schemas,
    );
    let attributes = request.resource().as_resource().resolved_attributes();
    let writeback = crate::binding_index::SecretPreservingAttributes::from_provider_values(
        &attributes,
        &attributes,
    );
    let write = check
        .prepare_create(request, writeback, schemas)
        .expect("fixture input discharge");
    let ScheduledBasicEffect::Create { create, .. } = ready else {
        unreachable!("fixture create slot")
    };
    create.authorize(write)
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn update_permit_for_test(
    identifier: crate::provider::AppliedIdentifier,
    request: crate::provider::UpdateRequest,
) -> UpdatePermit {
    update_permit_with_schemas_for_test(identifier, request, &crate::schema::SchemaRegistry::new())
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn update_permit_with_schemas_for_test(
    identifier: crate::provider::AppliedIdentifier,
    request: crate::provider::UpdateRequest,
    schemas: &crate::schema::SchemaRegistry,
) -> UpdatePermit {
    let desired = request.desired_for_fixture().clone();
    let (ready, check) = fixture_write_authority(
        Effect::Update {
            from: Box::new(request.from().clone()),
            identifier,
            to: desired.as_inner().clone(),
            changed_attributes: request
                .patch()
                .ops
                .iter()
                .map(|op| op.key().to_owned())
                .collect(),
        },
        schemas,
    );
    let attributes = desired
        .as_resource()
        .attributes
        .clone()
        .into_iter()
        .collect();
    let writeback = crate::binding_index::SecretPreservingAttributes::from_provider_values(
        &attributes,
        &attributes,
    );
    let write = check
        .prepare_update_fixture(request, writeback, schemas)
        .expect("fixture input discharge");
    let ScheduledBasicEffect::Update { update, .. } = ready else {
        unreachable!("fixture update slot")
    };
    update.authorize(write)
}

#[cfg(any(test, feature = "test-utils"))]
fn fixture_write_authority(
    effect: Effect<crate::resource::ProviderModuleSource>,
    schemas: &crate::schema::SchemaRegistry,
) -> (
    ScheduledBasicEffect<crate::resource::ProviderModuleSource>,
    super::input_discharge::WriteCheck,
) {
    let mut plan = crate::plan::Plan::default();
    plan.add(effect);
    let checked = crate::input_rules::FinalizedPlan::new(plan)
        .check_inputs(schemas, &HashMap::new())
        .expect("fixture plan admission");
    let scheduled = checked.schedule(&HashMap::new());
    let dependencies = build_scheduler_deps(&scheduled, std::iter::empty());
    assert!(dependencies.unresolved.is_empty());
    let ready = release_ready_effect(
        &scheduled,
        0,
        &dependencies.deps_of,
        &HashSet::new(),
        &HashSet::new(),
    )
    .expect("fixture slot is dependency ready")
    .expect("fixture has no failed dependencies")
    .into_basic()
    .expect("fixture provider write");
    let check = super::input_discharge::ExecutionInputChecks::new(
        checked.execution_input_checks(),
        scheduled.effects(),
        &[0],
    )
    .expect("fixture input checks")
    .take_write(0)
    .expect("unconsumed fixture slot");
    (ready, check)
}

pub(crate) struct ReadyDelete {
    instance: crate::effect::DeletedInstanceKey,
}
impl ReadyDelete {
    pub(crate) fn instance(&self) -> &crate::effect::DeletedInstanceKey {
        &self.instance
    }
    #[cfg(test)]
    pub(crate) fn for_test(instance: crate::effect::DeletedInstanceKey) -> Self {
        Self { instance }
    }
}

/// Dependency-ready authority to send one exact, input-discharged update.
///
/// ```compile_fail,E0277
/// use carina_core::{replacement::UpdatePermit, provider::UpdateRequest};
/// fn bypass(request: UpdateRequest) -> UpdatePermit { request.into() }
/// ```
pub struct UpdatePermit {
    pub(super) write: super::PreparedWrite<crate::provider::UpdateRequest>,
    pub(super) identifier: crate::provider::AppliedIdentifier,
}
pub(crate) struct ReadyUpdate {
    target: crate::resource::ResourceId,
    identifier: crate::provider::AppliedIdentifier,
}
impl ReadyUpdate {
    pub(super) fn authorize(
        self,
        write: super::PreparedWrite<crate::provider::UpdateRequest>,
    ) -> UpdatePermit {
        assert_eq!(
            &self.target,
            write.resource_id(),
            "scheduled update target changed"
        );
        UpdatePermit {
            write,
            identifier: self.identifier,
        }
    }
}
impl UpdatePermit {
    pub fn request(&self) -> &crate::provider::UpdateRequest {
        self.write.request()
    }
}

/// Only dependency-ready release can construct this token or its sealed resource.
pub(crate) struct ReadyCreate<S> {
    resource: Box<crate::resource::ResolvedResource<S>>,
}
impl<S: Clone> ReadyCreate<S> {
    #[cfg(test)]
    pub(crate) fn target(&self) -> &crate::resource::ResourceId {
        &self.resource.id
    }

    pub(super) fn authorize(
        self,
        request: super::PreparedWrite<crate::provider::CreateRequest>,
    ) -> CreatePermit {
        assert_eq!(
            &self.resource.id,
            request.resource_id(),
            "scheduled create target changed"
        );
        CreatePermit {
            request: Box::new(request),
        }
    }
}

pub(crate) enum ScheduledResume<S> {
    Resend {
        pending: Box<crate::provider::InFlightCreate>,
        create: ReadyCreate<S>,
    },
    Poll(Box<crate::provider::InFlightCreate>),
    Missing,
}

pub(super) enum PreparedResume {
    Resend(crate::provider::ResumeCreateRequest),
    Poll(crate::provider::PollCreateRequest),
}
impl<S: Clone> ScheduledResume<S> {
    pub(super) fn prepare(
        self,
        resource: &crate::resource::ResolvedResource<S>,
        write: super::PreparedWrite<crate::provider::CreateRequest>,
    ) -> Result<
        (
            PreparedResume,
            crate::binding_index::SecretPreservingAttributes,
            super::AcceptedRequest,
        ),
        super::EffectFailureKind,
    > {
        let attributes = write.writeback().clone();
        let accepted = super::AcceptedRequest::from_create_request(write.request())
            .with_resolved_desired_attributes(attributes.clone());
        let request = match self {
            Self::Resend { pending, create } => PreparedResume::Resend(
                crate::provider::ResumeCreateRequest::new(*pending, create.authorize(write))
                    .map_err(super::EffectFailureKind::execution)?,
            ),
            Self::Poll(pending) => PreparedResume::Poll(
                crate::provider::PollCreateRequest::new(*pending, write.into_parts().0)
                    .map_err(super::EffectFailureKind::execution)?,
            ),
            Self::Missing => {
                return Err(super::EffectFailureKind::message(format!(
                    "saved create continuation for {} changed before dispatch",
                    resource.id
                )));
            }
        };
        Ok((request, attributes, accepted))
    }
}

/// Basic provider operations released by the dependency scheduler.
///
/// ```compile_fail,E0599
/// use carina_core::effect::Effect;
/// fn narrow(effect: &Effect) { effect.as_basic(); }
/// ```
pub(crate) enum ScheduledBasicEffect<S> {
    Create {
        effect: Effect<S>,
        resource: crate::resource::ResolvedResource<S>,
        create: ReadyCreate<S>,
    },
    ResumeCreate {
        effect: Effect<S>,
        resource: crate::resource::ResolvedResource<S>,
        resume: ScheduledResume<S>,
    },
    Update {
        effect: Effect<S>,
        identifier: crate::provider::AppliedIdentifier,
        to: crate::resource::ResolvedResource<S>,
        update: ReadyUpdate,
    },
    Delete {
        effect: Effect<S>,
        permit: ReadyDelete,
    },
}
impl<S> ScheduledBasicEffect<S> {
    pub(crate) fn as_effect(&self) -> &Effect<S> {
        match self {
            Self::Create { effect, .. }
            | Self::ResumeCreate { effect, .. }
            | Self::Update { effect, .. }
            | Self::Delete { effect, .. } => effect,
        }
    }
}

pub(super) struct ReadyEffect<S> {
    effect: Effect<S>,
    continuation: Option<crate::provider::InFlightCreate>,
}
impl<S: Clone> ReadyEffect<S> {
    pub(super) fn into_basic(self) -> Option<ScheduledBasicEffect<S>> {
        let effect = self.effect;
        Some(match effect.clone() {
            Effect::Create(resource) => ScheduledBasicEffect::Create {
                effect,
                create: ReadyCreate {
                    resource: Box::new(resource.clone()),
                },
                resource,
            },
            Effect::ResumeCreate { resource, .. } => {
                let resume = match self.continuation {
                    Some(pending) if pending.id() != &resource.id => ScheduledResume::Missing,
                    Some(pending) if pending.identifier().is_some() => {
                        ScheduledResume::Poll(Box::new(pending))
                    }
                    Some(pending) => ScheduledResume::Resend {
                        pending: Box::new(pending),
                        create: ReadyCreate {
                            resource: Box::new(resource.clone()),
                        },
                    },
                    None => ScheduledResume::Missing,
                };
                ScheduledBasicEffect::ResumeCreate {
                    effect,
                    resource,
                    resume,
                }
            }
            Effect::Update {
                from: _,
                identifier,
                to,
                changed_attributes: _,
            } => ScheduledBasicEffect::Update {
                effect,
                update: ReadyUpdate {
                    target: to.id.clone(),
                    identifier: identifier.clone(),
                },
                identifier,
                to,
            },
            Effect::Delete {
                id,
                generation,
                identifier,
                ..
            } => {
                let instance = crate::effect::DeletedInstanceKey::new(
                    id.as_inner().clone(),
                    generation,
                    identifier.as_str(),
                );
                ScheduledBasicEffect::Delete {
                    effect,
                    permit: ReadyDelete { instance },
                }
            }
            Effect::Read { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
            | Effect::DeferredReplace(_)
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. } => return None,
        })
    }
}

/// This is the only production constructor for dispatch-capable endpoints.
/// The schedule owns both the prepared targets and the continuation choice;
/// completion and failure state come from the executor, never a public index API.
pub(super) fn release_ready_effect<S: Clone>(
    scheduled: &crate::replacement::ScheduledPlan<'_, S>,
    index: usize,
    dependencies: &HashMap<usize, HashSet<usize>>,
    completed: &HashSet<usize>,
    failed: &HashSet<usize>,
) -> Option<Result<ReadyEffect<S>, FailedDependency>> {
    release_ready_effect_mapped(
        scheduled,
        index,
        index,
        scheduled.effects(),
        dependencies,
        completed,
        failed,
    )
}

pub(super) fn release_ready_effect_mapped<S: Clone>(
    scheduled: &crate::replacement::ScheduledPlan<'_, S>,
    source: usize,
    index: usize,
    effects: &[Effect<S>],
    dependencies: &HashMap<usize, HashSet<usize>>,
    completed: &HashSet<usize>,
    failed: &HashSet<usize>,
) -> Option<Result<ReadyEffect<S>, FailedDependency>> {
    if !dependencies
        .get(&index)?
        .iter()
        .all(|dependency| completed.contains(dependency))
    {
        return None;
    }
    if let Some(failed) =
        FailureView::new(effects, dependencies, failed).find_failed_dependency(index)
    {
        return Some(Err(failed));
    }
    let effect = scheduled.effects().get(source)?.clone();
    let continuation = scheduled.continuation(effect.resource_id()).cloned();
    Some(Ok(ReadyEffect {
        effect,
        continuation,
    }))
}

/// Prepare an initially ready create without starting a full apply, for
/// bootstrap callers. This uses the executor's release and the sealed target;
/// an endpoint with pending prerequisites yields no permission.
/// A schedule without input admission cannot authorize bootstrap dispatch:
///
/// ```compile_fail,E0308
/// use carina_core::{
///     executor::{prepare_initial_create, ProviderPreparationContext},
///     replacement::ScheduledPlan,
/// };
/// async fn bypass(
///     scheduled: &ScheduledPlan<'_>,
///     context: &ProviderPreparationContext<'_>,
/// ) {
///     let _ = prepare_initial_create(scheduled, context).await;
/// }
/// ```
///
/// Input admission cannot be paired with a caller-chosen readiness graph:
///
/// ```compile_fail,E0061
/// use carina_core::{
///     executor::{prepare_initial_create, ProviderPreparationContext},
///     replacement::InputCheckedScheduledPlan, resolved_graph::ExpandedBindingIndex,
/// };
/// async fn swap_graph(
///     scheduled: &InputCheckedScheduledPlan<'_>, graph: &ExpandedBindingIndex,
///     context: &ProviderPreparationContext<'_>,
/// ) {
///     let _ = prepare_initial_create(scheduled, graph, context).await;
/// }
/// ```
pub async fn prepare_initial_create(
    plan: &crate::resolved_graph::InputCheckedInitialCreatePlan,
    context: &super::ProviderPreparationContext<'_>,
) -> Result<Option<CreatePermit>, super::ProviderPreparationError> {
    let scheduled = plan.checked.schedule(&HashMap::new());
    let obligations = scheduled.execution_input_checks();
    let (scheduled, _, sources) = scheduled.clone().expand_deferred_replacements();
    let input_error = super::ProviderPreparationError::InputDischarge;
    let mut checks = super::input_discharge::ExecutionInputChecks::new(
        obligations,
        scheduled.effects(),
        &sources,
    )
    .map_err(|error| input_error(super::EffectFailureKind::execution(error)))?;
    let dependencies = build_scheduler_deps(&scheduled, plan.graph.scheduler_target_ids());
    let completed = HashSet::new();
    for index in 0..scheduled.effects().len() {
        if dependencies
            .unresolved
            .iter()
            .any(|dependency| dependency.effect_index() == index)
        {
            continue;
        }
        let Some(Ok(ready)) = release_ready_effect(
            &scheduled,
            index,
            &dependencies.deps_of,
            &completed,
            &HashSet::new(),
        ) else {
            continue;
        };
        if let Some(ScheduledBasicEffect::Create { create, .. }) = ready.into_basic() {
            let resource = super::provider_ready::prepare_provider_ready_resource(
                create.resource.as_inner().clone(),
                context,
            )
            .await?;
            let request = crate::provider::CreateRequest::checked(resource);
            let attributes = request.resource().as_resource().resolved_attributes();
            let writeback = crate::binding_index::SecretPreservingAttributes::from_provider_values(
                &create.resource.resolved_attributes(),
                &attributes,
            );
            let write = checks
                .take_write(index)
                .and_then(|check| check.prepare_create(request, writeback, context.schemas()))
                .map_err(input_error)?;
            return Ok(Some(create.authorize(write)));
        }
    }
    Ok(None)
}

#[cfg(test)]
pub(crate) fn release_basic_for_test<S: Clone>(
    scheduled: &crate::replacement::ScheduledPlan<'_, S>,
    index: usize,
    completed: &HashSet<usize>,
) -> Option<ScheduledBasicEffect<S>> {
    let dependencies = build_scheduler_deps(scheduled, std::iter::empty());
    release_ready_effect(
        scheduled,
        index,
        &dependencies.deps_of,
        completed,
        &HashSet::new(),
    )?
    .ok()?
    .into_basic()
}

pub(super) struct SchedulerDeps {
    pub(super) deps_of: HashMap<usize, HashSet<usize>>,
    pub(super) unresolved: Vec<UnresolvedRequiredDependency>,
}

/// Internal preparation nodes mirror their write solely for diagnostics. They
/// are excluded from effect analysis, so they cannot become value producers.
#[derive(Default)]
pub(super) struct PreparationNodes {
    writes: HashMap<usize, usize>,
    pairs: Vec<(usize, usize, Vec<usize>)>,
    replacement_templates: HashMap<usize, HashSet<usize>>,
    schedule_to_runtime: Vec<usize>,
}

impl PreparationNodes {
    pub(super) fn install<S: Clone>(
        effects: &mut Vec<Effect<S>>,
        input_checks: &super::input_discharge::ExecutionInputChecks,
    ) -> Self {
        let mut nodes = Self {
            schedule_to_runtime: (0..effects.len()).collect(),
            replacement_templates: input_checks
                .checks
                .keys()
                .copied()
                .filter(|&index| matches!(effects[index], Effect::DeferredReplace(_)))
                .map(|index| {
                    (
                        index,
                        input_checks
                            .absorbed
                            .get(&index)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect(),
            ..Self::default()
        };
        let mut pairs = input_checks
            .paired
            .iter()
            .filter(|(write, _)| {
                matches!(
                    effects[**write],
                    Effect::Create(_) | Effect::ResumeCreate { .. }
                )
            })
            .collect::<Vec<_>>();
        pairs.sort_by_key(|(write, _)| *write);
        for (&write, deletes) in pairs {
            let preparation = effects.len();
            effects.push(effects[write].clone());
            nodes.writes.insert(preparation, write);
            nodes.pairs.push((preparation, write, deletes.clone()));
        }
        nodes
    }

    pub(super) fn write(&self, node: usize) -> Option<usize> {
        self.writes.get(&node).copied()
    }

    pub(super) fn dependencies<S>(
        &self,
        effects: &[Effect<S>],
        deps: &mut HashMap<usize, HashSet<usize>>,
    ) {
        for (&template, absorbed) in &self.replacement_templates {
            // Only the template's own expanded teardown slots can form the
            // template <-> absorbed-delete cycle. Preserve anonymous identity
            // gates, DependsOn, and every other scheduler prerequisite.
            if let Some(dependencies) = deps.get_mut(&template) {
                dependencies.retain(|index| !absorbed.contains(index));
            }
        }
        for (preparation, write, deletes) in &self.pairs {
            // A request's value prerequisites do not include teardown/order
            // edges. Inheriting the paired Delete here creates a false cycle.
            let inputs = deps
                .get(write)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&index| {
                    let (Effect::Create(resource) | Effect::ResumeCreate { resource, .. }) =
                        &effects[*write]
                    else {
                        unreachable!("preparation write")
                    };
                    !matches!(effects[index], Effect::Delete { .. })
                        && resource
                            .value_dependencies()
                            .iter()
                            .any(|id| id.as_inner() == effects[index].resource_id())
                })
                .collect();
            deps.insert(*preparation, inputs);
            deps.entry(*write).or_default().insert(*preparation);
            for delete in deletes {
                deps.entry(*delete).or_default().insert(*preparation);
            }
        }
    }

    pub(super) fn schedule_index(&self, runtime: usize) -> Option<usize> {
        self.schedule_to_runtime
            .iter()
            .position(|&index| index == runtime)
    }
    pub(super) fn runtime_index(&self, scheduled: usize) -> usize {
        self.schedule_to_runtime[scheduled]
    }
    pub(super) fn synchronize<S: Clone>(
        &mut self,
        effects: &mut Vec<Effect<S>>,
        admitted: &crate::replacement::AdmittedReplacementEffects<S>,
    ) {
        for (index, effect) in admitted.effects().iter().enumerate() {
            if let Some(&runtime) = self.schedule_to_runtime.get(index) {
                effects[runtime] = effect.clone();
            } else {
                self.schedule_to_runtime.push(effects.len());
                effects.push(effect.clone());
            }
        }
    }
    pub(super) fn rebuild<'a, S: Clone>(
        &self,
        effects: &[Effect<S>],
        admitted: &crate::replacement::AdmittedReplacementEffects<S>,
        origins: impl IntoIterator<Item = &'a ResolvedResourceId>,
    ) -> SchedulerDeps {
        let SchedulerDeps {
            deps_of,
            mut unresolved,
        } = build_scheduler_deps(admitted, origins);
        let mut deps = deps_of
            .into_iter()
            .map(|(index, dependencies)| {
                (
                    self.runtime_index(index),
                    dependencies
                        .into_iter()
                        .map(|index| self.runtime_index(index))
                        .collect(),
                )
            })
            .collect();
        for dependency in &mut unresolved {
            dependency.remap_effect_index(self.runtime_index(dependency.effect_index()));
        }
        self.dependencies(effects, &mut deps);
        SchedulerDeps {
            deps_of: deps,
            unresolved,
        }
    }
}

pub(super) struct PureMetaStep<'a, S> {
    effect: &'a Effect<S>,
    template: &'a DeferredForExpression<S>,
    dependencies: &'a crate::resource::DeferredDependencyTemplate,
}

impl<'a, S> PureMetaStep<'a, S> {
    fn from_effect(effect: &'a Effect<S>) -> Option<Self> {
        match effect {
            Effect::DeferredCreate(payload) => Some(Self {
                effect,
                template: payload.template.as_ref(),
                dependencies: &payload.dependency_template,
            }),
            Effect::DeferredReplace(payload) => Some(Self {
                effect,
                template: payload.template.as_ref(),
                dependencies: payload.dependency_template(),
            }),
            Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Wait { .. }
            | Effect::Read { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. } => None,
        }
    }
}

pub(super) enum PureMetaOutcome<S> {
    NotPureMeta,
    Materialized(crate::replacement::AdmittedReplacementEffects<S>),
    Failed(super::DeferredCreateFailure),
}

pub(super) fn try_dispatch_pure_meta<S: Clone + crate::resource::AnonymousIdentityModuleSource>(
    effect: &Effect<S>,
    bindings: &ResolvedBindings,
    ctx: &PureMetaCtx<'_, S>,
) -> PureMetaOutcome<S> {
    let Some(step) = PureMetaStep::from_effect(effect) else {
        return PureMetaOutcome::NotPureMeta;
    };

    match dispatch_deferred_create(step.effect, step.template, step.dependencies, bindings, ctx) {
        Ok(children) => PureMetaOutcome::Materialized(children),
        Err(failure) => PureMetaOutcome::Failed(failure),
    }
}

/// Equality-bearing key used to carry failures across scheduler phases.
/// Unbound loops retain their stable system key here; this value must never
/// be rendered to users.
pub(super) fn failure_dependency_keys<S>(effect: &Effect<S>) -> Vec<SchedulerKey> {
    effect.scheduling_keys()
}

/// User-facing label for waiting and dependency-failure messages.
pub(super) fn failure_display_name<S>(effect: &Effect<S>) -> Option<String> {
    match effect {
        Effect::DeferredCreate(payload) => Some(payload.template.target().display_name()),
        Effect::DeferredReplace(payload) => Some(payload.template.target().display_name()),
        Effect::ReleaseListKeys { .. } => None,
        Effect::Read { .. }
        | Effect::Create(_)
        | Effect::ResumeCreate { .. }
        | Effect::Update { .. }
        | Effect::Delete { .. }
        | Effect::Import { .. }
        | Effect::Remove { .. }
        | Effect::Move { .. }
        | Effect::Wait { .. } => effect.binding_address().map(|binding| binding.to_string()),
    }
}

pub(super) fn build_scheduler_deps<'a, S: Clone>(
    admitted: &crate::replacement::AdmittedReplacementEffects<S>,
    unresolved_resources: impl IntoIterator<Item = &'a ResolvedResourceId>,
) -> SchedulerDeps {
    let (analysis, unresolved) = match admitted.dependency_analysis(unresolved_resources) {
        Ok(analysis) => (analysis, Vec::new()),
        Err(failure) => failure.into_parts(),
    };
    let deps_of = analysis.into_deps_of();
    SchedulerDeps {
        deps_of,
        unresolved,
    }
}

/// Format the user-facing skip reason for a non-Wait effect whose
/// dependency failed. Centralizing this here keeps both schedulers
/// (phased / parallel) and both `Labeled` / `Anonymous` cases in one
/// place — adding a new variant to [`FailedDependency`] is a compile
/// error here, not a silent message-format drift across call sites.
pub(super) fn dependency_failed_reason(failed: &FailedDependency) -> String {
    match failed {
        FailedDependency::Labeled(label) => format!("dependency '{label}' failed"),
        FailedDependency::Anonymous => "dependency failed".to_string(),
    }
}

/// Format the user-facing skip reason for a Wait effect whose
/// dependency failed (renders as `"unsatisfiable: ..."`). Anonymous
/// dependencies render as a plain `dependency-failed` token rather
/// than fabricating a binding-shaped placeholder.
pub(super) fn wait_dependency_failed_reason(failed: &FailedDependency) -> String {
    match failed {
        FailedDependency::Labeled(binding) => {
            let detail = super::wait::unsatisfiable_reason_message(
                &super::wait::UnsatisfiableReason::DependencyFailed {
                    binding: binding.clone(),
                },
            );
            format!("unsatisfiable: {detail}")
        }
        FailedDependency::Anonymous => "unsatisfiable: dependency-failed".to_string(),
    }
}

/// A failed dependency that blocks `effects[idx]` from dispatching.
///
/// The two variants are distinct concepts that callers must handle
/// separately — they are not interchangeable strings. `Labeled` carries a
/// user-facing binding or source-location label; `Anonymous` means the failed
/// dependency has no display identity (e.g. an anonymous `Replace` whose
/// `to.binding` is `None`). The message must not fabricate a binding-shaped
/// placeholder for it.
#[derive(Debug, Clone)]
pub(super) enum FailedDependency {
    Labeled(String),
    Anonymous,
}

/// A snapshot of cumulative cross-phase failure state, paired with the
/// dependency graph and effect list. Dispatch-time failure detection and
/// Wait terminal checks share this view so their visibility cannot drift.
pub(super) struct FailureView<'a, S> {
    pub(super) effects: &'a [Effect<S>],
    pub(super) deps_of: &'a HashMap<usize, HashSet<usize>>,
    pub(super) failed_indices: &'a HashSet<usize>,
    failed_dependency_labels: HashMap<SchedulerKey, String>,
}

impl<'a, S> FailureView<'a, S> {
    pub(super) fn new(
        effects: &'a [Effect<S>],
        deps_of: &'a HashMap<usize, HashSet<usize>>,
        failed_indices: &'a HashSet<usize>,
    ) -> Self {
        let failed_dependency_labels = failed_indices
            .iter()
            .flat_map(|idx| {
                let effect = effects.get(*idx)?;
                let label = failure_display_name(effect)?;
                Some(
                    failure_dependency_keys(effect)
                        .into_iter()
                        .map(move |key| (key, label.clone())),
                )
            })
            .flatten()
            .collect();
        Self {
            effects,
            deps_of,
            failed_indices,
            failed_dependency_labels,
        }
    }

    /// Find the failed dependency that would cause `effects[idx]` to be
    /// skipped before dispatch. Checks both in-phase graph edges and the
    /// cross-phase binding fallback used for phase-scoped dependency maps.
    pub(super) fn find_failed_dependency(&self, idx: usize) -> Option<FailedDependency> {
        if let Some(deps) = self.deps_of.get(&idx)
            && let Some(&dep_idx) = deps.iter().find(|d| self.failed_indices.contains(d))
        {
            let label = self.effects.get(dep_idx).and_then(failure_display_name);
            return Some(match label {
                Some(label) => FailedDependency::Labeled(label),
                None => FailedDependency::Anonymous,
            });
        }

        self.effects[idx]
            .blocking_keys()
            .into_iter()
            .find_map(|key| self.failed_dependency_labels.get(&key).cloned())
            .map(FailedDependency::Labeled)
    }

    pub(super) fn is_effectively_pre_skipped(&self, idx: usize) -> bool {
        self.find_failed_dependency(idx).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::EffectGeneration;
    use crate::resource::ResourceId;

    #[test]
    fn resource_identity_does_not_collide_with_binding_failure_key() {
        use crate::resource::{ResolvedResource, ResolvedResourceId, RootScope};
        use crate::wait::{AttrPath, ManagedWaitTarget, WaitPredicate};
        use std::time::Duration;

        let shared = RootScope::new().resolve("shared".into());
        let target = ResolvedResource::from_id(ResolvedResourceId::new(ResourceId::with_identity(
            "test.Resource",
            "shared",
        )));
        let effects: Vec<Effect> = vec![
            Effect::Delete {
                id: ResolvedResourceId::new(ResourceId::with_identity(
                    "test.Resource",
                    "different-resource",
                )),
                identifier: crate::effect::fixture_applied_identifier("failed-resource"),
                generation: EffectGeneration::Current,
                directives: crate::resource::Directives::default(),
                binding: Some(shared),
                dependencies: Default::default(),
                explicit_dependencies: Default::default(),
                blocked_by_updates: Default::default(),
            },
            Effect::Wait {
                id: crate::resource::ResolvedResourceId::with_identity(
                    "__wait",
                    "wait_for_shared_resource",
                ),
                target_id: ManagedWaitTarget::for_test(&target),
                until: WaitPredicate::equals(
                    AttrPath::single("status"),
                    serde_json::from_value::<crate::wait::WaitRhs>(
                        serde_json::json!({"String": "'ready'"}),
                    )
                    .unwrap(),
                )
                .expect("valid wait predicate"),
                until_surface: "shared.status == ready".to_string(),
                timeout: Duration::from_secs(60),
                interval: Duration::from_secs(1),
                explicit_dependencies: Default::default(),
            },
        ];
        let deps_of = HashMap::new();
        let failed_indices = HashSet::from([0]);

        assert!(
            FailureView::new(&effects, &deps_of, &failed_indices)
                .find_failed_dependency(1)
                .is_none(),
            "a failed binding named `shared` must not masquerade as the distinct resource identity `shared`",
        );
    }

    #[test]
    fn unbound_loop_key_is_distinct_from_binding_and_graph_edge_uses_source_location() {
        let parsed = crate::parser::parse_file(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.values {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
            std::path::Path::new("main.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("unbound deferred loop parses");
        let parsed = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| {
                crate::module_resolver::resolve_modules_providerless(
                    crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
                    &crate::paths::ResolvedPath::new(std::path::Path::new(".")).unwrap(),
                    session,
                )
                .expect("deferred fixture crosses the module-resolution seam")
            },
        )
        .into_expanded(
            &crate::schema::SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("deferred-loop fixture contains no parser placeholders");
        let template = parsed.deferred_for_expressions()[0].clone();
        let scheduler_key = template.target().scheduler_key().to_string();
        assert!(scheduler_key.starts_with("_loop_"));
        let _deferred_id = crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "failed_unbound_loop",
        ));

        let graph = crate::resolved_graph::resolve_expanded_graph(parsed.into())
            .expect("upstream fixture graph");
        let (template, descriptor) = graph
            .compile_deferred_for(template)
            .expect("compiled fixture loop");
        let effects = vec![
            Effect::DeferredCreate(crate::effect::DeferredCreatePayload::new(
                Box::new(template),
                descriptor,
            )),
            Effect::Delete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "test.Child",
                    "dependent",
                )),
                identifier: crate::effect::fixture_applied_identifier("dependent"),
                generation: EffectGeneration::Current,
                directives: crate::resource::Directives::default(),
                binding: Some(crate::resource::RootScope::new().resolve("dependent".into())),
                dependencies: Default::default(),
                explicit_dependencies: std::iter::once(ResolvedResourceId::with_identity(
                    "test.Resource",
                    crate::resource::ResourceIdentity::new(scheduler_key),
                ))
                .collect(),
                blocked_by_updates: Default::default(),
            },
        ];
        let failed_indices = HashSet::from([0]);

        assert!(
            FailureView::new(&effects, &HashMap::new(), &failed_indices)
                .find_failed_dependency(1)
                .is_none(),
            "a binding that spells like an unbound-loop key must remain a distinct scheduler key",
        );

        let deps_of = HashMap::from([(1, HashSet::from([0]))]);
        let failed = FailureView::new(&effects, &deps_of, &failed_indices)
            .find_failed_dependency(1)
            .expect("the explicit graph dependency must be recognized");

        assert!(matches!(failed, FailedDependency::Labeled(label) if label == "main.crn:2"));
    }
    #[test]
    fn cross_phase_unbound_failure_uses_source_location_not_scheduler_key() {
        let parsed = crate::parser::parse_file(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.values {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
            std::path::Path::new("main.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("unbound deferred loop parses");
        let parsed = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| {
                crate::module_resolver::resolve_modules_providerless(
                    crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
                    &crate::paths::ResolvedPath::new(".").unwrap(),
                    session,
                )
                .map_err(Box::new)
            },
        )
        .map_err(|error| *error)
        .expect("module resolution")
        .into_expanded(
            &crate::schema::SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("expanded loop");
        let template = parsed.deferred_for_expressions()[0].clone();
        let graph = crate::resolved_graph::resolve_expanded_graph(parsed.into())
            .expect("upstream fixture graph");
        let (template, dependency_template) = graph
            .compile_deferred_for(template)
            .expect("loop compilation");
        let scheduler_key = template.target().scheduler_key().to_string();
        assert!(scheduler_key.starts_with("_loop_"));
        let deferred_id = template.deferred_effect_id().clone();

        let effects = vec![
            Effect::DeferredCreate(crate::effect::DeferredCreatePayload::new(
                Box::new(template),
                dependency_template,
            )),
            Effect::Delete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "test.Child",
                    "dependent",
                )),
                identifier: crate::effect::fixture_applied_identifier("dependent"),
                generation: EffectGeneration::Current,
                directives: crate::resource::Directives::default(),
                binding: Some(crate::resource::absolute_binding_address_for_test(
                    "dependent",
                )),
                dependencies: Default::default(),
                explicit_dependencies: std::iter::once(deferred_id).collect(),
                blocked_by_updates: Default::default(),
            },
        ];
        let deps_of = HashMap::new();
        let failed_indices = HashSet::from([0]);

        let failed = FailureView::new(&effects, &deps_of, &failed_indices)
            .find_failed_dependency(1)
            .expect("the cross-phase dependency must be recognized");

        assert!(matches!(failed, FailedDependency::Labeled(label) if label == "main.crn:2"));
    }
}
