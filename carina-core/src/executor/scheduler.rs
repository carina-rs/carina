use std::collections::{HashMap, HashSet};

use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::DeferredForExpression;
use crate::resource::ResourceId;

use super::UnresolvedResource;
use super::deferred_dispatch::{DeferredDispatchResult, PureMetaCtx, dispatch_deferred_create};
use super::parallel::apply_deferred_replace_delete_deps;
use super::wait::SKIP_REASON_CANCELLED;
use super::{ExecutionEvent, ExecutionObserver, ProgressInfo};
use crate::effect::deps::{
    ScheduleInputs, build_effect_dependency_analysis, relax_update_update_edges,
};

pub(super) struct PureMetaStep<'a> {
    effect: &'a Effect,
    upstream_binding: &'a str,
    template: &'a DeferredForExpression,
}

impl<'a> PureMetaStep<'a> {
    fn from_effect(effect: &'a Effect) -> Option<Self> {
        match effect {
            Effect::DeferredCreate {
                upstream_binding,
                template,
                ..
            }
            | Effect::DeferredReplace {
                upstream_binding,
                template,
                ..
            } => Some(Self {
                effect,
                upstream_binding,
                template,
            }),
            Effect::Create(_)
            | Effect::Update { .. }
            | Effect::Replace { .. }
            | Effect::Delete { .. }
            | Effect::Wait { .. }
            | Effect::Read { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. } => None,
        }
    }
}

pub(super) enum PureMetaOutcome {
    NotPureMeta,
    Materialized(Vec<Effect>),
    Failed,
}

pub(super) fn try_dispatch_pure_meta(
    effect: &Effect,
    bindings: &ResolvedBindings,
    ctx: &PureMetaCtx<'_>,
) -> PureMetaOutcome {
    let Some(step) = PureMetaStep::from_effect(effect) else {
        return PureMetaOutcome::NotPureMeta;
    };

    match dispatch_deferred_create(
        step.effect,
        step.upstream_binding,
        step.template,
        bindings,
        ctx,
    ) {
        DeferredDispatchResult::Materialized(children) => PureMetaOutcome::Materialized(children),
        DeferredDispatchResult::MaterializeFailed => PureMetaOutcome::Failed,
    }
}

pub(super) fn failure_binding_name(effect: &Effect) -> Option<String> {
    match effect {
        Effect::DeferredCreate { template, .. } | Effect::DeferredReplace { template, .. } => {
            Some(template.binding_name.clone())
        }
        _ => effect.binding_name(),
    }
}

pub(super) fn build_scheduler_deps(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    compositions: &[crate::resource::Composition],
    deferred_replace_delete_deps: &[(usize, usize)],
) -> HashMap<usize, HashSet<usize>> {
    let mut analysis = build_effect_dependency_analysis(
        effects,
        unresolved_resources,
        compositions,
        ScheduleInputs::Apply,
    );
    relax_update_update_edges(effects, &mut analysis);
    let mut deps_of = analysis.into_deps_of();
    apply_deferred_replace_delete_deps(&mut deps_of, deferred_replace_delete_deps);
    deps_of
}

pub(super) fn build_phase_scheduler_deps(
    effects: &[Effect],
    phase_indices: &[usize],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    compositions: &[crate::resource::Composition],
    deferred_replace_delete_deps: &[(usize, usize)],
) -> HashMap<usize, HashSet<usize>> {
    let mut deps_of = super::phased::build_phase_dependency_map(
        effects,
        phase_indices,
        unresolved_resources,
        compositions,
    );
    apply_deferred_replace_delete_deps(&mut deps_of, deferred_replace_delete_deps);
    deps_of
}

/// Build the dependency map for a "post-replace wait" phase. Combines
/// `build_phase_dependency_map`'s binding-based edges with cross-phase
/// target-id edges: each wait's target effect is looked up across the full
/// effect list, so anonymous replaces still gate their waits.
pub(super) fn build_post_replace_wait_scheduler_deps(
    effects: &[Effect],
    post_replace_wait_indices: &[usize],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    compositions: &[crate::resource::Composition],
) -> HashMap<usize, HashSet<usize>> {
    let mut deps_of = super::phased::build_phase_dependency_map(
        effects,
        post_replace_wait_indices,
        unresolved_resources,
        compositions,
    );
    for &idx in post_replace_wait_indices {
        if let Effect::Wait { target_id, .. } = &effects[idx] {
            let target_deps = effects.iter().enumerate().filter_map(|(dep_idx, effect)| {
                (dep_idx != idx && effect.resource_id() == target_id).then_some(dep_idx)
            });
            deps_of.entry(idx).or_default().extend(target_deps);
        }
    }
    deps_of
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
pub(super) struct FailureView<'a> {
    pub(super) effects: &'a [Effect],
    pub(super) deps_of: &'a HashMap<usize, HashSet<usize>>,
    pub(super) failed_indices: &'a HashSet<usize>,
    failed_binding_names: HashSet<String>,
}

impl<'a> FailureView<'a> {
    pub(super) fn new(
        effects: &'a [Effect],
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

pub(super) fn emit_cancelled_skips_with_progress(
    effects: &[Effect],
    indices: &[usize],
    dispatched: &mut HashSet<usize>,
    completed_indices: &mut HashSet<usize>,
    skip_count: &mut usize,
    observer: &dyn ExecutionObserver,
    progress_for: &mut dyn FnMut(usize) -> ProgressInfo,
) {
    for &idx in indices {
        if dispatched.contains(&idx) {
            continue;
        }
        dispatched.insert(idx);
        completed_indices.insert(idx);
        observer.on_event(&ExecutionEvent::EffectSkipped {
            effect: &effects[idx],
            reason: SKIP_REASON_CANCELLED,
            progress: progress_for(idx),
        });
        *skip_count += 1;
    }
}
