use super::*;
use crate::resource::ResourceKind;

#[test]
fn create_before_destroy_generates_temporary_name_for_name_attribute() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("s3.Bucket", "my-bucket")
        .with_attribute("bucket_name", Value::String("my-bucket".to_string()))
        .with_attribute("object_lock_enabled", Value::Bool(true));
    resource.lifecycle.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::String("my-bucket".to_string()),
    );
    attrs.insert("object_lock_enabled".to_string(), Value::Bool(false));
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = HashMap::new();
    schemas.insert(
        "s3.Bucket".to_string(),
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
                Some(&Value::String(temp.temporary_value.clone()))
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
            Value::String("my-log-group".to_string()),
        )
        .with_attribute("kms_key_id", Value::String("new-key".to_string()));
    resource.lifecycle.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "log_group_name".to_string(),
        Value::String("my-log-group".to_string()),
    );
    attrs.insert(
        "kms_key_id".to_string(),
        Value::String("old-key".to_string()),
    );
    current_states.insert(
        ResourceId::new("logs.LogGroup", "my-log-group"),
        State::existing(ResourceId::new("logs.LogGroup", "my-log-group"), attrs),
    );

    let mut schemas = HashMap::new();
    schemas.insert(
        "logs.LogGroup".to_string(),
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

    // Default lifecycle (create_before_destroy = false)
    let resources = vec![
        Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("bucket_name", Value::String("my-bucket".to_string()))
            .with_attribute("object_lock_enabled", Value::Bool(true)),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::String("my-bucket".to_string()),
    );
    attrs.insert("object_lock_enabled".to_string(), Value::Bool(false));
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = HashMap::new();
    schemas.insert(
        "s3.Bucket".to_string(),
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
        .with_attribute("bucket_name", Value::String("my-app-abc12345".to_string()))
        .with_attribute("object_lock_enabled", Value::Bool(true));
    resource.lifecycle.create_before_destroy = true;
    // Simulate that name_prefix was used
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::String("my-app-abc12345".to_string()),
    );
    attrs.insert("object_lock_enabled".to_string(), Value::Bool(false));
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = HashMap::new();
    schemas.insert(
        "s3.Bucket".to_string(),
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
        .with_attribute("bucket_name", Value::String("new-bucket".to_string()))
        .with_attribute("object_lock_enabled", Value::Bool(true));
    resource.lifecycle.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::String("old-bucket".to_string()),
    );
    attrs.insert("object_lock_enabled".to_string(), Value::Bool(true));
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = HashMap::new();
    schemas.insert(
        "s3.Bucket".to_string(),
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
    let desired = Resource::new("s3.Bucket", "test")
        .with_attribute("region", Value::String("ap-northeast-1".to_string()));

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::String("ap-northeast-1".to_string()),
    );
    current_attrs.insert(
        "tags".to_string(),
        Value::Map(HashMap::from([(
            "Name".to_string(),
            Value::String("test".to_string()),
        )])),
    );
    let current = State::existing(ResourceId::new("s3.Bucket", "test"), current_attrs);

    // Previous desired state had both "region" and "tags"
    let prev_keys = vec!["region".to_string(), "tags".to_string()];

    let result = diff(&desired, &current, None, Some(&prev_keys), None);
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
        Value::String("ap-northeast-1".to_string()),
    );
    current_attrs.insert(
        "arn".to_string(),
        Value::String("arn:aws:s3:::test".to_string()),
    );
    let current = State::existing(ResourceId::new("s3.Bucket", "test"), current_attrs);

    // User previously only specified "region", not "arn"
    let prev_keys = vec!["region".to_string()];

    let result = diff(&desired, &current, None, Some(&prev_keys), None);
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
fn diff_no_change_without_prev_desired_keys() {
    // Without prev_desired_keys, removed attributes should NOT be detected
    let desired = Resource::new("s3.Bucket", "test")
        .with_attribute("region", Value::String("ap-northeast-1".to_string()));

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::String("ap-northeast-1".to_string()),
    );
    current_attrs.insert(
        "tags".to_string(),
        Value::Map(HashMap::from([(
            "Name".to_string(),
            Value::String("test".to_string()),
        )])),
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
    let resources = vec![
        Resource::new("s3.Bucket", "test")
            .with_attribute("region", Value::String("ap-northeast-1".to_string())),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::String("ap-northeast-1".to_string()),
    );
    attrs.insert(
        "tags".to_string(),
        Value::Map(HashMap::from([(
            "Name".to_string(),
            Value::String("test".to_string()),
        )])),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "test"),
        State::existing(ResourceId::new("s3.Bucket", "test"), attrs),
    );

    let mut prev_desired_keys = HashMap::new();
    prev_desired_keys.insert(
        ResourceId::new("s3.Bucket", "test"),
        vec!["region".to_string(), "tags".to_string()],
    );

    let plan = create_plan(
        &resources,
        &current_states,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &prev_desired_keys,
        &HashMap::new(),
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
    let resources = vec![
        Resource::new("s3.Bucket", "test")
            .with_attribute("region", Value::String("ap-northeast-1".to_string())),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::String("ap-northeast-1".to_string()),
    );
    attrs.insert(
        "tags".to_string(),
        Value::Map(HashMap::from([(
            "Name".to_string(),
            Value::String("test".to_string()),
        )])),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "test"),
        State::existing(ResourceId::new("s3.Bucket", "test"), attrs),
    );

    let mut prev_desired_keys = HashMap::new();
    prev_desired_keys.insert(
        ResourceId::new("s3.Bucket", "test"),
        vec!["region".to_string(), "tags".to_string()],
    );

    // Schema: tags is auto-removable (optional, not create-only),
    // region is explicitly non-removable (provider-inherited)
    let mut schemas = HashMap::new();
    schemas.insert(
        "s3.Bucket".to_string(),
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
        &prev_desired_keys,
        &HashMap::new(),
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
    let resources = vec![
        Resource::new("s3.Bucket", "test")
            .with_attribute("bucket", Value::String("my-bucket".to_string())),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert("bucket".to_string(), Value::String("my-bucket".to_string()));
    attrs.insert(
        "region".to_string(),
        Value::String("ap-northeast-1".to_string()),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "test"),
        State::existing(ResourceId::new("s3.Bucket", "test"), attrs),
    );

    let mut prev_desired_keys = HashMap::new();
    prev_desired_keys.insert(
        ResourceId::new("s3.Bucket", "test"),
        vec!["bucket".to_string(), "region".to_string()],
    );

    // Schema: region is explicitly non-removable, bucket is required
    let mut schemas = HashMap::new();
    schemas.insert(
        "s3.Bucket".to_string(),
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
        &prev_desired_keys,
        &HashMap::new(),
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
    let desired = Resource::new("s3.Bucket", "test")
        .with_attribute("region", Value::String("ap-northeast-1".to_string()));

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::String("ap-northeast-1".to_string()),
    );
    current_attrs.insert(
        "_internal".to_string(),
        Value::String("something".to_string()),
    );
    let current = State::existing(ResourceId::new("s3.Bucket", "test"), current_attrs);

    let prev_keys = vec!["region".to_string(), "_internal".to_string()];

    let result = diff(&desired, &current, None, Some(&prev_keys), None);
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
        Value::String("10.0.0.0/16".to_string()),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    // Lifecycle from state says prevent_destroy
    let mut lifecycles = HashMap::new();
    lifecycles.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        LifecycleConfig {
            prevent_destroy: true,
            ..Default::default()
        },
    );

    let plan = create_plan(
        &[], // no desired resources
        &current_states,
        &lifecycles,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
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
    let mut resource = Resource::new("ec2.Vpc", "my-vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));
    resource.lifecycle.prevent_destroy = true;

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
    let mut resource = Resource::new("s3.Bucket", "my-bucket")
        .with_attribute("versioning", Value::String("Enabled".to_string()));
    resource.lifecycle.prevent_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "versioning".to_string(),
        Value::String("Disabled".to_string()),
    );
    current_states.insert(
        ResourceId::new("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::new("s3.Bucket", "my-bucket"), attrs),
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
    let mut resource = Resource::new("s3.Bucket", "my-bucket")
        .with_attribute("bucket", Value::String("my-bucket".to_string()));
    resource.lifecycle.prevent_destroy = true;

    let resources = vec![resource];

    let plan = create_plan(
        &resources,
        &HashMap::new(), // no current states (resource doesn't exist)
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
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
        Value::String("10.0.0.0/16".to_string()),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::new("ec2.Vpc", "my-vpc"), attrs),
    );

    let plan = create_plan(
        &[], // no desired resources
        &current_states,
        &HashMap::new(), // no lifecycles (default = prevent_destroy: false)
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
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
        Value::String("10.0.0.0/16".to_string()),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "vpc-1"),
        State::existing(ResourceId::new("ec2.Vpc", "vpc-1"), attrs1),
    );
    let mut attrs2 = HashMap::new();
    attrs2.insert(
        "cidr_block".to_string(),
        Value::String("10.1.0.0/16".to_string()),
    );
    current_states.insert(
        ResourceId::new("ec2.Vpc", "vpc-2"),
        State::existing(ResourceId::new("ec2.Vpc", "vpc-2"), attrs2),
    );

    let mut lifecycles = HashMap::new();
    lifecycles.insert(
        ResourceId::new("ec2.Vpc", "vpc-1"),
        LifecycleConfig {
            prevent_destroy: true,
            ..Default::default()
        },
    );
    lifecycles.insert(
        ResourceId::new("ec2.Vpc", "vpc-2"),
        LifecycleConfig {
            prevent_destroy: true,
            ..Default::default()
        },
    );

    let plan = create_plan(
        &[],
        &current_states,
        &lifecycles,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
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
        Value::String("sg-123".to_string()),
    );

    let real_resource = Resource::new("ec2.SecurityGroup", "sg")
        .with_attribute("group_name", Value::String("my-sg".to_string()));

    let resources = vec![virtual_resource, real_resource];

    let plan = create_plan(
        &resources,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    // Only the real resource should generate an effect (Create)
    assert_eq!(plan.effects().len(), 1);
    assert_eq!(
        plan.effects()[0].resource_id(),
        &ResourceId::new("ec2.SecurityGroup", "sg")
    );
}
