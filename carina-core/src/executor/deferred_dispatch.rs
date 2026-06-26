use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::{ExecutionEvent, ExecutionObserver, ProgressInfo};
use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::{DeferredForExpression, ShapeMismatch, expand_deferred_children};

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
    /// Deferred-for materialization failed before any child effects existed.
    /// Callers must increment `failure_count` for this meta-effect failure.
    MaterializeFailed,
}

/// Shared inputs required to materialize deferred scheduler-meta effects.
pub(super) struct PureMetaCtx<'a> {
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
    ctx: &PureMetaCtx<'_>,
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
