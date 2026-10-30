use super::{create_plan_for_tests as create_plan, *};

use crate::resource::ConcreteValue;
use indexmap::IndexMap;

fn existing_state(id: ResourceId, attributes: HashMap<String, Value>) -> State {
    let identifier = id
        .identity_str()
        .expect("fixture resource identity must be resolved")
        .to_string();
    State::existing(id, attributes).with_identifier(identifier)
}

fn replacement_metadata<'a>(
    plan: &'a Plan,
    id: &ResourceId,
) -> &'a crate::plan::ReplaceDisplayMetadata {
    plan.replace_display
        .iter()
        .find(|metadata| plan.effects()[metadata.create_idx].resource_id() == id)
        .unwrap_or_else(|| panic!("replacement metadata for {id} not found"))
}

#[test]
fn keyed_partial_update_release_requires_the_element_key_to_be_authored() {
    let mut fields = IndexMap::new();
    fields.insert(
        "key".to_string(),
        Value::Concrete(ConcreteValue::String("b".into())),
    );
    fields.insert(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String("2".into())),
    );
    let saved = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(fields),
    )]));
    let value_only = crate::explicit::ExplicitFields::ListElements {
        elements: vec![crate::explicit::ExplicitFields::Struct {
            children: HashMap::from([("value".to_string(), crate::explicit::ExplicitFields::Leaf)]),
        }],
    };

    assert!(authored_saved_keys(&saved, &value_only, "key").is_empty());

    let keyed = crate::explicit::ExplicitFields::ListElements {
        elements: vec![crate::explicit::ExplicitFields::Struct {
            children: HashMap::from([
                ("key".to_string(), crate::explicit::ExplicitFields::Leaf),
                ("value".to_string(), crate::explicit::ExplicitFields::Leaf),
            ]),
        }],
    };
    assert_eq!(
        authored_saved_keys(&saved, &keyed, "key"),
        std::collections::BTreeSet::from(["b".to_string()])
    );
}

#[test]
fn diff_create_when_not_exists() {
    let desired = Resource::new("bucket", "test");
    let current = State::not_found(ResourceId::with_identity("bucket", "test"));

    let result = diff_test(&desired, &current, None, None, None);
    assert!(matches!(result, Diff::Create(_)));
}

#[test]
fn diff_no_change_when_same() {
    let desired = Resource::new("bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );

    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    let current = existing_state(ResourceId::with_identity("bucket", "test"), attrs);

    let result = diff_test(&desired, &current, None, None, None);
    assert!(matches!(result, Diff::NoChange(_)));
}

#[test]
fn diff_update_when_different() {
    let desired = Resource::new("bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("us-east-1".into())),
    );

    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    let current = existing_state(ResourceId::with_identity("bucket", "test"), attrs);

    let result = diff_test(&desired, &current, None, None, None);
    match result {
        Diff::Update {
            changed_attributes, ..
        } => {
            assert!(changed_attributes.contains(&"region".to_string()));
        }
        _ => panic!("Expected Update"),
    }
}

#[test]
fn diff_reports_string_list_vs_generic_string_list_shape_mismatch() {
    let mut statement = IndexMap::new();
    statement.insert(
        "action".to_string(),
        Value::Concrete(ConcreteValue::StringList(vec!["s3:GetObject".into()])),
    );
    let desired_policy = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "statement".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(statement),
        )])),
    )])));
    let desired = Resource::new("awscc.ec2.VpcEndpoint", "vpce")
        .with_attribute("policy_document", desired_policy);

    let mut statement = IndexMap::new();
    statement.insert(
        "action".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("s3:GetObject".into()),
        )])),
    );
    let current_policy = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "statement".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(statement),
        )])),
    )])));
    let current = existing_state(
        desired.id.clone(),
        HashMap::from([("policy_document".to_string(), current_policy)]),
    );

    let result = diff_test(&desired, &current, None, None, None);

    match result {
        Diff::Update {
            changed_attributes, ..
        } => assert_eq!(changed_attributes, vec!["policy_document".to_string()]),
        other => panic!("expected Update for non-canonical generic List shape, got {other:?}"),
    }
}

#[derive(Clone, Copy)]
enum PolicyValueListOrdering {
    Ordered,
    Unordered,
}

enum SavedPolicyAttrs {
    Include,
    Omit,
}

fn plan_for_reordered_iam_policy_value_lists(
    ordering: PolicyValueListOrdering,
    saved_policy_attrs: SavedPolicyAttrs,
) -> Plan {
    use crate::schema::{AttributeSchema, ResourceSchema, StructField};
    use crate::value::canonicalize_resources_with_schemas;

    let string_or_list = || {
        let list = match ordering {
            PolicyValueListOrdering::Ordered => AttributeType::list(AttributeType::string()),
            PolicyValueListOrdering::Unordered => {
                AttributeType::unordered_list(AttributeType::string())
            }
        };
        AttributeType::union(vec![AttributeType::string(), list])
    };
    let principal = AttributeType::struct_(
        "Principal",
        vec![StructField::optional("aws", string_or_list()).build()],
    );
    let statement = AttributeType::struct_(
        "Statement",
        vec![
            StructField::optional("action", string_or_list()).build(),
            StructField::optional("principal", principal).build(),
        ],
    );
    let policy_document = AttributeType::struct_(
        "PolicyDocument",
        vec![StructField::optional("statement", AttributeType::list(statement)).build()],
    );
    let schema = ResourceSchema::new("iam.Policy")
        .attribute(AttributeSchema::optional("policy_document", policy_document).build());
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );

    let string_list = |items: &[&str]| {
        Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|item| Value::Concrete(ConcreteValue::String((*item).into())))
                .collect(),
        ))
    };
    let policy = |actions: &[&str], principals: &[&str]| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "statement".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([
                    ("action".to_string(), string_list(actions)),
                    (
                        "principal".to_string(),
                        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                            "aws".to_string(),
                            string_list(principals),
                        )]))),
                    ),
                ])),
            )])),
        )])))
    };

    let desired_policy = policy(
        &["s3:GetObject", "s3:ListBucket"],
        &[
            "arn:aws:iam::111122223333:root",
            "arn:aws:iam::444455556666:root",
        ],
    );
    let mut resources = vec![
        Resource::with_provider("aws", "iam.Policy", "policy", None)
            .with_attribute("policy_document", desired_policy.clone()),
    ];
    canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();

    let current_policy = policy(
        &["s3:ListBucket", "s3:GetObject"],
        &[
            "arn:aws:iam::444455556666:root",
            "arn:aws:iam::111122223333:root",
        ],
    );
    let id = resources[0].id.clone();
    let saved_attrs = match saved_policy_attrs {
        SavedPolicyAttrs::Include => {
            crate::provider::RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
                id.clone(),
                HashMap::from([("policy_document".to_string(), desired_policy)]),
            )]))
            .lift(&registry)
        }
        SavedPolicyAttrs::Omit => empty_lifted_saved_attrs(),
    };
    let current_states = HashMap::from([(
        id.clone(),
        existing_state(
            id,
            HashMap::from([("policy_document".to_string(), current_policy)]),
        ),
    )]);
    let current = crate::resource::into_plan_input_current_map_from_states(
        current_states,
        &registry,
        &resources,
    );

    create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &current,
        &HashMap::new(),
        &registry,
        &saved_attrs,
        &HashMap::new(),
        &Orphans::new(),
        &[],
    )
}

#[test]
fn plan_ignores_reordered_unordered_iam_policy_value_lists() {
    let plan = plan_for_reordered_iam_policy_value_lists(
        PolicyValueListOrdering::Unordered,
        SavedPolicyAttrs::Omit,
    );

    assert!(
        plan.effects().is_empty(),
        "reordering unordered IAM action and principal lists must not produce an Update; got {:?}",
        plan.effects()
    );
}

#[test]
fn plan_ignores_reordered_unordered_iam_policy_value_lists_with_saved_attrs() {
    let plan = plan_for_reordered_iam_policy_value_lists(
        PolicyValueListOrdering::Unordered,
        SavedPolicyAttrs::Include,
    );

    assert!(
        plan.effects().is_empty(),
        "reordering unordered IAM action and principal lists must not produce an Update when the \
         saved policy preserves the declared order; got {:?}",
        plan.effects()
    );
}

#[test]
fn plan_updates_reordered_ordered_iam_policy_value_lists() {
    let plan = plan_for_reordered_iam_policy_value_lists(
        PolicyValueListOrdering::Ordered,
        SavedPolicyAttrs::Omit,
    );

    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(plan.effects()[0], Effect::Update { .. }),
        "reordering ordered IAM action and principal lists must produce an Update; got {:?}",
        plan.effects()
    );
}

#[test]
fn create_plan_from_resources() {
    let resources = vec![
        Resource::new("bucket", "new-bucket"),
        Resource::new("bucket", "existing-bucket")
            .with_attribute("versioning", Value::Concrete(ConcreteValue::Bool(true))),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "versioning".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    current_states.insert(
        ResourceId::with_identity("bucket", "existing-bucket"),
        existing_state(
            ResourceId::with_identity("bucket", "existing-bucket"),
            attrs,
        ),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    assert!(matches!(plan.effects()[0], Effect::Create(_)));
    assert!(matches!(plan.effects()[1], Effect::Update { .. }));
}

#[test]
fn create_plan_with_read_only_resource() {
    // carina#3181: data sources are a distinct typestate fed to
    // `create_plan` via its own `&[DataSource]` argument.
    let data_sources = vec![
        crate::resource::DataSource::new("bucket", "existing-bucket").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("existing-bucket".into())),
        ),
    ];
    let resources = vec![Resource::new("bucket", "new-bucket")];

    let current_states = HashMap::new();
    let plan = create_plan(
        &resources,
        &data_sources,
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    // Should have 2 effects: Read for data source, Create for new bucket
    assert_eq!(plan.effects().len(), 2);
    assert!(matches!(plan.effects()[0], Effect::Read { .. }));
    assert!(matches!(plan.effects()[1], Effect::Create(_)));
}

#[test]
fn diff_update_when_list_of_maps_changed() {
    let mut ingress1 = IndexMap::new();
    ingress1.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );
    ingress1.insert(
        "from_port".to_string(),
        Value::Concrete(ConcreteValue::Int(80)),
    );
    ingress1.insert(
        "to_port".to_string(),
        Value::Concrete(ConcreteValue::Int(80)),
    );

    let mut ingress2 = IndexMap::new();
    ingress2.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );
    ingress2.insert(
        "from_port".to_string(),
        Value::Concrete(ConcreteValue::Int(443)),
    );
    ingress2.insert(
        "to_port".to_string(),
        Value::Concrete(ConcreteValue::Int(443)),
    );

    let desired = Resource::new("ec2_security_group", "test-sg").with_attribute(
        "security_group_ingress",
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(ingress1.clone())),
            Value::Concrete(ConcreteValue::Map(ingress2)),
        ])),
    );

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "security_group_ingress".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(ingress1),
        )])),
    );
    let current = existing_state(
        ResourceId::with_identity("ec2_security_group", "test-sg"),
        current_attrs,
    );

    let result = diff_test(&desired, &current, None, None, None);
    match result {
        Diff::Update {
            changed_attributes, ..
        } => {
            assert!(
                changed_attributes.contains(&"security_group_ingress".to_string()),
                "Should detect security_group_ingress as changed"
            );
        }
        _ => panic!("Expected Update when list-of-maps changed"),
    }
}

#[test]
fn create_plan_detects_typed_orphan_for_deletion() {
    let desired = vec![Resource::new("bucket", "keep-this")];

    let mut current_states = HashMap::new();
    // "keep-this" exists and matches
    current_states.insert(
        ResourceId::with_identity("bucket", "keep-this"),
        existing_state(
            ResourceId::with_identity("bucket", "keep-this"),
            HashMap::new(),
        ),
    );
    // "orphaned-bucket" exists in state but not in desired
    let mut orphan_attrs = HashMap::new();
    orphan_attrs.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("orphaned-bucket".into())),
    );
    let orphan_id = ResourceId::with_identity("bucket", "orphaned-bucket");
    let orphans = Orphans::from([Orphan::from_state(
        existing_state(orphan_id, orphan_attrs),
        None,
        Directives::default(),
    )]);

    let plan = create_plan(
        &desired,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    // Should have 1 effect: Delete for orphaned-bucket
    // (keep-this has NoChange, so no effect)
    let delete_effects: Vec<_> = plan
        .effects()
        .iter()
        .filter(|e| matches!(e, Effect::Delete { .. }))
        .collect();
    assert_eq!(
        delete_effects.len(),
        1,
        "Expected 1 Delete effect for orphaned resource, got {}. Effects: {:?}",
        delete_effects.len(),
        plan.effects()
    );
}

#[test]
fn read_only_resource_always_generates_read_effect() {
    // Even if the resource "exists", read-only resources (data sources)
    // should only generate a Read effect. carina#3181: data sources are
    // a distinct typestate.
    let data_sources = vec![
        crate::resource::DataSource::new("bucket", "existing-bucket").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("existing-bucket".into())),
        ),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("existing-bucket".into())),
    );
    current_states.insert(
        ResourceId::with_identity("bucket", "existing-bucket"),
        existing_state(
            ResourceId::with_identity("bucket", "existing-bucket"),
            attrs,
        ),
    );

    let plan = create_plan(
        &[],
        &data_sources,
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    // Should still have Read effect, not NoChange
    assert_eq!(plan.effects().len(), 1);
    assert!(matches!(plan.effects()[0], Effect::Read { .. }));
}

/// Regression test for issue #146: when neither desired nor current state has
/// a "name" attribute (the normal case for AWSCC resources after PR #151),
/// the differ should report NoChange, not a false update.
#[test]
fn no_false_update_without_unique_name_attribute() {
    // Simulate AWSCC resource: desired has cidr_block but no "name"
    let desired = Resource::new("ec2.Vpc", "vpc").with_attribute(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );

    // Current state from provider read also has cidr_block but no "name"
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    let current = existing_state(ResourceId::with_identity("ec2.Vpc", "vpc"), attrs);

    let result = diff_test(&desired, &current, None, None, None);
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Expected NoChange when neither side has 'name', got {:?}",
        result
    );
}

#[test]
fn replace_when_create_only_attr_changed() {
    use crate::schema::{AttributeSchema, AttributeType};

    let resources = vec![Resource::new("ec2.Vpc", "my-vpc").with_attribute(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
    )];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    current_states.insert(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        existing_state(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    // Build schema with cidr_block marked as create-only
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (crate::schema::ResourceSchema::new("ec2.Vpc").attribute(
            AttributeSchema::optional("cidr_block", AttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    let metadata = replacement_metadata(&plan, &ResourceId::with_identity("ec2.Vpc", "my-vpc"));
    assert_eq!(
        &metadata.changed_create_only[..],
        &["cidr_block".to_string()]
    );
}

fn create_only_nested_write_only_fixture(
    password: &str,
) -> (Resource, State, crate::schema::ResourceSchema) {
    use crate::resource::DeferredValue;
    use crate::schema::{AttributeSchema, AttributeType, StructField};

    let schema = crate::schema::ResourceSchema::new("test.Credential").attribute(
        AttributeSchema::optional(
            "creds",
            AttributeType::struct_(
                "Credentials",
                vec![
                    StructField::optional("username", AttributeType::string()).build(),
                    StructField::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                ],
            ),
        )
        .create_only()
        .build(),
    );
    let desired_creds = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "username".to_string(),
            Value::Concrete(ConcreteValue::String("alice".into())),
        ),
        (
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(password.into()),
            )))),
        ),
    ])));
    let desired =
        Resource::new("test.Credential", "example").with_attribute("creds", desired_creds);
    let current = existing_state(
        desired.id.clone(),
        HashMap::from([(
            "creds".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "username".to_string(),
                Value::Concrete(ConcreteValue::String("alice".into())),
            )]))),
        )]),
    );
    (desired, current, schema)
}

#[test]
fn create_only_nested_write_only_without_record_plans_no_change() {
    let (desired, current, schema) = create_only_nested_write_only_fixture("accepted-secret");
    let id = desired.id.clone();
    let mut schemas = SchemaRegistry::new();
    schemas.insert("", schema.validate().expect("schema fixture must be valid"));
    let saved = crate::provider::RawSavedAttrs::from_persisted(
        HashMap::new(),
        HashMap::from([(
            id.clone(),
            crate::provider::NestedWriteOnlyRecord::default(),
        )]),
    )
    .lift(&schemas);
    let current_states = HashMap::from([(id.clone(), current)]);

    let plan = create_plan(
        std::slice::from_ref(&desired),
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states,
            &schemas,
            std::slice::from_ref(&desired),
        ),
        &HashMap::new(),
        &schemas,
        &saved,
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(
        plan.effects().is_empty(),
        "an empty migrated/imported provenance record must not replace a create-only resource: {:?}",
        plan.effects()
    );
}

#[test]
fn create_only_nested_write_only_changed_record_plans_replace() {
    let (previous_desired, _previous_current, schema) =
        create_only_nested_write_only_fixture("old-secret");
    let (desired, current, _equivalent_schema) =
        create_only_nested_write_only_fixture("new-secret");
    let id = desired.id.clone();
    let mut record = crate::provider::NestedWriteOnlyRecord::default();
    record
        .record_sent(&schema, ["creds"], &previous_desired.attributes, None)
        .expect("fixture credentials are persistable");
    let mut schemas = SchemaRegistry::new();
    schemas.insert("", schema.validate().expect("schema fixture must be valid"));
    let saved = crate::provider::RawSavedAttrs::from_persisted(
        HashMap::new(),
        HashMap::from([(id.clone(), record)]),
    )
    .lift(&schemas);
    let current_states = HashMap::from([(id.clone(), current)]);

    let plan = create_plan(
        std::slice::from_ref(&desired),
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states,
            &schemas,
            std::slice::from_ref(&desired),
        ),
        &HashMap::new(),
        &schemas,
        &saved,
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    let metadata = replacement_metadata(&plan, &id);
    assert_eq!(&metadata.changed_create_only[..], &["creds".to_string()]);
}

#[test]
fn normal_update_when_non_create_only_attr_changed() {
    use crate::schema::{AttributeSchema, AttributeType};

    let resources = vec![Resource::new("ec2.Vpc", "my-vpc").with_attribute(
        "enable_dns_support",
        Value::Concrete(ConcreteValue::Bool(true)),
    )];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "enable_dns_support".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    current_states.insert(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        existing_state(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    // cidr_block is create-only, but enable_dns_support is not
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (crate::schema::ResourceSchema::new("ec2.Vpc")
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("enable_dns_support", AttributeType::bool()).build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(plan.effects()[0], Effect::Update { .. }),
        "Expected Update, got {:?}",
        plan.effects()[0]
    );
}

#[test]
fn replace_when_mix_of_create_only_and_normal_attrs_changed() {
    use crate::schema::{AttributeSchema, AttributeType};

    let resources = vec![
        Resource::new("ec2.Vpc", "my-vpc")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            )
            .with_attribute(
                "enable_dns_support",
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    attrs.insert(
        "enable_dns_support".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    current_states.insert(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        existing_state(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (crate::schema::ResourceSchema::new("ec2.Vpc")
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("enable_dns_support", AttributeType::bool()).build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    let metadata = replacement_metadata(&plan, &ResourceId::with_identity("ec2.Vpc", "my-vpc"));
    assert_eq!(
        &metadata.changed_create_only[..],
        &["cidr_block".to_string()]
    );
}

#[test]
fn replace_carries_create_before_destroy_directives() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("ec2.Vpc", "my-vpc")
        .with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("my-vpc".into())),
        )
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
        );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("my-vpc".into())),
    );
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    current_states.insert(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        existing_state(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (crate::schema::ResourceSchema::new("ec2.Vpc")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("name"))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    let metadata = replacement_metadata(&plan, &ResourceId::with_identity("ec2.Vpc", "my-vpc"));
    assert!(metadata.create_before_destroy);
    assert_eq!(
        &metadata.changed_create_only[..],
        &["cidr_block".to_string()]
    );
}

#[test]
fn diff_no_change_when_list_of_maps_reordered() {
    let mut rule1 = IndexMap::new();
    rule1.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );
    rule1.insert(
        "from_port".to_string(),
        Value::Concrete(ConcreteValue::Int(80)),
    );
    rule1.insert(
        "to_port".to_string(),
        Value::Concrete(ConcreteValue::Int(80)),
    );

    let mut rule2 = IndexMap::new();
    rule2.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );
    rule2.insert(
        "from_port".to_string(),
        Value::Concrete(ConcreteValue::Int(443)),
    );
    rule2.insert(
        "to_port".to_string(),
        Value::Concrete(ConcreteValue::Int(443)),
    );

    // Desired: [rule1, rule2]
    let desired = Resource::new("ec2_security_group", "test-sg").with_attribute(
        "security_group_egress",
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(rule1.clone())),
            Value::Concrete(ConcreteValue::Map(rule2.clone())),
        ])),
    );

    // Current (from AWS): [rule2, rule1] — same content, different order
    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "security_group_egress".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(rule2)),
            Value::Concrete(ConcreteValue::Map(rule1)),
        ])),
    );
    let current = existing_state(
        ResourceId::with_identity("ec2_security_group", "test-sg"),
        current_attrs,
    );

    let result = diff_test(&desired, &current, None, None, None);
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Expected NoChange when list-of-maps has same content in different order, got {:?}",
        result
    );
}

#[test]
fn replace_with_provider_prefixed_schema_key() {
    use crate::schema::{AttributeSchema, AttributeType};

    // In production, schemas are keyed by "awscc.ec2.Vpc" but resource_type is "ec2.Vpc"
    // The resource must have provider set so the generic lookup works
    let resources = vec![
        Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None).with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
        ),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    current_states.insert(
        ResourceId::with_provider_identity("awscc", "ec2.Vpc", "my-vpc", None),
        existing_state(
            ResourceId::with_provider_identity("awscc", "ec2.Vpc", "my-vpc", None),
            attrs,
        ),
    );

    // Schema registered under provider "awscc"
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (crate::schema::ResourceSchema::new("ec2.Vpc").attribute(
            AttributeSchema::optional("cidr_block", AttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    assert_eq!(plan.replace_display.len(), 1);
    replacement_metadata(
        &plan,
        &ResourceId::with_provider_identity("awscc", "ec2.Vpc", "my-vpc", None),
    );
}

/// Regression test for issue #172: desired has 2 fields in a struct,
/// current (AWS) returns 3, saved state has 3. Should be NoChange.
#[test]
fn diff_no_change_when_struct_has_extra_fields_with_saved() {
    let desired = Resource::new("ec2.Subnet", "test-subnet").with_attribute(
        "private_dns_name_options_on_launch",
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::Concrete(ConcreteValue::String("ip-name".into())),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
        ]))),
    );

    let current_attrs = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::Concrete(ConcreteValue::String("ip-name".into())),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(false)),
            ),
        ]))),
    )]);
    let current = existing_state(
        ResourceId::with_identity("ec2.Subnet", "test-subnet"),
        current_attrs,
    );

    let saved = IndexMap::from([
        (
            "hostname_type".to_string(),
            Value::Concrete(ConcreteValue::String("ip-name".into())),
        ),
        (
            "enable_resource_name_dns_a_record".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "enable_resource_name_dns_aaaa_record".to_string(),
            Value::Concrete(ConcreteValue::Bool(false)),
        ),
    ]);
    let saved_map = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Concrete(ConcreteValue::Map(saved)),
    )]);

    let result = diff_test(&desired, &current, Some(&saved_map), None, None);
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Expected NoChange when saved fills extra struct fields, got {:?}",
        result
    );
}

/// When an unmanaged field drifts externally, diff should still detect the change.
#[test]
fn diff_detects_drift_on_unmanaged_field() {
    let desired = Resource::new("ec2.Subnet", "test-subnet").with_attribute(
        "private_dns_name_options_on_launch",
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::Concrete(ConcreteValue::String("ip-name".into())),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
        ]))),
    );

    // AWS returns aaaa_record: true (drifted from saved false)
    let current_attrs = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::Concrete(ConcreteValue::String("ip-name".into())),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
        ]))),
    )]);
    let current = existing_state(
        ResourceId::with_identity("ec2.Subnet", "test-subnet"),
        current_attrs,
    );

    let saved = IndexMap::from([
        (
            "hostname_type".to_string(),
            Value::Concrete(ConcreteValue::String("ip-name".into())),
        ),
        (
            "enable_resource_name_dns_a_record".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "enable_resource_name_dns_aaaa_record".to_string(),
            Value::Concrete(ConcreteValue::Bool(false)),
        ),
    ]);
    let saved_map = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Concrete(ConcreteValue::Map(saved)),
    )]);

    let result = diff_test(&desired, &current, Some(&saved_map), None, None);
    assert!(
        matches!(result, Diff::Update { .. }),
        "Expected Update when unmanaged field drifted, got {:?}",
        result
    );
}

/// Regression test for issue #350: desired is Map (from `= {}` syntax),
/// but current and saved are List([Map]) (from provider read path).
/// After merge + semantic comparison, this should be NoChange.
#[test]
fn diff_no_change_when_bare_struct_with_extra_fields() {
    let desired = Resource::new("ec2.Subnet", "test-subnet").with_attribute(
        "private_dns_name_options_on_launch",
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::Concrete(ConcreteValue::String("ip-name".into())),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
        ]))),
    );

    // Provider read returns Map with extra fields not in desired
    let current_attrs = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::Concrete(ConcreteValue::String("ip-name".into())),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(false)),
            ),
        ]))),
    )]);
    let current = existing_state(
        ResourceId::with_identity("ec2.Subnet", "test-subnet"),
        current_attrs,
    );

    // Saved state has the same Map with extra fields
    let saved_map = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::Concrete(ConcreteValue::String("ip-name".into())),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Concrete(ConcreteValue::Bool(false)),
            ),
        ]))),
    )]);

    let result = diff_test(&desired, &current, Some(&saved_map), None, None);
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Expected NoChange for bare struct with extra fields from saved, got {:?}",
        result
    );
}

/// When saved state is None, behavior should be unchanged from before.
#[test]
fn diff_works_without_saved_state() {
    // Desired has 2 fields, current has 3 (extra field). Without saved state,
    // this should still be NoChange because find_changed_attributes only checks
    // desired keys against current (not the other direction).
    let desired = Resource::new("ec2.Subnet", "test-subnet").with_attribute(
        "opts",
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            ("b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
        ]))),
    );

    let current_attrs = HashMap::from([(
        "opts".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            ("b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
            ("c".to_string(), Value::Concrete(ConcreteValue::Int(3))),
        ]))),
    )]);
    let current = existing_state(
        ResourceId::with_identity("ec2.Subnet", "test-subnet"),
        current_attrs,
    );

    // Without saved state, the map comparison uses semantically_equal which
    // checks both key count AND values. Since desired map has 2 keys and current
    // has 3, this will show as Update (which is the existing behavior).
    let result = diff_test(&desired, &current, None, None, None);
    assert!(
        matches!(result, Diff::Update { .. }),
        "Expected Update without saved state when maps have different sizes, got {:?}",
        result
    );
}

#[test]
fn orphan_delete_preserves_typed_binding_and_dependencies() {
    // Orphan resources (in state but not in desired) carry host metadata
    // separately from provider-facing state attributes.
    let desired = vec![];

    let orphan_id = ResourceId::with_identity("subnet", "my-subnet");
    let orphan_state = existing_state(orphan_id.clone(), HashMap::new())
        .with_dependency_bindings(std::collections::BTreeSet::from(["my_vpc".to_string()]));
    let orphans = crate::differ::Orphans::from([Orphan::from_state(
        orphan_state,
        Some("my_subnet".to_string()),
        crate::resource::Directives::default(),
    )]);

    let plan = create_plan(
        &desired,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            HashMap::new(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    let delete_effects: Vec<_> = plan
        .effects()
        .iter()
        .filter(|e| matches!(e, Effect::Delete { .. }))
        .collect();
    assert_eq!(delete_effects.len(), 1);

    match &delete_effects[0] {
        Effect::Delete {
            binding,
            dependencies,
            ..
        } => {
            assert_eq!(
                binding.as_deref(),
                Some("my_subnet"),
                "Orphan Delete should preserve the typed binding"
            );
            assert!(
                dependencies.contains("my_vpc"),
                "Orphan Delete should preserve typed dependency bindings"
            );
        }
        _ => unreachable!(),
    }
}

/// Regression test for issue #1439: security_group_egress struct list should
/// be idempotent after apply -> plan-verify when saved state contains
/// namespaced enum values that differ from alias-resolved values.
///
/// Scenario: After apply, the state file stores ip_protocol values in
/// namespaced format (e.g., "awscc.ec2.SecurityGroup.IpProtocol.tcp"),
/// while plan-time alias resolution converts "all" to "-1".
/// The differ should see no changes when comparing merged-desired vs current.
#[test]
fn diff_no_change_for_struct_list_with_saved_state_egress_rules() {
    use crate::schema::{ResourceSchema, StructField};

    // Build a schema that matches ec2.security_group's security_group_egress attribute
    let egress_struct = AttributeType::struct_(
        "Egress".to_string(),
        vec![
            StructField::optional("cidr_ip", AttributeType::string()).build(),
            StructField::optional("description", AttributeType::string()).build(),
            StructField::optional("from_port", AttributeType::int()).build(),
            StructField::optional(
                "ip_protocol",
                AttributeType::enum_(
                    crate::schema::enum_identity("IpProtocol", Some("awscc.ec2.SecurityGroup")),
                    Some(vec![
                        "tcp".to_string(),
                        "udp".to_string(),
                        "icmp".to_string(),
                        "-1".to_string(),
                        "all".to_string(),
                    ]),
                    vec![("-1".to_string(), "all".to_string())],
                    None,
                    None,
                ),
            )
            .build(),
            StructField::optional("to_port", AttributeType::int()).build(),
        ],
    );
    let schema = ResourceSchema::new("awscc.ec2.SecurityGroup").attribute(
        crate::schema::AttributeSchema::optional(
            "security_group_egress",
            AttributeType::unordered_list(egress_struct),
        )
        .build(),
    );

    // Desired state (post-normalization, post-alias-resolution)
    // "all" -> "-1" (alias resolved), "tcp" stays as namespaced identifier
    let desired = Resource::with_provider("awscc", "ec2.SecurityGroup", "test-sg", None)
        .with_attribute(
            "security_group_egress",
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    (
                        "ip_protocol".to_string(),
                        Value::Concrete(ConcreteValue::String(
                            "awscc.ec2.SecurityGroup.IpProtocol.tcp".into(),
                        )),
                    ),
                    (
                        "from_port".to_string(),
                        Value::Concrete(ConcreteValue::Int(443)),
                    ),
                    (
                        "to_port".to_string(),
                        Value::Concrete(ConcreteValue::Int(443)),
                    ),
                    (
                        "cidr_ip".to_string(),
                        Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
                    ),
                    (
                        "description".to_string(),
                        Value::Concrete(ConcreteValue::String("Allow HTTPS outbound".into())),
                    ),
                ]))),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    (
                        "ip_protocol".to_string(),
                        Value::Concrete(ConcreteValue::String("-1".into())),
                    ),
                    (
                        "cidr_ip".to_string(),
                        Value::Concrete(ConcreteValue::String("10.0.0.0/8".into())),
                    ),
                    (
                        "description".to_string(),
                        Value::Concrete(ConcreteValue::String(
                            "Allow all to private ranges".into(),
                        )),
                    ),
                ]))),
            ])),
        );

    // Current state (from AWS read, post-normalization, post-alias-resolution)
    // Same as desired, but the "all" rule also has from_port: -1, to_port: -1 from AWS
    let current_attrs = HashMap::from([(
        "security_group_egress".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "ip_protocol".to_string(),
                    Value::Concrete(ConcreteValue::String(
                        "awscc.ec2.SecurityGroup.IpProtocol.tcp".into(),
                    )),
                ),
                (
                    "from_port".to_string(),
                    Value::Concrete(ConcreteValue::Int(443)),
                ),
                (
                    "to_port".to_string(),
                    Value::Concrete(ConcreteValue::Int(443)),
                ),
                (
                    "cidr_ip".to_string(),
                    Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
                ),
                (
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String("Allow HTTPS outbound".into())),
                ),
            ]))),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "ip_protocol".to_string(),
                    Value::Concrete(ConcreteValue::String("-1".into())),
                ),
                (
                    "from_port".to_string(),
                    Value::Concrete(ConcreteValue::Int(-1)),
                ),
                (
                    "to_port".to_string(),
                    Value::Concrete(ConcreteValue::Int(-1)),
                ),
                (
                    "cidr_ip".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.0.0/8".into())),
                ),
                (
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String("Allow all to private ranges".into())),
                ),
            ]))),
        ])),
    )]);
    let current = existing_state(
        ResourceId::with_provider_identity("awscc", "ec2.SecurityGroup", "test-sg", None),
        current_attrs,
    );

    // Saved state (from state file, NOT alias-resolved)
    // This is the state as written after apply: namespaced enum values, AWS-returned fields
    let saved = HashMap::from([(
        "security_group_egress".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "ip_protocol".to_string(),
                    Value::Concrete(ConcreteValue::String(
                        "awscc.ec2.SecurityGroup.IpProtocol.tcp".into(),
                    )),
                ),
                (
                    "from_port".to_string(),
                    Value::Concrete(ConcreteValue::Int(443)),
                ),
                (
                    "to_port".to_string(),
                    Value::Concrete(ConcreteValue::Int(443)),
                ),
                (
                    "cidr_ip".to_string(),
                    Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
                ),
                (
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String("Allow HTTPS outbound".into())),
                ),
            ]))),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "ip_protocol".to_string(),
                    Value::Concrete(ConcreteValue::String(
                        "awscc.ec2.SecurityGroup.IpProtocol.all".into(),
                    )),
                ),
                (
                    "from_port".to_string(),
                    Value::Concrete(ConcreteValue::Int(-1)),
                ),
                (
                    "to_port".to_string(),
                    Value::Concrete(ConcreteValue::Int(-1)),
                ),
                (
                    "cidr_ip".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.0.0/8".into())),
                ),
                (
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String("Allow all to private ranges".into())),
                ),
            ]))),
        ])),
    )]);

    let result = diff_test(&desired, &current, Some(&saved), None, Some(&schema));
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Expected NoChange for idempotent egress rules, got: {:?}",
        result
    );
}

/// Regression test for the root cause of issue #1439: when the schema
/// has `ordered: true` (as it would be after losing `ordered` info in the
/// protocol roundtrip), struct lists are compared positionally instead of
/// as multisets. If AWS returns items in a different order, positional
/// comparison fails and falsely detects changes.
#[test]
fn diff_false_positive_when_ordered_true_for_struct_list() {
    use crate::schema::{ResourceSchema, StructField};

    let egress_struct = AttributeType::struct_(
        "Egress".to_string(),
        vec![
            StructField::optional("cidr_ip", AttributeType::string()).build(),
            StructField::optional("description", AttributeType::string()).build(),
            StructField::optional("from_port", AttributeType::int()).build(),
            StructField::optional("ip_protocol", AttributeType::string()).build(),
            StructField::optional("to_port", AttributeType::int()).build(),
        ],
    );

    // Bug: ordered: true causes positional comparison of struct list items
    let schema_ordered = ResourceSchema::new("awscc.ec2.SecurityGroup").attribute(
        crate::schema::AttributeSchema::optional(
            "security_group_egress",
            AttributeType::list(egress_struct.clone()),
        )
        .build(),
    );

    // Same items in different order
    let item_a = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "ip_protocol".to_string(),
            Value::Concrete(ConcreteValue::String("tcp".into())),
        ),
        (
            "from_port".to_string(),
            Value::Concrete(ConcreteValue::Int(443)),
        ),
        (
            "to_port".to_string(),
            Value::Concrete(ConcreteValue::Int(443)),
        ),
        (
            "cidr_ip".to_string(),
            Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
        ),
        (
            "description".to_string(),
            Value::Concrete(ConcreteValue::String("HTTPS".into())),
        ),
    ])));
    let item_b = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "ip_protocol".to_string(),
            Value::Concrete(ConcreteValue::String("-1".into())),
        ),
        (
            "cidr_ip".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/8".into())),
        ),
        (
            "description".to_string(),
            Value::Concrete(ConcreteValue::String("All".into())),
        ),
        (
            "from_port".to_string(),
            Value::Concrete(ConcreteValue::Int(-1)),
        ),
        (
            "to_port".to_string(),
            Value::Concrete(ConcreteValue::Int(-1)),
        ),
    ])));

    let desired = Resource::with_provider("awscc", "ec2.SecurityGroup", "test-sg", None)
        .with_attribute(
            "security_group_egress",
            Value::Concrete(ConcreteValue::List(vec![item_a.clone(), item_b.clone()])),
        );
    let current = existing_state(
        ResourceId::with_provider_identity("awscc", "ec2.SecurityGroup", "test-sg", None),
        HashMap::from([(
            "security_group_egress".to_string(),
            // AWS returns items in reversed order
            Value::Concrete(ConcreteValue::List(vec![item_b.clone(), item_a.clone()])),
        )]),
    );

    // With ordered: true, differ falsely detects changes (reordered items)
    let result = diff_test(&desired, &current, None, None, Some(&schema_ordered));
    assert!(
        matches!(result, Diff::Update { .. }),
        "Expected false positive Update with ordered:true, got: {:?}",
        result
    );

    // With ordered: false (unordered_list), differ correctly sees no change
    let schema_unordered = ResourceSchema::new("awscc.ec2.SecurityGroup").attribute(
        crate::schema::AttributeSchema::optional(
            "security_group_egress",
            AttributeType::unordered_list(egress_struct),
        )
        .build(),
    );
    let result = diff_test(&desired, &current, None, None, Some(&schema_unordered));
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Expected NoChange with ordered:false, got: {:?}",
        result
    );
}

/// Regression for aws#271: enum DSL alias (snake_case) and API canonical
/// (PascalCase compound) must compare equal under Enum even when
/// `eq_ignore_ascii_case` alone is not enough.
///
/// Scenario: BucketOwnershipControls.object_ownership — one side carries the
/// schema-declared DSL alias `bucket_owner_enforced` and the other carries the
/// API spelling `BucketOwnerEnforced`. Host schema resolution proves that the
/// two name the same enum value, so the differ must see NoChange.
#[test]
fn diff_no_change_for_compound_word_dsl_alias() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("aws.s3.BucketOwnershipControls").attribute(
        AttributeSchema::optional(
            "object_ownership",
            AttributeType::enum_(
                crate::schema::enum_identity(
                    "ObjectOwnership",
                    Some("aws.s3.BucketOwnershipControls"),
                ),
                Some(vec![
                    "BucketOwnerEnforced".to_string(),
                    "BucketOwnerPreferred".to_string(),
                    "ObjectWriter".to_string(),
                ]),
                vec![
                    (
                        "BucketOwnerEnforced".to_string(),
                        "bucket_owner_enforced".to_string(),
                    ),
                    (
                        "BucketOwnerPreferred".to_string(),
                        "bucket_owner_preferred".to_string(),
                    ),
                    ("ObjectWriter".to_string(), "object_writer".to_string()),
                ],
                None,
                None,
            ),
        )
        .build(),
    );

    // Desired: typed DSL alias emitted by the parser.
    let desired = Resource::with_provider("aws", "s3.BucketOwnershipControls", "test", None)
        .with_attribute(
            "object_ownership",
            Value::Concrete(ConcreteValue::enum_identifier(
                "aws.s3.BucketOwnershipControls.ObjectOwnership.bucket_owner_enforced",
            )),
        );

    // Current state: provider API spelling.
    let mut attrs = HashMap::new();
    attrs.insert(
        "object_ownership".to_string(),
        Value::Concrete(ConcreteValue::String("BucketOwnerEnforced".into())),
    );
    let current = existing_state(
        ResourceId::with_provider_identity("aws", "s3.BucketOwnershipControls", "test", None),
        attrs,
    );

    let result = diff_test(&desired, &current, None, None, Some(&schema));
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Expected NoChange (DSL alias must equal API canonical), got: {:?}",
        result
    );
}
