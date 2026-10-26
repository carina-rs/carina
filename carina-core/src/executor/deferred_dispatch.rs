use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::{ExecutionEvent, ExecutionObserver, ProgressInfo};
use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::{
    DeferredForExpansionError, DeferredForExpression, ShapeMismatch, expand_deferred_children,
};
use crate::resource::ResolvedResource;

/// Failure while expanding a deferred-for create half against apply-time
/// upstream state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeferredCreateFailure {
    /// The upstream binding exists but does not contain the iterable
    /// attribute named by the deferred-for template.
    IterableAttrMissing {
        upstream_binding: String,
        attr: String,
    },
    /// The iterable still depends on an incomplete producer even though the
    /// scheduler attempted to dispatch the deferred effect.
    IterableUnknown {
        expression: String,
        upstream_binding: String,
    },
    /// The iterable attribute exists but is not a collection shape supported
    /// by deferred-for expansion.
    ShapeMismatch {
        upstream_binding: String,
        attr: String,
        mismatch: ShapeMismatch,
    },
    ValueAccess {
        error: DeferredForExpansionError,
    },
}

impl DeferredCreateFailure {
    pub(super) fn message(&self) -> String {
        match self {
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
            DeferredCreateFailure::IterableUnknown {
                expression,
                upstream_binding,
            } => format!(
                "deferred-for iterable `{expression}` is still unknown at dispatch (scheduled upstream binding: `{upstream_binding}`)"
            ),
            DeferredCreateFailure::ValueAccess { error } => error.to_string(),
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
    let iterable = match crate::resolver::resolve_ref_value_status(&template.iterable, bindings)
        .map_err(|message| DeferredCreateFailure::ValueAccess {
            error: DeferredForExpansionError::Resolution {
                line: template.line,
                expression: crate::value::format_value(&template.iterable),
                message,
            },
        })? {
        crate::resolver::ValueResolution::Present(value) => value,
        crate::resolver::ValueResolution::Absent => {
            return Err(DeferredCreateFailure::IterableAttrMissing {
                upstream_binding: upstream_binding.to_string(),
                attr: template.iterable_attr.clone(),
            });
        }
        crate::resolver::ValueResolution::Unknown(_) => {
            return Err(DeferredCreateFailure::IterableUnknown {
                expression: crate::value::format_value(&template.iterable),
                upstream_binding: upstream_binding.to_string(),
            });
        }
    };
    let miss_policy = bindings.access_miss_policy_for_value(&template.iterable);

    Ok(expand_deferred_children(template, &iterable, miss_policy)
        .map_err(|error| match error {
            DeferredForExpansionError::ShapeMismatch(mismatch) => {
                DeferredCreateFailure::ShapeMismatch {
                    upstream_binding: upstream_binding.to_string(),
                    attr: template.iterable_attr.clone(),
                    mismatch,
                }
            }
            error @ DeferredForExpansionError::ValueAccess { .. } => {
                DeferredCreateFailure::ValueAccess { error }
            }
            error @ DeferredForExpansionError::Resolution { .. } => {
                DeferredCreateFailure::ValueAccess { error }
            }
        })?
        .into_iter()
        .map(|resource| {
            debug_assert!(
                upstream_binding.is_empty()
                    || resource.dependency_bindings.contains(upstream_binding),
                "apply-time deferred-for child must retain iterable dependencies"
            );
            Effect::Create(ResolvedResource::new(resource))
        })
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::binding_index::{BindingCompleteness, BindingValueSource};
    use crate::parser::ForBinding;
    use crate::resource::{ConcreteValue, DeferredValue, Resource, UnknownReason, Value};

    fn coalesced_template() -> DeferredForExpression {
        let mut template_resource = Resource::new("test.Child", "children");
        template_resource.binding = Some("children".to_string());
        template_resource.set_attr(
            "name",
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
        );
        DeferredForExpression {
            file: None,
            line: 1,
            header: "for account in (up.accounts ?? [])".to_string(),
            resource_type: "test.Child".to_string(),
            attributes: template_resource
                .attributes
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            binding_name: "children".to_string(),
            iterable: Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::resource_ref("up", "accounts", vec![])),
                default: Box::new(Value::Concrete(ConcreteValue::List(Vec::new()))),
            }),
            iterable_binding: "up".to_string(),
            iterable_attr: "accounts".to_string(),
            binding: ForBinding::Simple("account".to_string()),
            template_resource,
        }
    }

    #[test]
    fn apply_time_coalesced_iterable_uses_present_value() {
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "up",
            HashMap::from([(
                "accounts".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("a".into())),
                    Value::Concrete(ConcreteValue::String("b".into())),
                ])),
            )]),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );

        let children = materialize_deferred_create("up", &coalesced_template(), &bindings)
            .expect("present iterable should materialize");
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn apply_time_coalesced_iterable_uses_default_only_for_complete_miss() {
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "up",
            HashMap::new(),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );

        let children = materialize_deferred_create("up", &coalesced_template(), &bindings)
            .expect("complete missing attribute should select []");
        assert!(children.is_empty());
    }

    #[test]
    fn apply_time_coalesced_iterable_does_not_default_for_incomplete_binding() {
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "up",
            HashMap::new(),
            BindingValueSource::Local,
            BindingCompleteness::Incomplete,
        );

        assert!(matches!(
            materialize_deferred_create("up", &coalesced_template(), &bindings),
            Err(DeferredCreateFailure::IterableUnknown { .. })
        ));
    }
}
