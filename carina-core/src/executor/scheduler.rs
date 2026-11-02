use std::collections::{HashMap, HashSet};

use crate::binding_index::ResolvedBindings;
use crate::effect::{Effect, SchedulerKey};
use crate::parser::CompiledDeferredForExpression as DeferredForExpression;
use crate::resource::ResolvedResourceId;

use super::UnresolvedResource;
use super::deferred_dispatch::{DeferredDispatchResult, PureMetaCtx, dispatch_deferred_create};
use super::parallel::apply_deferred_replace_delete_deps;
use crate::effect::deps::{
    DependencyAnalysis, DependencyAnalysisFailure, ScheduleInputs, UnresolvedRequiredDependency,
    build_effect_dependency_analysis, relax_update_update_edges,
};

pub(super) struct SchedulerDeps {
    pub(super) deps_of: HashMap<usize, HashSet<usize>>,
    pub(super) unresolved: Vec<UnresolvedRequiredDependency>,
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
                dependencies: &payload.dependency_template,
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
    Materialized(Vec<Effect<S>>),
    Failed(super::DeferredCreateFailure),
}

pub(super) fn try_dispatch_pure_meta<S: Clone + crate::resource::AnonymousIdentityModuleSource>(
    effect: &Effect<S>,
    bindings: &ResolvedBindings,
    ctx: &PureMetaCtx<'_>,
) -> PureMetaOutcome<S> {
    let Some(step) = PureMetaStep::from_effect(effect) else {
        return PureMetaOutcome::NotPureMeta;
    };

    match dispatch_deferred_create(step.effect, step.template, step.dependencies, bindings, ctx) {
        DeferredDispatchResult::Materialized(children) => PureMetaOutcome::Materialized(children),
        DeferredDispatchResult::MaterializeFailed(failure) => PureMetaOutcome::Failed(failure),
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

pub(super) fn build_scheduler_deps<S>(
    effects: &[Effect<S>],
    unresolved_resources: &HashMap<ResolvedResourceId, UnresolvedResource<S>>,
    deferred_replace_delete_deps: &[(usize, usize)],
) -> SchedulerDeps {
    finish_scheduler_deps(
        effects,
        build_effect_dependency_analysis(
            effects,
            unresolved_resources.keys(),
            ScheduleInputs::Apply,
        ),
        deferred_replace_delete_deps,
    )
}

pub(super) fn build_scheduler_deps_from_resources(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResolvedResourceId, &UnresolvedResource>,
    deferred_replace_delete_deps: &[(usize, usize)],
) -> SchedulerDeps {
    let effect_refs: Vec<_> = effects.iter().collect();
    finish_scheduler_deps(
        effects,
        build_effect_dependency_analysis(
            &effect_refs,
            unresolved_resources.keys(),
            ScheduleInputs::Apply,
        ),
        deferred_replace_delete_deps,
    )
}

fn finish_scheduler_deps<S>(
    effects: &[Effect<S>],
    analysis: Result<DependencyAnalysis, DependencyAnalysisFailure>,
    deferred_replace_delete_deps: &[(usize, usize)],
) -> SchedulerDeps {
    let (mut analysis, unresolved) = match analysis {
        Ok(analysis) => (analysis, Vec::new()),
        Err(failure) => failure.into_parts(),
    };
    relax_update_update_edges(effects, &mut analysis);
    let mut deps_of = analysis.into_deps_of();
    apply_deferred_replace_delete_deps(effects, &mut deps_of, deferred_replace_delete_deps);
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
        use crate::resource::{
            ConcreteValue, ResolvedResource, ResolvedResourceId, RootScope, Value,
        };
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
                    Value::Concrete(ConcreteValue::String("ready".into())),
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
            &crate::parser::ProviderContext::default(),
        )
        .expect("unbound deferred loop parses");
        let parsed = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::ProviderContext::default(),
            |session| {
                crate::module_resolver::resolve_modules_providerless(
                    crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
                    std::path::Path::new("."),
                    session,
                )
                .expect("deferred fixture crosses the module-resolution seam")
            },
        )
        .into_expanded(&crate::schema::SchemaRegistry::new())
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
            &crate::parser::ProviderContext::default(),
        )
        .expect("unbound deferred loop parses");
        let parsed = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::ProviderContext::default(),
            |session| {
                crate::module_resolver::resolve_modules_providerless(
                    crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
                    std::path::Path::new("."),
                    session,
                )
                .map_err(Box::new)
            },
        )
        .map_err(|error| *error)
        .expect("module resolution")
        .into_expanded(&crate::schema::SchemaRegistry::new())
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
