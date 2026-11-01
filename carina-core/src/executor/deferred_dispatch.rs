use std::collections::HashSet;

use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::{
    DeferredForExpansionError, DeferredForExpression, DeferredForTarget, ShapeMismatch,
    expand_deferred_children,
};
use crate::resource::{ResolvedResource, ResolvedResourceError, ResourceId};
#[cfg(test)]
use crate::schema::SchemaRegistry;

#[derive(Clone, Copy)]
pub(super) struct DeferredIdentityInputs<'a> {
    pub(super) provider_configs: &'a [crate::parser::ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    pub(super) factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    pub(super) schemas: &'a crate::schema::SchemaRegistry,
}

/// Failure while expanding a deferred-for create half against apply-time
/// upstream state.
#[derive(Debug)]
pub enum DeferredCreateFailure {
    /// Every candidate was complete, but a direct iterable reference was
    /// absent and there was no coalesce fallback.
    IterableAbsent {
        file: Option<String>,
        line: usize,
        binding: Box<DeferredForTarget>,
        expression: String,
        candidates: Vec<String>,
    },
    /// The iterable still depends on an incomplete producer even though the
    /// scheduler attempted to dispatch the deferred effect.
    IterableUnknown {
        file: Option<String>,
        line: usize,
        binding: Box<DeferredForTarget>,
        expression: String,
        candidates: Vec<String>,
    },
    /// The iterable attribute exists but is not a collection shape supported
    /// by deferred-for expansion.
    ShapeMismatch {
        file: Option<String>,
        line: usize,
        binding: Box<DeferredForTarget>,
        expression: String,
        candidates: Vec<String>,
        mismatch: Box<ShapeMismatch>,
    },
    ValueAccess {
        error: DeferredForExpansionError,
    },
    ResolveResource {
        error: ResolvedResourceError,
    },
    ProviderConfig {
        error: crate::value::ProviderConfigCanonicalizationError,
    },
    EnumCanonicalization {
        error: crate::value::EnumCanonicalizationError,
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
pub enum AnonymousIdentityCollisionSource {
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
            template.target(),
            crate::parser::DeferredForTarget::Unbound {
                template: _template
            }
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

    pub(super) fn release_completed_current_delete(&mut self, id: &ResourceId) {
        self.current.remove(id);
    }
}

impl std::fmt::Display for DeferredCreateFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeferredCreateFailure::IterableAbsent {
                file,
                line,
                binding,
                expression,
                candidates,
            } => write!(
                formatter,
                "{}deferred-for iterable `{expression}` is absent for binding `{binding}` after resolving candidates: {}",
                source_location(file.as_deref(), *line),
                candidates.join(", "),
                binding = binding.scheduler_key(),
            ),
            DeferredCreateFailure::ShapeMismatch {
                file,
                line,
                binding,
                expression,
                candidates,
                mismatch,
            } => write!(
                formatter,
                "{}deferred-for expansion for binding `{binding}` expected {} for `{expression}` but got {} (candidates: {})",
                source_location(file.as_deref(), *line),
                mismatch.expected_kind(),
                mismatch.got_kind(),
                candidates.join(", "),
                binding = binding.scheduler_key(),
            ),
            DeferredCreateFailure::IterableUnknown {
                file,
                line,
                binding,
                expression,
                candidates,
            } => write!(
                formatter,
                "{}deferred-for iterable `{expression}` for binding `{binding}` is still unknown at dispatch (candidates: {})",
                source_location(file.as_deref(), *line),
                candidates.join(", "),
                binding = binding.scheduler_key(),
            ),
            DeferredCreateFailure::ValueAccess { error: _error } => {
                formatter.write_str("deferred-for value resolution failed")
            }
            DeferredCreateFailure::ResolveResource { error: _error } => {
                formatter.write_str("deferred-for resource resolution failed")
            }
            DeferredCreateFailure::ProviderConfig { error: _error } => {
                formatter.write_str("deferred-for provider configuration failed")
            }
            DeferredCreateFailure::EnumCanonicalization { error: _error } => {
                formatter.write_str("deferred-for enum canonicalization failed")
            }
            DeferredCreateFailure::AnonymousIdentity { error: _error } => {
                formatter.write_str("deferred-for anonymous identity failed")
            }
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
                write!(
                    formatter,
                    "anonymous deferred-loop identity collided at `{id}`: the address is already owned by {owner}"
                )
            }
        }
    }
}

fn source_location(file: Option<&str>, line: usize) -> String {
    match file {
        Some(file) => format!("{file}:{line}: "),
        None if line > 0 => format!("line {line}: "),
        None => String::new(),
    }
}

impl std::error::Error for DeferredCreateFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ValueAccess { error } => Some(error),
            Self::ResolveResource { error } => Some(error),
            Self::ProviderConfig { error } => Some(error),
            Self::EnumCanonicalization { error } => Some(error),
            Self::AnonymousIdentity { error } => Some(error),
            Self::IterableAbsent {
                file: _file,
                line: _line,
                binding: _binding,
                expression: _expression,
                candidates: _candidates,
            }
            | Self::IterableUnknown {
                file: _file,
                line: _line,
                binding: _binding,
                expression: _expression,
                candidates: _candidates,
            } => None,
            Self::ShapeMismatch {
                file: _file,
                line: _line,
                binding: _binding,
                expression: _expression,
                candidates: _candidates,
                mismatch: _mismatch,
            } => None,
            Self::AnonymousIdentityCollision {
                id: _id,
                occupied_by: _occupied_by,
            } => None,
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
                line: template.line(),
                expression: crate::value::format_value(&template.iterable),
                message,
            },
        })? {
        crate::resolver::ValueResolution::Present(value) => value,
        crate::resolver::ValueResolution::Absent => {
            return Err(DeferredCreateFailure::IterableAbsent {
                file: template.file().map(str::to_owned),
                line: template.line(),
                binding: Box::new(template.target().clone()),
                expression,
                candidates,
            });
        }
        crate::resolver::ValueResolution::Unknown(_) => {
            return Err(DeferredCreateFailure::IterableUnknown {
                file: template.file().map(str::to_owned),
                line: template.line(),
                binding: Box::new(template.target().clone()),
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
                    file: template.file().map(str::to_owned),
                    line: template.line(),
                    binding: Box::new(template.target().clone()),
                    expression: crate::value::format_value(&template.iterable),
                    candidates: template.reference_candidates().into_iter().collect(),
                    mismatch: Box::new(mismatch),
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
        template.target(),
        crate::parser::DeferredForTarget::Unbound {
            template: _template
        }
    ) {
        let canonical_resources = crate::value::canonicalize_resources_with_schemas(
            &mut resources,
            identity_inputs.schemas,
        )
        .map_err(|error| DeferredCreateFailure::EnumCanonicalization { error })?;
        crate::identifier::compute_anonymous_identifiers_with_factories(
            canonical_resources,
            identity_inputs.provider_configs,
            identity_inputs.factories,
            identity_inputs.schemas,
            crate::identifier::AnonymousIdentityConflictPolicy::RejectAttributeDerived,
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

    resources
        .into_iter()
        .map(|resource| {
            debug_assert!(
                template
                    .root_bindings()
                    .is_subset(&resource.dependency_bindings),
                "apply-time deferred-for child must retain iterable dependencies"
            );
            ResolvedResource::resolve_with_schemas(resource, identity_inputs.schemas)
                .map(Effect::Create)
                .map_err(|error| DeferredCreateFailure::ResolveResource { error })
        })
        .collect::<Result<Vec<_>, _>>()
}

/// Result of synchronously dispatching a deferred scheduler-meta effect.
pub(super) enum DeferredDispatchResult<S> {
    /// DeferredCreate or DeferredReplace create children were materialized and
    /// should be appended to the scheduler queue.
    Materialized(Vec<Effect<S>>),
    /// Deferred-for materialization failed before any child effects existed.
    MaterializeFailed(DeferredCreateFailure),
}

/// Shared inputs required to materialize deferred scheduler-meta effects.
pub(super) struct PureMetaCtx<'a> {
    pub(super) identity_inputs: DeferredIdentityInputs<'a>,
}

/// Dispatch a DeferredCreate meta-effect by materializing its runtime
/// `Effect::Create` children or returning the typed materialization failure.
pub(super) fn dispatch_deferred_create<
    S: Clone + crate::resource::AnonymousIdentityModuleSource,
>(
    _effect: &Effect<S>,
    template: &DeferredForExpression<S>,
    bindings: &ResolvedBindings,
    ctx: &PureMetaCtx<'_>,
) -> DeferredDispatchResult<S> {
    match materialize_deferred_create(template, bindings, &ctx.identity_inputs) {
        Ok(children) => DeferredDispatchResult::Materialized(children),
        Err(err) => DeferredDispatchResult::MaterializeFailed(err),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::binding_index::{BindingCompleteness, BindingValueSource};
    use crate::parser::{ExpandedDeferredForExpression, ForBinding};
    use crate::resource::{
        AbsoluteBindingAddress, ExpandedConcreteValue as ConcreteValue,
        ExpandedDeferredValue as DeferredValue, ExpandedValue as Value, LoopSlot, ModuleSource,
        ParsedIgnorePath, ParsedIgnorePathSegment, Resource, RootScope,
    };
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    fn coalesced_template() -> ExpandedDeferredForExpression {
        let scope = RootScope::new();
        let mut template_resource: Resource<ModuleSource, LoopSlot, AbsoluteBindingAddress> =
            Resource::new("test.Child", "children");
        template_resource.binding = Some(scope.resolve("children".into()));
        template_resource.set_attr(
            "name",
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
                LoopSlot::Value,
            )),
        );
        DeferredForExpression::new(
            None,
            1,
            "for account in (up.accounts ?? [])".to_string(),
            crate::parser::DeferredForTarget::bound(
                crate::resource::absolute_binding_address_for_test("children"),
            )
            .expect("fixture binding is valid"),
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::root_resource_ref("up", "accounts", vec![])),
                default: Box::new(Value::Concrete(ConcreteValue::List(Vec::new()))),
            }),
            ForBinding::Simple("account".to_string()),
            template_resource,
        )
    }

    fn materialize_for_test(
        template: &ExpandedDeferredForExpression,
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
    fn deferred_create_resolves_authored_ignore_changes() {
        let mut template = coalesced_template();
        let comment = crate::resource::Value::<LoopSlot, AbsoluteBindingAddress>::Concrete(
            crate::resource::ConcreteValue::String("managed-elsewhere".into()),
        );

        template
            .template_attributes_mut()
            .insert("comment".to_string(), comment);
        template.template_directives_mut().ignore_changes = vec![ParsedIgnorePath::new(vec![
            ParsedIgnorePathSegment::Attribute("comment".to_string()),
        ])];
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "up",
            HashMap::from([(
                "accounts".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("a".into()),
                )])),
            )]),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );

        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "",
            ResourceSchema::new("test.Child")
                .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
                .validate()
                .expect("valid deferred child schema"),
        );
        let identity_inputs = DeferredIdentityInputs {
            provider_configs: &[],
            factories: &[],
            schemas: &schemas,
        };
        let children = materialize_deferred_create(&template, &bindings, &identity_inputs)
            .expect("deferred children must resolve authored ignore paths");

        let Effect::Create(child) = &children[0] else {
            panic!("expected a create child");
        };
        assert_eq!(child.ignore_changes().iter().len(), 1);
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
        let parsed = crate::module_resolver::resolve_modules_providerless(
            crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
            std::path::Path::new("."),
        )
        .expect("deferred fixture crosses the module-resolution seam")
        .into_expanded()
        .expect("deferred-loop fixture contains no parser placeholders");
        let template = &parsed.deferred_for_expressions()[0];
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
    fn apply_time_unbound_enum_identity_matches_plan_time_canonical_identity() {
        let parsed = crate::parser::parse_file(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.values {\n\
               mock.test.Child { mode = value }\n\
             }\n",
            std::path::Path::new("main.crn"),
            &crate::parser::ProviderContext::default(),
        )
        .expect("deferred unbound enum loop parses");
        let parsed = crate::module_resolver::resolve_modules_providerless(
            crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
            std::path::Path::new("."),
        )
        .expect("deferred fixture crosses the module-resolution seam")
        .into_expanded()
        .expect("deferred-loop fixture contains no parser placeholders");
        let template = &parsed.deferred_for_expressions()[0];
        let iterable = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::enum_identifier("foo"),
        )]));
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "upstream",
            HashMap::from([("values".to_string(), iterable.clone())]),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );
        let mode_type = crate::schema::AttributeType::enum_(
            crate::schema::enum_identity("Mode", Some("mock.test.Child")),
            Some(vec!["API_A".to_string()]),
            vec![("API_A".to_string(), "foo".to_string())],
            None,
            None,
        );
        let schema = crate::schema::ResourceSchema::new("test.Child").attribute(
            crate::schema::AttributeSchema::optional("mode", mode_type)
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

        let runtime_children = materialize_deferred_create(template, &bindings, &identity_inputs)
            .expect("apply-time enum child materializes");
        let [Effect::Create(runtime_child)] = runtime_children.as_slice() else {
            panic!("one runtime child expected");
        };

        let mut plan_time_resources = crate::parser::expand_deferred_children(
            template,
            &iterable,
            crate::resource::AccessMissPolicy::Error,
        )
        .expect("plan-time child expands");
        let canonical =
            crate::value::canonicalize_resources_with_schemas(&mut plan_time_resources, &schemas)
                .expect("plan-time child canonicalizes");
        crate::identifier::compute_anonymous_identifiers_with_factories(
            canonical,
            &[],
            &[],
            &schemas,
            crate::identifier::AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        )
        .expect("plan-time anonymous identity computes");

        assert!(matches!(
            runtime_child.get_attr("mode"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "API_A"
        ));
        assert_eq!(
            runtime_child.id, plan_time_resources[0].id,
            "apply-time hashing must consume the same canonical enum representation as planning"
        );
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
        let parsed = crate::module_resolver::resolve_modules_providerless(
            crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
            std::path::Path::new("."),
        )
        .expect("deferred fixture crosses the module-resolution seam")
        .into_expanded()
        .expect("deferred-loop fixture contains no parser placeholders");
        let template = &parsed.deferred_for_expressions()[0];
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
            value: Box::new(Value::root_resource_ref("primary", "items", vec![])),
            default: Box::new(Value::root_resource_ref("fallback", "items", vec![])),
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
        let message = error.to_string();
        assert!(message.contains("fallback.items"), "{message}");
        assert!(message.contains("primary.items"), "{message}");
    }
}
