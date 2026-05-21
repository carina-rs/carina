use super::*;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
use std::time::Duration;

#[test]
fn build_state_after_apply_finds_write_only_with_provider_prefix() {
    // The schema map is keyed by provider-prefixed names (e.g., "awscc.ec2.Vpc"),
    // but the buggy code used resource.id.resource_type (e.g., "ec2.Vpc") for lookup.
    // This test verifies that write-only attributes are found when the schema key
    // includes the provider prefix.
    let mut schemas = SchemaRegistry::new();
    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
        .attribute(AttributeSchema::new("ipv4_netmask_length", AttributeType::Int).write_only());
    // Schema is registered with provider-prefixed key
    schemas.insert("awscc", schema);

    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    resource.set_attr(
        "ipv4_netmask_length".to_string(),
        Value::Concrete(ConcreteValue::String("16".to_string())),
    );

    let sorted_resources = vec![resource];

    // Simulate provider returning state without the write-only attribute
    let mut applied_attrs = HashMap::new();
    applied_attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    let applied_state = State::existing(sorted_resources[0].id.clone(), applied_attrs);
    let mut applied_states = HashMap::new();
    applied_states.insert(sorted_resources[0].id.clone(), applied_state);

    let current_states = HashMap::new();
    let permanent_name_overrides = HashMap::new();
    let plan = Plan::new();
    let successfully_deleted = HashSet::new();
    let failed_refreshes = HashSet::new();

    let result = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        current_states: &current_states,
        applied_states: &applied_states,
        permanent_name_overrides: &permanent_name_overrides,
        plan: &plan,
        successfully_deleted: &successfully_deleted,
        failed_refreshes: &failed_refreshes,
        schemas: &schemas,
    })
    .unwrap();

    // The write-only attribute should be merged from the desired resource into state
    let saved = result
        .find_resource("awscc", "ec2.Vpc", "my-vpc")
        .expect("resource should exist in state");
    assert_eq!(
        saved.attributes.get("ipv4_netmask_length"),
        Some(&serde_json::Value::String("16".to_string())),
        "write-only attribute should be persisted in state"
    );
}

#[test]
fn build_state_after_apply_preserves_block_name_attribute() {
    // When a block_name attribute (e.g., "policies" with block_name "policy")
    // is carried over by the provider because CloudControl doesn't return it,
    // the state after apply should include the attribute under the canonical name.
    // This is the scenario in issue #1499 (iam_role/with_policy).
    use carina_core::schema::StructField;

    let mut schemas = SchemaRegistry::new();
    let schema = ResourceSchema::new("iam.role")
        .attribute(AttributeSchema::new("role_name", AttributeType::String).create_only())
        .attribute(
            AttributeSchema::new(
                "policies",
                AttributeType::unordered_list(AttributeType::Struct {
                    name: "Policy".to_string(),
                    fields: vec![
                        StructField::new("policy_name", AttributeType::String).required(),
                        StructField::new("policy_document", AttributeType::String).required(),
                    ],
                }),
            )
            .with_block_name("policy"),
        );
    schemas.insert("awscc", schema);

    // Resource with resolved block name (policy -> policies)
    let mut resource = Resource::with_provider("awscc", "iam.role", "test-role", None);
    resource.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("test-role".to_string())),
    );
    resource.set_attr(
        "policies".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::Concrete(ConcreteValue::String("test-policy".to_string())),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::Concrete(ConcreteValue::String("{}".to_string())),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )])),
    );

    let sorted_resources = vec![resource];

    // Simulate provider returning state WITH carried-over policies attribute
    // (This is what AwsccProvider::create_resource does in the carry-over logic)
    let mut applied_attrs = HashMap::new();
    applied_attrs.insert(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("test-role".to_string())),
    );
    applied_attrs.insert(
        "policies".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::Concrete(ConcreteValue::String("test-policy".to_string())),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::Concrete(ConcreteValue::String("{}".to_string())),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )])),
    );
    let applied_state = State::existing(sorted_resources[0].id.clone(), applied_attrs)
        .with_identifier("some-identifier");
    let mut applied_states = HashMap::new();
    applied_states.insert(sorted_resources[0].id.clone(), applied_state);

    let current_states = HashMap::new();
    let permanent_name_overrides = HashMap::new();
    let plan = Plan::new();
    let successfully_deleted = HashSet::new();
    let failed_refreshes = HashSet::new();

    let state = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        current_states: &current_states,
        applied_states: &applied_states,
        permanent_name_overrides: &permanent_name_overrides,
        plan: &plan,
        successfully_deleted: &successfully_deleted,
        failed_refreshes: &failed_refreshes,
        schemas: &schemas,
    })
    .unwrap();

    // Verify state has the policies attribute
    let saved = state
        .find_resource("awscc", "iam.role", "test-role")
        .expect("resource should exist in state");
    assert!(
        saved.attributes.contains_key("policies"),
        "state should contain 'policies' attribute (carried over from desired)"
    );

    // Verify explicit tree includes "policies" (canonical name, not "policy")
    let carina_core::explicit::ExplicitFields::Struct {
        children: explicit_children,
    } = &saved.explicit
    else {
        panic!("saved.explicit must be Struct, got: {:?}", saved.explicit);
    };
    assert!(
        explicit_children.contains_key("policies"),
        "explicit children should contain 'policies': {:?}",
        explicit_children.keys().collect::<Vec<_>>()
    );

    // Now simulate second plan: build_saved_attrs should return the policies
    let saved_attrs = state.build_saved_attrs();
    let id =
        carina_core::resource::ResourceId::with_provider("awscc", "iam.role", "test-role", None);
    let attrs = saved_attrs.get(&id).unwrap();
    assert!(
        attrs.contains_key("policies"),
        "saved_attrs should contain 'policies': {:?}",
        attrs.keys().collect::<Vec<_>>()
    );
}

#[test]
fn block_name_attribute_no_diff_when_hydrated() {
    // After apply, the state file contains the block_name attribute (canonical name).
    // On re-plan, if hydrate_read_state restores it into current_states,
    // the differ should see no changes.
    //
    // This tests the scenario from issue #1499 where plan-verify fails
    // because the block_name attribute shows as an addition.
    use carina_core::differ::diff;
    use carina_core::schema::StructField;

    let schema = ResourceSchema::new("awscc.iam.role")
        .attribute(AttributeSchema::new("role_name", AttributeType::String).create_only())
        .attribute(
            AttributeSchema::new(
                "policies",
                AttributeType::unordered_list(AttributeType::Struct {
                    name: "Policy".to_string(),
                    fields: vec![
                        StructField::new("policy_name", AttributeType::String).required(),
                        StructField::new("policy_document", AttributeType::String).required(),
                    ],
                }),
            )
            .with_block_name("policy"),
        );

    // Desired resource (after resolve_block_names: "policy" -> "policies")
    let mut resource = Resource::with_provider("awscc", "iam.role", "test-role", None);
    resource.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("test-role".to_string())),
    );
    resource.set_attr(
        "policies".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::Concrete(ConcreteValue::String("test-policy".to_string())),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::Concrete(ConcreteValue::String("{}".to_string())),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )])),
    );

    // Current state: simulate hydration restoring the policies attribute
    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("test-role".to_string())),
    );
    state_attrs.insert(
        "policies".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::Concrete(ConcreteValue::String("test-policy".to_string())),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::Concrete(ConcreteValue::String("{}".to_string())),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )])),
    );
    let current = State::existing(resource.id.clone(), state_attrs).with_identifier("some-id");

    // Saved attrs: same as current (from previous apply)
    let saved: HashMap<String, Value> = current.attributes.clone();

    // Previous explicit tree: what the user wrote on first apply
    let prev_explicit = carina_core::explicit::ExplicitFields::Struct {
        children: std::collections::HashMap::from([
            (
                "policies".to_string(),
                carina_core::explicit::ExplicitFields::Leaf,
            ),
            (
                "role_name".to_string(),
                carina_core::explicit::ExplicitFields::Leaf,
            ),
        ]),
    };

    let d = diff(
        &resource,
        &current,
        Some(&saved),
        Some(&prev_explicit),
        Some(&schema),
    );

    assert!(
        matches!(d, carina_core::differ::Diff::NoChange(_)),
        "Expected no change, but got: {:?}",
        d
    );
}

#[test]
fn block_name_attribute_state_roundtrip() {
    // Verify that block_name attributes (saved under canonical name in state)
    // roundtrip correctly through state save/load, meaning the saved_attrs
    // returned by build_saved_attrs have the correct canonical key.
    //
    // This covers the ec2_ipam case (operating_region -> operating_regions)
    // from issue #1499.
    use carina_core::schema::StructField;

    let mut schemas = SchemaRegistry::new();
    let schema = ResourceSchema::new("ec2.ipam")
        .attribute(
            AttributeSchema::new(
                "operating_regions",
                AttributeType::unordered_list(AttributeType::Struct {
                    name: "IpamOperatingRegion".to_string(),
                    fields: vec![StructField::new("region_name", AttributeType::String).required()],
                }),
            )
            .with_block_name("operating_region"),
        )
        .attribute(AttributeSchema::new("description", AttributeType::String));
    schemas.insert("awscc", schema);

    // Resource with resolved block name
    let mut resource = Resource::with_provider("awscc", "ec2.ipam", "test-ipam", None);
    resource.set_attr(
        "operating_regions".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![(
                    "region_name".to_string(),
                    Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
                )]
                .into_iter()
                .collect(),
            ),
        )])),
    );
    resource.set_attr(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("test IPAM".to_string())),
    );

    let sorted_resources = vec![resource];

    // Simulate provider state with carried-over operating_regions
    let mut applied_attrs = HashMap::new();
    applied_attrs.insert(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("test IPAM".to_string())),
    );
    applied_attrs.insert(
        "operating_regions".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![(
                    "region_name".to_string(),
                    Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
                )]
                .into_iter()
                .collect(),
            ),
        )])),
    );
    let applied_state = State::existing(sorted_resources[0].id.clone(), applied_attrs)
        .with_identifier("ipam-12345");
    let mut applied_states = HashMap::new();
    applied_states.insert(sorted_resources[0].id.clone(), applied_state);

    let state = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        current_states: &HashMap::new(),
        applied_states: &applied_states,
        permanent_name_overrides: &HashMap::new(),
        plan: &Plan::new(),
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &schemas,
    })
    .unwrap();

    // Verify state contains operating_regions
    let saved_rs = state
        .find_resource("awscc", "ec2.ipam", "test-ipam")
        .expect("resource should exist");
    assert!(
        saved_rs.attributes.contains_key("operating_regions"),
        "state should contain 'operating_regions'"
    );
    let carina_core::explicit::ExplicitFields::Struct {
        children: explicit_children,
    } = &saved_rs.explicit
    else {
        panic!(
            "saved_rs.explicit must be Struct, got: {:?}",
            saved_rs.explicit
        );
    };
    assert!(
        explicit_children.contains_key("operating_regions"),
        "explicit children should contain 'operating_regions'"
    );

    // Verify roundtrip through saved_attrs
    let saved_attrs = state.build_saved_attrs();
    let id =
        carina_core::resource::ResourceId::with_provider("awscc", "ec2.ipam", "test-ipam", None);
    let attrs = saved_attrs.get(&id).unwrap();
    let operating_regions = attrs
        .get("operating_regions")
        .expect("should have operating_regions");

    // Verify the value structure is preserved
    if let Value::Concrete(ConcreteValue::List(items)) = operating_regions {
        assert_eq!(items.len(), 1);
        if let Value::Concrete(ConcreteValue::Map(map)) = &items[0] {
            assert_eq!(
                map.get("region_name"),
                Some(&Value::Concrete(ConcreteValue::String(
                    "ap-northeast-1".to_string()
                )))
            );
        } else {
            panic!("Expected Map in list, got {:?}", items[0]);
        }
    } else {
        panic!("Expected List, got {:?}", operating_regions);
    }
}

/// Move + Replace targeting the same `to` ResourceId must end up with
/// the post-Replace `identifier` and `attributes` in state, not the
/// pre-Replace values inherited from the `from` row.
///
/// Regression coverage for carina#3170 (root cause of carina#3167).
/// Before the WritebackPlan refactor, `build_state_after_apply`
/// processed `Effect::Move` in a second loop that copied the
/// pre-Replace `from` row's contents into the `to` row via
/// `upsert_resource`, overwriting Phase 1's post-Replace Upsert. The
/// result was a state row with the new SimHash address but the
/// pre-Replace identifier and attributes, which on the next plan
/// produces a spurious `+ Create` because `provider.read()` against
/// the stale identifier returns `NoSuchEntity`.
#[test]
fn move_plus_replace_keeps_post_replace_identifier_and_attributes() {
    use carina_core::effect::Effect;
    use carina_core::resource::Directives;
    use carina_state::{ResourceState, StateFile};

    let mut schemas = SchemaRegistry::new();
    let schema = ResourceSchema::new("iam.RolePolicy")
        .attribute(AttributeSchema::new("role_name", AttributeType::String).create_only())
        .attribute(AttributeSchema::new("policy_name", AttributeType::String).create_only())
        .attribute(AttributeSchema::new(
            "policy_document",
            AttributeType::String,
        ));
    schemas.insert("awscc", schema);

    // Desired resource lives at the post-rename SimHash address with
    // the post-rename role_name + policy_name (the values the user
    // wrote in .crn after the IAM Role rename).
    let new_id = ResourceId::with_provider(
        "awscc",
        "iam.RolePolicy",
        "rd.awscc_iam_role_policy_0cd2c914",
        None,
    );
    let mut resource = Resource {
        id: new_id.clone(),
        ..Resource::with_provider(
            new_id.provider.clone(),
            new_id.resource_type.clone(),
            new_id.name.as_str(),
            None,
        )
    };
    resource.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "carina-registry-infra-deploy".to_string(),
        )),
    );
    resource.set_attr(
        "policy_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "carina-registry-infra-deploy-inline".to_string(),
        )),
    );
    resource.set_attr(
        "policy_document".to_string(),
        Value::Concrete(ConcreteValue::String("{}".to_string())),
    );
    let sorted_resources = vec![resource];

    // applied_states holds the provider's post-create State for the
    // new address: new identifier + new attribute values.
    let mut applied_attrs = HashMap::new();
    applied_attrs.insert(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "carina-registry-infra-deploy".to_string(),
        )),
    );
    applied_attrs.insert(
        "policy_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "carina-registry-infra-deploy-inline".to_string(),
        )),
    );
    applied_attrs.insert(
        "policy_document".to_string(),
        Value::Concrete(ConcreteValue::String("{}".to_string())),
    );
    let applied = State::existing(new_id.clone(), applied_attrs)
        .with_identifier("carina-registry-infra-deploy-inline|carina-registry-infra-deploy");
    let mut applied_states = HashMap::new();
    applied_states.insert(new_id.clone(), applied);

    // Pre-existing state file has the old SimHash address row, with
    // the pre-rename role_name + policy_name and the pre-rename
    // identifier. materialize_moved_states normally transfers this
    // row's State to the new id in current_states; build_state_after_apply
    // is invoked here after that transfer would have happened, so the
    // saved `from` row in the file still has the old values.
    let old_row = ResourceState::new(
        "iam.RolePolicy",
        "rd.awscc_iam_role_policy_02942703",
        "awscc",
    )
    .with_identifier("carina-registry-deploy-inline|carina-registry-deploy")
    .with_attribute(
        "role_name",
        serde_json::Value::String("carina-registry-deploy".to_string()),
    )
    .with_attribute(
        "policy_name",
        serde_json::Value::String("carina-registry-deploy-inline".to_string()),
    )
    .with_attribute(
        "policy_document",
        serde_json::Value::String("{}".to_string()),
    );
    let mut state_file = StateFile::default();
    state_file.resources.push(old_row);

    // Plan has Replace (handled via applied_states in Phase 1) plus
    // Move from the old address to the new one (Phase 2).
    let mut plan = Plan::new();
    let from_id = ResourceId::with_provider(
        "awscc",
        "iam.RolePolicy",
        "rd.awscc_iam_role_policy_02942703",
        None,
    );
    plan.add(Effect::Replace {
        id: new_id.clone(),
        from: Box::new(State::existing(from_id.clone(), HashMap::new())),
        to: sorted_resources[0].clone(),
        directives: Directives::default(),
        changed_create_only: vec!["role_name".to_string(), "policy_name".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });
    plan.add(Effect::Move {
        from: from_id.clone(),
        to: new_id.clone(),
    });

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(state_file),
        sorted_resources: &sorted_resources,
        current_states: &HashMap::new(),
        applied_states: &applied_states,
        permanent_name_overrides: &HashMap::new(),
        plan: &plan,
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &schemas,
    })
    .expect("writeback should succeed");

    // The old address must no longer exist in state.
    assert!(
        saved
            .find_resource(
                "awscc",
                "iam.RolePolicy",
                "rd.awscc_iam_role_policy_02942703"
            )
            .is_none(),
        "old SimHash row must be removed after Move",
    );

    // The new address row must carry the post-Replace identifier and
    // post-Replace attribute values, not the pre-Replace ones from the
    // old row.
    let new_row = saved
        .find_resource(
            "awscc",
            "iam.RolePolicy",
            "rd.awscc_iam_role_policy_0cd2c914",
        )
        .expect("new SimHash row must exist");
    assert_eq!(
        new_row.identifier.as_deref(),
        Some("carina-registry-infra-deploy-inline|carina-registry-infra-deploy"),
        "Move must not overwrite Replace's post-create identifier",
    );
    assert_eq!(
        new_row.attributes.get("role_name"),
        Some(&serde_json::Value::String(
            "carina-registry-infra-deploy".to_string()
        )),
        "Move must not overwrite Replace's post-create role_name",
    );
    assert_eq!(
        new_row.attributes.get("policy_name"),
        Some(&serde_json::Value::String(
            "carina-registry-infra-deploy-inline".to_string()
        )),
        "Move must not overwrite Replace's post-create policy_name",
    );
}

/// Move + Update — same shape as the Replace case but the Update
/// effect changes a non-create-only attribute. The post-Update
/// attribute value must survive Phase 2's Move cleanup.
#[test]
fn move_plus_update_keeps_post_update_attributes() {
    use carina_core::effect::Effect;
    use carina_state::{ResourceState, StateFile};

    let mut schemas = SchemaRegistry::new();
    let schema = ResourceSchema::new("ec2.Tag")
        .attribute(AttributeSchema::new("key", AttributeType::String).create_only())
        .attribute(AttributeSchema::new("value", AttributeType::String));
    schemas.insert("awscc", schema);

    let new_id = ResourceId::with_provider("awscc", "ec2.Tag", "tag_new", None);
    let mut resource = Resource::with_provider("awscc", "ec2.Tag", "tag_new", None);
    resource.set_attr(
        "key".to_string(),
        Value::Concrete(ConcreteValue::String("env".to_string())),
    );
    resource.set_attr(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String("prod".to_string())),
    );
    let sorted_resources = vec![resource];

    let mut applied_attrs = HashMap::new();
    applied_attrs.insert(
        "key".to_string(),
        Value::Concrete(ConcreteValue::String("env".to_string())),
    );
    applied_attrs.insert(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String("prod".to_string())),
    );
    let applied = State::existing(new_id.clone(), applied_attrs).with_identifier("tag-abc");
    let mut applied_states = HashMap::new();
    applied_states.insert(new_id.clone(), applied);

    // Old state row carries the pre-Update value.
    let old_row = ResourceState::new("ec2.Tag", "tag_old", "awscc")
        .with_identifier("tag-abc")
        .with_attribute("key", serde_json::Value::String("env".to_string()))
        .with_attribute("value", serde_json::Value::String("staging".to_string()));
    let mut state_file = StateFile::default();
    state_file.resources.push(old_row);

    let mut plan = Plan::new();
    let from_id = ResourceId::with_provider("awscc", "ec2.Tag", "tag_old", None);
    plan.add(Effect::Update {
        id: new_id.clone(),
        from: Box::new(State::existing(from_id.clone(), HashMap::new())),
        to: sorted_resources[0].clone(),
        changed_attributes: vec!["value".to_string()],
    });
    plan.add(Effect::Move {
        from: from_id,
        to: new_id.clone(),
    });

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(state_file),
        sorted_resources: &sorted_resources,
        current_states: &HashMap::new(),
        applied_states: &applied_states,
        permanent_name_overrides: &HashMap::new(),
        plan: &plan,
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &schemas,
    })
    .expect("writeback should succeed");

    assert!(
        saved.find_resource("awscc", "ec2.Tag", "tag_old").is_none(),
        "old row must be removed by Move cleanup",
    );
    let new_row = saved
        .find_resource("awscc", "ec2.Tag", "tag_new")
        .expect("new row must exist");
    assert_eq!(
        new_row.attributes.get("value"),
        Some(&serde_json::Value::String("prod".to_string())),
        "Move must not overwrite Update's post-apply value",
    );
}

/// Pure-rename `moved {}` block — no Create/Update/Replace on the
/// `to` address. `materialize_moved_states` transferred the `from`
/// State into `current_states[to]` before writeback runs, so Phase 1
/// must pick up the row from `current_states` and the new address
/// must end up with the carried-over attributes.
#[test]
fn move_alone_carries_attributes_via_current_states() {
    use carina_core::effect::Effect;
    use carina_state::{ResourceState, StateFile};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only()),
    );

    let new_id = ResourceId::with_provider("awscc", "s3.Bucket", "bucket_new", None);
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "bucket_new", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    let sorted_resources = vec![resource];

    // current_states already carries the migrated row at the new id.
    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    let current = State::existing(new_id.clone(), current_attrs).with_identifier("my-bucket");
    let mut current_states = HashMap::new();
    current_states.insert(new_id.clone(), current);

    // State file still has the pre-move address — that's what the
    // Move's `from` cleanup targets.
    let from_id = ResourceId::with_provider("awscc", "s3.Bucket", "bucket_old", None);
    let old_row = ResourceState::new("s3.Bucket", "bucket_old", "awscc")
        .with_identifier("my-bucket")
        .with_attribute("bucket_name", serde_json::Value::String("my-bucket".into()));
    let mut state_file = StateFile::default();
    state_file.resources.push(old_row);

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: from_id,
        to: new_id.clone(),
    });

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(state_file),
        sorted_resources: &sorted_resources,
        current_states: &current_states,
        applied_states: &HashMap::new(),
        permanent_name_overrides: &HashMap::new(),
        plan: &plan,
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &schemas,
    })
    .expect("writeback should succeed");

    assert!(
        saved
            .find_resource("awscc", "s3.Bucket", "bucket_old")
            .is_none(),
        "old row must be removed",
    );
    let new_row = saved
        .find_resource("awscc", "s3.Bucket", "bucket_new")
        .expect("new row must exist");
    assert_eq!(
        new_row.identifier.as_deref(),
        Some("my-bucket"),
        "identifier carried over via current_states",
    );
    assert_eq!(
        new_row.attributes.get("bucket_name"),
        Some(&serde_json::Value::String("my-bucket".to_string())),
    );
}

/// A `Move` whose `from` is not present in state and whose `to` has
/// no Phase-1 source must be a no-op. This is the "moved block left
/// behind after the move already happened" case that carina#3167's
/// reporter saw — the block is harmless until the user deletes it.
#[test]
fn move_with_absent_from_is_no_op() {
    use carina_core::effect::Effect;
    use carina_state::StateFile;

    let schemas = SchemaRegistry::new();
    let from_id = ResourceId::with_provider("awscc", "s3.Bucket", "stale_from", None);
    let to_id = ResourceId::with_provider("awscc", "s3.Bucket", "stale_to", None);

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: from_id.clone(),
        to: to_id.clone(),
    });

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(StateFile::default()),
        sorted_resources: &[],
        current_states: &HashMap::new(),
        applied_states: &HashMap::new(),
        permanent_name_overrides: &HashMap::new(),
        plan: &plan,
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &schemas,
    })
    .expect("writeback should succeed");

    assert!(saved.resources.is_empty(), "no-op move leaves state empty");
}

/// `failed_refreshes` must skip both Upsert and Cleanup: the
/// pre-existing row stays untouched because we don't know whether
/// the live resource still exists.
#[test]
fn failed_refresh_preserves_existing_row() {
    use carina_state::{ResourceState, StateFile};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only()),
    );

    let id = ResourceId::with_provider("awscc", "s3.Bucket", "stuck", None);
    let resource = Resource::with_provider("awscc", "s3.Bucket", "stuck", None);
    let sorted_resources = vec![resource];

    let mut failed_refreshes = HashSet::new();
    failed_refreshes.insert(id.clone());

    let existing = ResourceState::new("s3.Bucket", "stuck", "awscc")
        .with_identifier("preserved-id")
        .with_attribute(
            "bucket_name",
            serde_json::Value::String("preserved".to_string()),
        );
    let mut state_file = StateFile::default();
    state_file.resources.push(existing);

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(state_file),
        sorted_resources: &sorted_resources,
        current_states: &HashMap::new(),
        applied_states: &HashMap::new(),
        permanent_name_overrides: &HashMap::new(),
        plan: &Plan::new(),
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &failed_refreshes,
        schemas: &schemas,
    })
    .expect("writeback should succeed");

    let row = saved
        .find_resource("awscc", "s3.Bucket", "stuck")
        .expect("row must be preserved");
    assert_eq!(row.identifier.as_deref(), Some("preserved-id"));
}

/// A `Move` whose `from` collides with a desired resource at the
/// same address is a `WritebackConflict::UpsertCleanupOverlap` — the
/// apply pipeline upstream of writeback is supposed to prevent this
/// (a `moved` block's `from` should never be in `sorted_resources`),
/// so surface it as a validation error rather than silently dropping
/// the desired row.
#[test]
fn move_from_overlapping_desired_resource_errors() {
    use carina_core::effect::Effect;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only()),
    );

    let id = ResourceId::with_provider("awscc", "s3.Bucket", "collision", None);
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "collision", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    let sorted_resources = vec![resource.clone()];

    let mut applied = HashMap::new();
    applied.insert(
        id.clone(),
        State::existing(id.clone(), HashMap::new()).with_identifier("x"),
    );

    let to_id = ResourceId::with_provider("awscc", "s3.Bucket", "elsewhere", None);
    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: id.clone(),
        to: to_id,
    });

    let result = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        current_states: &HashMap::new(),
        applied_states: &applied,
        permanent_name_overrides: &HashMap::new(),
        plan: &plan,
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &schemas,
    });

    let err = result.expect_err("overlap must surface as a writeback error");
    let msg = err.to_string();
    assert!(
        msg.contains("upsert") && msg.contains("cleanup"),
        "error must name the overlap class, got: {msg}",
    );
}

/// `Effect::Remove` whose `id` is still in `sorted_resources` (user
/// wrote both `removed { from = X }` and X still in DSL) must
/// surface the same `UpsertCleanupOverlap` as the Move overlap case.
/// Locks in that the structural invariant covers every Phase-2
/// cleanup-emitting effect, not just Move.
#[test]
fn remove_overlapping_desired_resource_errors() {
    use carina_core::effect::Effect;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only()),
    );

    let id = ResourceId::with_provider("awscc", "s3.Bucket", "collision", None);
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "collision", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    let sorted_resources = vec![resource];

    let mut applied = HashMap::new();
    applied.insert(
        id.clone(),
        State::existing(id.clone(), HashMap::new()).with_identifier("x"),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Remove { id: id.clone() });

    let result = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        current_states: &HashMap::new(),
        applied_states: &applied,
        permanent_name_overrides: &HashMap::new(),
        plan: &plan,
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &schemas,
    });

    let err = result.expect_err("overlap must surface as a writeback error");
    assert!(
        err.to_string().contains("upsert") && err.to_string().contains("cleanup"),
        "error must name the overlap class, got: {err}",
    );
}

/// A self-move (`Effect::Move { from: X, to: X }`) where the
/// resource is in `sorted_resources` must error. Phase 1 upserts the
/// row, Phase 2 then tries to clean it up at the same id — surfacing
/// the upstream planner bug rather than silently deleting the row.
#[test]
fn self_move_overlapping_desired_resource_errors() {
    use carina_core::effect::Effect;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only()),
    );

    let id = ResourceId::with_provider("awscc", "s3.Bucket", "self", None);
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "self", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    let sorted_resources = vec![resource];

    let mut applied = HashMap::new();
    applied.insert(
        id.clone(),
        State::existing(id.clone(), HashMap::new()).with_identifier("x"),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: id.clone(),
        to: id.clone(),
    });

    let result = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        current_states: &HashMap::new(),
        applied_states: &applied,
        permanent_name_overrides: &HashMap::new(),
        plan: &plan,
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &schemas,
    });

    let err = result.expect_err("self-move must surface as a writeback error");
    assert!(
        err.to_string().contains("upsert") && err.to_string().contains("cleanup"),
        "error must name the overlap class, got: {err}",
    );
}

#[test]
fn format_duration_sub_second() {
    let d = Duration::from_millis(500);
    assert_eq!(format_duration(d), "0.5s");
}

#[test]
fn format_duration_seconds() {
    let d = Duration::from_secs_f64(3.25);
    assert_eq!(format_duration(d), "3.2s");
}

#[test]
fn format_duration_minutes() {
    let d = Duration::from_secs_f64(65.3);
    assert_eq!(format_duration(d), "1m 5.3s");
}

#[test]
fn format_duration_zero() {
    let d = Duration::from_secs(0);
    assert_eq!(format_duration(d), "0.0s");
}

#[test]
fn resolve_exports_resolves_cross_file_dot_notation_strings() {
    use carina_core::parser::{InferredExportParam as ExportParameter, TypeExpr};
    use carina_core::resource::{ConcreteValue, Value};
    use carina_state::StateFile;

    // Build a state file with a resource that has a binding and attributes
    let state = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 1,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": [
                {
                    "resource_type": "organizations.account",
                    "name": "registry-prod",
                    "identifier": "459524413166",
                    "provider": "awscc",
                    "binding": "registry_prod",
                    "attributes": {
                        "account_id": "459524413166",
                        "account_name": "registry-prod"
                    }
                }
            ]
        });
        serde_json::from_value::<StateFile>(json).unwrap()
    };

    // Export param references registry_prod.account_id as a dot-notation string
    // (this is how cross-file references are parsed: exports.crn doesn't see
    // the let binding in main.crn, so the parser emits a plain string)
    let export_params = vec![ExportParameter {
        name: "account_id".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Concrete(ConcreteValue::String(
            "registry_prod.account_id".to_string(),
        ))),
    }];

    // Mirror production callers: the resource is in sorted_resources
    // with a binding; provider-returned attributes flow in via
    // `current_states` derived from `state.resources`.
    let mut registry_prod =
        Resource::with_provider("awscc", "organizations.account", "registry-prod", None);
    registry_prod.binding = Some("registry_prod".to_string());
    let sorted_resources = vec![registry_prod];

    let exports = resolve_exports(&export_params, &sorted_resources, &[], &state, &[]).unwrap();

    assert_eq!(
        exports.get("account_id"),
        Some(&serde_json::Value::String("459524413166".to_string())),
        "resolve_exports should resolve dot-notation strings to actual values. Got: {:?}",
        exports
    );
}

#[test]
fn resolve_exports_resolves_module_call_attribute_via_virtual_resource() {
    // #2479: writeback used to build bindings from `state.resources`
    // only. A virtual resource (synthesised by module-call expansion to
    // expose `attributes { role_arn = role.arn }`) carries no provider
    // identity and never lands in `state.resources`, so an export
    // referencing `<module_call>.<attr>` failed with
    // `unresolved reference <call>.<attr>`.
    use carina_core::parser::{InferredExportParam as ExportParameter, TypeExpr};
    use carina_core::resource::{AccessPath, DeferredValue, ResourceKind, Value};
    use carina_state::StateFile;

    let state = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 1,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": [
                {
                    "resource_type": "iam.Role",
                    "name": "github_actions_carina.role",
                    "identifier": "github-actions-carina",
                    "provider": "awscc",
                    "binding": "github_actions_carina.role",
                    "attributes": {
                        "arn": "arn:aws:iam::123456789012:role/github-actions-carina"
                    }
                }
            ]
        });
        serde_json::from_value::<StateFile>(json).unwrap()
    };

    let mut role_resource =
        Resource::with_provider("awscc", "iam.Role", "github_actions_carina.role", None);
    role_resource.binding = Some("github_actions_carina.role".to_string());

    // Virtual resource as `expand_module_call` produces it: binding is
    // the module-call alias, and each attribute is a ResourceRef into
    // an expanded sub-resource.
    let mut virtual_resource = Resource::new("_virtual", "github_actions_carina");
    virtual_resource.binding = Some("github_actions_carina".to_string());
    virtual_resource.kind = ResourceKind::Virtual;
    virtual_resource.virtual_module = Some((
        "github_module".to_string(),
        "github_actions_carina".to_string(),
    ));
    virtual_resource.attributes.insert(
        "role_arn".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("github_actions_carina.role", "arn"),
        }),
    );
    let sorted_resources = vec![role_resource, virtual_resource];
    let pre_resolve_virtuals: Vec<carina_core::resource::VirtualResource> = sorted_resources
        .iter()
        .filter_map(|r| carina_core::resource::VirtualResource::try_from(r).ok())
        .collect();

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("github_actions_carina", "role_arn"),
        })),
    }];

    let exports = resolve_exports(
        &export_params,
        &sorted_resources,
        &pre_resolve_virtuals,
        &state,
        &[],
    )
    .unwrap();

    assert_eq!(
        exports.get("role_arn"),
        Some(&serde_json::Value::String(
            "arn:aws:iam::123456789012:role/github-actions-carina".to_string()
        )),
        "module-call attribute export must resolve via virtual binding + provider state, got: {:?}",
        exports
    );
}

#[test]
fn resolve_exports_resolves_chained_module_call_attribute_via_two_virtuals() {
    // #2479 follow-up: a module-call binding whose attribute itself
    // points at *another* module-call binding's attribute (e.g.
    // `${outer_module.public_role_arn}` where the outer module's
    // `attributes { public_role_arn = inner_module.role_arn }` exposes
    // an inner module-call binding's attribute). Two `Virtual` hops
    // through `ResourceRef` recursion before bottoming out at the real
    // role's `arn` from state. Pins the resolver's transitive walk so a
    // regression that broke after a single hop would surface.
    use carina_core::parser::{InferredExportParam as ExportParameter, TypeExpr};
    use carina_core::resource::{AccessPath, DeferredValue, ResourceKind, Value};
    use carina_state::StateFile;

    let state = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 1,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": [
                {
                    "resource_type": "iam.Role",
                    "name": "outer.inner.role",
                    "identifier": "github-actions-carina",
                    "provider": "awscc",
                    "binding": "outer.inner.role",
                    "attributes": {
                        "arn": "arn:aws:iam::123456789012:role/chained"
                    }
                }
            ]
        });
        serde_json::from_value::<StateFile>(json).unwrap()
    };

    let mut role_resource = Resource::with_provider("awscc", "iam.Role", "outer.inner.role", None);
    role_resource.binding = Some("outer.inner.role".to_string());

    let mut inner_virtual = Resource::new("_virtual", "outer.inner");
    inner_virtual.binding = Some("outer.inner".to_string());
    inner_virtual.kind = ResourceKind::Virtual;
    inner_virtual.virtual_module = Some(("inner_module".to_string(), "outer.inner".to_string()));
    inner_virtual.attributes.insert(
        "role_arn".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("outer.inner.role", "arn"),
        }),
    );

    let mut outer_virtual = Resource::new("_virtual", "outer");
    outer_virtual.binding = Some("outer".to_string());
    outer_virtual.kind = ResourceKind::Virtual;
    outer_virtual.virtual_module = Some(("outer_module".to_string(), "outer".to_string()));
    outer_virtual.attributes.insert(
        "public_role_arn".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("outer.inner", "role_arn"),
        }),
    );

    let sorted_resources = vec![role_resource, inner_virtual, outer_virtual];

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("outer", "public_role_arn"),
        })),
    }];

    let pre_resolve_virtuals: Vec<carina_core::resource::VirtualResource> = sorted_resources
        .iter()
        .filter_map(|r| carina_core::resource::VirtualResource::try_from(r).ok())
        .collect();
    let exports = resolve_exports(
        &export_params,
        &sorted_resources,
        &pre_resolve_virtuals,
        &state,
        &[],
    )
    .unwrap();

    assert_eq!(
        exports.get("role_arn"),
        Some(&serde_json::Value::String(
            "arn:aws:iam::123456789012:role/chained".to_string()
        )),
        "two-hop module-call chain must resolve through both virtuals, got: {:?}",
        exports
    );
}

#[test]
fn resolve_exports_picks_post_apply_role_arn_after_replace_3169() {
    // #3169 root-cause regression test (carina-rs/infra PR #64 drift).
    //
    // **The bug** (pre-#3177): the apply path's head-of-pipeline
    // call to `resolve_refs_with_state_and_remote(&mut
    // resources_for_plan, &pre_apply_current_states, …)` collapses
    // every `ResourceRef` — including the ones inside a
    // `VirtualResource`'s `attributes` — into the pre-apply
    // concrete value (here: OLD_ARN). The virtual's `role_arn`
    // attribute thus becomes a frozen string snapshot of the
    // pre-apply state. Later in `finalize_apply`, the resource
    // graph is sent into `resolve_exports`, which feeds the
    // virtual's stale `role_arn` into `state.exports` — even
    // though the writeback wrote the new ARN into
    // `state.resources[role].arn`. So `state.exports.role_arn` and
    // `state.resources[role].arn` disagree, with exports holding
    // the old ARN.
    //
    // **The fix** (#3177): `apply/mod.rs` snapshots every virtual
    // *before* the head-of-pipeline `ResourceRef` collapse — that
    // snapshot still carries the authored `ref role.arn`. It
    // threads that pre-resolve snapshot into `finalize_apply` →
    // `resolve_exports`, which uses it (instead of the mutated
    // `sorted_resources`) to build a fresh post-apply
    // `ResolvedBindings` view via the typestate-split API
    // (`from_managed_with_state` + `resolve_virtual_refs_post_apply`
    // + `add_virtual_resources`). Exports then resolve against the
    // post-apply view.
    //
    // **The test** mirrors that production pipeline: it (1)
    // constructs the same managed + virtual graph, (2) takes a
    // pre-resolve snapshot of the virtual, (3) runs the head-of-
    // pipeline resolver against a *pre-apply* `current_states`
    // (OLD_ARN), then (4) feeds the pre-resolve snapshot and the
    // resolved working slice into `resolve_exports` along with a
    // *post-apply* `StateFile` (NEW_ARN). The assertion checks the
    // export ends up at NEW_ARN.
    //
    // Without the #3177 fix, `resolve_exports` would consult the
    // mutated virtual's stale concrete attribute and return
    // OLD_ARN. With the fix in place, the pre-resolve snapshot
    // kicks in and re-resolves against the post-apply state.
    use carina_core::parser::{InferredExportParam as ExportParameter, TypeExpr};
    use carina_core::resolver::resolve_refs_with_state_and_remote;
    use carina_core::resource::{
        AccessPath, ConcreteValue, DeferredValue, ResourceId, ResourceKind, State as ResourceState,
        Value, VirtualResource,
    };
    use carina_state::StateFile;
    use std::collections::HashMap;

    let pre_apply_arn = "arn:aws:iam::123456789012:role/role-OLD";
    let post_apply_arn = "arn:aws:iam::123456789012:role/role-NEW";

    // Step (a): post-apply `StateFile` — what `state.resources`
    // looks like after the writeback wrote the new ARN. This is
    // what `resolve_exports` sees as its `state` argument.
    let post_apply_state_file = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 2,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": [
                {
                    "resource_type": "iam.Role",
                    "name": "carina_role",
                    "identifier": "carina-role-NEW",
                    "provider": "awscc",
                    "binding": "role",
                    "attributes": { "arn": post_apply_arn }
                }
            ]
        });
        serde_json::from_value::<StateFile>(json).unwrap()
    };

    // Step (b): pre-apply `current_states` — what the head-of-
    // pipeline resolver sees. Holds the OLD ARN, so the pre-apply
    // pass freezes virtual `role_arn` to OLD_ARN.
    let pre_apply_current_states: HashMap<ResourceId, ResourceState> = {
        let id = ResourceId::with_provider("awscc", "iam.Role", "carina_role", None);
        let mut attrs: HashMap<String, Value> = HashMap::new();
        attrs.insert(
            "arn".to_string(),
            Value::Concrete(ConcreteValue::String(pre_apply_arn.to_string())),
        );
        let mut m = HashMap::new();
        m.insert(id.clone(), ResourceState::existing(id, attrs));
        m
    };

    // Build the authored resource graph: a managed `role` (DSL
    // does not inline `arn` — provider returns it) and a virtual
    // module-call binding that references `role.arn`.
    let mut role_managed = Resource::with_provider("awscc", "iam.Role", "carina_role", None);
    role_managed.binding = Some("role".to_string());

    let mut virtual_resource = Resource::new("_virtual", "carina_module");
    virtual_resource.binding = Some("carina_module".to_string());
    virtual_resource.kind = ResourceKind::Virtual;
    virtual_resource.virtual_module =
        Some(("carina_module".to_string(), "carina_module".to_string()));
    virtual_resource.attributes.insert(
        "role_arn".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("role", "arn"),
        }),
    );

    let mut sorted_resources = vec![role_managed, virtual_resource];

    // Step (c): pre-resolve snapshot — same `apply/mod.rs` does
    // before the head-of-pipeline resolver runs.
    let pre_resolve_virtuals: Vec<VirtualResource> = sorted_resources
        .iter()
        .filter_map(|r| VirtualResource::try_from(r).ok())
        .collect();

    // Step (d): head-of-pipeline resolver. After this call,
    // `sorted_resources[1].attributes["role_arn"]` is
    // `Value::Concrete(String(OLD_ARN))` — the bug-class state.
    resolve_refs_with_state_and_remote(
        &mut sorted_resources,
        &pre_apply_current_states,
        &HashMap::new(),
        &[],
    )
    .unwrap();

    // Sanity-check the bug condition is in place before the fix
    // runs: the virtual now carries the stale OLD_ARN inline.
    assert_eq!(
        sorted_resources[1].attributes.get("role_arn"),
        Some(&Value::Concrete(ConcreteValue::String(
            pre_apply_arn.to_string()
        ))),
        "head-of-pipeline must have frozen virtual.role_arn to pre-apply OLD_ARN",
    );

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("carina_module", "role_arn"),
        })),
    }];

    let exports = resolve_exports(
        &export_params,
        &sorted_resources,
        &pre_resolve_virtuals,
        &post_apply_state_file,
        &[],
    )
    .unwrap();

    assert_eq!(
        exports.get("role_arn"),
        Some(&serde_json::Value::String(post_apply_arn.to_string())),
        "exports.role_arn must be the POST-apply ARN ({post_apply_arn}) — \
         the value carried by state.resources, NOT a pre-apply snapshot. \
         Got: {exports:?}",
    );
}

#[test]
fn emit_newline_on_interrupt_writes_newline_when_interrupted() {
    let mut buf: Vec<u8> = Vec::new();
    let result: Result<String, AppError> = Err(AppError::Interrupted);
    emit_newline_on_interrupt(&mut buf, &result);
    assert_eq!(buf, b"\n");
}

#[test]
fn emit_newline_on_interrupt_writes_nothing_on_ok() {
    let mut buf: Vec<u8> = Vec::new();
    let result: Result<String, AppError> = Ok("yes".to_string());
    emit_newline_on_interrupt(&mut buf, &result);
    assert!(buf.is_empty());
}

#[test]
fn emit_newline_on_interrupt_writes_nothing_on_other_error() {
    let mut buf: Vec<u8> = Vec::new();
    let result: Result<String, AppError> = Err(AppError::Config("boom".to_string()));
    emit_newline_on_interrupt(&mut buf, &result);
    assert!(buf.is_empty());
}

#[tokio::test]
async fn confirm_apply_returns_confirmed_on_yes() {
    let input = &b"yes\n"[..];
    let interrupt = std::future::pending::<()>();
    let outcome = confirm_apply(input, interrupt, false).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Confirmed);
}

#[tokio::test]
async fn confirm_apply_returns_cancelled_on_no() {
    let input = &b"no\n"[..];
    let interrupt = std::future::pending::<()>();
    let outcome = confirm_apply(input, interrupt, false).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Cancelled);
}

#[tokio::test]
async fn confirm_apply_returns_cancelled_on_empty_input() {
    let input = &b"\n"[..];
    let interrupt = std::future::pending::<()>();
    let outcome = confirm_apply(input, interrupt, false).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Cancelled);
}

#[tokio::test]
async fn confirm_apply_auto_approve_skips_read() {
    // Reader would hang forever; auto_approve must short-circuit without reading.
    let input = tokio::io::BufReader::new(tokio::io::empty());
    let interrupt = std::future::pending::<()>();
    let outcome = confirm_apply(input, interrupt, true).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Confirmed);
}

#[tokio::test]
async fn confirm_apply_propagates_interrupt() {
    // A reader that never resolves, to force the interrupt path.
    struct NeverReady;
    impl tokio::io::AsyncRead for NeverReady {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }
    let reader = tokio::io::BufReader::new(NeverReady);
    let interrupt = async {};
    let err = confirm_apply(reader, interrupt, false).await.unwrap_err();
    assert!(matches!(err, AppError::Interrupted));
}

// carina#3132 PR-2: the apply path expands deferred-for loops
// *post-refresh* via the *same* `crate::wiring::expand_same_config_deferred_for`
// the plan path uses (PR-1). The function itself is exhaustively
// unit-tested in `wiring::tests::expand_same_config_deferred_for_tests`;
// these tests pin the **apply-side contract**: `run_apply_locked`
// depends on that exact shared function (plan/apply parity —
// MEMORY "unit-test path ≠ apply path"), and the carina#3132 real
// registry shape (chained `opt.resource_record.name`, resolvable since
// carina#3136) materializes + resolves identically on the apply side.
mod apply_deferred_for_parity {
    use carina_core::binding_index::WaitAliasSpec;
    use carina_core::parser::{ProviderContext, parse};
    use carina_core::resource::{ConcreteValue, ResourceId, State, Value};
    use std::collections::HashMap;

    fn dvo_state(parsed: &carina_core::parser::ParsedFile) -> HashMap<ResourceId, State> {
        let cert = parsed
            .resources
            .iter()
            .find(|r| r.binding.as_deref() == Some("cert"))
            .expect("parsed cert resource");
        let mut rr = indexmap::IndexMap::new();
        rr.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.r.example.com".into())),
        );
        rr.insert(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.acm-validations.aws.".into())),
        );
        let mut entry = indexmap::IndexMap::new();
        entry.insert(
            "resource_record".to_string(),
            Value::Concrete(ConcreteValue::Map(rr)),
        );
        let mut attrs = HashMap::new();
        attrs.insert(
            "domain_validation_options".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(entry),
            )])),
        );
        let mut states = HashMap::new();
        states.insert(cert.id.clone(), State::existing(cert.id.clone(), attrs));
        states
    }

    /// The apply path's exact call: same-config `let cert` read
    /// iterable with a chained loop-var body, fed a post-refresh
    /// `current_states`. Asserts the apply side materializes AND
    /// resolves the chained ref — parity with the plan path's
    /// `chained_loop_var_field_access_resolves_post_expansion`.
    #[test]
    fn apply_post_refresh_expansion_resolves_registry_shape() {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet {
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    resource_records = [opt.resource_record.value]
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        let sorted = carina_core::deps::sort_resources_by_dependencies(&parsed.resources).unwrap();
        let states = dvo_state(&parsed);

        // This is the exact function `run_apply_locked` calls
        // post-refresh (carina#3132 PR-2). Reaching it through the
        // apply module's `crate::wiring::` path pins the dependency.
        let out = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        let record_sets: Vec<_> = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("RecordSet"))
            .collect();
        assert_eq!(
            record_sets.len(),
            1,
            "apply materializes one RecordSet per domain_validation_options entry"
        );
        assert_eq!(
            record_sets[0].get_attr("name"),
            Some(&Value::Concrete(ConcreteValue::String(
                "_a1.r.example.com".into()
            ))),
            "apply resolves the chained loop-var ref identically to plan \
             (carina#3132 PR-3 registry shape)"
        );
        assert!(
            out.residual_deferred_for.is_empty(),
            "resolved loop leaves no residual on the apply path"
        );
        assert_eq!(
            out.new_child_ids.len(),
            1,
            "the materialized RecordSet is reported for the apply-side \
             targeted child-refresh"
        );
    }

    /// No refreshed cert state ⇒ the loop stays deferred on the apply
    /// path too (no mis-expansion), matching the plan path's
    /// unresolvable-iterable behavior.
    #[test]
    fn apply_unresolvable_iterable_stays_deferred() {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet { name = opt.resource_record.name }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        let sorted = carina_core::deps::sort_resources_by_dependencies(&parsed.resources).unwrap();
        let empty: HashMap<ResourceId, State> = HashMap::new();

        let out = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &empty,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        assert!(
            out.sorted_resources
                .iter()
                .all(|r| !r.id.resource_type.contains("RecordSet")),
            "no RecordSet materializes without a resolvable iterable on apply"
        );
        assert_eq!(out.residual_deferred_for.len(), 1);
        assert!(out.new_child_ids.is_empty());
    }

    /// carina#3141 apply-side contract: `run_apply_locked` refreshes
    /// exactly `refreshable_child_ids`, the same typed field the plan
    /// path consumes. Reaching `expand_same_config_deferred_for` through
    /// the apply module's `crate::wiring::` path pins that the apply
    /// path's moved-exclusion is the *identical* computation as the plan
    /// path's — a divergence would be a compile error against
    /// `DeferredForExpansion`, not a silent parity bug
    /// (MEMORY "unit-test path ≠ apply path"). Parity twin of
    /// `wiring::tests::...::moved_target_child_is_excluded_from_refreshable_set`.
    #[test]
    fn apply_moved_target_child_excluded_from_refreshable_set() {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet {
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    resource_records = [opt.resource_record.value]
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        let sorted = carina_core::deps::sort_resources_by_dependencies(&parsed.resources).unwrap();
        let states = dvo_state(&parsed);

        // No moved targets: the expanded child is refreshable.
        let baseline = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");
        assert_eq!(baseline.new_child_ids.len(), 1);
        let child_id = baseline
            .new_child_ids
            .iter()
            .next()
            .expect("materialized child")
            .clone();
        assert!(
            baseline.refreshable_child_ids.contains(&child_id),
            "apply: a non-moved expanded child must still be refreshed"
        );

        // Declare it a `moved` `to`: it must drop out of the refreshable
        // set so `run_apply_locked` does not clobber the migrated state.
        let mut moved_targets = std::collections::HashSet::new();
        moved_targets.insert(child_id.clone());
        let out = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &moved_targets,
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        assert_eq!(
            out.new_child_ids, baseline.new_child_ids,
            "apply: moved-exclusion must not change what materialized"
        );
        assert!(
            !out.refreshable_child_ids.contains(&child_id),
            "apply: a `moved` target child must be excluded from \
             refreshable_child_ids (carina#3141 parity with plan path)"
        );
    }
}
