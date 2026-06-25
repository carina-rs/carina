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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeferredCreateFailure {
    UpstreamBindingMissing {
        upstream_binding: String,
    },
    IterableAttrMissing {
        upstream_binding: String,
        attr: String,
    },
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

pub(super) enum DeferredDispatchResult {
    Materialized(Vec<Effect>),
    DeleteFailed,
    MaterializeFailed,
}

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
