//! Read-only scheduling specialized from admitted plans and locked continuations.
use std::collections::HashMap;
use std::ops::Deref;

use crate::effect::Effect;
use crate::provider::InFlightCreate;
use crate::resource::{Current, ResourceId};

use super::{AdmittedReplacementEffects, ContinuationOrder};

/// Read-only endpoint metadata. An index never carries dispatch authority.
#[derive(Debug, Clone)]
pub struct ScheduledEffect<S = crate::resource::ModuleSource> {
    effect: Effect<S>,
}

impl<S> ScheduledEffect<S> {
    pub fn effect(&self) -> &Effect<S> {
        &self.effect
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
        admitted.into_scheduled(&self.continuations)
    }

    pub fn effect(&self, index: usize) -> ScheduledEffect<S> {
        ScheduledEffect {
            effect: self.admitted.effects[index].clone(),
        }
    }

    pub(crate) fn continuation(&self, id: &ResourceId) -> Option<&InFlightCreate> {
        self.continuations.get(id)
    }
}

impl<S: Clone> AdmittedReplacementEffects<S> {
    #[cfg(test)]
    pub(super) fn specialize_for_test(
        self,
        continuations: &HashMap<ResourceId, InFlightCreate>,
    ) -> ScheduledEffects<S> {
        self.into_scheduled(continuations)
    }

    fn into_scheduled(
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
/// An indexed schedule view cannot authorize a create before prerequisites:
///
/// ```compile_fail,E0599
/// use carina_core::replacement::ScheduledPlan;
/// fn create_before_delete_finishes(scheduled: &ScheduledPlan<'_>) {
///     let _ = scheduled.effect(0).into_create_permit();
/// }
/// ```
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
            effects: self.admitted_effects().into_scheduled(&continuations),
        }
    }
}

/// A schedule retaining the input obligations of its exact source plan.
/// Only scheduling an [`crate::input_rules::InputCheckedPlan`] constructs it.
#[derive(Debug, Clone)]
pub struct InputCheckedScheduledPlan<'plan, S = crate::resource::ModuleSource> {
    checked: &'plan crate::input_rules::InputCheckedPlan<S>,
    scheduled: ScheduledPlan<'plan, S>,
}

impl<'plan, S> Deref for InputCheckedScheduledPlan<'plan, S> {
    type Target = ScheduledPlan<'plan, S>;

    fn deref(&self) -> &Self::Target {
        &self.scheduled
    }
}

impl<'plan, S: Clone> InputCheckedScheduledPlan<'plan, S> {
    pub(crate) fn execution_input_checks(&self) -> Vec<crate::input_rules::PendingInputObligation> {
        self.checked.execution_input_checks()
    }

    pub(crate) fn expand_deferred_replacements(
        self,
    ) -> (
        ScheduledPlan<'plan, S>,
        std::collections::HashSet<usize>,
        Vec<usize>,
    ) {
        self.scheduled.expand_deferred_replacements()
    }
}

impl<S: Clone> crate::input_rules::InputCheckedPlan<S> {
    pub fn schedule(
        &self,
        current: &HashMap<ResourceId, Current>,
    ) -> InputCheckedScheduledPlan<'_, S> {
        InputCheckedScheduledPlan {
            checked: self,
            scheduled: self.plan().schedule(current),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::{DeletedInstanceKey, EffectGeneration};
    use crate::executor::scheduler::{
        ScheduledBasicEffect, ScheduledResume, release_basic_for_test,
    };
    use crate::provider::AppliedIdentifier;
    use crate::provider::{CreateAttempt, PendingToken, Sent};
    use crate::resource::ResolvedResource;
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

    fn plan(effects: &[Effect], schemas: &SchemaRegistry) -> crate::plan::AdmittedPlan {
        let mut plan = crate::plan::Plan::new();
        for effect in effects {
            plan.add(effect.clone());
        }
        plan.admit(schemas).unwrap()
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
        let plan = plan(&effects, &schemas());
        let scheduled = plan.schedule(&HashMap::new());
        assert!(
            release_basic_for_test(&scheduled, 0, &Default::default()).is_none(),
            "a conflicting create cannot be released before its old generations finish"
        );
        let completed = (1..=old.len()).collect();
        let Some(ScheduledBasicEffect::Create { create, .. }) =
            release_basic_for_test(&scheduled, 0, &completed)
        else {
            panic!("the create must be released after all old generations finish");
        };
        assert_eq!(create.target(), target.as_inner());
        let graph = scheduled.dependency_analysis(std::iter::empty()).unwrap();
        assert_eq!(graph.deps_of(0).unwrap(), &(1..=old.len()).collect());
        for (index, (id, generation)) in old.iter().enumerate() {
            let key = DeletedInstanceKey::new(
                id.as_inner().clone(),
                generation.clone(),
                "old-provider-id",
            );
            assert!(graph.deps_of(0).unwrap().contains(&(index + 1)));
            let Some(ScheduledBasicEffect::Delete { permit, .. }) =
                release_basic_for_test(&scheduled, index + 1, &Default::default())
            else {
                panic!("old generation must have a scheduled Delete capability");
            };
            assert_eq!(permit.instance(), &key);
            assert_eq!(
                permit.instance().id().provider_instance,
                id.provider_instance
            );
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
            let admitted = plan(&effects, &schemas());
            let scheduled = admitted.schedule(&HashMap::from([(
                target.as_inner().clone(),
                Current::CreateInFlight(pending),
            )]));
            let graph = scheduled.dependency_analysis(std::iter::empty()).unwrap();
            assert_eq!(graph.deps_of(1).unwrap().contains(&0), identified);
            assert_eq!(graph.deps_of(0).unwrap().contains(&1), !identified);
            let completed = if identified {
                Default::default()
            } else {
                std::collections::HashSet::from([1])
            };
            match release_basic_for_test(&scheduled, 0, &completed).unwrap() {
                ScheduledBasicEffect::ResumeCreate {
                    resume: ScheduledResume::Poll(_),
                    ..
                } => assert!(identified),
                ScheduledBasicEffect::ResumeCreate {
                    resume: ScheduledResume::Resend { create, .. },
                    ..
                } => {
                    assert!(!identified);
                    assert_eq!(create.target(), target.as_inner());
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
                let rescheduled = admitted.schedule(&HashMap::from([(
                    target.as_inner().clone(),
                    Current::CreateInFlight(unissued),
                )]));
                let graph = rescheduled.dependency_analysis(std::iter::empty()).unwrap();
                assert!(graph.deps_of(0).unwrap().contains(&1));
                assert!(!graph.deps_of(1).unwrap().contains(&0));
                assert!(matches!(
                    release_basic_for_test(&rescheduled, 0, &std::collections::HashSet::from([1])),
                    Some(ScheduledBasicEffect::ResumeCreate {
                        resume: ScheduledResume::Resend { .. },
                        ..
                    })
                ));
            }
        }
    }

    #[tokio::test]
    async fn initial_create_release_preserves_prepared_name_and_waits_for_deletes() {
        use crate::resource::{ConcreteValue, Resource, Value};
        use crate::schema::{PatternAttributeSchema, PatternAttributeType};
        for create_first in [false, true] {
            let string = |value: &str| Value::Concrete(ConcreteValue::String(value.into()));
            let mut resource =
                Resource::new("Record", "target").with_attribute("name", string("stable"));
            resource.directives.create_before_destroy = create_first;
            let target = ResolvedResource::collect_resolved([resource])
                .unwrap()
                .remove(0);
            let mut schemas = SchemaRegistry::new();
            let schema = PatternResourceSchema::new("Record").attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            );
            schemas.insert(
                "",
                if create_first {
                    schema.with_unique_name_attribute("name")
                } else {
                    schema.with_conflicting_replacement()
                }
                .validate()
                .unwrap(),
            );
            let mut draft = crate::plan::Plan::new();
            draft.add(Effect::Create(target.clone()));
            draft.add_delete(crate::effect::DeferredReplaceDelete {
                id: target.resolved_id(),
                identifier: AppliedIdentifier::new("old-id").unwrap(),
                generation: EffectGeneration::Current,
                directives: Default::default(),
                binding: None,
                dependencies: Default::default(),
                explicit_dependencies: Default::default(),
                blocked_by_updates: Default::default(),
                previous_attributes: HashMap::from([(
                    "name".into(),
                    crate::resource::SnapshotValue::Concrete(ConcreteValue::String(
                        "stable".into(),
                    )),
                )]),
            });
            let admitted = draft
                .admit(&schemas)
                .unwrap()
                .check_inputs(&schemas, &HashMap::new())
                .unwrap();
            let mut configuration = crate::parser::ExpandedInferredFile::default();
            configuration.resources.push(
                target
                    .as_inner()
                    .clone()
                    .with_dependency_phase(Default::default()),
            );
            let graph =
                crate::resolved_graph::resolve_expanded_graph(configuration.into()).unwrap();
            // Exercise a replacement cohort through the same scheduler release;
            // public bootstrap construction only produces a single create.
            let checked = crate::resolved_graph::InputCheckedInitialCreatePlan {
                graph: graph.into_parts().1,
                checked: admitted,
            };
            let scheduled = checked.checked.schedule(&Default::default());
            let bindings = crate::binding_index::ResolvedBindings::default();
            let gate = crate::executor::ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test_async(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                async |type_checks| {
                    let context = crate::executor::ProviderPreparationContext::new(
                        &bindings,
                        &gate,
                        &[],
                        &crate::provider::NoopNormalizer,
                        &[],
                        &schemas,
                        type_checks,
                    );
                    let permit = crate::executor::prepare_initial_create(&checked, &context)
                        .await
                        .unwrap();
                    if create_first {
                        let permit =
                            permit.expect("a checked create-first endpoint is initially ready");
                        let Effect::Create(prepared) = &scheduled.effects()[0] else {
                            unreachable!()
                        };
                        assert_ne!(prepared.get_attr("name"), target.get_attr("name"));
                        assert_eq!(
                            permit.request().resource().as_resource().get_attr("name"),
                            prepared.get_attr("name")
                        );
                    } else {
                        assert!(
                            permit.is_none(),
                            "a conflicting create must wait for Delete completion"
                        );
                    }
                },
            )
            .await;
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
        let plan = plan(&effects, &schemas());
        let scheduled = plan.schedule(&HashMap::new());
        let graph = scheduled.dependency_analysis(std::iter::empty()).unwrap();
        assert!(graph.deps_of(1).unwrap().contains(&0));
        assert!(!graph.deps_of(0).unwrap().contains(&1));
        assert!(matches!(
            release_basic_for_test(&scheduled, 0, &Default::default()),
            Some(ScheduledBasicEffect::ResumeCreate {
                resume: ScheduledResume::Missing,
                ..
            })
        ));
    }
}
