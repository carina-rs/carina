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
    /// Every candidate was complete, but a direct iterable reference was
    /// absent and there was no coalesce fallback.
    IterableAbsent {
        expression: String,
        candidates: Vec<String>,
    },
    /// The iterable still depends on an incomplete producer even though the
    /// scheduler attempted to dispatch the deferred effect.
    IterableUnknown {
        expression: String,
        candidates: Vec<String>,
    },
    /// The iterable attribute exists but is not a collection shape supported
    /// by deferred-for expansion.
    ShapeMismatch {
        expression: String,
        candidates: Vec<String>,
        mismatch: ShapeMismatch,
    },
    ValueAccess {
        error: DeferredForExpansionError,
    },
}

impl DeferredCreateFailure {
    pub(super) fn message(&self) -> String {
        match self {
            DeferredCreateFailure::IterableAbsent {
                expression,
                candidates,
            } => {
                format!(
                    "deferred-for iterable `{expression}` is absent after resolving candidates: {}",
                    candidates.join(", ")
                )
            }
            DeferredCreateFailure::ShapeMismatch {
                expression,
                candidates,
                mismatch,
            } => {
                format!(
                    "deferred-for expansion expected {} for `{expression}` but got {} (candidates: {})",
                    mismatch.expected_kind(),
                    mismatch.got_kind(),
                    candidates.join(", ")
                )
            }
            DeferredCreateFailure::IterableUnknown {
                expression,
                candidates,
            } => format!(
                "deferred-for iterable `{expression}` is still unknown at dispatch (candidates: {})",
                candidates.join(", ")
            ),
            DeferredCreateFailure::ValueAccess { error } => error.to_string(),
        }
    }
}

/// Materialize `Effect::Create` children for a deferred-for template using the
/// runtime values published under every binding referenced by the iterable.
pub(super) fn materialize_deferred_create<S: Clone>(
    template: &DeferredForExpression<S>,
    bindings: &ResolvedBindings,
) -> Result<Vec<Effect<S>>, DeferredCreateFailure> {
    let expression = crate::value::format_value(&template.iterable);
    let candidates = template
        .reference_candidates()
        .into_iter()
        .collect::<Vec<_>>();
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
            return Err(DeferredCreateFailure::IterableAbsent {
                expression,
                candidates,
            });
        }
        crate::resolver::ValueResolution::Unknown(_) => {
            return Err(DeferredCreateFailure::IterableUnknown {
                expression,
                candidates,
            });
        }
    };
    let miss_policy = bindings.access_miss_policy_for_value(&template.iterable);

    Ok(expand_deferred_children(template, &iterable, miss_policy)
        .map_err(|error| match error {
            DeferredForExpansionError::ShapeMismatch(mismatch) => {
                DeferredCreateFailure::ShapeMismatch {
                    expression: crate::value::format_value(&template.iterable),
                    candidates: template.reference_candidates().into_iter().collect(),
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
                template
                    .root_bindings()
                    .is_subset(&resource.dependency_bindings),
                "apply-time deferred-for child must retain iterable dependencies"
            );
            Effect::Create(ResolvedResource::new(resource))
        })
        .collect())
}

/// Result of synchronously dispatching a deferred scheduler-meta effect.
pub(super) enum DeferredDispatchResult<S> {
    /// DeferredCreate or DeferredReplace create children were materialized and
    /// should be appended to the scheduler queue.
    Materialized(Vec<Effect<S>>),
    /// Deferred-for materialization failed before any child effects existed.
    /// Callers must increment `failure_count` for this meta-effect failure.
    MaterializeFailed,
}

/// Shared inputs required to materialize deferred scheduler-meta effects.
pub(super) struct PureMetaCtx<'a, S> {
    pub(super) completed: &'a AtomicUsize,
    pub(super) total: usize,
    pub(super) observer: &'a dyn ExecutionObserver<S>,
}

/// Dispatch a DeferredCreate meta-effect by materializing its runtime
/// `Effect::Create` children, or emitting the meta-effect failure event when
/// upstream state is missing or has the wrong shape.
pub(super) fn dispatch_deferred_create<S: Clone>(
    effect: &Effect<S>,
    template: &DeferredForExpression<S>,
    bindings: &ResolvedBindings,
    ctx: &PureMetaCtx<'_, S>,
) -> DeferredDispatchResult<S> {
    match materialize_deferred_create(template, bindings) {
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

        let children = materialize_deferred_create(&coalesced_template(), &bindings)
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

        let children = materialize_deferred_create(&coalesced_template(), &bindings)
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
            materialize_deferred_create(&coalesced_template(), &bindings),
            Err(DeferredCreateFailure::IterableUnknown { .. })
        ));
    }

    #[test]
    fn deferred_iterable_error_reports_every_candidate_reference() {
        let mut template = coalesced_template();
        template.iterable = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("primary", "items", vec![])),
            default: Box::new(Value::resource_ref("fallback", "items", vec![])),
        });

        let error = materialize_deferred_create(&template, &ResolvedBindings::default())
            .expect_err("both unresolved candidates keep the iterable unknown");
        let DeferredCreateFailure::IterableUnknown { candidates, .. } = &error else {
            panic!("expected unknown iterable, got {error:?}");
        };
        assert_eq!(
            candidates,
            &["fallback.items".to_string(), "primary.items".to_string()],
        );
        let message = error.message();
        assert!(message.contains("fallback.items"), "{message}");
        assert!(message.contains("primary.items"), "{message}");
    }
}
