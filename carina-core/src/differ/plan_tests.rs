use super::*;

use indexmap::IndexMap;
use std::collections::{BTreeSet, HashSet};

use crate::explicit::ExplicitFields;
use crate::plan::{
    PlanErrorKind, PreventDestroyAction, ReplacementCannotCoexistError, SchemaNotRegisteredError,
};
use crate::resource::{ConcreteValue, DataSource, ResolvedResource, ResourceIdentity};

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

struct HintProvider {
    hints: Vec<crate::wait::BindingPattern>,
}

impl crate::provider::Provider for HintProvider {
    fn name(&self) -> &str {
        "hint"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: Option<&str>,
        _request: crate::provider::ReadRequest,
    ) -> crate::provider::BoxFuture<'_, crate::provider::ProviderResult<State>> {
        Box::pin(async { panic!("unexpected read") })
    }

    fn read_data_source(
        &self,
        _resource: &DataSource,
    ) -> crate::provider::BoxFuture<'_, crate::provider::ProviderResult<State>> {
        Box::pin(async { panic!("unexpected read_data_source") })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: crate::provider::CreateRequest,
    ) -> crate::provider::BoxFuture<
        '_,
        crate::provider::ProviderResult<crate::provider::CreateOutcome>,
    > {
        Box::pin(async { panic!("unexpected create") })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: crate::provider::UpdateRequest,
    ) -> crate::provider::BoxFuture<
        '_,
        crate::provider::ProviderResult<crate::provider::UpdateOutcome>,
    > {
        Box::pin(async { panic!("unexpected update") })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: crate::provider::DeleteRequest,
    ) -> crate::provider::BoxFuture<'_, crate::provider::ProviderResult<()>> {
        Box::pin(async { panic!("unexpected delete") })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: crate::effect::PlanOp) -> Vec<String> {
        Vec::new()
    }

    fn satisfier_hint(
        &self,
        _target_id: &ResourceId,
        _attr_path: &crate::wait::predicate::AttrPath,
    ) -> Vec<crate::wait::BindingPattern> {
        self.hints.clone()
    }
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
fn create_before_destroy_generates_temporary_name_for_unique_name_attribute() {
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
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::string()).create_only())
            .attribute(
                AttributeSchema::new("object_lock_enabled", AttributeType::bool()).create_only(),
            )
            .with_unique_name_attribute("bucket_name"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    let metadata =
        replacement_metadata(&plan, &ResourceId::with_identity("s3.Bucket", "my-bucket"));
    let temp = metadata
        .temporary_name
        .as_ref()
        .expect("Should have temporary_name for create_before_destroy with unique_name_attribute");
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
    match &plan.effects()[metadata.create_idx] {
        Effect::Create(to) => assert_eq!(
            to.get_attr("bucket_name"),
            Some(&Value::Concrete(ConcreteValue::String(
                temp.temporary_value.clone()
            )))
        ),
        other => panic!("Expected replacement Create, got {:?}", other),
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
        ResourceId::with_identity("logs.LogGroup", "my-log-group"),
        State::existing(
            ResourceId::with_identity("logs.LogGroup", "my-log-group"),
            attrs,
        ),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("logs.LogGroup")
            .attribute(
                // log_group_name is NOT create-only in this test (can be renamed)
                AttributeSchema::new("log_group_name", AttributeType::string()),
            )
            .attribute(AttributeSchema::new("kms_key_id", AttributeType::string()).create_only())
            .with_unique_name_attribute("log_group_name"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    let temp = replacement_metadata(
        &plan,
        &ResourceId::with_identity("logs.LogGroup", "my-log-group"),
    )
    .temporary_name
    .as_ref()
    .expect("Should have temporary_name");
    assert_eq!(temp.attribute, "log_group_name");
    assert_eq!(temp.original_value, "my-log-group");
    // log_group_name is not create-only, so can_rename should be true
    assert!(temp.can_rename);
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
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::string()).create_only())
            .attribute(
                AttributeSchema::new("object_lock_enabled", AttributeType::bool()).create_only(),
            )
            .with_unique_name_attribute("bucket_name"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    assert!(
        replacement_metadata(&plan, &ResourceId::with_identity("s3.Bucket", "my-bucket"))
            .temporary_name
            .is_none(),
        "Should not have temporary_name without create_before_destroy"
    );
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
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::string()).create_only())
            .attribute(
                AttributeSchema::new("object_lock_enabled", AttributeType::bool()).create_only(),
            )
            .with_unique_name_attribute("bucket_name"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    assert!(
        replacement_metadata(&plan, &ResourceId::with_identity("s3.Bucket", "my-bucket"))
            .temporary_name
            .is_none(),
        "Should not generate temporary_name when name_prefix is used"
    );
}

#[test]
fn create_before_destroy_without_unique_name_attribute_emits_plan_error() {
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
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::string()).create_only())
            .with_conflicting_replacement(),
        // No unique_name_attribute set
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert!(plan.effects().is_empty());
    assert!(plan.has_errors());
    assert_eq!(
        plan.errors()[0].resource_id,
        ResourceId::with_identity("ec2.Vpc", "my-vpc")
    );
    assert_eq!(
        &plan.errors()[0].kind,
        &PlanErrorKind::ReplacementCannotCoexist(ReplacementCannotCoexistError {
            resource_type: "ec2.Vpc".to_string(),
            resource_identity: "my-vpc".to_string(),
        })
    );
    assert_eq!(
        plan.errors()[0].to_string(),
        "ec2.Vpc.my-vpc: resource type 'ec2.Vpc' does not support create_before_destroy: the replacement cannot coexist with the original"
    );
}

#[test]
fn create_before_destroy_with_coexisting_replacement_uses_no_temporary_name() {
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
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::string()).create_only())
            .with_coexisting_replacement(),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert!(!plan.has_errors(), "unexpected errors: {:?}", plan.errors());
    assert_eq!(plan.effects().len(), 2);
    let metadata = replacement_metadata(&plan, &ResourceId::with_identity("ec2.Vpc", "my-vpc"));
    assert!(metadata.create_before_destroy);
    assert!(
        metadata.create_idx < metadata.delete_idx,
        "create effect must be ordered before delete for CBD"
    );
    assert!(
        metadata.temporary_name.is_none(),
        "coexisting replacements must not generate a temporary name"
    );
    match &plan.effects()[metadata.create_idx] {
        Effect::Create(to) => assert_eq!(
            to.get_attr("cidr_block"),
            Some(&Value::Concrete(ConcreteValue::String(
                "10.1.0.0/16".to_string()
            )))
        ),
        other => panic!("Expected replacement Create, got {:?}", other),
    }
    assert!(matches!(
        &plan.effects()[metadata.delete_idx],
        Effect::Delete { .. }
    ));
}

#[test]
fn create_before_destroy_with_missing_schema_reports_schema_not_registered() {
    let resource = Resource::new("ec2.Vpc", "my-vpc");
    let from = State::existing(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        HashMap::new(),
    );
    let schemas = SchemaRegistry::new();

    let err = plan::temporary_name_for_cbd(&resource, &from, &schemas).unwrap_err();

    assert_eq!(
        PlanErrorKind::from(err),
        PlanErrorKind::SchemaNotRegistered(SchemaNotRegisteredError {
            resource_type: "ec2.Vpc".to_string(),
            resource_identity: "my-vpc".to_string(),
        })
    );
}

#[test]
fn no_temporary_name_when_unique_name_attribute_changes() {
    use crate::schema::{AttributeSchema, AttributeType};

    // unique_name_attribute itself changed: old-bucket → new-bucket
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
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::string()).create_only())
            .attribute(
                AttributeSchema::new("object_lock_enabled", AttributeType::bool()).create_only(),
            )
            .with_unique_name_attribute("bucket_name"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    assert!(
        replacement_metadata(&plan, &ResourceId::with_identity("s3.Bucket", "my-bucket"))
            .temporary_name
            .is_none(),
        "Should not generate temporary_name when unique_name_attribute value changes"
    );
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
    let current = State::existing(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

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
    let current = State::existing(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

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
    let current = State::existing(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

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
fn nested_authored_field_removal_produces_update_patch_without_removed_field() {
    let distribution_id =
        ResourceId::with_identity("awscc.cloudfront.Distribution", "distribution");
    let desired_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    )])));
    let desired = Resource::new("awscc.cloudfront.Distribution", "distribution")
        .with_attribute("distribution_config", desired_config);

    let current_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "enabled".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "web_acl_id".to_string(),
            Value::Concrete(ConcreteValue::String("arn:aws:wafv2:acl/test".to_string())),
        ),
    ])));
    let current_attrs = HashMap::from([("distribution_config".to_string(), current_config)]);
    let current = State::existing(distribution_id.clone(), current_attrs.clone());
    let saved_attrs = HashMap::from([(distribution_id, current_attrs)]);
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "distribution_config".to_string(),
            ExplicitFields::Struct {
                children: HashMap::from([
                    ("enabled".to_string(), ExplicitFields::Leaf),
                    ("web_acl_id".to_string(), ExplicitFields::Leaf),
                ]),
            },
        )]),
    };

    let result = diff(
        &desired,
        &current,
        saved_attrs.get(&desired.id),
        Some(&prev_explicit),
        None,
    );
    let Diff::Update {
        from,
        to,
        changed_attributes,
        ..
    } = result
    else {
        panic!("Expected nested authored removal to produce Update");
    };

    assert_eq!(changed_attributes, vec!["distribution_config".to_string()]);
    let patch =
        crate::provider::build_update_patch(&changed_attributes, &ResolvedResource::new(to), &from);
    assert_eq!(patch.ops.len(), 1);
    assert_eq!(patch.ops[0].kind, crate::provider::PatchOpKind::Replace);
    let Some(Value::Concrete(ConcreteValue::Map(patch_config))) = &patch.ops[0].value else {
        panic!("Expected replacement value to be the desired distribution config");
    };
    assert!(
        !patch_config.contains_key("web_acl_id"),
        "Update patch must not merge the removed nested field back into the desired value"
    );
}

#[test]
fn saved_server_default_nested_field_not_authored_remains_no_change() {
    let bucket_id = ResourceId::with_identity("s3.Bucket", "test");
    let desired_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "rule".to_string(),
        Value::Concrete(ConcreteValue::String("expire".to_string())),
    )])));
    let desired = Resource::new("s3.Bucket", "test")
        .with_attribute("lifecycle_configuration", desired_config);

    let current_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "rule".to_string(),
            Value::Concrete(ConcreteValue::String("expire".to_string())),
        ),
        (
            "transition_default_minimum_object_size".to_string(),
            Value::Concrete(ConcreteValue::String(
                "all_storage_classes_128K".to_string(),
            )),
        ),
    ])));
    let current_attrs = HashMap::from([("lifecycle_configuration".to_string(), current_config)]);
    let current = State::existing(bucket_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "lifecycle_configuration".to_string(),
            ExplicitFields::Struct {
                children: HashMap::from([("rule".to_string(), ExplicitFields::Leaf)]),
            },
        )]),
    };

    let result = diff(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );

    assert!(
        matches!(result, Diff::NoChange(_)),
        "Saved server default absent from prior authoring must remain unmanaged, got {result:?}"
    );
}

#[test]
fn list_union_authoring_does_not_turn_provider_field_into_removal() {
    let resource_id = ResourceId::with_identity("example.Listener", "listener");
    let desired_rules = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(80))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("web".to_string())),
            ),
        ]))),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(443)),
        )]))),
    ]));
    let current_rules = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(80))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("web".to_string())),
            ),
        ]))),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(443))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("provider-default".to_string())),
            ),
        ]))),
    ]));
    let desired =
        Resource::new("example.Listener", "listener").with_attribute("rules", desired_rules);
    let current_attrs = HashMap::from([("rules".to_string(), current_rules)]);
    let current = State::existing(resource_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::List {
                element: Box::new(ExplicitFields::Struct {
                    children: HashMap::from([
                        ("port".to_string(), ExplicitFields::Leaf),
                        ("description".to_string(), ExplicitFields::Leaf),
                    ]),
                }),
            },
        )]),
    };

    let result = diff(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );

    assert!(
        matches!(result, Diff::NoChange(_)),
        "List-union authoring must not unset a field populated on only one element, got {result:?}"
    );
}

#[test]
fn list_elements_authored_field_removal_produces_update_for_paired_element() {
    let resource_id = ResourceId::with_identity("example.Listener", "listener");
    let rule = |port, description: Option<&str>| {
        let mut fields = IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(port)),
        )]);
        if let Some(description) = description {
            fields.insert(
                "description".to_string(),
                Value::Concrete(ConcreteValue::String(description.to_string())),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let desired_rules = Value::Concrete(ConcreteValue::List(vec![rule(80, None), rule(443, None)]));
    let current_rules = Value::Concrete(ConcreteValue::List(vec![
        rule(443, Some("provider-default")),
        rule(80, Some("web")),
    ]));
    let desired =
        Resource::new("example.Listener", "listener").with_attribute("rules", desired_rules);
    let current_attrs = HashMap::from([("rules".to_string(), current_rules)]);
    let current = State::existing(resource_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::ListElements {
                elements: vec![
                    ExplicitFields::Struct {
                        children: HashMap::from([("port".to_string(), ExplicitFields::Leaf)]),
                    },
                    ExplicitFields::Struct {
                        children: HashMap::from([
                            ("port".to_string(), ExplicitFields::Leaf),
                            ("description".to_string(), ExplicitFields::Leaf),
                        ]),
                    },
                ],
            },
        )]),
    };

    let ExplicitFields::Struct { children } = &prev_explicit else {
        unreachable!();
    };
    let effective_desired = crate::resource::merge_with_saved(
        &desired.attributes["rules"],
        crate::resource::SavedValueViews::same(&current_attrs["rules"]),
        &children["rules"],
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );
    let Value::Concrete(ConcreteValue::List(effective_rules)) = effective_desired else {
        panic!("expected effective desired rules list");
    };
    let Value::Concrete(ConcreteValue::Map(port_80)) = &effective_rules[0] else {
        panic!("expected port 80 map");
    };
    let Value::Concrete(ConcreteValue::Map(port_443)) = &effective_rules[1] else {
        panic!("expected port 443 map");
    };
    assert!(!port_80.contains_key("description"));
    assert_eq!(
        port_443.get("description"),
        Some(&Value::Concrete(ConcreteValue::String(
            "provider-default".to_string()
        )))
    );

    let result = diff(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );

    let Diff::Update {
        changed_attributes, ..
    } = result
    else {
        panic!("Expected the paired authored-field removal to produce an Update");
    };
    assert_eq!(changed_attributes, vec!["rules".to_string()]);
}

#[test]
fn unchanged_dsl_after_writeback_pairs_raw_saved_list_without_phantom_removal() {
    let cfg = |with_timeout: bool| {
        let mut fields = IndexMap::from([(
            "retries".to_string(),
            Value::Concrete(ConcreteValue::Int(3)),
        )]);
        if with_timeout {
            fields.insert(
                "timeout".to_string(),
                Value::Concrete(ConcreteValue::Int(30)),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let rule = |with_description: bool, with_timeout: bool| {
        let mut fields = IndexMap::from([("cfg".to_string(), cfg(with_timeout))]);
        if with_description {
            fields.insert(
                "desc".to_string(),
                Value::Concrete(ConcreteValue::String(String::new())),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };

    let authored = Resource::new("example.Listener", "listener").with_attribute(
        "rules",
        Value::Concrete(ConcreteValue::List(vec![
            rule(false, false),
            rule(true, false),
        ])),
    );
    let saved_attributes = HashMap::from([(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            rule(true, true),
            rule(true, true),
        ])),
    )]);
    let prior =
        crate::explicit::build_from_resource_for_stored_values(&authored, &saved_attributes, None);
    let ExplicitFields::Struct { children } = &prior else {
        panic!("expected resource-root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("expected stored-index-aligned ListElements");
    };
    assert!(matches!(elements[1], ExplicitFields::Unrecorded));

    let current = State::existing(authored.id.clone(), saved_attributes.clone());
    let result = diff(
        &authored,
        &current,
        Some(&saved_attributes),
        Some(&prior),
        None,
    );

    assert!(
        matches!(result, Diff::NoChange(_)),
        "unchanged DSL after aligned writeback must not remove a sibling provider default, got {result:?}"
    );
}

#[test]
fn migrated_leaf_authoring_for_map_preserves_provider_nested_fields() {
    let resource_id = ResourceId::with_identity("example.Service", "service");
    let desired_settings = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "mode".to_string(),
        Value::Concrete(ConcreteValue::String("active".to_string())),
    )])));
    let current_settings = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "mode".to_string(),
            Value::Concrete(ConcreteValue::String("active".to_string())),
        ),
        (
            "provider_default".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
    ])));
    let desired =
        Resource::new("example.Service", "service").with_attribute("settings", desired_settings);
    let current_attrs = HashMap::from([("settings".to_string(), current_settings)]);
    let current = State::existing(resource_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([("settings".to_string(), ExplicitFields::Leaf)]),
    };

    let result = diff(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );

    assert!(
        matches!(result, Diff::NoChange(_)),
        "Migrated Leaf authoring must retain nested provider fields, got {result:?}"
    );
}

#[test]
fn nested_reference_removal_updates_consumer_before_deleting_orphan() {
    let distribution_id =
        ResourceId::with_identity("awscc.cloudfront.Distribution", "distribution");
    let web_acl_id = ResourceId::with_identity("awscc.wafv2.WebACL", "web-acl");
    let desired_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    )])));
    let desired = Resource::new("awscc.cloudfront.Distribution", "distribution")
        .with_binding("distribution")
        .with_attribute("distribution_config", desired_config);

    let current_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "enabled".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "web_acl_id".to_string(),
            Value::Concrete(ConcreteValue::String("arn:aws:wafv2:acl/test".to_string())),
        ),
    ])));
    let distribution_attrs = HashMap::from([("distribution_config".to_string(), current_config)]);
    let distribution_state = State::existing(distribution_id.clone(), distribution_attrs.clone())
        .with_dependency_bindings(BTreeSet::from(["web_acl".to_string()]));
    let web_acl_state = State::existing(
        web_acl_id.clone(),
        HashMap::from([(
            "_binding".to_string(),
            Value::Concrete(ConcreteValue::String("web_acl".to_string())),
        )]),
    )
    .with_identifier("arn:aws:wafv2:acl/test");
    let current_states = HashMap::from([
        (distribution_id.clone(), distribution_state),
        (web_acl_id.clone(), web_acl_state),
    ]);
    let saved_attrs = crate::provider::RawSavedAttrs::from_persisted(HashMap::from([(
        distribution_id.clone(),
        distribution_attrs,
    )]))
    .lift(&SchemaRegistry::new());
    let prev_explicit = HashMap::from([(
        distribution_id,
        ExplicitFields::Struct {
            children: HashMap::from([(
                "distribution_config".to_string(),
                ExplicitFields::Struct {
                    children: HashMap::from([
                        ("enabled".to_string(), ExplicitFields::Leaf),
                        ("web_acl_id".to_string(), ExplicitFields::Leaf),
                    ]),
                },
            )]),
        },
    )]);

    let plan = create_plan(
        &[desired],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &saved_attrs,
        &prev_explicit,
        &HashMap::new(),
        &[],
    );

    let update = plan
        .effects()
        .iter()
        .find(|effect| matches!(effect, Effect::Update { .. }))
        .expect("Distribution must be updated");
    let Effect::Update {
        from,
        to,
        changed_attributes,
    } = update
    else {
        unreachable!();
    };
    assert_eq!(changed_attributes, &["distribution_config".to_string()]);
    let patch = crate::provider::build_update_patch(changed_attributes, to, from);
    let Some(Value::Concrete(ConcreteValue::Map(patch_config))) = &patch.ops[0].value else {
        panic!("Expected distribution_config replacement value");
    };
    assert!(!patch_config.contains_key("web_acl_id"));

    let delete = plan
        .effects()
        .iter()
        .find(|effect| effect.resource_id() == &web_acl_id)
        .expect("WebACL must be deleted");
    let Effect::Delete {
        blocked_by_updates, ..
    } = delete
    else {
        panic!("Expected orphan WebACL Delete");
    };
    assert_eq!(
        blocked_by_updates,
        &HashSet::from([ResourceIdentity::new("distribution")])
    );
}

#[test]
fn replaced_producer_delete_waits_for_consumer_that_dropped_prior_reference() {
    use crate::schema::{AttributeSchema, AttributeType};

    let producer_id = ResourceId::with_identity("example.Producer", "producer-id");
    let consumer_id = ResourceId::with_identity("example.Consumer", "consumer-id");
    let producer = Resource::new("example.Producer", "producer-id")
        .with_binding("producer")
        .with_attribute(
            "replace_key",
            Value::Concrete(ConcreteValue::String("new".to_string())),
        );
    let consumer = Resource::new("example.Consumer", "consumer-id").with_binding("consumer");
    let producer_state = State::existing(
        producer_id.clone(),
        HashMap::from([(
            "replace_key".to_string(),
            Value::Concrete(ConcreteValue::String("old".to_string())),
        )]),
    )
    .with_identifier("producer-old");
    let consumer_state = State::existing(
        consumer_id.clone(),
        HashMap::from([(
            "producer_id".to_string(),
            Value::Concrete(ConcreteValue::String("producer-old".to_string())),
        )]),
    )
    .with_dependency_bindings(BTreeSet::from(["producer".to_string()]));
    let current_states = HashMap::from([
        (producer_id.clone(), producer_state),
        (consumer_id.clone(), consumer_state),
    ]);
    let prev_explicit = HashMap::from([(consumer_id, explicit_top_level(&["producer_id"]))]);
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("example.Producer")
            .attribute(AttributeSchema::new("replace_key", AttributeType::string()).create_only()),
    );

    let plan = create_plan(
        &[producer, consumer],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &prev_explicit,
        &HashMap::new(),
        &[],
    );

    let delete = plan
        .effects()
        .iter()
        .find(|effect| matches!(effect, Effect::Delete { id, .. } if id.as_inner() == &producer_id))
        .expect("Replacement must contain the old producer Delete");
    let Effect::Delete {
        blocked_by_updates, ..
    } = delete
    else {
        unreachable!();
    };
    assert_eq!(
        blocked_by_updates,
        &HashSet::from([ResourceIdentity::new("consumer-id")])
    );
}

#[test]
fn prior_consumer_edge_is_not_added_when_desired_still_depends_on_delete_binding() {
    let dependency_id = ResourceId::with_identity("example.Dependency", "dependency-id");
    let consumer_id = ResourceId::with_identity("example.Consumer", "consumer-id");
    let consumer = Resource::new("example.Consumer", "consumer-id")
        .with_binding("consumer")
        .with_dependency_bindings(BTreeSet::from(["dependency".to_string()]))
        .with_attribute("version", Value::Concrete(ConcreteValue::Int(2)));
    let consumer_state = State::existing(
        consumer_id.clone(),
        HashMap::from([(
            "version".to_string(),
            Value::Concrete(ConcreteValue::Int(1)),
        )]),
    )
    .with_dependency_bindings(BTreeSet::from(["dependency".to_string()]));
    let dependency_state = State::existing(
        dependency_id.clone(),
        HashMap::from([(
            "_binding".to_string(),
            Value::Concrete(ConcreteValue::String("dependency".to_string())),
        )]),
    )
    .with_identifier("dependency-old");
    let current_states = HashMap::from([
        (consumer_id, consumer_state),
        (dependency_id.clone(), dependency_state),
    ]);

    let plan = create_plan(
        &[consumer],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    let delete = plan
        .effects()
        .iter()
        .find(|effect| effect.resource_id() == &dependency_id)
        .expect("Orphan dependency must be deleted");
    let Effect::Delete {
        blocked_by_updates, ..
    } = delete
    else {
        panic!("Expected orphan Delete");
    };
    assert!(
        blocked_by_updates.is_empty(),
        "The inverse pass must not add an edge while the desired consumer still depends on the binding"
    );
}

#[test]
fn orphan_delete_waits_for_consumer_update_from_prior_depends_on() {
    let dependency_id = ResourceId::with_identity("example.Dependency", "dependency-id");
    let consumer_id = ResourceId::with_identity("example.Consumer", "consumer-id");
    let consumer = Resource::new("example.Consumer", "consumer-id")
        .with_binding("consumer")
        .with_attribute("version", Value::Concrete(ConcreteValue::Int(2)));
    let consumer_state = State::existing(
        consumer_id.clone(),
        HashMap::from([(
            "version".to_string(),
            Value::Concrete(ConcreteValue::Int(1)),
        )]),
    );
    let dependency_state = State::existing(
        dependency_id.clone(),
        HashMap::from([(
            "_binding".to_string(),
            Value::Concrete(ConcreteValue::String("dependency".to_string())),
        )]),
    )
    .with_identifier("dependency-old");
    let current_states = HashMap::from([
        (consumer_id.clone(), consumer_state),
        (dependency_id.clone(), dependency_state),
    ]);
    let directives_map = HashMap::from([(
        consumer_id,
        Directives {
            depends_on: vec!["dependency".to_string()],
            ..Default::default()
        },
    )]);

    let plan = create_plan(
        &[consumer],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &directives_map,
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );

    let delete = plan
        .effects()
        .iter()
        .find(|effect| effect.resource_id() == &dependency_id)
        .expect("Orphan dependency must be deleted");
    let Effect::Delete {
        blocked_by_updates, ..
    } = delete
    else {
        panic!("Expected orphan Delete");
    };
    assert_eq!(
        blocked_by_updates,
        &HashSet::from([ResourceIdentity::new("consumer-id")])
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
    let current = State::existing(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

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
    let current = State::existing(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

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
        ResourceId::with_identity("s3.Bucket", "test"),
        State::existing(ResourceId::with_identity("s3.Bucket", "test"), attrs),
    );

    let mut prev_explicit = HashMap::new();
    prev_explicit.insert(
        ResourceId::with_identity("s3.Bucket", "test"),
        explicit_top_level(&["region", "tags"]),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
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
        ResourceId::with_identity("s3.Bucket", "test"),
        State::existing(ResourceId::with_identity("s3.Bucket", "test"), attrs),
    );

    let mut prev_explicit = HashMap::new();
    prev_explicit.insert(
        ResourceId::with_identity("s3.Bucket", "test"),
        explicit_top_level(&["region", "tags"]),
    );

    // Schema: tags is auto-removable (optional, not create-only),
    // region is explicitly non-removable (provider-inherited)
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("region", AttributeType::string()).non_removable())
            .attribute(AttributeSchema::new(
                "tags",
                AttributeType::map(AttributeType::string()),
            )),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
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
        ResourceId::with_identity("s3.Bucket", "test"),
        State::existing(ResourceId::with_identity("s3.Bucket", "test"), attrs),
    );

    let mut prev_explicit = HashMap::new();
    prev_explicit.insert(
        ResourceId::with_identity("s3.Bucket", "test"),
        explicit_top_level(&["bucket", "region"]),
    );

    // Schema: region is explicitly non-removable, bucket is required
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket", AttributeType::string()).required())
            .attribute(AttributeSchema::new("region", AttributeType::string()).non_removable()),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
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
    let current = State::existing(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

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
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    // Directives from state say prevent_destroy
    let mut directives_map = HashMap::new();
    directives_map.insert(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        Directives {
            prevent_destroy: true,
            ..Default::default()
        },
    );

    let plan = create_plan(
        &[],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &directives_map,
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
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
    assert_eq!(
        &plan.errors()[0].kind,
        &PlanErrorKind::PreventDestroy {
            action: PreventDestroyAction::DeleteRemovedFromConfiguration
        }
    );
    assert_eq!(
        plan.errors()[0].resource_id,
        ResourceId::with_identity("ec2.Vpc", "my-vpc")
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
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::string()).create_only()),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
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
    assert_eq!(
        &plan.errors()[0].kind,
        &PlanErrorKind::PreventDestroy {
            action: PreventDestroyAction::Replace
        }
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
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        State::existing(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
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
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(), // no current states (resource doesn't exist)
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
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
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        State::existing(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    let plan = create_plan(
        &[],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(), // no directives (default = prevent_destroy: false)
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
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
        ResourceId::with_identity("ec2.Vpc", "vpc-1"),
        State::existing(ResourceId::with_identity("ec2.Vpc", "vpc-1"), attrs1),
    );
    let mut attrs2 = HashMap::new();
    attrs2.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
    );
    current_states.insert(
        ResourceId::with_identity("ec2.Vpc", "vpc-2"),
        State::existing(ResourceId::with_identity("ec2.Vpc", "vpc-2"), attrs2),
    );

    let mut directives_map = HashMap::new();
    directives_map.insert(
        ResourceId::with_identity("ec2.Vpc", "vpc-1"),
        Directives {
            prevent_destroy: true,
            ..Default::default()
        },
    );
    directives_map.insert(
        ResourceId::with_identity("ec2.Vpc", "vpc-2"),
        Directives {
            prevent_destroy: true,
            ..Default::default()
        },
    );

    let plan = create_plan(
        &[],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &directives_map,
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
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

// carina#3181: the former `virtual_resources_are_skipped_in_plan` test
// is obsolete — `create_plan` now takes a `&[Resource]` /
// `&[DataSource]` pair, so a `Composition` cannot be passed into the
// plan input at all. The "compositions produce no effect" invariant is now
// enforced by the type system rather than a runtime skip.

#[test]
fn wait_binding_lowers_to_wait_effect() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    // A downstream consumer that references the wait binding and is
    // itself a pending change (Create) — carina#3101: the wait is
    // emitted only when it gates a real downstream change.
    let mut consumer = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    consumer
        .dependency_bindings
        .insert("cert_issued".to_string());
    let resources = vec![cert, consumer];

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
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );

    let wait_effect = plan
        .effects()
        .iter()
        .find(|e| e.is_wait())
        .expect("expected Wait effect");
    let Effect::Wait {
        identity,
        target_id,
        until,
        until_surface,
        timeout,
        ..
    } = wait_effect
    else {
        unreachable!();
    };
    assert_eq!(identity.as_str(), "cert_issued");
    assert_eq!(target_id.identity_str(), "cert");
    assert_eq!(target_id.resource_type, "acm.Certificate");
    assert_eq!(
        until,
        &WaitPredicate::Equals {
            attr: AttrPath::single("status"),
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
fn create_plan_records_error_for_empty_predicate_attr_path() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    // The wait must gate a pending downstream change, otherwise
    // create_plan legitimately skips emitting or lowering the wait.
    let mut consumer = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    consumer
        .dependency_bindings
        .insert("cert_issued".to_string());
    let resources = vec![cert, consumer];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );

    assert!(
        plan.errors().iter().any(|err| {
            matches!(
                &err.kind,
                PlanErrorKind::WaitPredicateInvalid {
                    wait_binding,
                    reason,
                } if wait_binding == "cert_issued" && reason.contains("empty")
            )
        }),
        "expected invalid empty predicate attr path error, got {:?}",
        plan.errors()
    );
}

#[test]
fn wait_provider_satisfier_hint_augments_explicit_dependencies() {
    use crate::effect::Effect;
    use crate::wait::BindingPattern;

    let parsed = crate::parser::parse_and_resolve(
        r#"
        provider aws {
            region = "us-east-1"
        }

        let cert = aws.acm.Certificate {
            domain_name       = "example.com"
            validation_method = "DNS"
        }

        let validation_records = aws.route53.RecordSet {
            name = "_acme.example.com"
        }

        let cert_issued = wait cert {
            until = cert.status == "ISSUED"
        }

        let dist = aws.cloudfront.Distribution {
            cert_status = cert_issued.status
        }
        "#,
    )
    .expect("parsed file should be valid");
    let provider = HintProvider {
        hints: vec![BindingPattern::Exact("validation_records".to_string())],
    };

    let plan = create_plan(
        &parsed.resources,
        &parsed.data_sources,
        &provider,
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &parsed.wait_bindings,
    );

    let Effect::Wait {
        explicit_dependencies,
        ..
    } = plan
        .effects()
        .iter()
        .find(|effect| effect.is_wait())
        .expect("wait should be emitted for changed downstream consumer")
    else {
        unreachable!();
    };
    assert!(
        explicit_dependencies.contains("validation_records"),
        "provider satisfier hint should add validation_records; got {explicit_dependencies:?}"
    );
}

#[test]
fn wait_user_depends_on_conflict_with_provider_hint_deduplicates() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::wait::BindingPattern;

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    let dependency = Resource::new("route53.RecordSet", "record").with_binding("something");
    let mut consumer = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    consumer
        .dependency_bindings
        .insert("cert_issued".to_string());
    let resources = vec![cert, dependency, consumer];
    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        timeout_secs: None,
        depends_on: vec!["something".into()],
        line: 1,
    };
    let provider = HintProvider {
        hints: vec![BindingPattern::Exact("something".to_string())],
    };

    let plan = create_plan(
        &resources,
        &[],
        &provider,
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );

    let Effect::Wait {
        explicit_dependencies,
        ..
    } = plan
        .effects()
        .iter()
        .find(|effect| effect.is_wait())
        .expect("wait should be emitted")
    else {
        unreachable!();
    };
    assert_eq!(explicit_dependencies.len(), 1);
    assert!(explicit_dependencies.contains("something"));
}

#[test]
fn wait_uses_schema_default_timeout_when_omitted() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    // Downstream consumer with a pending change so the wait gates
    // something (carina#3101).
    let mut consumer = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    consumer
        .dependency_bindings
        .insert("cert_issued".to_string());
    let resources = vec![cert, consumer];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("acm.Certificate")
            .attribute(AttributeSchema::new("status", AttributeType::string()))
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
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );
    let Effect::Wait {
        timeout, interval, ..
    } = plan
        .effects()
        .iter()
        .find(|e| e.is_wait())
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
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );
    assert!(
        !plan.errors().is_empty(),
        "missing-target wait should surface a plan error"
    );
    assert!(
        matches!(
            &plan.errors()[0].kind,
            PlanErrorKind::WaitTargetMissing { target, .. } if target == "nonexistent"
        ),
        "error message should mention the missing target, got: {}",
        plan.errors()[0]
    );
}

/// carina#3101: a `wait` gates nothing when every downstream consumer
/// is unchanged — no `Effect::Wait` (no lone `> binding (until …)`
/// header on a 0-change plan, no apply-time poll). Mirrors the real
/// `carina-rs/infra registry/dev/registry` shape: cert (unchanged) +
/// distribution (unchanged) referencing `cert_issued`.
#[test]
fn wait_omitted_when_all_consumers_unchanged() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    // Both resources already exist with identical state → NoChange,
    // so neither produces a mutating effect.
    let mut current_states = HashMap::new();
    current_states.insert(
        ResourceId::with_identity("acm.Certificate", "cert"),
        State::existing(
            ResourceId::with_identity("acm.Certificate", "cert"),
            HashMap::new(),
        ),
    );
    current_states.insert(
        ResourceId::with_identity("cloudfront.Distribution", "dist"),
        State::existing(
            ResourceId::with_identity("cloudfront.Distribution", "dist"),
            HashMap::new(),
        ),
    );

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );

    assert!(
        !plan.effects().iter().any(|e| e.is_wait()),
        "carina#3101: no Effect::Wait when every consumer is unchanged; \
         effects were {:?}",
        plan.effects()
    );
}

/// carina#3101 counterpart (two-faced invariant, like carina#3085):
/// the wait IS emitted when a downstream consumer has a pending change
/// — the dependency-edge behavior (carina#3085 / carina#3061) must not
/// regress just because the no-op case is now suppressed.
#[test]
fn wait_emitted_when_a_consumer_has_a_pending_change() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    // `dist` is new (absent from current_states) → Create → mutating.
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );

    assert!(
        plan.effects().iter().any(
            |e| matches!(e, Effect::Wait { identity, .. } if identity.as_str() == "cert_issued")
        ),
        "carina#3101: the wait must still be emitted when a consumer \
         has a pending change (carina#3085/#3061 not regressed); \
         effects were {:?}",
        plan.effects()
    );
}

/// carina#3358: an already-satisfied wait whose target is unchanged must
/// be elided even when a consumer has a pending change that merely
/// *references* the wait. The wait has no work to do — on `apply` its
/// `until` predicate is already true, so it would poll-and-return
/// immediately, contributing nothing. Mirrors the registry-dev stack:
/// cert (unchanged, already ISSUED) + a changed distribution that wires
/// in `web_acl_id` while still referencing `cert_issued`.
#[test]
fn wait_omitted_when_already_satisfied_and_target_unchanged() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    // cert: exists with status == ISSUED and is unchanged (desired
    // matches state) → NoChange, no mutating effect on the target.
    let cert = Resource::new("acm.Certificate", "cert")
        .with_binding("cert")
        .with_attribute(
            "status",
            Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        );
    // dist is new (absent from current_states) → Create → mutating.
    // It references the wait binding, so the wait "gates a pending change".
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    let mut cert_state_attrs = HashMap::new();
    cert_state_attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
    );
    let mut current_states = HashMap::new();
    current_states.insert(
        ResourceId::with_identity("acm.Certificate", "cert"),
        State::existing(
            ResourceId::with_identity("acm.Certificate", "cert"),
            cert_state_attrs,
        )
        .with_identifier("arn:aws:acm:::certificate/abc"),
    );

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );

    assert!(
        !plan.effects().iter().any(|e| e.is_wait()),
        "carina#3358: an already-satisfied wait whose target is unchanged \
         must be elided even when a consumer has a pending change that \
         merely references it; effects were {:?}",
        plan.effects()
    );
}

/// carina#3358 counterpart: when the wait's target *is* changing in the
/// same plan, the cached state is stale — its post-apply attributes are
/// unknown, so the wait must still be emitted to re-poll on `apply`.
/// Here the target is new and has no current state entry.
#[test]
fn wait_emitted_when_target_is_changing_even_if_cached_state_satisfies() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};

    // cert is new (absent from current_states) → Create → mutating
    // target. Even though no cached state exists, the wait must poll
    // post-apply.
    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );

    assert!(
        plan.effects().iter().any(
            |e| matches!(e, Effect::Wait { identity, .. } if identity.as_str() == "cert_issued")
        ),
        "carina#3358: the wait must still be emitted when its target is \
         changing in this plan (cached state is stale); effects were {:?}",
        plan.effects()
    );
}

/// carina#3358: pins the existing-target + `target_is_mutating`
/// branch of `wait_has_work`. An existing target whose cached state
/// already satisfies the predicate, but which has a pending `Update` in
/// this plan, must still emit the wait: the cached `ISSUED` is stale
/// relative to the about-to-apply change, so the executor must re-poll.
/// Without this test the `target_is_mutating ||` half of the gate is
/// dead under the suite and could be silently dropped.
#[test]
fn wait_emitted_when_known_target_has_pending_update_even_if_cached_state_satisfies() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};

    // cert EXISTS in state and its cached `status` already satisfies
    // the predicate, BUT it has a
    // pending Update (desired `domain_name` differs from state).
    let cert = Resource::new("acm.Certificate", "cert")
        .with_binding("cert")
        .with_attribute(
            "domain_name",
            Value::Concrete(ConcreteValue::String("new.example.com".to_string())),
        );
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    let mut cert_state_attrs = HashMap::new();
    cert_state_attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
    );
    cert_state_attrs.insert(
        "domain_name".to_string(),
        Value::Concrete(ConcreteValue::String("old.example.com".to_string())),
    );
    let mut current_states = HashMap::new();
    current_states.insert(
        ResourceId::with_identity("acm.Certificate", "cert"),
        State::existing(
            ResourceId::with_identity("acm.Certificate", "cert"),
            cert_state_attrs,
        )
        .with_identifier("arn:aws:acm:::certificate/abc"),
    );

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &HashMap::new(),
        &[wait],
    );

    // Precondition: cert really has a pending Update, so this hits the
    // `target_is_mutating` arm for an existing state row and not the
    // `target_needs_wait` arm.
    assert!(
        plan.effects()
            .iter()
            .any(|e| matches!(e, Effect::Update { to, .. }
            if to.id == ResourceId::with_identity("acm.Certificate", "cert"))),
        "test precondition: cert must have a pending Update; effects were {:?}",
        plan.effects()
    );
    assert!(
        plan.effects().iter().any(
            |e| matches!(e, Effect::Wait { identity, .. } if identity.as_str() == "cert_issued")
        ),
        "carina#3358: the wait must still be emitted when an existing \
         (Known) target has a pending Update, even though its cached \
         state satisfies the predicate; effects were {:?}",
        plan.effects()
    );
}
