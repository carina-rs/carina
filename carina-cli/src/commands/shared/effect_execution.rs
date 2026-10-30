//! Helpers for executing import and state-only effects with user feedback.

use carina_core::effect::{Effect, resolve_import_identifier};
use carina_core::executor::{ExecutionCheckpointSink, ExecutionResult};
use carina_core::plan::Plan;
use carina_core::provider::{AppliedIdentifier, Provider, ReadRequest};
use carina_core::shutdown::{CleanupInterrupted, LoopShutdownPhase, LoopStep, ShutdownToken};
use colored::Colorize;

use crate::commands::shared::progress::{FailedOutcome, ProgressBackend, ProgressSink};

/// Owns all result mutation for one import iteration and publishes at most one
/// checkpoint when the iteration leaves scope, including through `continue`.
struct ImportResultIteration<'a, S: Clone> {
    result: &'a mut ExecutionResult<S>,
    checkpoint_sink: &'a ExecutionCheckpointSink<S>,
    dirty: bool,
}

impl<'a, S: Clone> ImportResultIteration<'a, S> {
    fn new(
        result: &'a mut ExecutionResult<S>,
        checkpoint_sink: &'a ExecutionCheckpointSink<S>,
    ) -> Self {
        Self {
            result,
            checkpoint_sink,
            dirty: false,
        }
    }

    fn mutate<T>(&mut self, mutation: impl FnOnce(&mut ExecutionResult<S>) -> T) -> T {
        self.dirty = true;
        mutation(self.result)
    }
}

impl<S: Clone> Drop for ImportResultIteration<'_, S> {
    fn drop(&mut self) {
        if self.dirty {
            // ponytail: this clones the full cumulative result once per import
            // iteration; use deltas if very large import plans make it material.
            self.checkpoint_sink.publish(self.result);
        }
    }
}

/// Execute import effects by reading the resource from the provider.
///
/// For each Import effect, calls provider.read() with the given identifier
/// to fetch the current state and stores the result in applied_states
/// so that finalize_apply can persist it.
pub(crate) async fn execute_import_effects<S: Clone>(
    plan: &Plan<S>,
    provider: &dyn Provider,
    result: &mut ExecutionResult<S>,
    shutdown: &ShutdownToken,
    checkpoint_sink: &ExecutionCheckpointSink<S>,
) {
    let progress = ProgressBackend::<()>::new(ProgressSink::Human);
    let cleanup_loop = shutdown.cleanup_aware_loop();
    for effect in plan.effects() {
        if let Effect::Import { id, identifier } = effect {
            let cleanup_wait = match cleanup_loop.step() {
                LoopStep::Continue(wait) => wait,
                LoopStep::Abandon => return,
            };
            match cleanup_wait.phase() {
                LoopShutdownPhase::Running => {}
                LoopShutdownPhase::Graceful => return,
            }
            let mut iteration = ImportResultIteration::new(result, checkpoint_sink);

            // carina#3329: the identifier is a `Value` so an
            // interpolation referencing a deferred upstream-state ref
            // can survive plan-time display as `(known after upstream
            // apply: …)`. The apply path requires a concrete string;
            // `resolve_import_identifier` is the single entry point
            // that performs this check, so any future caller is forced
            // to handle the deferred case explicitly via its `Result`
            // contract rather than rolling its own ad-hoc match.
            let identifier_str = match resolve_import_identifier(identifier) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    progress.failed_line(FailedOutcome::summary(format!(
                        "Import failed for {id}: {e}"
                    )));
                    iteration.mutate(|result| result.failure_count += 1);
                    continue;
                }
            };
            let applied_identifier = match AppliedIdentifier::new(identifier_str.clone()) {
                Ok(identifier) => identifier,
                Err(error) => {
                    progress.failed_line(FailedOutcome::summary(format!(
                        "Import failed for {id}: invalid provider identifier: {error}. Set import.id to the non-empty identifier used by the provider."
                    )));
                    iteration.mutate(|result| result.failure_count += 1);
                    continue;
                }
            };
            let identifier_display = if carina_core::value::contains_secret(identifier) {
                "(secret)"
            } else {
                identifier_str.as_str()
            };
            println!(
                "  {} Importing {} (id: {})...",
                "<-".cyan(),
                id,
                identifier_display
            );
            let read = provider.read(id, &applied_identifier, ReadRequest);
            let read_result = match cleanup_wait.until_cleanup_priority(read).await {
                CleanupInterrupted::Completed(result) => result,
                CleanupInterrupted::Abandoned => return,
            };
            match read_result {
                Ok(outcome) => {
                    let state = outcome.into_state();
                    if state.exists {
                        println!("  {} Imported {}", "✓".green(), id);
                        iteration.mutate(|result| {
                            result.applied_states.insert(
                                id.clone().into_inner(),
                                carina_core::executor::Applied::Imported(state),
                            );
                            result.success_count += 1;
                        });
                    } else {
                        progress.failed_line(FailedOutcome::summary(format!(
                            "Import failed: resource {id} with id {identifier_display} not found"
                        )));
                        iteration.mutate(|result| result.failure_count += 1);
                    }
                }
                Err(e) => {
                    progress.failed_line(FailedOutcome::summary(format!(
                        "Import failed for {id}: {e}"
                    )));
                    iteration.mutate(|result| result.failure_count += 1);
                }
            }
        }
    }
}

/// Format a single state-only effect line for `apply` output.
///
/// Extracted so the rendering can be asserted without intercepting
/// stdout. carina#3332: the `Remove` arm previously led with a red
/// lowercase `x`, which is visually indistinguishable from the `✗`
/// failure indicator used elsewhere in the apply output. The leading
/// token here is now a neutral `~` and the operation name
/// (`removed-from-state`) carries the meaning, so a state-only
/// success line can never be misread as a failure.
fn format_state_only_effect_line<S>(effect: &Effect<S>) -> Option<String> {
    match effect {
        Effect::Remove { id } => Some(format!("  {} removed-from-state {}", "~".yellow(), id)),
        Effect::Move { from, to } => Some(format!("  {} Moving {} -> {}", "->".yellow(), from, to)),
        Effect::ReleaseListKeys {
            id,
            attribute,
            keys,
            ..
        } => Some(format!(
            "  {} Released {} key{} from authored state for {}.{}",
            "!".yellow(),
            keys.len(),
            if keys.len() == 1 { "" } else { "s" },
            id,
            attribute,
        )),
        Effect::Read { .. }
        | Effect::Create(_)
        | Effect::ResumeCreate { .. }
        | Effect::Update { .. }
        | Effect::Delete { .. }
        | Effect::Import { .. }
        | Effect::Wait { .. }
        | Effect::DeferredCreate { .. }
        | Effect::DeferredReplace(_) => None,
    }
}

/// Report state-only effects (remove, move, keyed-list release) and return
/// their success count.
///
/// State writeback applies these effects to every checkpoint and to the final
/// state; this function only emits their user-facing output.
pub(crate) fn execute_state_only_effects<S>(plan: &Plan<S>) -> usize {
    let mut completed = 0;
    for effect in plan.effects() {
        if let Some(line) = format_state_only_effect_line(effect) {
            println!("{}", line);
            completed += 1;
        }
    }
    completed
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::executor::ExecutionResult;
    use carina_core::plan::Plan;
    use carina_core::provider::{
        CreateRequest, DeleteRequest, Provider, ProviderError, ProviderResult, ReadRequest,
        UpdateRequest,
    };
    use carina_core::resource::{
        ConcreteValue, DeferredValue, InterpolationPart, ResourceId, State, UnknownReason, Value,
    };
    use futures::future::BoxFuture;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    fn empty_result() -> ExecutionResult {
        ExecutionResult {
            success_count: 0,
            failure_count: 0,
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
            bindings: carina_core::binding_index::ResolvedBindings::default(),
            failed_refreshes: HashSet::new(),
        }
    }

    #[test]
    fn import_iteration_publishes_once_after_multiple_result_mutations() {
        let snapshots = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = ExecutionCheckpointSink::new({
            let snapshots = std::sync::Arc::clone(&snapshots);
            move |snapshot| snapshots.lock().unwrap().push(snapshot)
        });
        let mut result = empty_result();

        {
            let mut iteration = ImportResultIteration::new(&mut result, &sink);
            iteration.mutate(|result| result.success_count += 1);
            iteration.mutate(|result| result.failure_count += 1);
        }

        let snapshots = snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].success_count, 1);
        assert_eq!(snapshots[0].failure_count, 1);
    }

    /// Mock provider that records every `read` call so the test can
    /// assert apply *never* invokes the provider when the import
    /// identifier is still deferred.
    struct ReadRecorder {
        reads: Mutex<Vec<(ResourceId, String)>>,
    }

    impl ReadRecorder {
        fn new() -> Self {
            Self {
                reads: Mutex::new(Vec::new()),
            }
        }

        fn read_count(&self) -> usize {
            self.reads.lock().unwrap().len()
        }
    }

    impl Provider for ReadRecorder {
        fn name(&self) -> &str {
            "mock"
        }

        fn read(
            &self,
            id: &ResourceId,
            identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ReadOutcome>> {
            self.reads
                .lock()
                .unwrap()
                .push((id.clone(), identifier.as_str().to_string()));
            let id = id.clone();
            Box::pin(async move { Ok(carina_core::provider::ReadOutcome::not_found(id)) })
        }

        fn read_data_source(
            &self,
            _resource: &carina_core::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            _attempt: carina_core::provider::CreateAttempt,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
        > {
            unreachable!("import path must not call create")
        }

        fn resume_create(
            &self,
            _request: carina_core::provider::ResumeCreateRequest,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
        > {
            unreachable!("import path must not call resume_create")
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            unreachable!("import path must not call update")
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            unreachable!("import path must not call delete")
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: carina_core::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// carina#3329: an `Effect::Import` whose `identifier` is still a
    /// `Value::Deferred(Interpolation)` at apply time MUST be rejected
    /// before reaching the provider. Pre-#3329 the identifier was a
    /// plain `String`, so a partially-substituted "|literal|literal"
    /// would be passed straight to `provider.read()` with no warning.
    /// The `Value`-typed field plus `resolve_import_identifier` now
    /// gates that path.
    #[tokio::test]
    async fn execute_import_effects_rejects_deferred_identifier_without_calling_provider() {
        let id = ResourceId::with_identity("aws.route53.RecordSet", "r");
        let deferred = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                UnknownReason::UpstreamAllExports {
                    binding: "management_route53".into(),
                },
            ))),
            InterpolationPart::Literal("|registry-dev.carina-rs.dev|NS".into()),
        ]));

        let mut plan = Plan::new();
        plan.add(Effect::Import {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: deferred,
        });

        let recorder = ReadRecorder::new();
        let mut result = empty_result();
        execute_import_effects(
            &plan,
            &recorder,
            &mut result,
            &ShutdownToken::running(),
            &ExecutionCheckpointSink::noop(),
        )
        .await;

        assert_eq!(
            recorder.read_count(),
            0,
            "provider.read() must NOT be called when the import identifier \
             is still deferred — that is the regression #3329 is preventing"
        );
        assert_eq!(
            result.failure_count, 1,
            "the deferred import must count as a failure"
        );
        assert_eq!(result.success_count, 0);
    }

    /// Companion check: a concrete-string identifier still flows
    /// through to the provider unchanged. Guards against an overly-
    /// aggressive future tweak to `resolve_import_identifier` that
    /// would block the happy path.
    #[tokio::test]
    async fn execute_import_effects_passes_concrete_identifier_through() {
        let id = ResourceId::with_identity("aws.s3.Bucket", "b");
        let mut plan = Plan::new();
        plan.add(Effect::Import {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: Value::Concrete(ConcreteValue::String("my-bucket".into())),
        });

        let recorder = ReadRecorder::new();
        let mut result = empty_result();
        execute_import_effects(
            &plan,
            &recorder,
            &mut result,
            &ShutdownToken::running(),
            &ExecutionCheckpointSink::noop(),
        )
        .await;

        let reads = recorder.reads.lock().unwrap();
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].1, "my-bucket");
    }

    /// carina#3332: the state-only `Remove` line must not shape-collide
    /// with the `✗` failure indicator. The previous output led with a
    /// red lowercase `x`, which in monospace terminals reads as
    /// failure. The fix moves the meaning into a word
    /// (`removed-from-state`) and uses a non-cross leading token, so
    /// the rendered line contains no `x` / `✗` glyph at all.
    #[test]
    fn remove_effect_line_has_no_failure_shaped_glyph() {
        colored::control::set_override(true);
        let id =
            ResourceId::with_identity("aws.route53.RecordSet", "aws_route53_record_set_7059de08");
        let line =
            format_state_only_effect_line(&Effect::<carina_core::resource::ModuleSource>::Remove {
                id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            })
            .expect("Remove must render a line");
        colored::control::unset_override();

        // No literal `x` or `✗` anywhere — the leading token and the
        // word `removed-from-state` are both x-free.
        assert!(
            !line.contains('x'),
            "rendered Remove line must contain no lowercase `x` glyph \
             (it shape-collides with the failure indicator); got: {line:?}"
        );
        assert!(
            !line.contains('✗'),
            "rendered Remove line must contain no `✗`; got: {line:?}"
        );
        // The operation name and the id are both present in plain form
        // so the user can read what happened to which resource.
        assert!(
            line.contains("removed-from-state"),
            "rendered Remove line must name the operation; got: {line:?}"
        );
        assert!(
            line.contains(&id.to_string()),
            "rendered Remove line must include the resource id; got: {line:?}"
        );
    }

    /// The Move arm is out of scope for #3332 (its leading `->` does
    /// not collide with `✗`), so its rendering must stay byte-identical.
    /// This pins the format so a future sweep of state-only output
    /// does not change it by accident.
    #[test]
    fn move_effect_line_format_is_unchanged() {
        colored::control::set_override(false);
        let from = ResourceId::with_identity("aws.s3.Bucket", "old");
        let to = ResourceId::with_identity("aws.s3.Bucket", "new");
        let line =
            format_state_only_effect_line(&Effect::<carina_core::resource::ModuleSource>::Move {
                from: carina_core::resource::ResolvedResourceId::new(from.clone()),
                to: carina_core::resource::ResolvedResourceId::new(to.clone()),
            })
            .expect("Move must render a line");
        colored::control::unset_override();

        assert_eq!(line, format!("  -> Moving {} -> {}", from, to));
    }
}
