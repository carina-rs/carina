use super::*;

#[test]
fn diff_create_when_not_exists() {
    let desired = Resource::new("bucket", "test");
    let current = State::not_found(ResourceId::new("bucket", "test"));

    let result = diff(&desired, &current, None, None, None);
    assert!(matches!(result, Diff::Create(_)));
}

#[test]
fn diff_no_change_when_same() {
    let desired = Resource::new("bucket", "test")
        .with_attribute("region", Value::String("ap-northeast-1".to_string()));

    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::String("ap-northeast-1".to_string()),
    );
    let current = State::existing(ResourceId::new("bucket", "test"), attrs);

    let result = diff(&desired, &current, None, None, None);
    assert!(matches!(result, Diff::NoChange(_)));
}

#[test]
fn diff_update_when_different() {
    let desired = Resource::new("bucket", "test")
        .with_attribute("region", Value::String("us-east-1".to_string()));

    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::String("ap-northeast-1".to_string()),
    );
    let current = State::existing(ResourceId::new("bucket", "test"), attrs);

    let result = diff(&desired, &current, None, None, None);
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
fn create_plan_from_resources() {
    let resources = vec![
        Resource::new("bucket", "new-bucket"),
        Resource::new("bucket", "existing-bucket").with_attribute("versioning", Value::Bool(true)),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert("versioning".to_string(), Value::Bool(false));
    current_states.insert(
        ResourceId::new("bucket", "existing-bucket"),
        State::existing(ResourceId::new("bucket", "existing-bucket"), attrs),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(plan.effects().len(), 2);
    assert!(matches!(plan.effects()[0], Effect::Create(_)));
    assert!(matches!(plan.effects()[1], Effect::Update { .. }));
}

#[test]
fn create_plan_with_read_only_resource() {
    let resources = vec![
        Resource::new("bucket", "existing-bucket")
            .with_attribute("name", Value::String("existing-bucket".to_string()))
            .with_read_only(true),
        Resource::new("bucket", "new-bucket"),
    ];

    let current_states = HashMap::new();
    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    // Should have 2 effects: Read for data source, Create for new bucket
    assert_eq!(plan.effects().len(), 2);
    assert!(matches!(plan.effects()[0], Effect::Read { .. }));
    assert!(matches!(plan.effects()[1], Effect::Create(_)));
}

#[test]
fn diff_update_when_list_of_maps_changed() {
    let mut ingress1 = HashMap::new();
    ingress1.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
    ingress1.insert("from_port".to_string(), Value::Int(80));
    ingress1.insert("to_port".to_string(), Value::Int(80));

    let mut ingress2 = HashMap::new();
    ingress2.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
    ingress2.insert("from_port".to_string(), Value::Int(443));
    ingress2.insert("to_port".to_string(), Value::Int(443));

    let desired = Resource::new("ec2_security_group", "test-sg").with_attribute(
        "security_group_ingress",
        Value::List(vec![Value::Map(ingress1.clone()), Value::Map(ingress2)]),
    );

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "security_group_ingress".to_string(),
        Value::List(vec![Value::Map(ingress1)]),
    );
    let current = State::existing(
        ResourceId::new("ec2_security_group", "test-sg"),
        current_attrs,
    );

    let result = diff(&desired, &current, None, None, None);
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
fn create_plan_detects_orphaned_resources_for_deletion() {
    // A resource exists in current_states but NOT in desired list
    // create_plan() should generate a Delete effect for it
    let desired = vec![Resource::new("bucket", "keep-this")];

    let mut current_states = HashMap::new();
    // "keep-this" exists and matches
    current_states.insert(
        ResourceId::new("bucket", "keep-this"),
        State::existing(ResourceId::new("bucket", "keep-this"), HashMap::new()),
    );
    // "orphaned-bucket" exists in state but not in desired
    let mut orphan_attrs = HashMap::new();
    orphan_attrs.insert(
        "name".to_string(),
        Value::String("orphaned-bucket".to_string()),
    );
    current_states.insert(
        ResourceId::new("bucket", "orphaned-bucket"),
        State::existing(ResourceId::new("bucket", "orphaned-bucket"), orphan_attrs),
    );

    let plan = create_plan(
        &desired,
        &current_states,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
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
    // Even if the resource "exists", read-only resources should only generate Read effect
    let resources = vec![
        Resource::new("bucket", "existing-bucket")
            .with_attribute("name", Value::String("existing-bucket".to_string()))
            .with_read_only(true),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "name".to_string(),
        Value::String("existing-bucket".to_string()),
    );
    current_states.insert(
        ResourceId::new("bucket", "existing-bucket"),
        State::existing(ResourceId::new("bucket", "existing-bucket"), attrs),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    // Should still have Read effect, not NoChange
    assert_eq!(plan.effects().len(), 1);
    assert!(matches!(plan.effects()[0], Effect::Read { .. }));
}

/// Regression test for issue #146: when neither desired nor current state has
/// a "name" attribute (the normal case for AWSCC resources after PR #151),
/// the differ should report NoChange, not a false update.
#[test]
fn no_false_update_without_name_attribute() {
    // Simulate AWSCC resource: desired has cidr_block but no "name"
    let desired = Resource::new("ec2.Vpc", "vpc")
        .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));

    // Current state from provider read also has cidr_block but no "name"
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    let current = State::existing(ResourceId::new("ec2.Vpc", "vpc"), attrs);

    let result = diff(&desired, &current, None, None, None);
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Expected NoChange when neither side has 'name', got {:?}",
        result
    );
}

#[test]
fn replace_when_create_only_attr_changed() {
    use crate::schema::{AttributeSchema, AttributeType};

    let resources = vec![
        Resource::new("ec2.Vpc", "my-vpc")
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string())),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    // Build schema with cidr_block marked as create-only
    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.Vpc".to_string(),
        crate::schema::ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only()),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Replace {
            changed_create_only,
            ..
        } => {
            assert_eq!(changed_create_only, &vec!["cidr_block".to_string()]);
        }
        other => panic!("Expected Replace, got {:?}", other),
    }
}

#[test]
fn normal_update_when_non_create_only_attr_changed() {
    use crate::schema::{AttributeSchema, AttributeType};

    let resources = vec![
        Resource::new("ec2.Vpc", "my-vpc").with_attribute("enable_dns_support", Value::Bool(true)),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert("enable_dns_support".to_string(), Value::Bool(false));
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    // cidr_block is create-only, but enable_dns_support is not
    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.Vpc".to_string(),
        crate::schema::ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only())
            .attribute(AttributeSchema::new(
                "enable_dns_support",
                AttributeType::Bool,
            )),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(plan.effects()[0], Effect::Update { .. }),
        "Expected Update, got {:?}",
        plan.effects()[0]
    );
}

#[test]
fn replace_when_schema_force_replace() {
    use crate::schema::AttributeType;

    // Resource has changed attributes but NO create-only attributes
    let resources = vec![
        Resource::new("ec2.internet_gateway", "my-igw").with_attribute(
            "tags",
            Value::Map(
                vec![("Name".to_string(), Value::String("new-name".to_string()))]
                    .into_iter()
                    .collect(),
            ),
        ),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "tags".to_string(),
        Value::Map(
            vec![("Name".to_string(), Value::String("old-name".to_string()))]
                .into_iter()
                .collect(),
        ),
    );
    current_states.insert(
        ResourceId::new("ec2.internet_gateway", "my-igw"),
        State::existing(ResourceId::new("ec2.internet_gateway", "my-igw"), attrs),
    );

    // Schema has force_replace=true (no create-only attributes)
    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.internet_gateway".to_string(),
        crate::schema::ResourceSchema::new("ec2.internet_gateway")
            .attribute(crate::schema::AttributeSchema::new(
                "tags",
                AttributeType::String,
            ))
            .force_replace(),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(plan.effects()[0], Effect::Replace { .. }),
        "Expected Replace for force_replace schema, got {:?}",
        plan.effects()[0]
    );
}

#[test]
fn replace_when_mix_of_create_only_and_normal_attrs_changed() {
    use crate::schema::{AttributeSchema, AttributeType};

    let resources = vec![
        Resource::new("ec2.Vpc", "my-vpc")
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()))
            .with_attribute("enable_dns_support", Value::Bool(true)),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    attrs.insert("enable_dns_support".to_string(), Value::Bool(false));
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.Vpc".to_string(),
        crate::schema::ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only())
            .attribute(AttributeSchema::new(
                "enable_dns_support",
                AttributeType::Bool,
            )),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Replace {
            changed_create_only,
            ..
        } => {
            assert_eq!(changed_create_only, &vec!["cidr_block".to_string()]);
        }
        other => panic!("Expected Replace, got {:?}", other),
    }
}

#[test]
fn replace_carries_create_before_destroy_lifecycle() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("ec2.Vpc", "my-vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));
    resource.lifecycle.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.Vpc".to_string(),
        crate::schema::ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only()),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Replace {
            lifecycle,
            changed_create_only,
            ..
        } => {
            assert!(lifecycle.create_before_destroy);
            assert_eq!(changed_create_only, &vec!["cidr_block".to_string()]);
        }
        other => panic!("Expected Replace, got {:?}", other),
    }
}

#[test]
fn diff_no_change_when_list_of_maps_reordered() {
    let mut rule1 = HashMap::new();
    rule1.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
    rule1.insert("from_port".to_string(), Value::Int(80));
    rule1.insert("to_port".to_string(), Value::Int(80));

    let mut rule2 = HashMap::new();
    rule2.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
    rule2.insert("from_port".to_string(), Value::Int(443));
    rule2.insert("to_port".to_string(), Value::Int(443));

    // Desired: [rule1, rule2]
    let desired = Resource::new("ec2_security_group", "test-sg").with_attribute(
        "security_group_egress",
        Value::List(vec![Value::Map(rule1.clone()), Value::Map(rule2.clone())]),
    );

    // Current (from AWS): [rule2, rule1] — same content, different order
    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "security_group_egress".to_string(),
        Value::List(vec![Value::Map(rule2), Value::Map(rule1)]),
    );
    let current = State::existing(
        ResourceId::new("ec2_security_group", "test-sg"),
        current_attrs,
    );

    let result = diff(&desired, &current, None, None, None);
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
        Resource::with_provider("awscc", "ec2.Vpc", "my-vpc")
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string())),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    current_states.insert(
        ResourceId::with_provider("awscc", "ec2.Vpc", "my-vpc"),
        State::existing(
            ResourceId::with_provider("awscc", "ec2.Vpc", "my-vpc"),
            attrs,
        ),
    );

    // Schema keyed with provider prefix (as in production)
    let mut schemas = HashMap::new();
    schemas.insert(
        "awscc.ec2.Vpc".to_string(),
        crate::schema::ResourceSchema::new("awscc.ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only()),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(plan.effects()[0], Effect::Replace { .. }),
        "Expected Replace with awscc-prefixed schema key, got {:?}",
        plan.effects()[0]
    );
}

/// Regression test for issue #172: desired has 2 fields in a struct,
/// current (AWS) returns 3, saved state has 3. Should be NoChange.
#[test]
fn diff_no_change_when_struct_has_extra_fields_with_saved() {
    let desired = Resource::new("ec2.Subnet", "test-subnet").with_attribute(
        "private_dns_name_options_on_launch",
        Value::Map(HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Bool(true),
            ),
        ])),
    );

    let current_attrs = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Map(HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Bool(true),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Bool(false),
            ),
        ])),
    )]);
    let current = State::existing(ResourceId::new("ec2.Subnet", "test-subnet"), current_attrs);

    let saved = HashMap::from([
        (
            "hostname_type".to_string(),
            Value::String("ip-name".to_string()),
        ),
        (
            "enable_resource_name_dns_a_record".to_string(),
            Value::Bool(true),
        ),
        (
            "enable_resource_name_dns_aaaa_record".to_string(),
            Value::Bool(false),
        ),
    ]);
    let saved_map = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Map(saved),
    )]);

    let result = diff(&desired, &current, Some(&saved_map), None, None);
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
        Value::Map(HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Bool(true),
            ),
        ])),
    );

    // AWS returns aaaa_record: true (drifted from saved false)
    let current_attrs = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Map(HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Bool(true),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Bool(true),
            ),
        ])),
    )]);
    let current = State::existing(ResourceId::new("ec2.Subnet", "test-subnet"), current_attrs);

    let saved = HashMap::from([
        (
            "hostname_type".to_string(),
            Value::String("ip-name".to_string()),
        ),
        (
            "enable_resource_name_dns_a_record".to_string(),
            Value::Bool(true),
        ),
        (
            "enable_resource_name_dns_aaaa_record".to_string(),
            Value::Bool(false),
        ),
    ]);
    let saved_map = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Map(saved),
    )]);

    let result = diff(&desired, &current, Some(&saved_map), None, None);
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
        Value::Map(HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Bool(true),
            ),
        ])),
    );

    // Provider read returns Map with extra fields not in desired
    let current_attrs = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Map(HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Bool(true),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Bool(false),
            ),
        ])),
    )]);
    let current = State::existing(ResourceId::new("ec2.Subnet", "test-subnet"), current_attrs);

    // Saved state has the same Map with extra fields
    let saved_map = HashMap::from([(
        "private_dns_name_options_on_launch".to_string(),
        Value::Map(HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Bool(true),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Bool(false),
            ),
        ])),
    )]);

    let result = diff(&desired, &current, Some(&saved_map), None, None);
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
        Value::Map(HashMap::from([
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(2)),
        ])),
    );

    let current_attrs = HashMap::from([(
        "opts".to_string(),
        Value::Map(HashMap::from([
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(2)),
            ("c".to_string(), Value::Int(3)),
        ])),
    )]);
    let current = State::existing(ResourceId::new("ec2.Subnet", "test-subnet"), current_attrs);

    // Without saved state, the map comparison uses semantically_equal which
    // checks both key count AND values. Since desired map has 2 keys and current
    // has 3, this will show as Update (which is the existing behavior).
    let result = diff(&desired, &current, None, None, None);
    assert!(
        matches!(result, Diff::Update { .. }),
        "Expected Update without saved state when maps have different sizes, got {:?}",
        result
    );
}

#[test]
fn orphan_delete_preserves_binding_and_dependencies() {
    // Orphan resources (in state but not in desired) should carry
    // binding and dependencies extracted from the state attributes.
    let desired = vec![];

    let mut current_states = HashMap::new();
    let mut orphan_attrs = HashMap::new();
    orphan_attrs.insert(
        "_binding".to_string(),
        Value::String("my_subnet".to_string()),
    );
    orphan_attrs.insert(
        "vpc_id".to_string(),
        Value::resource_ref("my_vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    current_states.insert(
        ResourceId::new("subnet", "my-subnet"),
        State::existing(ResourceId::new("subnet", "my-subnet"), orphan_attrs),
    );

    let plan = create_plan(
        &desired,
        &current_states,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
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
                "Orphan Delete should preserve _binding from state"
            );
            assert!(
                dependencies.contains("my_vpc"),
                "Orphan Delete should extract dependencies from ResourceRef values in state"
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
    let egress_struct = AttributeType::Struct {
        name: "Egress".to_string(),
        fields: vec![
            StructField::new("cidr_ip", AttributeType::String),
            StructField::new("description", AttributeType::String),
            StructField::new("from_port", AttributeType::Int),
            StructField::new(
                "ip_protocol",
                AttributeType::StringEnum {
                    name: "IpProtocol".to_string(),
                    values: vec![
                        "tcp".to_string(),
                        "udp".to_string(),
                        "icmp".to_string(),
                        "-1".to_string(),
                        "all".to_string(),
                    ],
                    namespace: Some("awscc.ec2.SecurityGroup".to_string()),
                    to_dsl: Some(|s: &str| match s {
                        "-1" => "all".to_string(),
                        _ => s.to_string(),
                    }),
                },
            ),
            StructField::new("to_port", AttributeType::Int),
        ],
    };
    let schema = ResourceSchema::new("awscc.ec2.SecurityGroup").attribute(
        crate::schema::AttributeSchema::new(
            "security_group_egress",
            AttributeType::unordered_list(egress_struct),
        ),
    );

    // Desired state (post-normalization, post-alias-resolution)
    // "all" -> "-1" (alias resolved), "tcp" stays as namespaced identifier
    let desired = Resource::with_provider("awscc", "ec2.SecurityGroup", "test-sg").with_attribute(
        "security_group_egress",
        Value::List(vec![
            Value::Map(HashMap::from([
                (
                    "ip_protocol".to_string(),
                    Value::String("awscc.ec2.SecurityGroup.IpProtocol.tcp".to_string()),
                ),
                ("from_port".to_string(), Value::Int(443)),
                ("to_port".to_string(), Value::Int(443)),
                (
                    "cidr_ip".to_string(),
                    Value::String("0.0.0.0/0".to_string()),
                ),
                (
                    "description".to_string(),
                    Value::String("Allow HTTPS outbound".to_string()),
                ),
            ])),
            Value::Map(HashMap::from([
                ("ip_protocol".to_string(), Value::String("-1".to_string())),
                (
                    "cidr_ip".to_string(),
                    Value::String("10.0.0.0/8".to_string()),
                ),
                (
                    "description".to_string(),
                    Value::String("Allow all to private ranges".to_string()),
                ),
            ])),
        ]),
    );

    // Current state (from AWS read, post-normalization, post-alias-resolution)
    // Same as desired, but the "all" rule also has from_port: -1, to_port: -1 from AWS
    let current_attrs = HashMap::from([(
        "security_group_egress".to_string(),
        Value::List(vec![
            Value::Map(HashMap::from([
                (
                    "ip_protocol".to_string(),
                    Value::String("awscc.ec2.SecurityGroup.IpProtocol.tcp".to_string()),
                ),
                ("from_port".to_string(), Value::Int(443)),
                ("to_port".to_string(), Value::Int(443)),
                (
                    "cidr_ip".to_string(),
                    Value::String("0.0.0.0/0".to_string()),
                ),
                (
                    "description".to_string(),
                    Value::String("Allow HTTPS outbound".to_string()),
                ),
            ])),
            Value::Map(HashMap::from([
                ("ip_protocol".to_string(), Value::String("-1".to_string())),
                ("from_port".to_string(), Value::Int(-1)),
                ("to_port".to_string(), Value::Int(-1)),
                (
                    "cidr_ip".to_string(),
                    Value::String("10.0.0.0/8".to_string()),
                ),
                (
                    "description".to_string(),
                    Value::String("Allow all to private ranges".to_string()),
                ),
            ])),
        ]),
    )]);
    let current = State::existing(
        ResourceId::with_provider("awscc", "ec2.SecurityGroup", "test-sg"),
        current_attrs,
    );

    // Saved state (from state file, NOT alias-resolved)
    // This is the state as written after apply: namespaced enum values, AWS-returned fields
    let saved = HashMap::from([(
        "security_group_egress".to_string(),
        Value::List(vec![
            Value::Map(HashMap::from([
                (
                    "ip_protocol".to_string(),
                    Value::String("awscc.ec2.SecurityGroup.IpProtocol.tcp".to_string()),
                ),
                ("from_port".to_string(), Value::Int(443)),
                ("to_port".to_string(), Value::Int(443)),
                (
                    "cidr_ip".to_string(),
                    Value::String("0.0.0.0/0".to_string()),
                ),
                (
                    "description".to_string(),
                    Value::String("Allow HTTPS outbound".to_string()),
                ),
            ])),
            Value::Map(HashMap::from([
                (
                    "ip_protocol".to_string(),
                    Value::String("awscc.ec2.SecurityGroup.IpProtocol.all".to_string()),
                ),
                ("from_port".to_string(), Value::Int(-1)),
                ("to_port".to_string(), Value::Int(-1)),
                (
                    "cidr_ip".to_string(),
                    Value::String("10.0.0.0/8".to_string()),
                ),
                (
                    "description".to_string(),
                    Value::String("Allow all to private ranges".to_string()),
                ),
            ])),
        ]),
    )]);

    let result = diff(&desired, &current, Some(&saved), None, Some(&schema));
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

    let egress_struct = AttributeType::Struct {
        name: "Egress".to_string(),
        fields: vec![
            StructField::new("cidr_ip", AttributeType::String),
            StructField::new("description", AttributeType::String),
            StructField::new("from_port", AttributeType::Int),
            StructField::new("ip_protocol", AttributeType::String),
            StructField::new("to_port", AttributeType::Int),
        ],
    };

    // Bug: ordered: true causes positional comparison of struct list items
    let schema_ordered = ResourceSchema::new("awscc.ec2.SecurityGroup").attribute(
        crate::schema::AttributeSchema::new(
            "security_group_egress",
            AttributeType::List {
                inner: Box::new(egress_struct.clone()),
                ordered: true,
            },
        ),
    );

    // Same items in different order
    let item_a = Value::Map(HashMap::from([
        ("ip_protocol".to_string(), Value::String("tcp".to_string())),
        ("from_port".to_string(), Value::Int(443)),
        ("to_port".to_string(), Value::Int(443)),
        (
            "cidr_ip".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        ),
        (
            "description".to_string(),
            Value::String("HTTPS".to_string()),
        ),
    ]));
    let item_b = Value::Map(HashMap::from([
        ("ip_protocol".to_string(), Value::String("-1".to_string())),
        (
            "cidr_ip".to_string(),
            Value::String("10.0.0.0/8".to_string()),
        ),
        ("description".to_string(), Value::String("All".to_string())),
        ("from_port".to_string(), Value::Int(-1)),
        ("to_port".to_string(), Value::Int(-1)),
    ]));

    let desired = Resource::with_provider("awscc", "ec2.SecurityGroup", "test-sg").with_attribute(
        "security_group_egress",
        Value::List(vec![item_a.clone(), item_b.clone()]),
    );
    let current = State::existing(
        ResourceId::with_provider("awscc", "ec2.SecurityGroup", "test-sg"),
        HashMap::from([(
            "security_group_egress".to_string(),
            // AWS returns items in reversed order
            Value::List(vec![item_b.clone(), item_a.clone()]),
        )]),
    );

    // With ordered: true, differ falsely detects changes (reordered items)
    let result = diff(&desired, &current, None, None, Some(&schema_ordered));
    assert!(
        matches!(result, Diff::Update { .. }),
        "Expected false positive Update with ordered:true, got: {:?}",
        result
    );

    // With ordered: false (unordered_list), differ correctly sees no change
    let schema_unordered = ResourceSchema::new("awscc.ec2.SecurityGroup").attribute(
        crate::schema::AttributeSchema::new(
            "security_group_egress",
            AttributeType::unordered_list(egress_struct),
        ),
    );
    let result = diff(&desired, &current, None, None, Some(&schema_unordered));
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Expected NoChange with ordered:false, got: {:?}",
        result
    );
}
