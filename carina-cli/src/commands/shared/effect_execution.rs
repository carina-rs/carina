//! Helpers for executing import and state-only effects with user feedback.

use carina_core::effect::{Effect, resolve_import_identifier};
use carina_core::executor::ExecutionResult;
use carina_core::plan::Plan;
use carina_core::provider::{Provider, ReadRequest};
use colored::Colorize;

/// Execute import effects by reading the resource from the provider.
///
/// For each Import effect, calls provider.read() with the given identifier
/// to fetch the current state and stores the result in applied_states
/// so that finalize_apply can persist it.
pub(crate) async fn execute_import_effects(
    plan: &Plan,
    provider: &dyn Provider,
    result: &mut ExecutionResult,
) {
    for effect in plan.effects() {
        if let Effect::Import { id, identifier } = effect {
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
                    println!("  {} Import failed for {}: {}", "✗".red(), id, e);
                    result.failure_count += 1;
                    continue;
                }
            };
            println!(
                "  {} Importing {} (id: {})...",
                "<-".cyan(),
                id,
                identifier_str
            );
            match provider
                .read(id, Some(identifier_str.as_str()), ReadRequest)
                .await
            {
                Ok(state) => {
                    if state.exists {
                        println!("  {} Imported {}", "✓".green(), id);
                        result.applied_states.insert(id.clone(), state);
                        result.success_count += 1;
                    } else {
                        println!(
                            "  {} Import failed: resource {} with id {} not found",
                            "✗".red(),
                            id,
                            identifier_str
                        );
                        result.failure_count += 1;
                    }
                }
                Err(e) => {
                    println!("  {} Import failed for {}: {}", "✗".red(), id, e);
                    result.failure_count += 1;
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
fn format_state_only_effect_line(effect: &Effect) -> Option<String> {
    match effect {
        Effect::Remove { id } => Some(format!("  {} removed-from-state {}", "~".yellow(), id)),
        Effect::Move { from, to } => Some(format!("  {} Moving {} -> {}", "->".yellow(), from, to)),
        _ => None,
    }
}

/// Execute state-only effects (remove, move) with user feedback.
///
/// These effects only modify state and don't call the provider.
pub(crate) fn execute_state_only_effects(plan: &Plan, result: &mut ExecutionResult) {
    for effect in plan.effects() {
        if let Some(line) = format_state_only_effect_line(effect) {
            println!("{}", line);
            result.success_count += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::executor::ExecutionResult;
    use carina_core::plan::Plan;
    use carina_core::provider::{
        CreateRequest, DeleteRequest, Provider, ProviderResult, ReadRequest, UpdateRequest,
    };
    use carina_core::resource::{
        ConcreteValue, DataSource, DeferredValue, InterpolationPart, ResourceId, State,
        UnknownReason, Value,
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
            applied_states: HashMap::new(),
            runtime_synthesized_resources: Vec::new(),
            successfully_deleted: HashSet::new(),
            permanent_name_overrides: HashMap::new(),
            current_states: HashMap::new(),
            bindings: carina_core::binding_index::ResolvedBindings::default(),
            failed_refreshes: HashSet::new(),
        }
    }

    /// Mock provider that records every `read` call so the test can
    /// assert apply *never* invokes the provider when the import
    /// identifier is still deferred.
    struct ReadRecorder {
        reads: Mutex<Vec<(ResourceId, Option<String>)>>,
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
            identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.reads
                .lock()
                .unwrap()
                .push((id.clone(), identifier.map(str::to_string)));
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            self.read(&resource.id, None, ReadRequest)
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::CreateOutcome>> {
            unreachable!("import path must not call create")
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            unreachable!("import path must not call update")
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            unreachable!("import path must not call delete")
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: carina_core::effect::PlanOp,
        ) -> Vec<String> {
            Vec::new()
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
        let id = ResourceId::new("aws.route53.RecordSet", "r");
        let deferred = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                UnknownReason::UpstreamBareRef {
                    binding: "management_route53".into(),
                },
            ))),
            InterpolationPart::Literal("|registry-dev.carina-rs.dev|NS".into()),
        ]));

        let mut plan = Plan::new();
        plan.add(Effect::Import {
            id: id.clone(),
            identifier: deferred,
        });

        let recorder = ReadRecorder::new();
        let mut result = empty_result();
        execute_import_effects(&plan, &recorder, &mut result).await;

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
        let id = ResourceId::new("aws.s3.Bucket", "b");
        let mut plan = Plan::new();
        plan.add(Effect::Import {
            id: id.clone(),
            identifier: Value::Concrete(ConcreteValue::String("my-bucket".into())),
        });

        let recorder = ReadRecorder::new();
        let mut result = empty_result();
        execute_import_effects(&plan, &recorder, &mut result).await;

        let reads = recorder.reads.lock().unwrap();
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].1.as_deref(), Some("my-bucket"));
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
        let id = ResourceId::new("aws.route53.RecordSet", "aws_route53_record_set_7059de08");
        let line = format_state_only_effect_line(&Effect::Remove { id: id.clone() })
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
        let from = ResourceId::new("aws.s3.Bucket", "old");
        let to = ResourceId::new("aws.s3.Bucket", "new");
        let line = format_state_only_effect_line(&Effect::Move {
            from: from.clone(),
            to: to.clone(),
        })
        .expect("Move must render a line");
        colored::control::unset_override();

        assert_eq!(line, format!("  -> Moving {} -> {}", from, to));
    }
}
