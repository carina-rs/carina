use std::collections::{HashMap, HashSet};

use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::DeferredForExpression;
use crate::resource::ResourceId;

use super::UnresolvedResource;
use super::deferred_dispatch::{DeferredDispatchResult, PureMetaCtx, dispatch_deferred_create};
use super::parallel::apply_deferred_replace_delete_deps;
use crate::effect::deps::{
    DependencyAnalysis, DependencyAnalysisFailure, ScheduleInputs, UnresolvedRequiredDependency,
    build_effect_dependency_analysis, build_effect_dependency_analysis_from_resources,
    relax_update_update_edges,
};

pub(super) struct SchedulerDeps {
    pub(super) deps_of: HashMap<usize, HashSet<usize>>,
    pub(super) unresolved: Vec<UnresolvedRequiredDependency>,
}

pub(super) struct PureMetaStep<'a, S> {
    effect: &'a Effect<S>,
    template: &'a DeferredForExpression<S>,
}

impl<'a, S> PureMetaStep<'a, S> {
    fn from_effect(effect: &'a Effect<S>) -> Option<Self> {
        match effect {
            Effect::DeferredCreate { template, .. } => Some(Self { effect, template }),
            Effect::DeferredReplace(payload) => Some(Self {
                effect,
                template: payload.template.as_ref(),
            }),
            Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Wait { .. }
            | Effect::Read { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. } => None,
        }
    }
}

pub(super) enum PureMetaOutcome<S> {
    NotPureMeta,
    Materialized(Vec<Effect<S>>),
    Failed,
}

pub(super) fn try_dispatch_pure_meta<S: Clone + crate::resource::AnonymousIdentityModuleSource>(
    effect: &Effect<S>,
    bindings: &ResolvedBindings,
    ctx: &PureMetaCtx<'_, S>,
) -> PureMetaOutcome<S> {
    let Some(step) = PureMetaStep::from_effect(effect) else {
        return PureMetaOutcome::NotPureMeta;
    };

    match dispatch_deferred_create(step.effect, step.template, bindings, ctx) {
        DeferredDispatchResult::Materialized(children) => PureMetaOutcome::Materialized(children),
        DeferredDispatchResult::MaterializeFailed => PureMetaOutcome::Failed,
    }
}

pub(super) fn failure_binding_name<S>(effect: &Effect<S>) -> Option<String> {
    match effect {
        Effect::DeferredCreate { template, .. } => {
            Some(template.target.scheduler_key().to_string())
        }
        Effect::DeferredReplace(payload) => {
            Some(payload.template.target.scheduler_key().to_string())
        }
        _ => effect.binding_name(),
    }
}

pub(super) fn build_scheduler_deps<S>(
    effects: &[Effect<S>],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource<S>>,
    compositions: &[crate::resource::LiveComposition],
    deferred_replace_delete_deps: &[(usize, usize)],
) -> SchedulerDeps {
    finish_scheduler_deps(
        effects,
        build_effect_dependency_analysis(
            effects,
            unresolved_resources,
            compositions,
            ScheduleInputs::Apply,
        ),
        deferred_replace_delete_deps,
    )
}

pub(super) fn build_saved_scheduler_deps<S>(
    effects: &[Effect<S>],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource<S>>,
    compositions: &[crate::resource::SavedComposition],
    deferred_replace_delete_deps: &[(usize, usize)],
) -> SchedulerDeps {
    finish_scheduler_deps(
        effects,
        crate::effect::deps::build_saved_effect_dependency_analysis(
            effects,
            unresolved_resources,
            compositions,
            ScheduleInputs::Apply,
        ),
        deferred_replace_delete_deps,
    )
}

pub(super) fn build_scheduler_deps_from_resources(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, &UnresolvedResource>,
    compositions: &[crate::resource::LiveComposition],
    deferred_replace_delete_deps: &[(usize, usize)],
) -> SchedulerDeps {
    let effect_refs: Vec<_> = effects.iter().collect();
    finish_scheduler_deps(
        effects,
        build_effect_dependency_analysis_from_resources(
            &effect_refs,
            unresolved_resources,
            compositions,
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
/// (phased / parallel) and both `Named` / `Anonymous` cases in one
/// place — adding a new variant to [`FailedDependency`] is a compile
/// error here, not a silent message-format drift across call sites.
pub(super) fn dependency_failed_reason(failed: &FailedDependency) -> String {
    match failed {
        FailedDependency::Named(binding) => format!("dependency '{binding}' failed"),
        FailedDependency::Anonymous => "dependency failed".to_string(),
    }
}

/// Format the user-facing skip reason for a Wait effect whose
/// dependency failed (renders as `"unsatisfiable: ..."`). Anonymous
/// dependencies render as a plain `dependency-failed` token rather
/// than fabricating a binding-shaped placeholder.
pub(super) fn wait_dependency_failed_reason(failed: &FailedDependency) -> String {
    match failed {
        FailedDependency::Named(binding) => {
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
/// separately — they are not interchangeable strings. `Named` carries a
/// real binding name (suitable for `"dependency '<name>' failed"` style
/// messages); `Anonymous` means the failed dependency has no binding
/// identity (e.g. an anonymous `Replace` whose `to.binding` is `None`),
/// and the message must NOT fabricate a binding-shaped placeholder for
/// it. Returning a single `String` here would force every consumer to
/// re-derive that distinction with a `starts_with('#')` check; the
/// enum makes the broken state unrepresentable.
#[derive(Debug, Clone)]
pub(super) enum FailedDependency {
    Named(String),
    Anonymous,
}

/// A snapshot of cumulative cross-phase failure state, paired with the
/// dependency graph and effect list. Dispatch-time failure detection and
/// Wait terminal checks share this view so their visibility cannot drift.
pub(super) struct FailureView<'a, S> {
    pub(super) effects: &'a [Effect<S>],
    pub(super) deps_of: &'a HashMap<usize, HashSet<usize>>,
    pub(super) failed_indices: &'a HashSet<usize>,
    failed_binding_names: HashSet<String>,
}

impl<'a, S> FailureView<'a, S> {
    pub(super) fn new(
        effects: &'a [Effect<S>],
        deps_of: &'a HashMap<usize, HashSet<usize>>,
        failed_indices: &'a HashSet<usize>,
    ) -> Self {
        let failed_binding_names = failed_indices
            .iter()
            .filter_map(|idx| effects.get(*idx).and_then(failure_binding_name))
            .collect();
        Self {
            effects,
            deps_of,
            failed_indices,
            failed_binding_names,
        }
    }

    /// Find the failed dependency that would cause `effects[idx]` to be
    /// skipped before dispatch. Checks both in-phase graph edges and the
    /// cross-phase binding fallback used for phase-scoped dependency maps.
    pub(super) fn find_failed_dependency(&self, idx: usize) -> Option<FailedDependency> {
        if let Some(deps) = self.deps_of.get(&idx)
            && let Some(&dep_idx) = deps.iter().find(|d| self.failed_indices.contains(d))
        {
            let name = self.effects.get(dep_idx).and_then(failure_binding_name);
            return Some(match name {
                Some(name) => FailedDependency::Named(name),
                None => FailedDependency::Anonymous,
            });
        }

        self.effects[idx]
            .blocking_bindings()
            .into_iter()
            .find(|binding| self.failed_binding_names.contains(binding))
            .map(FailedDependency::Named)
    }

    pub(super) fn is_effectively_pre_skipped(&self, idx: usize) -> bool {
        self.find_failed_dependency(idx).is_some()
    }
}
