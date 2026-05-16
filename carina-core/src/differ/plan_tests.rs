use super::*;

use indexmap::IndexMap;

use crate::explicit::ExplicitFields;
use crate::resource::{ConcreteValue, ResourceKind};

/// Build an `ExplicitFields::Struct` whose children are all `Leaf` —
/// the shape `state v5 → v6` reads produce, and a convenient way to
/// express "user previously authored these top-level keys" in tests
/// that pre-date the nested authoring tree.
fn explicit_top_level(keys: &[&str]) -> ExplicitFields {
    ExplicitFields::Struct {
        children: keys
            .iter()
            .map(|k| ((*k).to_string(), ExplicitFields::Leaf))
            .collect(),
    }
}

#[test]
fn create_before_destroy_generates_temporary_name_for_name_attribute() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("s3.Bucket", "my-bucket")
        .with_attribute(
            "bucket_name",
            Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
        )
        .with_attribute(
            "object_lock_enabled",
            Value::Concrete(ConcreteValue::Bool(true)),
        );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    attrs.insert(
        "object_lock_enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only())
            .attribute(
                AttributeSchema::new("object_lock_enabled", AttributeType::Bool).create_only(),
            )
            .with_name_attribute("bucket_name"),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Replace {
            temporary_name, to, ..
        } => {
            let temp = temporary_name
                .as_ref()
                .expect("Should have temporary_name for create_before_destroy with name_attribute");
            assert_eq!(temp.attribute, "bucket_name");
            assert_eq!(temp.original_value, "my-bucket");
            assert!(
                temp.temporary_value.starts_with("my-bucket-"),
                "Temporary value '{}' should start with 'my-bucket-'",
                temp.temporary_value
            );
            assert_eq!(temp.temporary_value.len(), "my-bucket-".len() + 8);
            // bucket_name is create-only, so can_rename should be false
            assert!(!temp.can_rename);
            // The `to` resource should have the temporary name
            assert_eq!(
                to.get_attr("bucket_name"),
                Some(&Value::Concrete(ConcreteValue::String(
                    temp.temporary_value.clone()
                )))
            );
        }
        other => panic!("Expected Replace, got {:?}", other),
    }
}

#[test]
fn create_before_destroy_generates_temporary_name_with_can_rename() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("logs.LogGroup", "my-log-group")
        .with_attribute(
            "log_group_name".to_string(),
            Value::Concrete(ConcreteValue::String("my-log-group".to_string())),
        )
        .with_attribute(
            "kms_key_id",
            Value::Concrete(ConcreteValue::String("new-key".to_string())),
        );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "log_group_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-log-group".to_string())),
    );
    attrs.insert(
        "kms_key_id".to_string(),
        Value::Concrete(ConcreteValue::String("old-key".to_string())),
    );
    current_states.insert(
        ResourceId::new("logs.LogGroup", "my-log-group"),
        State::existing(ResourceId::new("logs.LogGroup", "my-log-group"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("logs.LogGroup")
            .attribute(
                // log_group_name is NOT create-only in this test (can be renamed)
                AttributeSchema::new("log_group_name", AttributeType::String),
            )
            .attribute(AttributeSchema::new("kms_key_id", AttributeType::String).create_only())
            .with_name_attribute("log_group_name"),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Replace { temporary_name, .. } => {
            let temp = temporary_name.as_ref().expect("Should have temporary_name");
            assert_eq!(temp.attribute, "log_group_name");
            assert_eq!(temp.original_value, "my-log-group");
            // log_group_name is not create-only, so can_rename should be true
            assert!(temp.can_rename);
        }
        other => panic!("Expected Replace, got {:?}", other),
    }
}

#[test]
fn no_temporary_name_without_create_before_destroy() {
    use crate::schema::{AttributeSchema, AttributeType};

    // Default directives (create_before_destroy = false)
    let resources = vec![
        Resource::new("s3.Bucket", "my-bucket")
            .with_attribute(
                "bucket_name",
                Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
            )
            .with_attribute(
                "object_lock_enabled",
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    attrs.insert(
        "object_lock_enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only())
            .attribute(
                AttributeSchema::new("object_lock_enabled", AttributeType::Bool).create_only(),
            )
            .with_name_attribute("bucket_name"),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Replace { temporary_name, .. } => {
            assert!(
                temporary_name.is_none(),
                "Should not have temporary_name without create_before_destroy"
            );
        }
        other => panic!("Expected Replace, got {:?}", other),
    }
}

#[test]
fn no_temporary_name_when_name_prefix_is_used() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("s3.Bucket", "my-bucket")
        .with_attribute(
            "bucket_name",
            Value::Concrete(ConcreteValue::String("my-app-abc12345".to_string())),
        )
        .with_attribute(
            "object_lock_enabled",
            Value::Concrete(ConcreteValue::Bool(true)),
        );
    resource.directives.create_before_destroy = true;
    // Simulate that name_prefix was used
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-abc12345".to_string())),
    );
    attrs.insert(
        "object_lock_enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only())
            .attribute(
                AttributeSchema::new("object_lock_enabled", AttributeType::Bool).create_only(),
            )
            .with_name_attribute("bucket_name"),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Replace { temporary_name, .. } => {
            assert!(
                temporary_name.is_none(),
                "Should not generate temporary_name when name_prefix is used"
            );
        }
        other => panic!("Expected Replace, got {:?}", other),
    }
}

#[test]
fn no_temporary_name_without_name_attribute_in_schema() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("ec2.Vpc", "my-vpc").with_attribute(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
    );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only()),
        // No name_attribute set
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Replace { temporary_name, .. } => {
            assert!(
                temporary_name.is_none(),
                "Should not generate temporary_name without name_attribute in schema"
            );
        }
        other => panic!("Expected Replace, got {:?}", other),
    }
}

#[test]
fn no_temporary_name_when_name_attribute_changes() {
    use crate::schema::{AttributeSchema, AttributeType};

    // name_attribute itself changed: old-bucket → new-bucket
    // No temporary name needed since names are already different
    let mut resource = Resource::new("s3.Bucket", "my-bucket")
        .with_attribute(
            "bucket_name",
            Value::Concrete(ConcreteValue::String("new-bucket".to_string())),
        )
        .with_attribute(
            "object_lock_enabled",
            Value::Concrete(ConcreteValue::Bool(true)),
        );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("old-bucket".to_string())),
    );
    attrs.insert(
        "object_lock_enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only())
            .attribute(
                AttributeSchema::new("object_lock_enabled", AttributeType::Bool).create_only(),
            )
            .with_name_attribute("bucket_name"),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Replace { temporary_name, .. } => {
            assert!(
                temporary_name.is_none(),
                "Should not generate temporary_name when name_attribute value changes"
            );
        }
        other => panic!("Expected Replace, got {:?}", other),
    }
}

#[test]
fn diff_detects_attribute_removal_with_prev_desired_keys() {
    // User previously had "region" and "tags" in .crn, now only has "region"
    let desired = Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );
    current_attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        )]))),
    );
    let current = State::existing(ResourceId::new("s3.Bucket", "test"), current_attrs);

    // Previous desired state had both "region" and "tags"
    let prev_explicit = explicit_top_level(&["region", "tags"]);

    let result = diff(&desired, &current, None, Some(&prev_explicit), None);
    match result {
        Diff::Update {
            changed_attributes, ..
        } => {
            assert!(
                changed_attributes.contains(&"tags".to_string()),
                "Should detect 'tags' removal, got: {:?}",
                changed_attributes
            );
        }
        _ => panic!("Expected Update, got {:?}", result),
    }
}

#[test]
fn diff_ignores_attributes_not_in_prev_desired_keys() {
    // Current state has "arn" and "region" from provider, but user only ever
    // specified "region" — "arn" was never in prev_desired_keys
    let desired = Resource::new("s3.Bucket", "test");

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );
    current_attrs.insert(
        "arn".to_string(),
        Value::Concrete(ConcreteValue::String("arn:aws:s3:::test".to_string())),
    );
    let current = State::existing(ResourceId::new("s3.Bucket", "test"), current_attrs);

    // User previously only specified "region", not "arn"
    let prev_explicit = explicit_top_level(&["region"]);

    let result = diff(&desired, &current, None, Some(&prev_explicit), None);
    match result {
        Diff::Update {
            changed_attributes, ..
        } => {
            assert!(
                changed_attributes.contains(&"region".to_string()),
                "Should detect 'region' removal"
            );
            assert!(
                !changed_attributes.contains(&"arn".to_string()),
                "Should NOT detect 'arn' removal since it was never in desired"
            );
        }
        _ => panic!("Expected Update, got {:?}", result),
    }
}

#[test]
fn server_default_struct_field_does_not_appear_in_diff() {
    // The user wrote `lifecycle_configuration { rules: [...] }` but did
    // NOT write `transition_default_minimum_object_size`. AWS returns
    // the latter in `current` as a server-side default. The differ must
    // project current through `prev_explicit` and skip the server-only
    // leaf — no `lifecycle_configuration` change should be reported.
    use indexmap::IndexMap;

    let mut desired_lc = IndexMap::new();
    desired_lc.insert(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![{
            let mut rule = IndexMap::new();
            rule.insert(
                "id".to_string(),
                Value::Concrete(ConcreteValue::String("expire".to_string())),
            );
            Value::Concrete(ConcreteValue::Map(rule))
        }])),
    );
    let desired = Resource::new("s3.Bucket", "test").with_attribute(
        "lifecycle_configuration",
        Value::Concrete(ConcreteValue::Map(desired_lc.clone())),
    );

    let mut current_lc = desired_lc;
    current_lc.insert(
        "transition_default_minimum_object_size".to_string(),
        Value::Concrete(ConcreteValue::String(
            "all_storage_classes_128K".to_string(),
        )),
    );
    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "lifecycle_configuration".to_string(),
        Value::Concrete(ConcreteValue::Map(current_lc)),
    );
    let current = State::existing(ResourceId::new("s3.Bucket", "test"), current_attrs);

    // prev_explicit reflects what the user wrote: lifecycle_configuration > rules
    let prev_explicit = ExplicitFields::Struct {
        children: std::collections::HashMap::from([(
            "lifecycle_configuration".to_string(),
            ExplicitFields::Struct {
                children: std::collections::HashMap::from([(
                    "rules".to_string(),
                    ExplicitFields::List {
                        element: Box::new(ExplicitFields::Struct {
                            children: std::collections::HashMap::from([(
                                "id".to_string(),
                                ExplicitFields::Leaf,
                            )]),
                        }),
                    },
                )]),
            },
        )]),
    };

    let result = diff(&desired, &current, None, Some(&prev_explicit), None);
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Server-side default struct leaf must not surface in diff, got: {:?}",
        result
    );
}

#[test]
fn explicit_top_level_removal_still_detected() {
    // Regression guard: removing the *whole* attribute (top-level key
    // gone from desired but still authored in prev_explicit) must
    // still produce an Update. This is the existing "explicit unset"
    // mechanism; the projection logic must not regress it.
    let desired = Resource::new("s3.Bucket", "test");

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Env".to_string(),
            Value::Concrete(ConcreteValue::String("prod".to_string())),
        )]))),
    );
    let current = State::existing(ResourceId::new("s3.Bucket", "test"), current_attrs);

    let prev_explicit = explicit_top_level(&["tags"]);
    let result = diff(&desired, &current, None, Some(&prev_explicit), None);

    match result {
        Diff::Update {
            changed_attributes, ..
        } => {
            assert!(
                changed_attributes.contains(&"tags".to_string()),
                "Removed top-level attr must still produce Update, got: {:?}",
                changed_attributes
            );
        }
        _ => panic!("Expected Update for explicit-unset, got {:?}", result),
    }
}

#[test]
fn diff_no_change_without_prev_desired_keys() {
    // Without prev_desired_keys, removed attributes should NOT be detected
    let desired = Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );
    current_attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        )]))),
    );
    let current = State::existing(ResourceId::new("s3.Bucket", "test"), current_attrs);

    let result = diff(&desired, &current, None, None, None);
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Without prev_desired_keys, extra attributes in current should not trigger Update, got {:?}",
        result
    );
}

#[test]
fn create_plan_detects_attribute_removal() {
    // Resource in .crn has no "tags", but current state (from AWS) has tags.
    // prev_desired_keys indicates user previously had "region" and "tags".
    let resources = vec![Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    )];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );
    attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        )]))),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "test"),
        State::existing(ResourceId::new("s3.Bucket", "test"), attrs),
    );

    let mut prev_explicit = HashMap::new();
    prev_explicit.insert(
        ResourceId::new("s3.Bucket", "test"),
        explicit_top_level(&["region", "tags"]),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &SchemaRegistry::new(),
        &HashMap::new(),
        &prev_explicit,
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(&plan.effects()[0], Effect::Update { .. }),
        "Expected Update effect for attribute removal, got {:?}",
        plan.effects()[0]
    );
}

#[test]
fn create_plan_filters_non_removable_attribute_removal() {
    use crate::schema::{AttributeSchema, AttributeType};
    // When schema is available, only removable attributes should trigger removal.
    // "region" is not removable, "tags" is removable.
    let resources = vec![Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    )];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );
    attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        )]))),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "test"),
        State::existing(ResourceId::new("s3.Bucket", "test"), attrs),
    );

    let mut prev_explicit = HashMap::new();
    prev_explicit.insert(
        ResourceId::new("s3.Bucket", "test"),
        explicit_top_level(&["region", "tags"]),
    );

    // Schema: tags is auto-removable (optional, not create-only),
    // region is explicitly non-removable (provider-inherited)
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("region", AttributeType::String).non_removable())
            .attribute(AttributeSchema::new(
                "tags",
                AttributeType::map(AttributeType::String),
            )),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &prev_explicit,
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Update {
            changed_attributes, ..
        } => {
            assert!(
                changed_attributes.contains(&"tags".to_string()),
                "Should detect removable 'tags' removal"
            );
            assert!(
                !changed_attributes.contains(&"region".to_string()),
                "Should NOT detect non-removable 'region' removal"
            );
        }
        _ => panic!("Expected Update effect"),
    }
}

#[test]
fn create_plan_skips_update_when_only_non_removable_removal() {
    use crate::schema::{AttributeSchema, AttributeType};
    // When the only "change" is a non-removable attribute removal,
    // the plan should have no effects (no spurious Update).
    let resources = vec![Resource::new("s3.Bucket", "test").with_attribute(
        "bucket",
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    )];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "test"),
        State::existing(ResourceId::new("s3.Bucket", "test"), attrs),
    );

    let mut prev_explicit = HashMap::new();
    prev_explicit.insert(
        ResourceId::new("s3.Bucket", "test"),
        explicit_top_level(&["bucket", "region"]),
    );

    // Schema: region is explicitly non-removable, bucket is required
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket", AttributeType::String).required())
            .attribute(AttributeSchema::new("region", AttributeType::String).non_removable()),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &prev_explicit,
        &HashMap::new(),
        &[],
    );

    assert!(
        plan.effects().is_empty(),
        "Should not generate spurious Update for non-removable attribute removal, got {:?}",
        plan.effects()
    );
}

#[test]
fn diff_skips_internal_attributes_in_removal_detection() {
    // prev_desired_keys includes "_internal" but it should be skipped
    let desired = Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
    );
    current_attrs.insert(
        "_internal".to_string(),
        Value::Concrete(ConcreteValue::String("something".to_string())),
    );
    let current = State::existing(ResourceId::new("s3.Bucket", "test"), current_attrs);

    let prev_explicit = explicit_top_level(&["region", "_internal"]);

    let result = diff(&desired, &current, None, Some(&prev_explicit), None);
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Should skip internal attributes starting with '_', got {:?}",
        result
    );
}

#[test]
fn prevent_destroy_blocks_delete_for_orphaned_resource() {
    // Orphaned resource (in state but removed from .crn) with prevent_destroy
    // should produce a PlanError instead of a Delete effect.

    // Orphaned resource: exists in current_states but NOT in desired
    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    // Directives from state say prevent_destroy
    let mut directives_map = HashMap::new();
    directives_map.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        Directives {
            prevent_destroy: true,
            ..Default::default()
        },
    );

    let plan = create_plan(
        &[], // no desired resources
        &current_states,
        &directives_map,
        &SchemaRegistry::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    // Should have NO delete effects
    assert!(
        plan.effects().is_empty(),
        "Should not generate Delete effect for prevent_destroy resource, got {:?}",
        plan.effects()
    );

    // Should have an error
    assert!(plan.has_errors(), "Should have prevent_destroy error");
    assert_eq!(plan.errors().len(), 1);
    assert!(
        plan.errors()[0].message.contains("prevent_destroy"),
        "Error message should mention prevent_destroy: {}",
        plan.errors()[0].message
    );
    assert_eq!(
        plan.errors()[0].resource_id,
        ResourceId::new("ec2.Vpc", "my-vpc")
    );
}

#[test]
fn prevent_destroy_blocks_replace() {
    use crate::schema::{AttributeSchema, AttributeType};

    // Resource with prevent_destroy that has a create-only attribute change
    // (which would normally trigger a Replace)
    let mut resource = Resource::new("ec2.Vpc", "my-vpc").with_attribute(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
    );
    resource.directives.prevent_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("ec2.Vpc")
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
        &[],
    );

    // Should have NO replace effects
    assert!(
        plan.effects().is_empty(),
        "Should not generate Replace effect for prevent_destroy resource, got {:?}",
        plan.effects()
    );

    // Should have an error
    assert!(
        plan.has_errors(),
        "Should have prevent_destroy error for replace"
    );
    assert_eq!(plan.errors().len(), 1);
    assert!(
        plan.errors()[0].message.contains("prevent_destroy"),
        "Error message should mention prevent_destroy: {}",
        plan.errors()[0].message
    );
}

#[test]
fn prevent_destroy_does_not_block_update() {
    // Resource with prevent_destroy that has a normal (non-create-only) attribute change
    // Updates don't destroy the resource, so they should be allowed
    let mut resource = Resource::new("s3.Bucket", "my-bucket").with_attribute(
        "versioning",
        Value::Concrete(ConcreteValue::String("Enabled".to_string())),
    );
    resource.directives.prevent_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "versioning".to_string(),
        Value::Concrete(ConcreteValue::String("Disabled".to_string())),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &SchemaRegistry::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    // Should generate an Update effect (not blocked)
    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(&plan.effects()[0], Effect::Update { .. }),
        "Expected Update effect, got {:?}",
        plan.effects()[0]
    );

    // Should have NO errors
    assert!(
        !plan.has_errors(),
        "Should not have errors for Update with prevent_destroy"
    );
}

#[test]
fn prevent_destroy_does_not_block_create() {
    // Resource with prevent_destroy that doesn't exist yet
    // Creates don't destroy anything, so they should be allowed
    let mut resource = Resource::new("s3.Bucket", "my-bucket").with_attribute(
        "bucket",
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    resource.directives.prevent_destroy = true;

    let resources = vec![resource];

    let plan = create_plan(
        &resources,
        &HashMap::new(), // no current states (resource doesn't exist)
        &HashMap::new(),
        &SchemaRegistry::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    // Should generate a Create effect (not blocked)
    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(&plan.effects()[0], Effect::Create(_)),
        "Expected Create effect, got {:?}",
        plan.effects()[0]
    );

    // Should have NO errors
    assert!(
        !plan.has_errors(),
        "Should not have errors for Create with prevent_destroy"
    );
}

#[test]
fn without_prevent_destroy_delete_works_normally() {
    // Orphaned resource without prevent_destroy should still be deleted normally
    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    let plan = create_plan(
        &[], // no desired resources
        &current_states,
        &HashMap::new(), // no directives (default = prevent_destroy: false)
        &SchemaRegistry::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    // Should generate a Delete effect
    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(&plan.effects()[0], Effect::Delete { .. }),
        "Expected Delete effect, got {:?}",
        plan.effects()[0]
    );

    // Should have NO errors
    assert!(!plan.has_errors());
}

#[test]
fn prevent_destroy_collects_multiple_errors() {
    // Two orphaned resources with prevent_destroy - both should generate errors
    let mut current_states = HashMap::new();
    let mut attrs1 = HashMap::new();
    attrs1.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "vpc-1"),
        State::existing(ResourceId::new("ec2.Vpc", "vpc-1"), attrs1),
    );
    let mut attrs2 = HashMap::new();
    attrs2.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "vpc-2"),
        State::existing(ResourceId::new("ec2.Vpc", "vpc-2"), attrs2),
    );

    let mut directives_map = HashMap::new();
    directives_map.insert(
        ResourceId::new("ec2.Vpc", "vpc-1"),
        Directives {
            prevent_destroy: true,
            ..Default::default()
        },
    );
    directives_map.insert(
        ResourceId::new("ec2.Vpc", "vpc-2"),
        Directives {
            prevent_destroy: true,
            ..Default::default()
        },
    );

    let plan = create_plan(
        &[],
        &current_states,
        &directives_map,
        &SchemaRegistry::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert!(plan.effects().is_empty());
    assert_eq!(
        plan.errors().len(),
        2,
        "Should collect errors for both resources"
    );
}

#[test]
fn virtual_resources_are_skipped_in_plan() {
    // Virtual resources (module attribute containers) should not generate any effects
    let mut virtual_resource = Resource::new("_virtual", "web").with_kind(ResourceKind::Virtual {
        module_name: "web_tier".to_string(),
        instance: "web".to_string(),
    });
    virtual_resource.binding = Some("web".to_string());
    virtual_resource.set_attr(
        "security_group".to_string(),
        Value::Concrete(ConcreteValue::String("sg-123".to_string())),
    );

    let real_resource = Resource::new("ec2.SecurityGroup", "sg").with_attribute(
        "group_name",
        Value::Concrete(ConcreteValue::String("my-sg".to_string())),
    );

    let resources = vec![virtual_resource, real_resource];

    let plan = create_plan(
        &resources,
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    // Only the real resource should generate an effect (Create)
    assert_eq!(plan.effects().len(), 1);
    assert_eq!(
        plan.effects()[0].resource_id(),
        &ResourceId::new("ec2.SecurityGroup", "sg")
    );
}

#[test]
fn wait_binding_lowers_to_wait_effect() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    let resources = vec![cert];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String(
                "aws.acm.Certificate.Status.Issued".to_string(),
            )),
        },
        timeout_secs: Some(75 * 60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );

    let wait_effect = plan
        .effects()
        .iter()
        .find(|e| matches!(e, Effect::Wait { .. }))
        .expect("expected Wait effect");
    let Effect::Wait {
        binding,
        target_id,
        until,
        until_surface,
        timeout,
        ..
    } = wait_effect
    else {
        unreachable!();
    };
    assert_eq!(binding, "cert_issued");
    assert_eq!(target_id.name.as_str(), "cert");
    assert_eq!(target_id.resource_type, "acm.Certificate");
    assert_eq!(
        until,
        &WaitPredicate::Equals {
            attr: AttrPath {
                segments: vec!["status".to_string()],
            },
            value: Value::Concrete(ConcreteValue::String(
                "aws.acm.Certificate.Status.Issued".to_string()
            )),
        }
    );
    assert_eq!(*timeout, std::time::Duration::from_secs(75 * 60));
    assert_eq!(
        until_surface,
        "cert.status == aws.acm.Certificate.Status.Issued"
    );
}

#[test]
fn wait_uses_schema_default_timeout_when_omitted() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    let resources = vec![cert];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("acm.Certificate")
            .attribute(AttributeSchema::new("status", AttributeType::String))
            .with_default_wait_timeout(std::time::Duration::from_secs(99))
            .with_default_wait_interval(std::time::Duration::from_secs(7)),
    );

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );
    let Effect::Wait {
        timeout, interval, ..
    } = plan
        .effects()
        .iter()
        .find(|e| matches!(e, Effect::Wait { .. }))
        .expect("expected Wait effect")
    else {
        unreachable!();
    };
    assert_eq!(*timeout, std::time::Duration::from_secs(99));
    assert_eq!(*interval, std::time::Duration::from_secs(7));
}

#[test]
fn wait_with_unknown_target_emits_plan_error() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let resources: Vec<Resource> = vec![];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "nonexistent".into(),
        until_raw: "nonexistent.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["nonexistent".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );
    assert!(
        !plan.errors().is_empty(),
        "missing-target wait should surface a plan error"
    );
    assert!(
        plan.errors()[0].message.contains("nonexistent"),
        "error message should mention the missing target, got: {}",
        plan.errors()[0].message
    );
}
