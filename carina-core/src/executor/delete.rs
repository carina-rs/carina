//! Delete-only executions share apply's dependency-ready release and permits.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::effect::Effect;
use crate::effect::deps::{
    DependencyAnalysis, DependencyAnalysisFailure, DestroyWaitAlias, KeptDestroyTargets,
    ScheduleInputs, build_effect_dependency_analysis,
};
use crate::plan::AdmittedPlan;
use crate::provider::{DeleteAttempt, Provider, ProviderResult};
use crate::replacement::ScheduledPlan;
use crate::resource::ModuleSource;
use crate::schema::SchemaRegistry;

use super::DeletePermit;
use super::input_discharge::ExecutionInputChecks;
use super::scheduler::{ScheduledBasicEffect, build_scheduler_deps, release_ready_effect};

/// Dispatch consumes the permit; raw ids, identifiers, and requests cannot
/// authorize a provider delete.
///
/// ```compile_fail,E0061
/// use carina_core::{executor::dispatch_delete, provider::{Provider, AppliedIdentifier, DeleteRequest}, resource::ResourceId};
/// fn bypass(provider: &dyn Provider, id: &ResourceId, identifier: &AppliedIdentifier, request: DeleteRequest) {
///     let _ = dispatch_delete(provider, id, identifier, request);
/// }
/// ```
pub async fn dispatch_delete<P: Provider + ?Sized>(
    provider: &P,
    permit: DeletePermit,
    schemas: &SchemaRegistry,
) -> ProviderResult<()> {
    provider.delete(DeleteAttempt::new(permit, schemas)).await
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteScheduleError {
    #[error("delete-only schedule contains a non-delete effect at index {index}")]
    NonDelete { index: usize },
    #[error(transparent)]
    Dependencies(#[from] DependencyAnalysisFailure),
}

/// Stateful readiness for a core-built destroy plan. Completion and retry
/// require a receipt returned by a finished provider call; caller-chosen index
/// sets cannot release dependencies.
pub struct DeleteScheduler<'plan, S = ModuleSource> {
    scheduled: ScheduledPlan<'plan, S>,
    analysis: DependencyAnalysis,
    dependencies: HashMap<usize, HashSet<usize>>,
    checks: ExecutionInputChecks,
    completed: HashSet<usize>,
    failed: HashSet<usize>,
    released: HashSet<usize>,
    run: Arc<()>,
}

/// One scheduler decision. A blocked slot is completed as failed by the
/// scheduler, allowing the caller to preserve its existing skip diagnostics.
pub enum DeleteRelease {
    Ready(ScheduledDelete),
    Blocked { dependency: usize },
}

/// One released delete. Only dispatch can yield its completion receipt.
pub struct ScheduledDelete {
    permit: DeletePermit,
    completion: DeleteCompletion,
}

/// Unforgeable completion of one provider attempt in one delete execution.
///
/// ```compile_fail,E0451
/// use carina_core::executor::DeleteCompletion;
/// let receipt = DeleteCompletion { run: todo!(), index: 0, failed: false };
/// ```
pub struct DeleteCompletion {
    run: Arc<()>,
    index: usize,
    failed: bool,
}

impl ScheduledDelete {
    pub async fn dispatch<P: Provider + ?Sized>(
        self,
        provider: &P,
        schemas: &SchemaRegistry,
    ) -> (ProviderResult<()>, DeleteCompletion) {
        let result = dispatch_delete(provider, self.permit, schemas).await;
        let mut completion = self.completion;
        completion.failed = result.is_err();
        (result, completion)
    }
}

impl<'plan, S: Clone> DeleteScheduler<'plan, S> {
    pub fn for_destroy(
        plan: &'plan crate::planning_state::DestroyPlan<S>,
    ) -> Result<Self, DeleteScheduleError> {
        Self::from_admitted(&plan.admitted, &plan.aliases, &plan.kept_targets)
    }

    /// Fixture-only scheduling of hand-built effects.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn for_test(
        plan: &'plan AdmittedPlan<S>,
        aliases: &[DestroyWaitAlias],
        kept_targets: &KeptDestroyTargets,
    ) -> Result<Self, DeleteScheduleError> {
        Self::from_admitted(plan, aliases, kept_targets)
    }

    pub(crate) fn from_admitted(
        plan: &'plan AdmittedPlan<S>,
        aliases: &[DestroyWaitAlias],
        kept_targets: &KeptDestroyTargets,
    ) -> Result<Self, DeleteScheduleError> {
        let scheduled = plan.schedule(&HashMap::new());
        if let Some(index) = scheduled
            .effects()
            .iter()
            .position(|effect| !matches!(effect, Effect::Delete { .. }))
        {
            return Err(DeleteScheduleError::NonDelete { index });
        }
        let analysis = build_effect_dependency_analysis(
            scheduled.effects(),
            std::iter::empty(),
            ScheduleInputs::Destroy {
                aliases,
                kept_targets,
            },
        )?;
        // Wait aliases are graph-only nodes. Project their paths onto the
        // delete slots once; this exact map controls display and release.
        let dependencies = delete_dependencies(&analysis, scheduled.effects().len());
        let sources = (0..scheduled.effects().len()).collect::<Vec<_>>();
        let checks = ExecutionInputChecks::new(Vec::new(), scheduled.effects(), &sources)
            .expect("delete-only schedule has no write input obligations");
        Ok(Self {
            scheduled,
            analysis,
            dependencies,
            checks,
            completed: HashSet::new(),
            failed: HashSet::new(),
            released: HashSet::new(),
            run: Arc::new(()),
        })
    }

    pub fn analysis(&self) -> &DependencyAnalysis {
        &self.analysis
    }

    pub fn dependencies(&self) -> &HashMap<usize, HashSet<usize>> {
        &self.dependencies
    }

    pub fn release(&mut self, index: usize) -> Option<DeleteRelease> {
        if self.completed.contains(&index) || self.released.contains(&index) {
            return None;
        }
        let ready = release_ready_effect(
            &self.scheduled,
            index,
            &self.dependencies,
            &self.completed,
            &self.failed,
        )?;
        let ready = match ready {
            Ok(ready) => ready,
            Err(_) => {
                // Keep destroy's existing diagnostic: name the lowest-index
                // failed descendant, including one behind an already skipped
                // delete. Readiness still uses only the scheduler's edges.
                let mut ancestors = HashSet::new();
                let mut pending = vec![index];
                while let Some(node) = pending.pop() {
                    for &dependency in &self.dependencies[&node] {
                        if ancestors.insert(dependency) {
                            pending.push(dependency);
                        }
                    }
                }
                let dependency = *ancestors
                    .intersection(&self.failed)
                    .min()
                    .expect("failed delete dependency");
                self.completed.insert(index);
                self.failed.insert(index);
                return Some(DeleteRelease::Blocked { dependency });
            }
        };
        let Some(ScheduledBasicEffect::Delete { permit: ready, .. }) = ready.into_basic() else {
            unreachable!("delete-only schedule");
        };
        let permit = self
            .checks
            .authorize_delete(index, &ready)
            .expect("ready delete-only slot has no paired writes");
        self.released.insert(index);
        Some(DeleteRelease::Ready(ScheduledDelete {
            permit,
            completion: DeleteCompletion {
                run: self.run.clone(),
                index,
                failed: false,
            },
        }))
    }

    pub fn complete(&mut self, completion: DeleteCompletion) {
        self.consume_completion(&completion);
        self.completed.insert(completion.index);
        if completion.failed {
            self.failed.insert(completion.index);
        }
    }

    /// Retry only an actual failed provider attempt, retaining its exact slot.
    pub fn retry(&mut self, completion: DeleteCompletion) {
        assert!(completion.failed, "only failed deletes can be retried");
        self.consume_completion(&completion);
        self.checks.retry_delete(
            completion.index,
            &self.scheduled.effects()[completion.index],
        );
    }

    fn consume_completion(&mut self, completion: &DeleteCompletion) {
        assert!(
            Arc::ptr_eq(&self.run, &completion.run),
            "delete completion belongs to another execution"
        );
        assert!(
            self.released.remove(&completion.index),
            "delete completion already consumed"
        );
    }
}

fn delete_dependencies(
    analysis: &DependencyAnalysis,
    count: usize,
) -> HashMap<usize, HashSet<usize>> {
    (0..count)
        .map(|index| {
            let mut dependencies = HashSet::new();
            let mut seen = HashSet::new();
            let mut pending = vec![index];
            while let Some(node) = pending.pop() {
                for &dependency in analysis.deps_of(node).into_iter().flatten() {
                    if !seen.insert(dependency) {
                        continue;
                    }
                    if dependency < count {
                        dependencies.insert(dependency);
                    } else {
                        pending.push(dependency);
                    }
                }
            }
            (index, dependencies)
        })
        .collect()
}

/// Release a single-resource delete against the same graph that resolved and
/// admitted it. Deletes have no desired values, so no input rules are evaluated.
///
/// ```compile_fail,E0308
/// use carina_core::{executor::prepare_initial_delete, plan::AdmittedPlan};
/// fn bypass(plan: &AdmittedPlan) { let _ = prepare_initial_delete(plan); }
/// ```
///
/// ```compile_fail,E0061
/// use carina_core::{executor::prepare_initial_delete, resolved_graph::{AdmittedInitialDeletePlan, ExpandedBindingIndex}};
/// fn swap_graph(plan: &AdmittedInitialDeletePlan, graph: &ExpandedBindingIndex) {
///     let _ = prepare_initial_delete(plan, graph);
/// }
/// ```
pub fn prepare_initial_delete(
    plan: &crate::resolved_graph::AdmittedInitialDeletePlan,
) -> Option<DeletePermit> {
    let scheduled = plan.admitted.schedule(&HashMap::new());
    let dependencies = build_scheduler_deps(&scheduled, plan.graph.scheduler_target_ids());
    if !dependencies.unresolved.is_empty() {
        return None;
    }
    let ready = release_ready_effect(
        &scheduled,
        0,
        &dependencies.deps_of,
        &HashSet::new(),
        &HashSet::new(),
    )?
    .ok()?
    .into_basic()?;
    let ScheduledBasicEffect::Delete { permit: ready, .. } = ready else {
        return None;
    };
    let mut checks = ExecutionInputChecks::new(Vec::new(), scheduled.effects(), &[0]).ok()?;
    checks.authorize_delete(0, &ready).ok()
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn delete_permit_for_test(
    id: crate::resource::ResourceId,
    identifier: crate::provider::AppliedIdentifier,
    directives: crate::resource::Directives,
) -> DeletePermit {
    let mut plan = crate::plan::Plan::new();
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(id),
        identifier,
        generation: crate::effect::EffectGeneration::Current,
        directives,
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    });
    let plan = plan
        .admit(&SchemaRegistry::new())
        .expect("fixture delete admission");
    let mut scheduler = DeleteScheduler::from_admitted(&plan, &[], &Default::default())
        .expect("fixture delete schedule");
    let Some(DeleteRelease::Ready(delete)) = scheduler.release(0) else {
        unreachable!("fixture ready delete")
    };
    delete.permit
}
