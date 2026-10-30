use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::{ExecutionEvent, ExecutionObserver, ProgressInfo};
use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::{
    DeferredForExpansionError, DeferredForExpression, ShapeMismatch, expand_deferred_children,
};
use crate::resource::ResolvedResource;
use crate::resource::ResourceId;

#[derive(Clone, Copy)]
pub(super) struct DeferredIdentityInputs<'a> {
    pub(super) provider_configs: &'a [crate::parser::ProviderConfig],
    pub(super) factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    pub(super) schemas: &'a crate::schema::SchemaRegistry,
}

/// Failure while expanding a deferred-for create half against apply-time
/// upstream state.
#[derive(Debug)]
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
    ProviderConfig {
        error: crate::value::ProviderConfigCanonicalizationError,
    },
    AnonymousIdentity {
        error: crate::identifier::AnonymousIdentityError,
    },
    AnonymousIdentityCollision {
        id: ResourceId,
        occupied_by: AnonymousIdentityCollisionSource,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnonymousIdentityCollisionSource {
    DesiredPlan,
    CurrentState,
    DeferredChild,
}

/// Apply-run ownership domain for IDs minted by unbound deferred loops.
pub(super) struct DeferredAnonymousIdentityGuard {
    desired: HashSet<ResourceId>,
    current: HashSet<ResourceId>,
    absorbed_current: HashSet<ResourceId>,
    materialized: HashSet<ResourceId>,
}

impl DeferredAnonymousIdentityGuard {
    pub(super) fn new(
        desired: HashSet<ResourceId>,
        current: HashSet<ResourceId>,
        absorbed_current: HashSet<ResourceId>,
    ) -> Self {
        Self {
            desired,
            current,
            absorbed_current,
            materialized: HashSet::new(),
        }
    }

    pub(super) fn admit<S>(
        &mut self,
        template: &DeferredForExpression<S>,
        children: &[Effect<S>],
    ) -> Result<(), DeferredCreateFailure> {
        if !matches!(
            template.target,
            crate::parser::DeferredForTarget::Unbound { .. }
        ) {
            return Ok(());
        }

        let mut batch = HashSet::new();
        for child in children {
            let Effect::Create(resource) = child else {
                continue;
            };
            let id = &resource.id;
            let occupied_by = if self.desired.contains(id) {
                Some(AnonymousIdentityCollisionSource::DesiredPlan)
            } else if self.current.contains(id) && !self.absorbed_current.contains(id) {
                Some(AnonymousIdentityCollisionSource::CurrentState)
            } else if self.materialized.contains(id) || !batch.insert(id.clone()) {
                Some(AnonymousIdentityCollisionSource::DeferredChild)
            } else {
                None
            };
            if let Some(occupied_by) = occupied_by {
                return Err(DeferredCreateFailure::AnonymousIdentityCollision {
                    id: id.clone(),
                    occupied_by,
                });
            }
        }
        self.materialized.extend(batch);
        Ok(())
    }
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
            DeferredCreateFailure::ProviderConfig { error } => error.to_string(),
            DeferredCreateFailure::AnonymousIdentity { error } => error.to_string(),
            DeferredCreateFailure::AnonymousIdentityCollision { id, occupied_by } => {
                let owner = match occupied_by {
                    AnonymousIdentityCollisionSource::DesiredPlan => "a desired plan resource",
                    AnonymousIdentityCollisionSource::CurrentState => {
                        "an unabsorbed current-state row"
                    }
                    AnonymousIdentityCollisionSource::DeferredChild => {
                        "another apply-time deferred child"
                    }
                };
                format!(
                    "anonymous deferred-loop identity collided at `{id}`: the address is already owned by {owner}"
                )
            }
        }
    }
}

impl std::fmt::Display for DeferredCreateFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for DeferredCreateFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ValueAccess { error } => Some(error),
            Self::ProviderConfig { error } => Some(error),
            Self::AnonymousIdentity { error } => Some(error),
            Self::IterableAbsent { .. }
            | Self::IterableUnknown { .. }
            | Self::ShapeMismatch { .. }
            | Self::AnonymousIdentityCollision { .. } => None,
        }
    }
}

/// Materialize `Effect::Create` children for a deferred-for template using the
/// runtime values published under every binding referenced by the iterable.
pub(super) fn materialize_deferred_create<
    S: Clone + crate::resource::AnonymousIdentityModuleSource,
>(
    template: &DeferredForExpression<S>,
    bindings: &ResolvedBindings,
    identity_inputs: &DeferredIdentityInputs<'_>,
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

    let mut resources = expand_deferred_children(template, &iterable, miss_policy).map_err(
        |error| match error {
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
        },
    )?;
    if matches!(
        template.target,
        crate::parser::DeferredForTarget::Unbound { .. }
    ) {
        crate::identifier::compute_anonymous_identifiers_with_factories(
            &mut resources,
            identity_inputs.provider_configs,
            identity_inputs.factories,
            identity_inputs.schemas,
        )
        .map_err(|error| match error {
            crate::identifier::AnonymousIdentityComputationError::ProviderConfig(error) => {
                DeferredCreateFailure::ProviderConfig { error }
            }
            crate::identifier::AnonymousIdentityComputationError::Identity(error) => {
                DeferredCreateFailure::AnonymousIdentity { error }
            }
        })?;
    }

    Ok(resources
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
    pub(super) identity_inputs: DeferredIdentityInputs<'a>,
}

pub(super) fn report_deferred_create_failure<S>(
    effect: &Effect<S>,
    error: &DeferredCreateFailure,
    ctx: &PureMetaCtx<'_, S>,
) {
    let message = error.message();
    ctx.observer.on_event(&ExecutionEvent::EffectFailed {
        effect,
        error: &message,
        duration: Duration::ZERO,
        progress: ProgressInfo {
            completed: ctx.completed.load(Ordering::Relaxed),
            total: ctx.total,
        },
    });
}

/// Dispatch a DeferredCreate meta-effect by materializing its runtime
/// `Effect::Create` children, or emitting the meta-effect failure event when
/// upstream state is missing or has the wrong shape.
pub(super) fn dispatch_deferred_create<
    S: Clone + crate::resource::AnonymousIdentityModuleSource,
>(
    effect: &Effect<S>,
    template: &DeferredForExpression<S>,
    bindings: &ResolvedBindings,
    ctx: &PureMetaCtx<'_, S>,
) -> DeferredDispatchResult<S> {
    match materialize_deferred_create(template, bindings, &ctx.identity_inputs) {
        Ok(children) => DeferredDispatchResult::Materialized(children),
        Err(err) => {
            report_deferred_create_failure(effect, &err, ctx);
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
            target: crate::parser::DeferredForTarget::Bound {
                binding: "children".to_string(),
            },
            iterable: Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::resource_ref("up", "accounts", vec![])),
                default: Box::new(Value::Concrete(ConcreteValue::List(Vec::new()))),
            }),
            binding: ForBinding::Simple("account".to_string()),
            template_resource,
        }
    }

    fn materialize_for_test(
        template: &DeferredForExpression,
        bindings: &ResolvedBindings,
    ) -> Result<Vec<Effect>, DeferredCreateFailure> {
        let schemas = crate::schema::SchemaRegistry::new();
        materialize_deferred_create(
            template,
            bindings,
            &DeferredIdentityInputs {
                provider_configs: &[],
                factories: &[],
                schemas: &schemas,
            },
        )
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

        let children = materialize_for_test(&coalesced_template(), &bindings)
            .expect("present iterable should materialize");
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn apply_time_unbound_children_receive_anonymous_ids() {
        let parsed = crate::parser::parse(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.values {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
            &crate::parser::ProviderContext::default(),
        )
        .expect("deferred unbound loop parses");
        let template = &parsed.deferred_for_expressions[0];
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "upstream",
            HashMap::from([(
                "values".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("stable".into()),
                )])),
            )]),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );
        let schema = crate::schema::ResourceSchema::new("test.Child").attribute(
            crate::schema::AttributeSchema::optional(
                "identity_input",
                crate::schema::AttributeType::string(),
            )
            .create_only()
            .build(),
        );
        let mut schemas = crate::schema::SchemaRegistry::new();
        schemas.insert(
            "mock",
            schema.validate().expect("schema fixture must be valid"),
        );
        let identity_inputs = DeferredIdentityInputs {
            provider_configs: &[],
            factories: &[],
            schemas: &schemas,
        };

        let children = materialize_deferred_create(template, &bindings, &identity_inputs)
            .expect("apply-time anonymous child materializes");

        let [Effect::Create(child)] = children.as_slice() else {
            panic!("one concrete create child expected");
        };
        assert!(
            child
                .id
                .identity_str()
                .is_some_and(|identity| identity.starts_with("mock_test_child_"))
        );
        assert!(child.binding.is_none());
        assert!(child.loop_provenance.direct_origin().is_some());
    }

    #[test]
    fn apply_time_anonymous_identity_failure_preserves_the_typed_source() {
        let parsed = crate::parser::parse(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.values {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
            &crate::parser::ProviderContext::default(),
        )
        .expect("deferred unbound loop parses");
        let template = &parsed.deferred_for_expressions[0];
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "upstream",
            HashMap::from([(
                "values".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("same".into())),
                    Value::Concrete(ConcreteValue::String("same".into())),
                ])),
            )]),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );
        let schema = crate::schema::ResourceSchema::new("test.Child").attribute(
            crate::schema::AttributeSchema::optional(
                "identity_input",
                crate::schema::AttributeType::string(),
            )
            .create_only()
            .build(),
        );
        let mut schemas = crate::schema::SchemaRegistry::new();
        schemas.insert(
            "mock",
            schema.validate().expect("schema fixture must be valid"),
        );
        let identity_inputs = DeferredIdentityInputs {
            provider_configs: &[],
            factories: &[],
            schemas: &schemas,
        };

        let error = materialize_deferred_create(template, &bindings, &identity_inputs)
            .expect_err("identical apply-time children must fail identity assignment");
        let source = std::error::Error::source(&error)
            .expect("deferred failure must retain its typed identity source");

        assert!(
            source
                .downcast_ref::<crate::identifier::AnonymousIdentityError>()
                .is_some(),
            "unexpected source type: {source:?}",
        );
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

        let children = materialize_for_test(&coalesced_template(), &bindings)
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
            materialize_for_test(&coalesced_template(), &bindings),
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

        let error = materialize_for_test(&template, &ResolvedBindings::default())
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
