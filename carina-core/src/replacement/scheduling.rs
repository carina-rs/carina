//! Execution capabilities minted from admitted endpoints, never raw effects.
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use crate::effect::{DeletedInstanceKey, Effect};
use crate::provider::{AppliedIdentifier, InFlightCreate};
use crate::resource::{Current, Directives, ResolvedResource, ResourceId, SnapshotState};

use super::{AdmittedReplacementEffects, ContinuationOrder};

/// Permission to send a create for one admitted target and generation set.
/// Only admitted scheduling can construct it; it cannot be deserialized.
///
/// The permit exposes the target that dispatch needs; replacement history
/// belongs to the admitted schedule rather than the dispatch capability.
///
/// ```compile_fail,E0599
/// use carina_core::replacement::CreatePermit;
/// fn history(permit: &CreatePermit) {
///     permit.old_instances();
/// }
/// ```
///
/// ```compile_fail,E0451
/// use carina_core::replacement::CreatePermit;
/// let permit = CreatePermit {
///     target: todo!(),
/// };
/// ```
#[derive(Debug, Clone)]
pub struct CreatePermit {
    target: Arc<ResourceId>,
}

impl CreatePermit {
    pub fn target(&self) -> &ResourceId {
        &self.target
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn create_permit_for_test(id: &ResourceId) -> CreatePermit {
    let target = ResolvedResource::from_id(crate::resource::ResolvedResourceId::new(id.clone()));
    AdmittedReplacementEffects::resolve(
        &[Effect::Create(target)],
        &crate::schema::SchemaRegistry::new(),
        |_| Default::default(),
    )
    .unwrap()
    .schedule(&Default::default())
    .effect(0)
    .into_create_permit()
    .unwrap()
}

/// A Delete keeps its complete provider route and generation. It cannot be
/// detached from the admitted schedule and retargeted to another instance.
#[derive(Debug, Clone)]
pub(crate) struct DeletePermit {
    instance: DeletedInstanceKey,
    identifier: AppliedIdentifier,
}

impl DeletePermit {
    pub(crate) fn id(&self) -> &ResourceId {
        self.instance.id()
    }

    pub(crate) fn identifier(&self) -> &AppliedIdentifier {
        &self.identifier
    }

    pub(crate) fn instance(&self) -> &DeletedInstanceKey {
        &self.instance
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ScheduledResume {
    Resend {
        pending: Box<InFlightCreate>,
        permit: CreatePermit,
    },
    Poll(Box<InFlightCreate>),
    Missing,
}

#[derive(Debug, Clone)]
enum Permission {
    Create(CreatePermit),
    Resume(ScheduledResume),
    Delete(DeletePermit),
    Other,
}

/// An immutable effect paired with its admission capability. Cloning the raw
/// effect does not retain that capability.
#[derive(Debug, Clone)]
pub struct ScheduledEffect<S = crate::resource::ModuleSource> {
    effect: Effect<S>,
    permission: Permission,
}

impl<S> ScheduledEffect<S> {
    pub fn effect(&self) -> &Effect<S> {
        &self.effect
    }

    pub fn into_create_permit(self) -> Option<CreatePermit> {
        match self.permission {
            Permission::Create(permit) => Some(permit),
            Permission::Resume(_) | Permission::Delete(_) | Permission::Other => None,
        }
    }

    pub(crate) fn as_basic(&self) -> Option<ScheduledBasicEffect<'_, S>> {
        let effect = &self.effect;
        match (&self.effect, &self.permission) {
            (Effect::Create(resource), Permission::Create(permit)) => {
                Some(ScheduledBasicEffect::Create {
                    effect,
                    resource,
                    permit: permit.clone(),
                })
            }
            (Effect::ResumeCreate { resource, .. }, Permission::Resume(resume)) => {
                Some(ScheduledBasicEffect::ResumeCreate {
                    effect,
                    resource,
                    resume: resume.clone(),
                })
            }
            (
                Effect::Update {
                    from,
                    identifier,
                    to,
                    changed_attributes,
                },
                Permission::Other,
            ) => Some(ScheduledBasicEffect::Update {
                effect,
                from,
                identifier,
                to,
                changed_attributes,
            }),
            (Effect::Delete { directives, .. }, Permission::Delete(permit)) => {
                Some(ScheduledBasicEffect::Delete {
                    effect,
                    directives,
                    permit: permit.clone(),
                })
            }
            _ => None,
        }
    }
}

/// The basic executor accepts only these capability-bearing endpoints.
///
/// Raw effects cannot narrow into a provider-dispatch endpoint.
///
/// ```compile_fail,E0599
/// use carina_core::effect::Effect;
/// fn narrow(effect: &Effect) {
///     effect.as_basic();
/// }
/// ```
pub(crate) enum ScheduledBasicEffect<'a, S> {
    Create {
        effect: &'a Effect<S>,
        resource: &'a ResolvedResource<S>,
        permit: CreatePermit,
    },
    ResumeCreate {
        effect: &'a Effect<S>,
        resource: &'a ResolvedResource<S>,
        resume: ScheduledResume,
    },
    Update {
        effect: &'a Effect<S>,
        from: &'a SnapshotState,
        identifier: &'a AppliedIdentifier,
        to: &'a ResolvedResource<S>,
        changed_attributes: &'a [String],
    },
    Delete {
        effect: &'a Effect<S>,
        directives: &'a Directives,
        permit: DeletePermit,
    },
}

impl<'a, S> ScheduledBasicEffect<'a, S> {
    pub(crate) fn as_effect(&self) -> &'a Effect<S> {
        match self {
            Self::Create { effect, .. }
            | Self::ResumeCreate { effect, .. }
            | Self::Update { effect, .. }
            | Self::Delete { effect, .. } => effect,
        }
    }
}

/// An admitted schedule specialized to the locked create continuations.
/// Identifierless continuations retain the resolver's resend order. Identified
/// continuations select a poll-only branch which carries no create permission.
#[derive(Debug, Clone)]
pub struct ScheduledEffects<S = crate::resource::ModuleSource> {
    admitted: AdmittedReplacementEffects<S>,
    continuations: HashMap<ResourceId, InFlightCreate>,
}

impl<S> Deref for ScheduledEffects<S> {
    type Target = AdmittedReplacementEffects<S>;

    fn deref(&self) -> &Self::Target {
        &self.admitted
    }
}

impl<S: Clone> ScheduledEffects<S> {
    pub(crate) fn expand_deferred_replacements(
        self,
    ) -> (Self, std::collections::HashSet<usize>, Vec<usize>) {
        let (admitted, teardowns, sources) = self.admitted.expand_deferred_replacements();
        (
            Self {
                admitted,
                continuations: self.continuations,
            },
            teardowns,
            sources,
        )
    }

    pub(crate) fn with_materialized(&self, admitted: AdmittedReplacementEffects<S>) -> Self {
        admitted.schedule(&self.continuations)
    }

    pub fn effect(&self, index: usize) -> ScheduledEffect<S> {
        let effect = self.admitted.effects[index].clone();
        let create_permit = || CreatePermit {
            target: Arc::new(effect.resource_id().clone()),
        };
        let permission = match &effect {
            Effect::Create(_) => Permission::Create(create_permit()),
            Effect::ResumeCreate { resource, .. } => {
                let pending = self.continuations.get(&resource.id).cloned();
                Permission::Resume(match pending {
                    Some(pending) if pending.id() != &resource.id => ScheduledResume::Missing,
                    Some(pending) if pending.identifier().is_some() => {
                        ScheduledResume::Poll(Box::new(pending))
                    }
                    Some(pending) => ScheduledResume::Resend {
                        pending: Box::new(pending),
                        permit: create_permit(),
                    },
                    None => ScheduledResume::Missing,
                })
            }
            Effect::Delete {
                id,
                generation,
                identifier,
                ..
            } => Permission::Delete(DeletePermit {
                instance: DeletedInstanceKey::new(
                    id.as_inner().clone(),
                    generation.clone(),
                    identifier.as_str(),
                ),
                identifier: identifier.clone(),
            }),
            _ => Permission::Other,
        };
        ScheduledEffect { effect, permission }
    }
}

impl<S: Clone> AdmittedReplacementEffects<S> {
    pub fn schedule(
        mut self,
        continuations: &HashMap<ResourceId, InFlightCreate>,
    ) -> ScheduledEffects<S> {
        for replacement in &mut self.scheduling.replacements {
            if replacement.order.continuation.is_none() {
                continue;
            }
            replacement.order.continuation = Some(
                match continuations.get(replacement.order.target.as_inner()) {
                    Some(pending) if pending.id() != replacement.order.target.as_inner() => {
                        ContinuationOrder::Blocked
                    }
                    Some(pending) if pending.identifier().is_some() => ContinuationOrder::Poll,
                    None => ContinuationOrder::Blocked,
                    Some(_) => ContinuationOrder::Resend,
                },
            );
        }
        ScheduledEffects {
            admitted: self,
            continuations: continuations.clone(),
        }
    }
}

/// A continuation-specialized schedule bound to its admitted source plan.
/// Only [`crate::plan::AdmittedPlan::schedule`] can construct this value.
/// Consumers obtain plan metadata through [`Self::plan`], so they cannot pair
/// scheduling decisions with another plan's effects or display metadata.
///
/// The source plan cannot be replaced while keeping the schedule:
///
/// ```compile_fail,E0616
/// use carina_core::{plan::AdmittedPlan, replacement::ScheduledPlan};
/// fn retarget<'a>(scheduled: &mut ScheduledPlan<'a>, other: &'a AdmittedPlan) {
///     scheduled.plan = other;
/// }
/// ```
///
/// An unrelated effects schedule cannot be attached to the source plan:
///
/// ```compile_fail,E0616
/// use carina_core::replacement::{ScheduledEffects, ScheduledPlan};
/// fn swap(scheduled: &mut ScheduledPlan<'_>, other: ScheduledEffects) {
///     scheduled.effects = other;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ScheduledPlan<'plan, S = crate::resource::ModuleSource> {
    plan: &'plan crate::plan::AdmittedPlan<S>,
    effects: ScheduledEffects<S>,
}

impl<'plan, S> ScheduledPlan<'plan, S> {
    pub fn plan(&self) -> &'plan crate::plan::AdmittedPlan<S> {
        self.plan
    }
}

impl<S> Deref for ScheduledPlan<'_, S> {
    type Target = ScheduledEffects<S>;

    fn deref(&self) -> &Self::Target {
        &self.effects
    }
}

impl<S: Clone> ScheduledPlan<'_, S> {
    pub(crate) fn expand_deferred_replacements(
        self,
    ) -> (Self, std::collections::HashSet<usize>, Vec<usize>) {
        let (effects, teardowns, sources) = self.effects.expand_deferred_replacements();
        (
            Self {
                plan: self.plan,
                effects,
            },
            teardowns,
            sources,
        )
    }

    pub(crate) fn with_materialized(&self, admitted: AdmittedReplacementEffects<S>) -> Self {
        Self {
            plan: self.plan,
            effects: self.effects.with_materialized(admitted),
        }
    }
}

impl<S: Clone> crate::plan::AdmittedPlan<S> {
    pub fn schedule(&self, current: &HashMap<ResourceId, Current>) -> ScheduledPlan<'_, S> {
        let continuations = current
            .iter()
            .filter_map(|(id, current)| match current {
                Current::CreateInFlight(pending) => Some((id.clone(), pending.clone())),
                _ => None,
            })
            .collect();
        ScheduledPlan {
            plan: self,
            effects: self.admitted_effects().schedule(&continuations),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::EffectGeneration;
    use crate::provider::{CreateAttempt, PendingToken, Sent};
    use crate::resource::{DeposedKey, ResolvedResourceId};
    use crate::schema::{PatternResourceSchema, SchemaRegistry};

    fn schemas() -> SchemaRegistry {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "",
            PatternResourceSchema::new("Record")
                .with_conflicting_replacement()
                .validate()
                .unwrap(),
        );
        schemas
    }

    fn id(identity: &'static str, route: &str) -> ResolvedResourceId {
        ResolvedResourceId::with_provider_identity("", "Record", identity, Some(route.into()))
    }

    fn delete(id: ResolvedResourceId, generation: EffectGeneration) -> Effect {
        Effect::Delete {
            id,
            identifier: AppliedIdentifier::new("old-provider-id").unwrap(),
            generation,
            directives: Default::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        }
    }

    #[test]
    fn scheduled_create_binds_all_state_row_lineage_generations_and_delete_routes() {
        let target = id("target", "new-route");
        let mut effects = vec![Effect::Create(ResolvedResource::from_id(target.clone()))];
        let old = [
            (id("target", "old-current"), EffectGeneration::Current),
            (
                id("target", "old-deposed"),
                EffectGeneration::Deposed(DeposedKey::new_unique()),
            ),
            (
                id("source", "moved-deposed"),
                EffectGeneration::Deposed(DeposedKey::new_unique()),
            ),
        ];
        for (id, generation) in &old {
            effects.push(delete(id.clone(), generation.clone()));
        }
        effects.push(delete(
            id("unrelated", "old-current"),
            EffectGeneration::Current,
        ));
        for (from, to) in [("source", "middle"), ("middle", "target")] {
            effects.push(Effect::Move {
                from: id(from, "move-route"),
                to: id(to, "move-route"),
                origin: None,
            });
        }
        let scheduled =
            AdmittedReplacementEffects::resolve(&effects, &schemas(), |_| Default::default())
                .unwrap()
                .schedule(&HashMap::new());
        let permit = scheduled.effect(0).into_create_permit().unwrap();
        assert_eq!(permit.target(), target.as_inner());
        let graph = scheduled.dependency_analysis(std::iter::empty()).unwrap();
        assert_eq!(graph.deps_of(0).unwrap(), &(1..=old.len()).collect());
        for (index, (id, generation)) in old.iter().enumerate() {
            let key = DeletedInstanceKey::new(
                id.as_inner().clone(),
                generation.clone(),
                "old-provider-id",
            );
            assert!(graph.deps_of(0).unwrap().contains(&(index + 1)));
            let endpoint = scheduled.effect(index + 1);
            let Some(ScheduledBasicEffect::Delete { permit, .. }) = endpoint.as_basic() else {
                panic!("old generation must have a scheduled Delete capability");
            };
            assert_eq!(permit.instance(), &key);
            assert_eq!(permit.id().provider_instance, id.provider_instance);
        }
    }

    #[test]
    fn scheduled_continuation_selects_separate_resend_and_poll_capabilities() {
        for identified in [false, true] {
            let target = id("target", "new-route");
            let (attempt, _cancellation) = CreateAttempt::new(target.as_inner().clone());
            let pending = if identified {
                attempt
                    .issued(
                        AppliedIdentifier::new("new-id").unwrap(),
                        Sent::AllRequestAttributes,
                    )
                    .pending(PendingToken::new("continuation"), 1)
            } else {
                attempt.pending(
                    PendingToken::new("continuation"),
                    Sent::AllRequestAttributes,
                    1,
                )
            }
            .persist(Default::default(), Default::default());
            let mut old = delete(
                id("target", "old-alias"),
                EffectGeneration::Deposed(DeposedKey::new_unique()),
            );
            if let Effect::Delete {
                blocked_by_updates, ..
            } = &mut old
            {
                blocked_by_updates.insert(target.clone());
            }
            let effects = [
                Effect::ResumeCreate {
                    resource: ResolvedResource::from_id(target.clone()),
                    started_at: pending.started_at().clone(),
                },
                old,
            ];
            let admitted =
                AdmittedReplacementEffects::resolve(&effects, &schemas(), |_| Default::default())
                    .unwrap();
            let scheduled =
                admitted.schedule(&HashMap::from([(target.as_inner().clone(), pending)]));
            let graph = scheduled.dependency_analysis(std::iter::empty()).unwrap();
            assert_eq!(graph.deps_of(1).unwrap().contains(&0), identified);
            assert_eq!(graph.deps_of(0).unwrap().contains(&1), !identified);
            let endpoint = scheduled.effect(0);
            match endpoint.as_basic().unwrap() {
                ScheduledBasicEffect::ResumeCreate {
                    resume: ScheduledResume::Poll(_),
                    ..
                } => assert!(identified),
                ScheduledBasicEffect::ResumeCreate {
                    resume: ScheduledResume::Resend { permit, .. },
                    ..
                } => {
                    assert!(!identified);
                    assert_eq!(permit.target(), target.as_inner());
                    assert_eq!(
                        graph.deps_of(0).unwrap(),
                        &std::collections::HashSet::from([1])
                    );
                }
                _ => panic!("continuation must carry exactly its admitted execution capability"),
            }
            if identified {
                let (attempt, _cancellation) = CreateAttempt::new(target.as_inner().clone());
                let unissued = attempt
                    .pending(PendingToken::new("unissued"), Sent::AllRequestAttributes, 1)
                    .persist(Default::default(), Default::default());
                let rescheduled = scheduled
                    .admitted
                    .clone()
                    .schedule(&HashMap::from([(target.as_inner().clone(), unissued)]));
                let graph = rescheduled.dependency_analysis(std::iter::empty()).unwrap();
                assert!(graph.deps_of(0).unwrap().contains(&1));
                assert!(!graph.deps_of(1).unwrap().contains(&0));
                assert!(matches!(
                    rescheduled.effect(0).as_basic(),
                    Some(ScheduledBasicEffect::ResumeCreate {
                        resume: ScheduledResume::Resend { .. },
                        ..
                    })
                ));
            }
        }
    }

    #[test]
    fn unavailable_continuation_cannot_release_old_generation_cleanup() {
        let target = id("target", "new-route");
        let effects = [
            Effect::ResumeCreate {
                resource: ResolvedResource::from_id(target),
                started_at: crate::provider::Timestamp::now(),
            },
            delete(
                id("target", "old-route"),
                EffectGeneration::Deposed(DeposedKey::new_unique()),
            ),
        ];
        let scheduled =
            AdmittedReplacementEffects::resolve(&effects, &schemas(), |_| Default::default())
                .unwrap()
                .schedule(&HashMap::new());
        let graph = scheduled.dependency_analysis(std::iter::empty()).unwrap();
        assert!(graph.deps_of(1).unwrap().contains(&0));
        assert!(!graph.deps_of(0).unwrap().contains(&1));
        assert!(matches!(
            scheduled.effect(0).as_basic(),
            Some(ScheduledBasicEffect::ResumeCreate {
                resume: ScheduledResume::Missing,
                ..
            })
        ));
    }
}
