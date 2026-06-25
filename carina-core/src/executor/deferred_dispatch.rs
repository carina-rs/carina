use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::stream::{self, StreamExt};

use crate::binding_index::ResolvedBindings;
use crate::effect::{DeferredReplaceDelete, Effect};
use crate::parser::{
    DeferredForExpression, ProviderConfig, ShapeMismatch, expand_deferred_children,
};
use crate::provider::{Provider, ProviderFactory, ProviderNormalizer};
use crate::resource::ResourceId;
use crate::schema::SchemaRegistry;

use super::basic::{
    BasicEffectCtx, BasicEffectResult, ExecutionState, RenormalizePipeline, execute_basic_effect,
    process_basic_result,
};
use super::{ExecutionEvent, ExecutionObserver, ProgressInfo, UnresolvedResource};

/// Failure while expanding a deferred-for create half against apply-time
/// upstream state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeferredCreateFailure {
    /// The upstream binding that should have published iterable attributes is
    /// absent from the runtime binding table.
    UpstreamBindingMissing { upstream_binding: String },
    /// The upstream binding exists but does not contain the iterable
    /// attribute named by the deferred-for template.
    IterableAttrMissing {
        upstream_binding: String,
        attr: String,
    },
    /// The iterable attribute exists but is not a collection shape supported
    /// by deferred-for expansion.
    ShapeMismatch {
        upstream_binding: String,
        attr: String,
        mismatch: ShapeMismatch,
    },
}

impl DeferredCreateFailure {
    pub(super) fn message(&self) -> String {
        match self {
            DeferredCreateFailure::UpstreamBindingMissing { upstream_binding } => {
                format!(
                    "deferred-for expansion upstream binding `{upstream_binding}` was not published before dispatch"
                )
            }
            DeferredCreateFailure::IterableAttrMissing {
                upstream_binding,
                attr,
            } => {
                format!(
                    "deferred-for expansion upstream binding `{upstream_binding}` does not contain iterable attribute `{attr}`"
                )
            }
            DeferredCreateFailure::ShapeMismatch {
                upstream_binding,
                attr,
                mismatch,
            } => {
                format!(
                    "deferred-for expansion expected {} for `{upstream_binding}.{attr}` but got {}",
                    mismatch.expected_kind(),
                    mismatch.got_kind()
                )
            }
        }
    }
}

/// Materialize `Effect::Create` children for a deferred-for template using the
/// runtime value published under `upstream_binding`.
pub(super) fn materialize_deferred_create(
    upstream_binding: &str,
    template: &DeferredForExpression,
    bindings: &ResolvedBindings,
) -> Result<Vec<Effect>, DeferredCreateFailure> {
    let upstream_attrs = bindings.get(upstream_binding).ok_or_else(|| {
        DeferredCreateFailure::UpstreamBindingMissing {
            upstream_binding: upstream_binding.to_string(),
        }
    })?;
    let iterable = upstream_attrs.get(&template.iterable_attr).ok_or_else(|| {
        DeferredCreateFailure::IterableAttrMissing {
            upstream_binding: upstream_binding.to_string(),
            attr: template.iterable_attr.clone(),
        }
    })?;

    Ok(expand_deferred_children(template, iterable)
        .map_err(|mismatch| DeferredCreateFailure::ShapeMismatch {
            upstream_binding: upstream_binding.to_string(),
            attr: template.iterable_attr.clone(),
            mismatch,
        })?
        .into_iter()
        .map(Effect::Create)
        .collect())
}

/// Result of synchronously dispatching a deferred scheduler-meta effect.
pub(super) enum DeferredDispatchResult {
    /// DeferredCreate or DeferredReplace create children were materialized and
    /// should be appended to the scheduler queue.
    Materialized(Vec<Effect>),
    /// At least one DeferredReplace delete half failed. The helper already
    /// processed those delete results, so callers must mark the template
    /// binding failed but must not increment `failure_count` again.
    DeleteFailed,
    /// Deferred-for materialization failed before any child effects existed.
    /// Callers must increment `failure_count` for this meta-effect failure.
    MaterializeFailed,
}

/// Shared inputs required to dispatch DeferredCreate and DeferredReplace
/// scheduler-meta effects.
pub(super) struct DeferredDispatchCtx<'a> {
    pub(super) provider: &'a dyn Provider,
    pub(super) unresolved: &'a HashMap<ResourceId, UnresolvedResource>,
    pub(super) normalizer: &'a dyn ProviderNormalizer,
    pub(super) provider_configs: &'a [ProviderConfig],
    pub(super) factories: &'a [Box<dyn ProviderFactory>],
    pub(super) schemas: &'a SchemaRegistry,
    pub(super) completed: &'a AtomicUsize,
    pub(super) total: usize,
    pub(super) observer: &'a dyn ExecutionObserver,
}

/// Dispatch a DeferredCreate meta-effect by materializing its runtime
/// `Effect::Create` children, or emitting the meta-effect failure event when
/// upstream state is missing or has the wrong shape.
pub(super) fn dispatch_deferred_create(
    effect: &Effect,
    upstream_binding: &str,
    template: &DeferredForExpression,
    bindings: &ResolvedBindings,
    ctx: &DeferredDispatchCtx<'_>,
) -> DeferredDispatchResult {
    match materialize_deferred_create(upstream_binding, template, bindings) {
        Ok(children) => DeferredDispatchResult::Materialized(children),
        Err(err) => {
            let message = err.message();
            ctx.observer.on_event(&ExecutionEvent::EffectFailed {
                effect,
                error: &message,
                duration: Duration::ZERO,
                progress: ProgressInfo {
                    completed: ctx.completed.load(Ordering::Relaxed),
                    total: ctx.total,
                },
            });
            DeferredDispatchResult::MaterializeFailed
        }
    }
}

/// Dispatch a DeferredReplace meta-effect.
///
/// The delete half is executed through the basic delete path and joined before
/// the create half is materialized, preserving the "deletes before create"
/// invariant while allowing independent absorbed deletes to run concurrently.
pub(super) async fn dispatch_deferred_replace(
    effect: &Effect,
    deletes: &[DeferredReplaceDelete],
    upstream_binding: &str,
    template: &DeferredForExpression,
    ctx: &DeferredDispatchCtx<'_>,
    exec: &mut ExecutionState<'_>,
) -> DeferredDispatchResult {
    let binding_snapshot = exec.bindings.clone();
    let pipeline = RenormalizePipeline {
        normalizer: ctx.normalizer,
        provider_configs: ctx.provider_configs,
        factories: ctx.factories,
        schemas: ctx.schemas,
    };

    let provider = ctx.provider;
    let unresolved = ctx.unresolved;
    let completed = ctx.completed;
    let total = ctx.total;
    let observer = ctx.observer;
    let concurrency = deletes.len().max(1);
    let binding_snapshot = &binding_snapshot;
    let pipeline = &pipeline;

    let results = stream::iter(deletes)
        .map(|delete| {
            let delete_effect = delete.to_delete_effect();
            async move {
                let basic = delete_effect
                    .as_basic()
                    .expect("deferred replace delete half must narrow to BasicEffect");
                execute_basic_effect(
                    basic,
                    &BasicEffectCtx {
                        provider,
                        bindings: binding_snapshot,
                        unresolved,
                        pipeline,
                        completed,
                        total,
                    },
                    observer,
                )
                .await
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<BasicEffectResult>>()
        .await;

    let delete_failed = results
        .iter()
        .any(|result| matches!(result, BasicEffectResult::Failure { .. }));
    for result in results {
        process_basic_result(result, exec);
    }

    if delete_failed {
        DeferredDispatchResult::DeleteFailed
    } else {
        dispatch_deferred_create(effect, upstream_binding, template, exec.bindings, ctx)
    }
}
