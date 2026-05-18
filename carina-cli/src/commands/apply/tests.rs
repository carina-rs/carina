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

    let exports = resolve_exports(&export_params, &sorted_resources, &state, &[]).unwrap();

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
    virtual_resource.kind = ResourceKind::Virtual {
        module_name: "github_module".to_string(),
        instance: "github_actions_carina".to_string(),
    };
    virtual_resource.attributes.insert(
        "role_arn".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("github_actions_carina.role", "arn"),
        }),
    );
    let sorted_resources = vec![role_resource, virtual_resource];

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("github_actions_carina", "role_arn"),
        })),
    }];

    let exports = resolve_exports(&export_params, &sorted_resources, &state, &[]).unwrap();

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
    inner_virtual.kind = ResourceKind::Virtual {
        module_name: "inner_module".to_string(),
        instance: "outer.inner".to_string(),
    };
    inner_virtual.attributes.insert(
        "role_arn".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("outer.inner.role", "arn"),
        }),
    );

    let mut outer_virtual = Resource::new("_virtual", "outer");
    outer_virtual.binding = Some("outer".to_string());
    outer_virtual.kind = ResourceKind::Virtual {
        module_name: "outer_module".to_string(),
        instance: "outer".to_string(),
    };
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

    let exports = resolve_exports(&export_params, &sorted_resources, &state, &[]).unwrap();

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
}
