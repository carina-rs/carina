use crate::provider::{LoadedProviderSnapshot, ProviderFactory};
use crate::schema::SchemaRegistry;
use crate::validation::admission::{kind, tests::selected_destination};
use std::{collections::HashMap, path::Path, sync::Arc};

// Schema-only factories copied from the LSP fixtures. The snapshot owns both
// the schemas and their loaded provider type context, just as custom_engine does.
struct SchemaFixtureFactory {
    name: String,
    schemas: Vec<crate::schema::ValidatedResourceSchema>,
}

impl ProviderFactory for SchemaFixtureFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    fn validate_config(
        &self,
        _attributes: &crate::provider::CanonicalProviderConfigAttributes,
        _session: &crate::provider::ProviderValidationInputs,
    ) -> crate::provider::ProviderResult<()> {
        Ok(())
    }

    fn create_provider(
        &self,
        _config: &crate::provider::ProviderSession,
        _initialize_outcome: crate::provider::InitializeOutcomeMint,
    ) -> crate::provider::BoxFuture<
        '_,
        crate::provider::ProviderResult<crate::provider::ProviderInitialization>,
    > {
        unimplemented!("schema fixture factories do not construct providers")
    }

    fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
        crate::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
        self.schemas.clone()
    }
}

fn provider_snapshot(schemas: SchemaRegistry) -> LoadedProviderSnapshot {
    let mut schemas_by_owner =
        std::collections::HashMap::<String, Vec<crate::schema::ValidatedResourceSchema>>::new();
    for (owner, _resource_type, _kind, schema) in schemas.iter() {
        schemas_by_owner
            .entry(owner.to_string())
            .or_default()
            .push(schema.clone());
    }
    let factories = schemas_by_owner
        .into_iter()
        .map(|(name, schemas)| {
            Box::new(SchemaFixtureFactory { name, schemas }) as Box<dyn ProviderFactory>
        })
        .collect::<Vec<_>>();
    crate::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        crate::EnvironmentSnapshot::empty(),
    )
}

fn named_annotation_snapshot(source_max: u64) -> LoadedProviderSnapshot {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};
    let name = |max| {
        AttributeType::string_member(
            crate::schema::StringIdentityLayer::new(
                Some(TypeIdentity::from_schema_type("aws", "iam.Role", "Name")),
                None,
                Some((Some(1), Some(64))),
                None,
            ),
            crate::schema::StringMemberConstraints::new(None, Some((Some(1), Some(max)))),
        )
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        ResourceSchema::new("iam.Role")
            .attribute(AttributeSchema::required("role_name", name(64)).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "aws",
        ResourceSchema::new("NameSource")
            .attribute(AttributeSchema::provider_populated("name", name(source_max)).build())
            .validate()
            .unwrap(),
    );
    provider_snapshot(schemas)
}

#[test]
fn wider_same_identity_name_reference_discharge_reports_the_literal_error_once() {
    use crate::binding_index::{BindingIndex, BindingValueSource, ResolvedBindings};
    use crate::resource::knowledge::KnowledgeContext;
    use crate::resource::{ConcreteValue, Value};
    use crate::validation::admission::{Candidate, admit};
    let snapshot = named_annotation_snapshot(128);
    let parsed = crate::parser::parse_file(
        "provider aws {}\nlet source = aws.NameSource {}\nlet role = aws.iam.Role { role_name = source.name }\n",
        std::path::Path::new("main.crn"), snapshot.provider_context().as_provider_context(),
    ).unwrap().resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(
        &parsed,
        snapshot.schema_registry(),
        snapshot.provider_context().as_provider_context(),
    );
    let destination = selected_destination::<kind::ResourceAttribute>(
        snapshot
            .schema_registry()
            .get(
                &parsed.resources[1].id.provider,
                &parsed.resources[1].id.resource_type,
                crate::schema::SchemaKind::Resource,
            )
            .unwrap(),
        &parsed.resources[1].id,
        "role_name",
        snapshot.provider_context().as_provider_context(),
    );
    crate::validation::deferred_types::with_session_for_test(
        snapshot.provider_context().as_provider_context(),
        |session| {
            let reference = Value::<crate::resource::NoPlaceholder>::Deferred(
                crate::resource::DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::new("source", "name")
                        .resolve_root_for_test(),
                },
            );
            let (candidate, boundary) = Candidate::validation_classified(
                reference
                    .knowledge(&KnowledgeContext::validation())
                    .unwrap()
                    .into_classification(),
                Some(&bindings),
                session,
                crate::test_utils::no_conversion_proof(),
            );
            let pending = admit(&destination, candidate, &boundary)
                .unwrap()
                .ready()
                .expect_err("the reference must be pending");
            let literal = Value::Concrete(ConcreteValue::String("A".repeat(65).into()));
            let (candidate, boundary) = Candidate::validation_classified(
                literal
                    .knowledge(&KnowledgeContext::validation())
                    .unwrap()
                    .into_classification(),
                Some(&bindings),
                session,
                crate::test_utils::no_conversion_proof(),
            );
            let literal_error = admit(&destination, candidate, &boundary).unwrap_err();
            let mut resolved = ResolvedBindings::default();
            resolved.set(
                "source",
                HashMap::from([("name".into(), literal)]),
                BindingValueSource::Local,
            );
            let error = pending.discharge(&resolved, session).unwrap_err();
            assert_eq!(error.resolution_error(), literal_error.resolution_error());
            assert_eq!(
                error
                    .resolution_error()
                    .to_string()
                    .matches("role_name:")
                    .count(),
                1
            );
        },
    );
}

fn widening_member_obligation_discharge_parity(decoded: bool) {
    use crate::binding_index::{BindingIndex, BindingValueSource, ResolvedBindings};
    use crate::resource::knowledge::KnowledgeContext;
    use crate::resource::{ConcreteValue, Value};
    use crate::schema::{
        AttributeSchema, AttributeType, PatternMatch, PatternSpec, ResourceSchema,
        StringIdentityLayer, StringMemberConstraints, StructField, TypeIdentity,
        ValidatedConfigSchema,
    };
    use crate::validation::admission::{Candidate, admit};

    let declaration = |ty: AttributeType| {
        if !decoded {
            return ty;
        }
        let schema = ValidatedConfigSchema::validate(std::collections::BTreeMap::from([(
            "value".into(),
            ty,
        )]))
        .unwrap();
        let wire =
            crate::protocol_convert::attribute_type_to_protocol(&schema.parts()["value"]).unwrap();
        let json = serde_json::to_string(&wire).unwrap();
        crate::protocol_convert::protocol_to_attribute_type(&serde_json::from_str(&json).unwrap())
            .unwrap()
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        ResourceSchema::new("iam.Role")
            .attribute(
                AttributeSchema::provider_populated(
                    "arn",
                    declaration(AttributeType::refined_string(
                        Some(TypeIdentity::from_dotted("aws.iam.Role.Arn")),
                        None,
                        None,
                        None,
                    )),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "awscc",
        ResourceSchema::new("scheduler.Schedule")
            .attribute(
                AttributeSchema::required(
                    "target",
                    declaration(AttributeType::struct_(
                        "Target",
                        vec![
                            StructField::required(
                                "role_arn",
                                AttributeType::string_member(
                                    StringIdentityLayer::new(
                                        Some(TypeIdentity::from_dotted("aws.Arn")),
                                        None,
                                        None,
                                        None,
                                    ),
                                    StringMemberConstraints::new(
                                        Some(PatternSpec::new(
                                            "arn:.*:role/[A-Za-z]+",
                                            PatternMatch::Full,
                                        )),
                                        Some((Some(1), Some(1600))),
                                    ),
                                ),
                            )
                            .build(),
                        ],
                    )),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let snapshot = provider_snapshot(schemas);
    let source = "provider aws {}\nprovider awscc {}\nlet producer = aws.iam.Role {}\nlet sink = awscc.scheduler.Schedule { target = { role_arn = producer.arn } }\n";
    let parse = |source: &str| {
        crate::parser::parse_file(
            source,
            Path::new("main.crn"),
            snapshot.provider_context().as_provider_context(),
        )
        .unwrap()
    };
    let parsed = parse(source).resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(
        &parsed,
        snapshot.schema_registry(),
        snapshot.provider_context().as_provider_context(),
    );
    let destination = selected_destination::<kind::ResourceAttribute>(
        snapshot
            .schema_registry()
            .get(
                &parsed.resources[1].id.provider,
                &parsed.resources[1].id.resource_type,
                crate::schema::SchemaKind::Resource,
            )
            .unwrap(),
        &parsed.resources[1].id,
        "target",
        snapshot.provider_context().as_provider_context(),
    );
    let target = |value| {
        Value::Concrete(ConcreteValue::Map(
            [("role_arn".into(), value)].into_iter().collect(),
        ))
    };
    let reference = target(Value::Deferred(
        crate::resource::DeferredValue::ResourceRef {
            path: crate::resource::AccessPath::new("producer", "arn").resolve_root_for_test(),
        },
    ));
    crate::validation::deferred_types::with_session_for_test(
        snapshot.provider_context().as_provider_context(),
        |session| {
            let judge = |value: &Value| {
                let (candidate, boundary) = Candidate::validation_classified(
                    value
                        .knowledge(&KnowledgeContext::validation())
                        .unwrap()
                        .into_classification(),
                    Some(&bindings),
                    session,
                    crate::test_utils::no_conversion_proof(),
                );
                admit(&destination, candidate, &boundary)
            };
            let long = format!("arn:aws:iam::123456789012:role/{}", "A".repeat(1600));
            for (raw, accepted) in [
                ("bad", false),
                (&long, false),
                ("arn:aws:iam::123456789012:role/Good", true),
            ] {
                let pending = judge(&reference).unwrap().ready().unwrap_err();
                assert!(pending.local_obligation().is_some());
                let literal = Value::Concrete(ConcreteValue::String(raw.into()));
                let mut resolved = ResolvedBindings::default();
                resolved.set(
                    "producer",
                    HashMap::from([("arn".into(), literal.clone())]),
                    BindingValueSource::Local,
                );
                if accepted {
                    assert!(pending.discharge(&resolved, session).is_ok());
                } else {
                    let expected = judge(&target(literal)).unwrap_err();
                    let error = pending.discharge(&resolved, session).unwrap_err();
                    assert_eq!(error.resolution_error(), expected.resolution_error());
                    assert_eq!(
                        error
                            .resolution_error()
                            .to_string()
                            .matches("target.role_arn")
                            .count(),
                        1
                    );
                }
            }
        },
    );
}

#[test]
fn widening_member_obligation_has_resolved_value_parity() {
    widening_member_obligation_discharge_parity(false);
}

#[test]
fn decoded_widening_member_obligation_has_resolved_value_parity() {
    widening_member_obligation_discharge_parity(true);
}
