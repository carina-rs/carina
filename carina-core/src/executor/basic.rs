//! Single-effect execution: Create, Update, Delete dispatch, resource resolution,
//! Secret unwrapping, and post-apply binding updates.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::provider::{
    CreateRequest, DeleteRequest, Provider, ProviderNormalizer, ReadRequest, UpdateRequest,
    build_update_patch,
};
use crate::resolver::resolve_ref_value;
use crate::resource::{ConcreteValue, DeferredValue, Resource, ResourceId, State, Value};

use super::{ExecutionEvent, ExecutionObserver, ProgressInfo};

/// Result of executing a basic effect (Create, Update, or Delete).
///
/// This is the shared result type used by `execute_basic_effect` to avoid
/// duplicating effect dispatch logic across sequential and phased paths.
///
/// `Success` carries `Option<State>` (typically the dominant variant); the
/// size disparity with `Failure` / `Deleted` is intentional because
/// success is the common path and boxing it would add an allocation per
/// effect on the hot path.
#[allow(clippy::large_enum_variant)]
pub(super) enum BasicEffectResult {
    Success {
        state: Option<State>,
        resource_id: ResourceId,
        resolved_attrs: Option<HashMap<String, Value>>,
        binding: Option<String>,
    },
    Failure {
        binding: Option<String>,
        refresh: Option<(ResourceId, String)>,
    },
    Deleted {
        resource_id: ResourceId,
    },
}

/// Mutable execution state shared across effect processing.
///
/// Groups the counters, result maps, and binding state that are threaded
/// through every call to `process_basic_result`, reducing parameter count.
pub(super) struct ExecutionState<'a> {
    pub(super) success_count: &'a mut usize,
    pub(super) failure_count: &'a mut usize,
    pub(super) applied_states: &'a mut HashMap<ResourceId, State>,
    pub(super) failed_bindings: &'a mut std::collections::HashSet<String>,
    pub(super) successfully_deleted: &'a mut std::collections::HashSet<ResourceId>,
    pub(super) pending_refreshes: &'a mut HashMap<ResourceId, String>,
    pub(super) bindings: &'a mut ResolvedBindings,
}

/// Queue a state refresh for a resource after a failed operation.
pub(super) fn queue_state_refresh(
    pending_refreshes: &mut HashMap<ResourceId, String>,
    id: &ResourceId,
    identifier: Option<&str>,
) {
    if let Some(identifier) = identifier.filter(|identifier| !identifier.is_empty()) {
        pending_refreshes.insert(id.clone(), identifier.to_string());
    }
}

/// Refresh states for resources whose operations failed.
pub(super) async fn refresh_pending_states(
    provider: &dyn Provider,
    current_states: &mut HashMap<ResourceId, State>,
    pending_refreshes: &HashMap<ResourceId, String>,
    observer: &dyn ExecutionObserver,
) -> std::collections::HashSet<ResourceId> {
    if pending_refreshes.is_empty() {
        return std::collections::HashSet::new();
    }

    observer.on_event(&ExecutionEvent::RefreshStarted);

    let mut refreshes: Vec<_> = pending_refreshes.iter().collect();
    refreshes.sort_by_key(|(left_id, _)| left_id.to_string());
    let mut failed_refreshes = std::collections::HashSet::new();

    for (id, identifier) in refreshes {
        match provider
            .read(id, Some(identifier.as_str()), ReadRequest)
            .await
        {
            Ok(state) => {
                observer.on_event(&ExecutionEvent::RefreshSucceeded { id });
                current_states.insert(id.clone(), state);
            }
            Err(error) => {
                let error_str = error.to_string();
                observer.on_event(&ExecutionEvent::RefreshFailed {
                    id,
                    error: &error_str,
                });
                failed_refreshes.insert(id.clone());
            }
        }
    }

    failed_refreshes
}

/// Resolve a resource's attributes using the current bindings.
/// Secret values are unwrapped so the provider receives the plain inner value.
///
/// Any attribute that still contains a `Value::Deferred(ResourceRef|
/// BindingRef|Interpolation|FunctionCall)` after resolution is rejected
/// here — pre-#3032 the executor passed such values straight through
/// to the provider, where the WASM serializer surfaced them as a
/// generic "cannot serialize at WASM provider boundary" error with no
/// hint about *why* the reference was unresolved.
///
/// The fail-fast happens at the executor seam rather than at the WASM
/// boundary so the error names the attribute and points the user at
/// `wait` as the synchronization mechanism for upstream attributes
/// that populate asynchronously (ACM `domain_validation_options`,
/// CloudFront `domain_name`, etc.).
pub(super) async fn resolve_resource(
    resource: &Resource,
    bindings: &ResolvedBindings,
    pipeline: &RenormalizePipeline<'_>,
) -> Result<Resource, String> {
    let mut resolved = resource.clone();
    for (key, expr) in &resource.attributes {
        let resolved_value = unwrap_secret(resolve_ref_value(expr, bindings)?);
        assert_fully_resolved(&resolved_value, key)?;
        resolved.attributes.insert(key.clone(), resolved_value);
    }
    renormalize(resolved, pipeline).await
}

/// Resolve a resource, preferring unresolved source for re-resolution.
/// Secret values are unwrapped so the provider receives the plain inner value.
///
/// See [`resolve_resource`] for the fail-fast contract on
/// still-deferred values.
pub(super) async fn resolve_resource_with_source(
    target: &Resource,
    source: &Resource,
    bindings: &ResolvedBindings,
    pipeline: &RenormalizePipeline<'_>,
) -> Result<Resource, String> {
    let mut resolved = target.clone();
    for (key, expr) in &source.attributes {
        let resolved_value = unwrap_secret(resolve_ref_value(expr, bindings)?);
        assert_fully_resolved(&resolved_value, key)?;
        resolved.attributes.insert(key.clone(), resolved_value);
    }
    renormalize(resolved, pipeline).await
}

/// The full plan-time normalization pipeline, threaded into the apply
/// executor so reference re-resolution cannot undo it.
///
/// Bundled into one struct (rather than three separate args) so the
/// resolve helpers and `BasicEffectCtx` carry a single field.
pub(super) struct RenormalizePipeline<'a> {
    pub(super) normalizer: &'a dyn ProviderNormalizer,
    pub(super) factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    pub(super) schemas: &'a crate::schema::SchemaRegistry,
}

/// Re-apply the full plan-time normalization pipeline to a freshly
/// resolved resource.
///
/// carina#3060 / carina#3063: reference re-resolution rebuilds
/// attributes from the un-normalized source, undoing every plan-time
/// normalization stage. Both resolve helpers funnel through here so
/// apply-path resolution and the plan-time pipeline can never diverge
/// again — "resolved" always means "resolved and fully re-normalized"
/// by construction.
///
/// The three desired-side stages, in the same order the plan path runs
/// them (`canonicalize_resources_with_schemas` at `apply/mod.rs` then
/// `PlanPreprocessor::prepare`):
/// 1. `canonicalize_resources_with_schemas` — `Union[String,
///    list(String)]` coercion (#2481, #2511).
/// 2. `ProviderNormalizer::normalize_desired` — enum-identifier
///    resolution (carina#3060).
/// 3. `resolve_enum_aliases_for_resources` — enum-alias → AWS canonical
///    (e.g. `IpProtocol.all` → `"-1"`), per-resource factory dispatch
///    (carina#3063).
///
/// Stage 3's *function* is the single core helper the plan path now
/// calls too, so the alias logic cannot diverge. The *ordering* of the
/// three stages is, however, still hand-mirrored here vs. the plan
/// path's own inline sequence (split across `apply/mod.rs` and
/// `PlanPreprocessor::prepare`, which also interleaves plan-only
/// state-side passes). Extracting one shared sequencing primitive so a
/// future reorder of either side cannot desync is tracked in
/// carina#3068 — it requires restructuring `PlanPreprocessor::prepare`,
/// a plan-pipeline refactor beyond this fix's scope.
///
/// Infallible today (no stage returns an error); the `Result` only
/// mirrors the caller's signature — `resolve_resource{,_with_source}`
/// fail-fast earlier on still-`Deferred` values — so this stays the tail
/// expression without forcing `Ok(...)` at every call site.
async fn renormalize(
    resolved: Resource,
    pipeline: &RenormalizePipeline<'_>,
) -> Result<Resource, String> {
    let mut one = [resolved];
    crate::value::canonicalize_resources_with_schemas(&mut one, pipeline.schemas);
    pipeline.normalizer.normalize_desired(&mut one).await;
    crate::value::resolve_enum_aliases_for_resources(&mut one, pipeline.factories);
    let [resolved] = one;
    Ok(resolved)
}

/// Reject a resolved attribute value that still carries an unresolved
/// `Deferred` payload (carina#3032).
///
/// Walks `List` / `Map` containers so a chained `[idx].field` ref
/// hidden inside `resource_records = [cert.dvo[0].rrv]` is caught.
/// `Secret` is peeled and recursed — secret-tagged refs still need to
/// be resolved before reaching the provider (the executor unwraps
/// secrets just downstream of this check).
///
/// `Unknown` is *not* rejected: it is the deliberately-modeled
/// "known after upstream apply" placeholder used by plan rendering
/// (`UnknownReason::UpstreamRef`) and never reaches the apply path
/// for local refs. Treating it as unresolved here would regress
/// `resolve_refs_for_plan`'s contract.
fn assert_fully_resolved(value: &Value, attribute_key: &str) -> Result<(), String> {
    match value {
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                assert_fully_resolved(item, attribute_key)?;
            }
            Ok(())
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for v in map.values() {
                assert_fully_resolved(v, attribute_key)?;
            }
            Ok(())
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            assert_fully_resolved(inner, attribute_key)
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => Err(format!(
            "attribute `{attribute_key}` references `{}` which has not been published yet by the upstream resource. \
                 Add a `wait` block on the upstream binding to synchronize on this attribute before downstream resources read it.",
            path.to_dot_string(),
        )),
        Value::Deferred(DeferredValue::BindingRef { binding }) => Err(format!(
            "attribute `{attribute_key}` references binding `{binding}` which has not been resolved at apply time. \
                 Add a `wait` block on the upstream binding to synchronize before downstream resources read it.",
        )),
        Value::Deferred(DeferredValue::Interpolation(_)) => Err(format!(
            "attribute `{attribute_key}` contains an interpolation that did not fully resolve at apply time. \
                 At least one referenced binding has not been published yet; add a `wait` block on the upstream attribute.",
        )),
        Value::Deferred(DeferredValue::FunctionCall { name, .. }) => Err(format!(
            "attribute `{attribute_key}` calls function `{name}()` whose arguments did not fully resolve at apply time. \
                 At least one referenced binding has not been published yet; add a `wait` block on the upstream attribute.",
        )),
        _ => Ok(()),
    }
}

/// Recursively unwrap `Value::Deferred(DeferredValue::Secret(inner))` to just the inner value.
/// This ensures the provider never sees the Secret wrapper.
fn unwrap_secret(value: Value) -> Value {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => unwrap_secret(*inner),
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items.into_iter().map(unwrap_secret).collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.into_iter()
                .map(|(k, v)| (k, unwrap_secret(v)))
                .collect(),
        )),
        other => other,
    }
}

/// Process a `BasicEffectResult` by updating shared execution state.
///
/// This helper is used by both sequential and phased execution paths to avoid
/// duplicating the result-processing logic for Create/Update/Delete effects.
pub(super) fn process_basic_result(result: BasicEffectResult, exec: &mut ExecutionState<'_>) {
    match result {
        BasicEffectResult::Success {
            state: effect_state,
            resource_id,
            resolved_attrs,
            binding,
        } => {
            *exec.success_count += 1;
            if let Some(s) = effect_state {
                if let Some(attrs) = &resolved_attrs {
                    exec.bindings.record_applied(binding.as_deref(), attrs, &s);
                }
                exec.applied_states.insert(resource_id, s);
            }
        }
        BasicEffectResult::Failure {
            binding, refresh, ..
        } => {
            *exec.failure_count += 1;
            if let Some(binding) = binding {
                exec.failed_bindings.insert(binding);
            }
            if let Some((id, identifier)) = &refresh {
                queue_state_refresh(exec.pending_refreshes, id, Some(identifier.as_str()));
            }
        }
        BasicEffectResult::Deleted { resource_id, .. } => {
            *exec.success_count += 1;
            exec.successfully_deleted.insert(resource_id);
        }
    }
}

/// Count the number of actionable effects (excluding Read and state operations).
pub(super) fn count_actionable_effects(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| !matches!(e, Effect::Read { .. }) && !e.is_state_operation())
        .count()
}

/// Resolution + dispatch context for a basic effect, bundled to keep
/// `execute_basic_effect`'s arity in check (clippy `too_many_arguments`)
/// and mirroring `ReplaceContext`'s shape.
pub(super) struct BasicEffectCtx<'a> {
    pub(super) provider: &'a dyn Provider,
    pub(super) bindings: &'a ResolvedBindings,
    pub(super) unresolved: &'a HashMap<ResourceId, Resource>,
    pub(super) pipeline: &'a RenormalizePipeline<'a>,
    pub(super) completed: &'a AtomicUsize,
    pub(super) total: usize,
}

/// Execute a single Create, Update, or Delete effect.
///
/// This helper encapsulates the shared dispatch logic: increment progress counter,
/// record start time, emit EffectStarted, resolve resource, call provider, and
/// emit EffectSucceeded/EffectFailed. Returns a `BasicEffectResult` that callers
/// map to their path-specific result types.
///
/// Panics if called with a Replace, Read, Import, Remove, or Move effect.
pub(super) async fn execute_basic_effect<'a>(
    effect: &'a Effect,
    ctx: &BasicEffectCtx<'a>,
    observer: &'a dyn ExecutionObserver,
) -> BasicEffectResult {
    let provider = ctx.provider;
    let bindings = ctx.bindings;
    let unresolved = ctx.unresolved;
    let pipeline = ctx.pipeline;
    let completed = ctx.completed;
    let total = ctx.total;
    let c = completed.fetch_add(1, Ordering::Relaxed) + 1;
    let started = Instant::now();
    let progress = ProgressInfo {
        completed: c,
        total,
    };
    observer.on_event(&ExecutionEvent::EffectStarted { effect });

    match effect {
        Effect::Create(resource) => {
            let resolved = match resolve_resource(resource, bindings, pipeline).await {
                Ok(r) => r,
                Err(e) => {
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &e,
                        duration: started.elapsed(),
                        progress,
                    });
                    return BasicEffectResult::Failure {
                        binding: effect.binding_name(),
                        refresh: None,
                    };
                }
            };
            match provider
                .create(
                    &resource.id,
                    CreateRequest {
                        resource: resolved.clone(),
                    },
                )
                .await
            {
                Ok(state) => {
                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                        effect,
                        state: Some(&state),
                        duration: started.elapsed(),
                        progress,
                    });
                    BasicEffectResult::Success {
                        state: Some(state),
                        resource_id: resource.id.clone(),
                        resolved_attrs: Some(resolved.resolved_attributes()),
                        binding: resource.binding.clone(),
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &error_str,
                        duration: started.elapsed(),
                        progress,
                    });
                    BasicEffectResult::Failure {
                        binding: effect.binding_name(),
                        refresh: None,
                    }
                }
            }
        }
        Effect::Update {
            id,
            from,
            to,
            changed_attributes,
        } => {
            let resolve_source = unresolved.get(id).unwrap_or(to);
            let resolved_to =
                match resolve_resource_with_source(to, resolve_source, bindings, pipeline).await {
                    Ok(r) => r,
                    Err(e) => {
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: &e,
                            duration: started.elapsed(),
                            progress,
                        });
                        return BasicEffectResult::Failure {
                            binding: effect.binding_name(),
                            refresh: None,
                        };
                    }
                };
            let identifier = from.identifier.as_deref().unwrap_or("");
            // Augment plan-time `changed_attributes` with any
            // ResourceRef-derived attributes whose resolved value at
            // apply time differs from `from`. This catches the case
            // where a dependency was just replaced (and the binding
            // map flipped to a new computed value) but the plan-time
            // diff did not see the change because both the old and
            // new ResourceRef expressions are syntactically identical.
            // Without this, the patch would omit the changed
            // reference value and the provider would never be told
            // to update it.
            let mut effective_changed: Vec<String> = changed_attributes.clone();
            for (key, new_value) in &resolved_to.attributes {
                if effective_changed.iter().any(|k| k == key) {
                    continue;
                }
                if from.attributes.get(key) != Some(new_value) {
                    effective_changed.push(key.clone());
                }
            }
            let patch = build_update_patch(&effective_changed, &resolved_to, from);
            let request = UpdateRequest {
                from: (**from).clone(),
                patch,
            };
            match provider.update(id, identifier, request).await {
                Ok(state) => {
                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                        effect,
                        state: Some(&state),
                        duration: started.elapsed(),
                        progress,
                    });
                    BasicEffectResult::Success {
                        state: Some(state),
                        resource_id: id.clone(),
                        resolved_attrs: Some(resolved_to.resolved_attributes()),
                        binding: to.binding.clone(),
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &error_str,
                        duration: started.elapsed(),
                        progress,
                    });
                    BasicEffectResult::Failure {
                        binding: effect.binding_name(),
                        refresh: Some((id.clone(), identifier.to_string())),
                    }
                }
            }
        }
        Effect::Delete {
            id,
            identifier,
            directives,
            ..
        } => match provider
            .delete(
                id,
                identifier,
                DeleteRequest {
                    directives: directives.clone(),
                },
            )
            .await
        {
            Ok(()) => {
                observer.on_event(&ExecutionEvent::EffectSucceeded {
                    effect,
                    state: None,
                    duration: started.elapsed(),
                    progress,
                });
                BasicEffectResult::Deleted {
                    resource_id: id.clone(),
                }
            }
            Err(e) => {
                let error_str = e.to_string();
                observer.on_event(&ExecutionEvent::EffectFailed {
                    effect,
                    error: &error_str,
                    duration: started.elapsed(),
                    progress,
                });
                BasicEffectResult::Failure {
                    binding: None,
                    refresh: Some((id.clone(), identifier.clone())),
                }
            }
        },
        _ => unreachable!("execute_basic_effect called with non-basic effect"),
    }
}

#[cfg(test)]
mod assert_fully_resolved_tests {
    //! Unit-level coverage of the apply-time fail-fast contract added
    //! for carina#3032. The end-to-end executor variant lives at
    //! `executor::tests::test_chained_index_then_field_unresolved_at_apply_fails_with_clear_error`;
    //! these tests cover the deferred-variant matrix without going
    //! through the executor + mock provider scaffolding.
    use super::*;
    use crate::resource::{
        AccessPath, ConcreteValue, DeferredValue, InterpolationPart, PathSegment, Subscript,
        UnknownReason, Value,
    };

    fn dvo_chained_ref() -> Value {
        let path = AccessPath::with_segments(
            "cert",
            "domain_validation_options",
            vec![
                PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                },
                PathSegment::Field {
                    name: "resource_record_value".to_string(),
                },
            ],
        );
        Value::Deferred(DeferredValue::ResourceRef { path })
    }

    #[test]
    fn concrete_value_is_fully_resolved() {
        assert!(
            assert_fully_resolved(&Value::Concrete(ConcreteValue::String("ok".into())), "name",)
                .is_ok()
        );
    }

    #[test]
    fn unknown_is_intentionally_allowed() {
        // `Unknown` is the plan-rendering placeholder for upstream
        // refs (#2371). It legitimately reaches the apply path during
        // re-resolution sweeps and must not be rejected by the local
        // fail-fast.
        let v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue));
        assert!(assert_fully_resolved(&v, "name").is_ok());
    }

    #[test]
    fn direct_chained_resource_ref_is_rejected_with_path_and_wait_hint() {
        let err = assert_fully_resolved(&dvo_chained_ref(), "name")
            .expect_err("direct ResourceRef must be rejected at apply seam");
        assert!(
            err.contains("cert.domain_validation_options[0].resource_record_value"),
            "error must name the unresolved path; got: {err}",
        );
        assert!(
            err.contains("wait"),
            "error must point at `wait` as the workaround; got: {err}",
        );
    }

    #[test]
    fn list_wrapped_chained_ref_is_rejected() {
        // The exact carina#3032 failing form: `resource_records =
        // [cert.dvo[0].rrv]`. The list literal must be walked.
        let v = Value::Concrete(ConcreteValue::List(vec![dvo_chained_ref()]));
        let err = assert_fully_resolved(&v, "resource_records")
            .expect_err("list-wrapped ResourceRef must be rejected");
        assert!(err.contains("resource_record_value"), "got: {err}");
    }

    #[test]
    fn map_wrapped_chained_ref_is_rejected() {
        let mut m: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        m.insert("nested".to_string(), dvo_chained_ref());
        let err = assert_fully_resolved(&Value::Concrete(ConcreteValue::Map(m)), "tags")
            .expect_err("map-wrapped ResourceRef must be rejected");
        assert!(err.contains("resource_record_value"), "got: {err}");
    }

    #[test]
    fn secret_wrapped_ref_is_peeled_and_rejected() {
        // `Secret(ResourceRef)` would otherwise sneak past — the
        // secret tag would survive but the ref would still be
        // unresolved at the WASM boundary.
        let v = Value::Deferred(DeferredValue::Secret(Box::new(dvo_chained_ref())));
        let err = assert_fully_resolved(&v, "password")
            .expect_err("Secret wrapping does not exempt unresolved refs");
        assert!(err.contains("resource_record_value"), "got: {err}");
    }

    #[test]
    fn binding_ref_is_rejected() {
        let v = Value::Deferred(DeferredValue::BindingRef {
            binding: "vpc".to_string(),
        });
        let err =
            assert_fully_resolved(&v, "vpc_id").expect_err("bare BindingRef must be rejected");
        assert!(err.contains("vpc"), "got: {err}");
        assert!(err.contains("wait"), "got: {err}");
    }

    #[test]
    fn interpolation_with_unresolved_part_is_rejected() {
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".to_string()),
            InterpolationPart::Expr(dvo_chained_ref()),
        ]));
        let err = assert_fully_resolved(&v, "name")
            .expect_err("Interpolation that did not collapse must be rejected");
        assert!(err.contains("interpolation"), "got: {err}");
    }

    #[test]
    fn function_call_with_unresolved_args_is_rejected() {
        let v = Value::Deferred(DeferredValue::FunctionCall {
            name: "concat".to_string(),
            args: vec![dvo_chained_ref()],
        });
        let err = assert_fully_resolved(&v, "name")
            .expect_err("FunctionCall that did not evaluate must be rejected");
        assert!(err.contains("concat"), "got: {err}");
    }
}
