use super::*;
use crate::resource::Resource;
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity};

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}

fn satisfied_rule() -> InputRule {
    InputRule::value(
        InputRuleScope::EveryWrite,
        InputPath::new("target").unwrap(),
        InputRuleCode::new("node.target").unwrap(),
        InputPredicate::Equals(InputLiteral::Bool(true)),
    )
}

#[test]
fn competing_recursive_struct_arms_admit_each_declaration_and_node_once() {
    const DEPTH: usize = 9;
    let local_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let local_counter = Arc::clone(&local_calls);
    let probe = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("Probe"),
        None,
        None,
        Arc::new(move |_| {
            local_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }),
        None,
    );
    let arm = |name: &str, rules| {
        AttributeType::struct_with_rules(
            name,
            vec![
                StructField::required(
                    "kind",
                    AttributeType::refined_string(Some(TypeIdentity::bare(name)), None, None, None),
                )
                .build(),
                StructField::optional("target", AttributeType::bool()).build(),
                StructField::required("probe", probe.clone()).build(),
                StructField::optional("child", AttributeType::ref_("Node")).build(),
            ],
            vec![],
            rules,
        )
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Review")
            .with_def(
                "Node",
                AttributeType::union(vec![arm("A", vec![]), arm("B", vec![satisfied_rule()])]),
            )
            .attribute(AttributeSchema::optional("node", AttributeType::ref_("Node")).build())
            .validate()
            .unwrap(),
    );
    let leaf = || {
        object([
            ("kind", Value::Concrete(ConcreteValue::String("b".into()))),
            (
                "probe",
                Value::Concrete(ConcreteValue::String("local".into())),
            ),
            ("target", Value::Concrete(ConcreteValue::Bool(true))),
        ])
    };
    let mut value = leaf();
    for _ in 1..DEPTH {
        let Value::Concrete(ConcreteValue::Map(mut fields)) = leaf() else {
            unreachable!()
        };
        fields.insert("child".into(), value);
        value = Value::Concrete(ConcreteValue::Map(fields));
    }
    let resource =
        Resource::with_provider("mock", "Review", "example", None).with_attribute("node", value);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let mut catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    catalog.custom_types = crate::parser::ProviderCustomTypes::loaded(
        HashSet::new(),
        Box::new(move |target, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let crate::parser::CustomTypeValidationTarget::Annotation(identity) = target else {
                panic!("provider callback")
            };
            if identity.identity().kind == "A" {
                Err(crate::parser::CustomTypeValidationError::Rejected {
                    owner: "mock".into(),
                    identity: identity.identity().clone(),
                    message: "arm A rejects this kind".into(),
                })
            } else {
                Ok(())
            }
        }),
    );
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let inputs = AuthoredInputs::resource_for_test(&resource).resolved_in_session(session);
        let report = check_authored_inputs(&inputs, &schemas);
        assert!(
            report.findings().is_empty() && report.pending().is_empty(),
            "{report:?}"
        );
    });
    let calls = calls.load(std::sync::atomic::Ordering::SeqCst);
    let local_calls = local_calls.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(calls, DEPTH * 2, "one provider check per arm and node");
    assert_eq!(local_calls, DEPTH * 2, "one local check per arm and node");
}

fn nested_secret_union_fixture(depth: usize) -> (SchemaRegistry, Resource) {
    let node = AttributeType::struct_with_rules(
        "Node",
        vec![
            StructField::optional("target", AttributeType::bool()).build(),
            StructField::optional("child", AttributeType::ref_("Node")).build(),
        ],
        vec![],
        vec![satisfied_rule()],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Review")
            .with_def(
                "Node",
                AttributeType::union(vec![node, AttributeType::bool()]),
            )
            .attribute(AttributeSchema::optional("node", AttributeType::ref_("Node")).build())
            .validate()
            .unwrap(),
    );
    let mut value = object([("target", Value::Concrete(ConcreteValue::Bool(true)))]);
    for _ in 1..depth {
        value = object([
            ("target", Value::Concrete(ConcreteValue::Bool(true))),
            ("child", value.into_secret()),
        ]);
    }
    let resource =
        Resource::with_provider("mock", "Review", "example", None).with_attribute("node", value);
    (schemas, resource)
}

#[test]
fn nested_secret_union_construction_reuses_protected_classification() {
    const DEPTH: usize = 24;
    let (schemas, resource) = nested_secret_union_fixture(DEPTH);
    crate::resource::knowledge::with_classification_budget(DEPTH * 10 + 32, || {
        let inputs = AuthoredInputs::resource_for_test(&resource);
        let report = check_authored_inputs(&inputs, &schemas);
        assert!(
            report.findings().is_empty() && report.pending().is_empty(),
            "{report:?}"
        );
    });
}

#[test]
fn structural_union_fitting_reuses_cached_secret_evidence() {
    let (schemas, resource) = nested_secret_union_fixture(24);
    let schema = schemas.get_for(&resource).unwrap();
    let schema = schema.as_schema();
    let attr = &schema.defs()["Node"];
    let proof = resource.attributes["node"]
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    crate::resource::knowledge::with_classification_budget(0, || {
        assert!(
            crate::schema::fit_value_to_resolved_type_on_path(
                attr,
                &proof,
                schema.defs(),
                crate::schema::FieldAncestry::WRITABLE,
                crate::value::CanonicalizationPurpose::Desired(
                    crate::value::DesiredValueSource::AuthoredDsl
                ),
                &mut Vec::new()
            )
            .is_some()
        );
    });
}
