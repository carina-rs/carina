use super::*;
use crate::resource::{ParsedIgnorePath, ParsedIgnorePathSegment, Resource, State};
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

fn text(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}
fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}
fn registry(schema: ResourceSchema) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema.validate().unwrap());
    schemas
}
fn ignore_key(resource: &mut Resource, attribute: &str, key: &str) {
    resource.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
        ParsedIgnorePathSegment::Attribute(attribute.into()),
        ParsedIgnorePathSegment::MapKey(key.into()),
    ])];
}
fn update(
    resource: Resource,
    previous: Value,
    attribute: &str,
    schemas: &SchemaRegistry,
) -> FinalizedPlan {
    let from = State::existing(
        resource.id.clone(),
        crate::test_support::snapshot_attributes::<crate::resource::NoPlaceholder>(HashMap::from(
            [(attribute.into(), previous)],
        )),
    )
    .with_identifier("existing");
    let to = crate::resource::ResolvedResource::resolve_raw_fixture(resource, schemas).unwrap();
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from),
        identifier: crate::provider::AppliedIdentifier::new("existing").unwrap(),
        to,
        changed_attributes: vec![attribute.into()],
    });
    FinalizedPlan::new(plan)
}

#[test]
fn ignored_default_key_removed_from_exact_patch_is_not_reintroduced() {
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .input_rule(InputRule::new(
                InputRuleScope::EveryWrite,
                InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
                InputRuleCode::new("tags.reserved").unwrap(),
                InputPredicate::NoPrefix(crate::schema::NonEmptyString::new("aws:").unwrap()),
            )),
    );
    let mut desired = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "tags",
        object([("aws:x", text("default")), ("safe", text("new"))]),
    );
    ignore_key(&mut desired, "tags", "aws:x");
    let config = crate::parser::ProviderConfig {
        name: "mock".into(),
        attributes: Default::default(),
        default_tags: IndexMap::from([("aws:x".into(), text("default"))]),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: Default::default(),
        binding: None,
    };
    let types = InputTypeContext::<ModuleSource>::empty()
        .with_default_tags(&[config], &schemas, [&desired.id])
        .unwrap();
    let finalized = update(desired, object([("safe", text("old"))]), "tags", &schemas);
    let result = finalized.check_inputs_with_types(&schemas, &HashMap::new(), &types);
    assert!(
        result.is_ok(),
        "the exact patch contains only the safe key: {:?}",
        result.err().map(|error| error.errors().to_vec())
    );
}

#[test]
fn ignored_union_instance_is_withheld_before_branch_selection() {
    let local = AttributeType::struct_with_rules(
        "Object",
        vec![StructField::optional("target", AttributeType::string()).build()],
        vec![],
        vec![InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new("target").unwrap(),
            InputRuleCode::new("object.target").unwrap(),
            InputPredicate::Present,
        )],
    );
    let schemas = registry(
        ResourceSchema::new("Review").attribute(
            AttributeSchema::optional(
                "objects",
                AttributeType::map(AttributeType::union(vec![local, AttributeType::bool()])),
            )
            .build(),
        ),
    );
    let mut desired = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "objects",
        object([
            ("selected", object([])),
            ("safe", object([("target", text("new"))])),
        ]),
    );
    ignore_key(&mut desired, "objects", "selected");
    let finalized = update(
        desired,
        object([
            ("selected", Value::Concrete(ConcreteValue::Bool(false))),
            ("safe", object([("target", text("old"))])),
        ]),
        "objects",
        &schemas,
    );
    let checked = finalized.check_inputs(&schemas, &HashMap::new()).unwrap();
    assert_eq!(
        checked.pending_rows(0).len(),
        1,
        "an ignored observed Bool must not suppress possible Struct rules"
    );
}

#[test]
fn erased_root_expression_requires_carried_correspondence() {
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .input_rule(InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("name.suffix").unwrap(),
                InputPredicate::NoSuffix(crate::schema::NonEmptyString::new(".fifo").unwrap()),
            )),
    );
    let authored = Resource::with_provider("mock", "Review", "example", None)
        .with_attribute("name", text("old.fifo"));
    let types = InputTypeContext::from_sources(
        &[crate::effect::deps::UnresolvedResource::from_pre_resolve(
            authored.clone(),
        )],
        &[],
        std::iter::empty(),
        &[],
        &schemas,
    );
    let normalized = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "name",
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::PendingReRead(
                crate::resource::PendingReRead::not_found(),
            ),
        )),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        crate::resource::ResolvedResource::resolve_raw_fixture(normalized, &schemas).unwrap(),
    ));
    let checked = FinalizedPlan::new(plan)
        .check_inputs_with_types(&schemas, &HashMap::new(), &types)
        .expect("resource identity and attribute spelling do not prove expression correspondence");
    assert_eq!(checked.pending_rows(0).len(), 1);
}

#[test]
fn ignored_missing_child_is_withheld_for_the_walker_and_predicate() {
    let local = AttributeType::struct_with_rules(
        "Child",
        vec![StructField::optional("target", AttributeType::string()).build()],
        vec![],
        vec![InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new("target").unwrap(),
            InputRuleCode::new("child.target").unwrap(),
            InputPredicate::Present,
        )],
    );
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::struct_(
                        "Settings",
                        vec![
                            StructField::optional("child", local).build(),
                            StructField::optional("changed", AttributeType::string()).build(),
                        ],
                    ),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("fallback", AttributeType::string()).build())
            .input_rule(
                InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("fallback").unwrap(),
                    InputRuleCode::new("fallback.present").unwrap(),
                    InputPredicate::Present,
                )
                .with_condition(InputCondition::Not(Box::new(
                    InputCondition::Present(InputPath::new("settings.child").unwrap()),
                ))),
            ),
    );
    let mut desired = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "settings",
        object([("child", object([])), ("changed", text("new"))]),
    );
    desired.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
        ParsedIgnorePathSegment::Attribute("settings".into()),
        ParsedIgnorePathSegment::Attribute("child".into()),
    ])];
    let finalized = update(
        desired,
        object([("changed", text("old"))]),
        "settings",
        &schemas,
    );
    let checked = finalized
        .check_inputs(&schemas, &HashMap::new())
        .expect("ignored child membership is unavailable");
    assert_eq!(
        checked.pending_rows(0).len(),
        2,
        "both the guard and possible child declaration must retain an obligation: {:?}",
        checked.pending_rows(0)
    );
}
