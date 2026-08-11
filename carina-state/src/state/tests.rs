use super::*;
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap};

fn list_rule(port: i64, description: Option<&str>) -> Value {
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
}

fn assert_explicit_struct_keys(explicit: &ExplicitFields, expected: &[&str]) {
    let ExplicitFields::Struct { children } = explicit else {
        panic!("expected Struct, got {explicit:?}");
    };
    assert_eq!(children.len(), expected.len());
    for key in expected {
        assert!(children.contains_key(*key), "missing authored key {key}");
    }
}

#[test]
fn test_state_file_new() {
    let state = StateFile::new();
    assert_eq!(state.version, StateFile::CURRENT_VERSION);
    assert_eq!(state.serial, 0);
    assert!(!state.lineage.is_empty());
    assert!(state.resources.is_empty());
}

#[test]
fn test_state_file_increment_serial() {
    let mut state = StateFile::new();
    assert_eq!(state.serial, 0);
    state.increment_serial();
    assert_eq!(state.serial, 1);
    state.increment_serial();
    assert_eq!(state.serial, 2);
}

#[test]
fn test_state_file_upsert_resource() {
    let mut state = StateFile::new();

    let resource1 = ResourceState::new("s3.Bucket", "my-bucket", "aws")
        .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"));

    state
        .upsert_resource(resource1)
        .expect("test state setup must be valid");
    assert_eq!(state.resources.len(), 1);

    // Update the same resource
    let resource2 = ResourceState::new("s3.Bucket", "my-bucket", "aws")
        .with_attribute("region".to_string(), serde_json::json!("us-west-2"));

    state
        .upsert_resource(resource2)
        .expect("test state setup must be valid");
    assert_eq!(state.resources.len(), 1);
    assert_eq!(
        state.resources[0].attributes.get("region"),
        Some(&serde_json::json!("us-west-2"))
    );
}

#[test]
fn upsert_resource_returns_error_for_duplicate_deposed_key() {
    let mut state = StateFile::new();
    let duplicate_key = DeposedKey::new_unique();
    let mut resource = ResourceState::new("ec2.Vpc", "main", "aws");
    resource.deposed = vec![
        DeposedInstance {
            key: duplicate_key.clone(),
            identifier: "vpc-old-a".to_string(),
            provider_instance: Some("west".to_string()),
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),
        },
        DeposedInstance {
            key: duplicate_key,
            identifier: "vpc-old-b".to_string(),
            provider_instance: Some("east".to_string()),
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),
        },
    ];

    let error = state
        .upsert_resource(resource)
        .expect_err("duplicate deposed keys must be rejected");

    assert!(
        matches!(error, BackendError::InvalidState(message) if message.contains("duplicate deposed generation key"))
    );
    assert!(state.resources().is_empty());
}

#[test]
fn rename_resource_identities_returns_error_for_duplicate_destination() {
    let mut state = StateFile::new();
    state
        .upsert_resource(ResourceState::new("ec2.Vpc", "old", "aws"))
        .expect("fresh state accepts the old identity");
    state
        .upsert_resource(ResourceState::new("ec2.Vpc", "existing", "aws"))
        .expect("fresh state accepts the destination identity");

    let error = state
        .rename_resource_identities(&[("old".to_string(), "existing".to_string())])
        .expect_err("rename must reject a duplicate destination identity");

    assert!(
        matches!(error, BackendError::InvalidState(message) if message.contains("duplicate resource identity"))
    );
    assert!(state.find_resource("aws", "ec2.Vpc", "old").is_some());
    assert!(state.find_resource("aws", "ec2.Vpc", "existing").is_some());
}

#[test]
fn upsert_deposed_generation_returns_error_when_key_and_identity_cross_generations() {
    let mut state = StateFile::new();
    let first_key = DeposedKey::new_unique();
    let second_key = DeposedKey::new_unique();
    let mut resource = ResourceState::new("ec2.Vpc", "main", "aws");
    resource.deposed = vec![
        DeposedInstance {
            key: first_key.clone(),
            identifier: "vpc-old-a".to_string(),
            provider_instance: Some("west".to_string()),
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),
        },
        DeposedInstance {
            key: second_key,
            identifier: "vpc-old-b".to_string(),
            provider_instance: Some("east".to_string()),
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),
        },
    ];
    state
        .upsert_resource(resource)
        .expect("setup generations are unique on both axes");

    let error = state
        .upsert_deposed_generation(
            "aws",
            "ec2.Vpc",
            "main",
            None,
            DeposedInstance {
                key: first_key,
                identifier: "vpc-old-b".to_string(),
                provider_instance: Some("east".to_string()),
                attributes: HashMap::new(),
                dependency_bindings: BTreeSet::new(),
            },
        )
        .expect_err("key and identity cannot select different generations");

    assert!(
        matches!(error, BackendError::InvalidState(message) if message.contains("match different existing generations"))
    );
    let row = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("failed upsert must preserve the existing row");
    assert_eq!(row.deposed.len(), 2);
}

#[test]
fn upsert_resource_preserves_existing_deposed_generations() {
    let mut state = StateFile::new();
    let mut existing = ResourceState::new("ec2.Vpc", "main", "aws")
        .with_identifier("vpc-old")
        .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
    let deposed_key = DeposedKey::new_unique();
    existing.deposed.push(DeposedInstance {
        key: deposed_key.clone(),
        identifier: "vpc-older".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: HashMap::from([("cidr_block".to_string(), serde_json::json!("10.255.0.0/16"))]),
        dependency_bindings: BTreeSet::from(["igw".to_string()]),
    });
    state
        .upsert_resource(existing)
        .expect("test state setup must be valid");

    let incoming = ResourceState::new("ec2.Vpc", "main", "aws")
        .with_identifier("vpc-new")
        .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
    state
        .upsert_resource(incoming)
        .expect("test state setup must be valid");

    let row = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("resource should still exist");
    assert_eq!(row.identifier.as_deref(), Some("vpc-new"));
    assert_eq!(
        row.attributes.get("cidr_block"),
        Some(&serde_json::json!("10.1.0.0/16"))
    );
    assert_eq!(row.deposed.len(), 1);
    assert_eq!(row.deposed[0].key, deposed_key);
    assert_eq!(row.deposed[0].identifier, "vpc-older");
    assert_eq!(row.deposed[0].provider_instance.as_deref(), Some("west"));
}

#[test]
fn upsert_resource_drops_deposed_generation_matching_current_identifier() {
    let mut state = StateFile::new();
    let retained_key = DeposedKey::new_unique();
    let mut existing = ResourceState::new("ec2.Vpc", "main", "aws")
        .with_identifier("vpc-current")
        .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
    existing.deposed.push(DeposedInstance {
        key: DeposedKey::new_unique(),
        identifier: "vpc-reused".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: HashMap::from([("cidr_block".to_string(), serde_json::json!("10.2.0.0/16"))]),
        dependency_bindings: BTreeSet::new(),
    });
    existing.deposed.push(DeposedInstance {
        key: retained_key.clone(),
        identifier: "vpc-older".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: HashMap::from([("cidr_block".to_string(), serde_json::json!("10.255.0.0/16"))]),
        dependency_bindings: BTreeSet::from(["igw".to_string()]),
    });
    state
        .upsert_resource(existing)
        .expect("test state setup must be valid");

    let mut incoming = ResourceState::new("ec2.Vpc", "main", "aws")
        .with_identifier("vpc-reused")
        .with_attribute("cidr_block", serde_json::json!("10.2.0.0/16"));
    incoming.directives.provider_instance = Some("west".to_string());
    state
        .upsert_resource(incoming)
        .expect("test state setup must be valid");

    let row = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("resource should still exist");
    assert_eq!(row.identifier.as_deref(), Some("vpc-reused"));
    assert_eq!(row.deposed.len(), 1);
    assert_eq!(row.deposed[0].key, retained_key);
    assert_eq!(row.deposed[0].identifier, "vpc-older");
}

#[test]
fn upsert_resource_keeps_deposed_generation_with_same_identifier_on_other_provider_instance() {
    let mut state = StateFile::new();
    let retained_key = DeposedKey::new_unique();
    let mut existing = ResourceState::new("ec2.Vpc", "main", "aws")
        .with_identifier("vpc-current")
        .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
    existing.deposed.push(DeposedInstance {
        key: retained_key.clone(),
        identifier: "vpc-reused".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: HashMap::from([("cidr_block".to_string(), serde_json::json!("10.2.0.0/16"))]),
        dependency_bindings: BTreeSet::new(),
    });
    state
        .upsert_resource(existing)
        .expect("test state setup must be valid");

    let mut incoming = ResourceState::new("ec2.Vpc", "main", "aws")
        .with_identifier("vpc-reused")
        .with_attribute("cidr_block", serde_json::json!("10.2.0.0/16"));
    incoming.directives.provider_instance = Some("east".to_string());
    state
        .upsert_resource(incoming)
        .expect("test state setup must be valid");

    let row = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("resource should still exist");
    assert_eq!(row.identifier.as_deref(), Some("vpc-reused"));
    assert_eq!(row.directives.provider_instance.as_deref(), Some("east"));
    assert_eq!(row.deposed.len(), 1);
    assert_eq!(row.deposed[0].key, retained_key);
    assert_eq!(row.deposed[0].identifier, "vpc-reused");
    assert_eq!(row.deposed[0].provider_instance.as_deref(), Some("west"));
}

#[test]
fn test_state_file_remove_resource() {
    let mut state = StateFile::new();

    let resource = ResourceState::new("s3.Bucket", "my-bucket", "aws");
    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");
    assert_eq!(state.resources.len(), 1);

    let removed = state.remove_resource("aws", "s3.Bucket", "my-bucket");
    assert!(removed.is_some());
    assert_eq!(state.resources.len(), 0);

    // Removing non-existent resource returns None
    let removed = state.remove_resource("aws", "s3.Bucket", "other-bucket");
    assert!(removed.is_none());
}

#[test]
fn remove_resource_clears_current_instance_and_keeps_deposed_generations() {
    let mut state = StateFile::new();
    let mut resource = ResourceState::new("ec2.Vpc", "main", "aws")
        .with_identifier("vpc-new")
        .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
    resource.protected = true;
    resource.directives.create_before_destroy = true;
    resource.directives.provider_instance = Some("west".to_string());
    resource
        .prefixes
        .insert("name".to_string(), "main-".to_string());
    resource.name_overrides.insert(
        "name".to_string(),
        NameOverride {
            temp_value: "main-cbd".to_string(),
            original_value: Some("main".to_string()),
        },
    );
    resource.explicit = ExplicitFields::Struct {
        children: HashMap::from([("cidr_block".to_string(), ExplicitFields::Leaf)]),
    };
    resource.binding = Some("vpc".to_string());
    resource.dependency_bindings.insert("internet".to_string());
    resource.write_only_attributes.push("token".to_string());
    resource.partial_read = Some(PartialReadMarker {
        detail: "partial".to_string(),
        missing_attributes: BTreeSet::from(["token".to_string()]),
    });
    resource.deposed.push(DeposedInstance {
        key: DeposedKey::new_unique(),
        identifier: "vpc-old".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: HashMap::from([("cidr_block".to_string(), serde_json::json!("10.0.0.0/16"))]),
        dependency_bindings: BTreeSet::from(["network".to_string()]),
    });
    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");

    let removed = state.remove_resource("aws", "ec2.Vpc", "main");

    assert_eq!(
        removed.and_then(|rs| rs.identifier),
        Some("vpc-new".to_string())
    );
    let retained = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("row with deposed generations should be retained");
    assert_eq!(retained.identifier, None);
    assert!(retained.attributes.is_empty());
    assert_eq!(retained.deposed.len(), 1);
    assert_eq!(retained.deposed[0].identifier, "vpc-old");
    assert_eq!(
        retained.deposed[0].provider_instance.as_deref(),
        Some("west")
    );
    assert!(!retained.protected);
    let expected_directives = Directives {
        provider_instance: Some("west".to_string()),
        ..Directives::default()
    };
    assert_eq!(retained.directives, expected_directives);
    assert_eq!(
        StateFile::id_for_resource_state(retained),
        ResourceId::with_provider_identity("aws", "ec2.Vpc", "main", Some("west".to_string()))
    );
    assert!(retained.prefixes.is_empty());
    assert!(retained.name_overrides.is_empty());
    assert_eq!(retained.explicit, ExplicitFields::default());
    assert_eq!(retained.binding, None);
    assert!(retained.dependency_bindings.is_empty());
    assert!(retained.write_only_attributes.is_empty());
    assert_eq!(retained.partial_read, None);
}

#[test]
fn test_resource_state_protected() {
    let resource = ResourceState::new("s3.Bucket", "state-bucket", "aws").with_protected(true);
    assert!(resource.protected);
}

#[test]
fn test_resource_state_managed_state_bucket_shape() {
    // The seed must use the desired resource's resolved anonymous identifier
    // as `identity` (so the differ matches the seed against the desired resource)
    // AND set the AWS bucket name as `identifier`. Conflating the two
    // reproduces #2533: phantom Delete on the seed's identity plus phantom
    // Create on the desired's identity.
    let resource = ResourceState::managed_state_bucket(
        "aws",
        "s3.Bucket",
        "aws_s3_bucket_a3f2b1c8",
        "my-state-bucket",
    );
    assert_eq!(resource.provider, "aws");
    assert_eq!(resource.resource_type, "s3.Bucket");
    assert_eq!(
        resource.identity, "aws_s3_bucket_a3f2b1c8",
        "identity must match the desired resource's anonymous identifier"
    );
    assert_eq!(
        resource.identifier.as_deref(),
        Some("my-state-bucket"),
        "identifier must be the AWS bucket name so the provider can Read/Update it"
    );
    assert!(resource.protected);
    assert_eq!(
        resource.attributes.get("bucket"),
        Some(&serde_json::json!("my-state-bucket"))
    );
}

#[test]
fn test_state_file_with_managed_state_bucket_contains_one_resource() {
    let state = StateFile::with_managed_state_bucket(
        "aws",
        "s3.Bucket",
        "aws_s3_bucket_a3f2b1c8",
        "my-state-bucket",
    );
    assert_eq!(state.resources.len(), 1);
    let bucket = &state.resources[0];
    assert_eq!(bucket.identity, "aws_s3_bucket_a3f2b1c8");
    assert_eq!(bucket.identifier.as_deref(), Some("my-state-bucket"));
    assert!(bucket.protected);
}

#[test]
fn test_state_file_serialization() {
    let mut state = StateFile::new();
    let resource = ResourceState::new("s3.Bucket", "my-bucket", "aws")
        .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"))
        .with_attribute("versioning".to_string(), serde_json::json!("Enabled"));

    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");

    let json = serde_json::to_string_pretty(&state).unwrap();
    let deserialized: StateFile = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.version, state.version);
    assert_eq!(deserialized.serial, state.serial);
    assert_eq!(deserialized.lineage, state.lineage);
    assert_eq!(deserialized.resources.len(), 1);
}

#[test]
fn test_resource_state_prefixes_serialization() {
    let mut resource = ResourceState::new("s3.Bucket", "test-bucket", "awscc").with_attribute(
        "bucket_name".to_string(),
        serde_json::json!("my-app-abcd1234"),
    );
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());

    let json = serde_json::to_string_pretty(&resource).unwrap();
    let deserialized: ResourceState = serde_json::from_str(&json).unwrap();

    assert_eq!(
        deserialized.prefixes.get("bucket_name"),
        Some(&"my-app-".to_string())
    );
}

#[test]
fn name_override_deserializes_legacy_bare_string_to_original_none() {
    let deserialized: NameOverride = serde_json::from_str(r#""foo""#).unwrap();

    assert_eq!(
        deserialized,
        NameOverride {
            temp_value: "foo".to_string(),
            original_value: None,
        }
    );
}

#[test]
fn name_override_deserializes_struct_form() {
    let original = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: Some("foo".to_string()),
    };

    let json = serde_json::to_string(&original).unwrap();
    let deserialized: NameOverride = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized, original);
}

#[test]
fn state_file_has_legacy_name_overrides_detects_v7_shape() {
    let legacy_json = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "serial": 0,
        "lineage": "lineage",
        "carina_version": "test",
        "resources": [
            {
                "resource_type": "test.resource",
                "identity": "legacy",
                "provider": "mock",
                "identifier": "legacy-id",
                "attributes": {},
                "name_overrides": {
                    "name": "legacy-temp"
                }
            },
            {
                "resource_type": "test.resource",
                "identity": "current",
                "provider": "mock",
                "identifier": "current-id",
                "attributes": {},
                "name_overrides": {
                    "name": {
                        "temp_value": "current-temp",
                        "original_value": "current"
                    }
                }
            }
        ]
    });
    let state: StateFile = serde_json::from_value(legacy_json).unwrap();

    assert!(state.has_legacy_name_overrides());
    let affected = state.legacy_name_override_resources();
    assert_eq!(affected.len(), 1);
    assert_eq!(affected[0].identity, "legacy");

    let typed_only_json = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "serial": 0,
        "lineage": "lineage",
        "carina_version": "test",
        "resources": [
            {
                "resource_type": "test.resource",
                "identity": "current",
                "provider": "mock",
                "identifier": "current-id",
                "attributes": {},
                "name_overrides": {
                    "name": {
                        "temp_value": "current-temp",
                        "original_value": "current"
                    }
                }
            }
        ]
    });
    let typed_only: StateFile = serde_json::from_value(typed_only_json).unwrap();
    assert!(!typed_only.has_legacy_name_overrides());
    assert!(typed_only.legacy_name_override_resources().is_empty());
}

#[test]
fn should_apply_override_returns_apply_when_dsl_matches_recorded() {
    let override_ = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: Some("foo".to_string()),
    };

    assert_eq!(
        should_apply_override(Some("foo"), &override_),
        ApplyDecision::Apply
    );
}

#[test]
fn should_apply_override_returns_skip_when_dsl_diverges() {
    let override_ = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: Some("foo".to_string()),
    };

    assert_eq!(
        should_apply_override(Some("bar"), &override_),
        ApplyDecision::Skip
    );
}

#[test]
fn should_apply_override_returns_apply_with_unknown_dsl_when_dsl_unresolved() {
    let override_ = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: Some("foo".to_string()),
    };

    assert_eq!(
        should_apply_override(None, &override_),
        ApplyDecision::ApplyWithUnknownDsl
    );
}

#[test]
fn should_apply_override_returns_apply_legacy_for_pre_phase_5_state() {
    let override_ = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: None,
    };

    assert_eq!(
        should_apply_override(Some("renamed"), &override_),
        ApplyDecision::ApplyLegacy
    );
}

#[test]
fn test_get_identifier_for_resource_from_state() {
    use carina_core::resource::Resource;

    let mut state = StateFile::new();
    let rs =
        ResourceState::new("s3.Bucket", "my-bucket", "awscc").with_identifier("my-bucket-abcd1234");
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    assert_eq!(
        state.get_identifier_for_resource(&resource),
        Some("my-bucket-abcd1234".to_string())
    );
}

#[test]
fn test_get_identifier_for_resource_returns_none() {
    use carina_core::resource::Resource;

    let state = StateFile::new();
    let resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    assert_eq!(state.get_identifier_for_resource(&resource), None);
}

#[test]
fn test_build_directives() {
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "my-bucket", "awscc");
    rs.directives.force_delete = true;
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let directives_map = state.build_directives();
    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "my-bucket", None);
    assert!(directives_map.get(&id).unwrap().force_delete);
}

#[test]
fn test_build_saved_attrs() {
    use carina_core::resource::{ConcreteValue, ResourceId, Value};

    let mut state = StateFile::new();
    let rs = ResourceState::new("s3.Bucket", "my-bucket", "awscc")
        .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"));
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let saved = state.build_saved_attrs();
    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "my-bucket", None);
    let attrs = saved.get(&id).unwrap();
    assert_eq!(
        attrs.get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1".to_string()
        )))
    );
}

#[test]
fn test_resource_state_serialization_with_binding_and_deps() {
    let json = r#"{
        "resource_type": "s3.Bucket",
        "name": "my-bucket",
        "provider": "aws",
        "attributes": {"region": "ap-northeast-1"},
        "protected": false,
        "directives": {},
        "prefixes": {},
        "name_overrides": {},
        "desired_keys": [],
        "binding": "my_bucket",
        "dependency_bindings": ["vpc", "subnet"]
    }"#;

    let deserialized: ResourceState = serde_json::from_str(json).unwrap();
    assert_eq!(deserialized.identity, "my-bucket");
    assert_eq!(deserialized.binding, Some("my_bucket".to_string()));
    assert_eq!(
        deserialized.dependency_bindings,
        BTreeSet::from(["vpc".to_string(), "subnet".to_string()])
    );
}

#[test]
fn test_resource_state_deserialization_without_v3_fields() {
    // v2 state files don't have binding or dependency_bindings fields
    let json = r#"{
        "resource_type": "s3.Bucket",
        "name": "my-bucket",
        "provider": "aws",
        "attributes": {"region": "ap-northeast-1"},
        "protected": false,
        "directives": {},
        "prefixes": {},
        "name_overrides": {},
        "desired_keys": []
    }"#;

    let deserialized: ResourceState = serde_json::from_str(json).unwrap();
    assert_eq!(deserialized.identity, "my-bucket");
    assert_eq!(deserialized.binding, None);
    assert!(deserialized.dependency_bindings.is_empty());
    assert!(deserialized.write_only_attributes.is_empty());
}

#[test]
fn resource_state_name_overrides_migrates_from_v7_bare_string() {
    let json = r#"{
        "resource_type": "s3.Bucket",
        "identity": "my-bucket",
        "provider": "aws",
        "attributes": {"bucket": "old-name"},
        "protected": false,
        "directives": {},
        "prefixes": {},
        "name_overrides": {"bucket": "old-name-cbd"},
        "binding": "my_bucket",
        "dependency_bindings": []
    }"#;

    let deserialized: ResourceState = serde_json::from_str(json).unwrap();

    assert_eq!(
        deserialized.name_overrides.get("bucket"),
        Some(&NameOverride {
            temp_value: "old-name-cbd".to_string(),
            original_value: None,
        })
    );
}

#[test]
fn test_from_provider_state() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    resource.directives.force_delete = true;
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("my-bucket-abcd1234".to_string()),
        attributes: [(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let existing = ResourceState::new("s3.Bucket", "my-bucket", "awscc").with_protected(true);

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        Some(&existing),
        None,
    )
    .unwrap();

    assert_eq!(rs.identifier, Some("my-bucket-abcd1234".to_string()));
    assert_eq!(
        rs.attributes.get("region"),
        Some(&serde_json::json!("ap-northeast-1"))
    );
    assert!(rs.protected);
    assert!(rs.directives.force_delete);
    assert_eq!(rs.prefixes.get("bucket_name"), Some(&"my-app-".to_string()));
}

#[test]
fn test_from_provider_state_without_existing() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let resource = Resource::with_provider("aws", "s3.Bucket", "test", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("test-id".to_string()),
        attributes: [(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();
    assert!(!rs.protected);
    assert_eq!(rs.identifier, Some("test-id".to_string()));
}

#[test]
fn from_provider_state_aligns_reordered_nested_and_provider_added_list_elements() {
    let nested_group = |rules| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("id".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            (
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(rules)),
            ),
        ])))
    };
    let mut resource = Resource::with_provider("mock", "listener.Listener", "listener", None);
    resource.set_attr(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            list_rule(80, Some("web")),
            list_rule(443, None),
        ])),
    );
    resource.set_attr(
        "groups".to_string(),
        Value::Concrete(ConcreteValue::List(vec![nested_group(vec![
            list_rule(80, Some("web")),
            list_rule(443, None),
        ])])),
    );
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([
            (
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    list_rule(443, Some("provider-default")),
                    list_rule(22, Some("provider-added")),
                    list_rule(80, Some("web")),
                ])),
            ),
            (
                "groups".to_string(),
                Value::Concrete(ConcreteValue::List(vec![nested_group(vec![
                    list_rule(443, Some("provider-default")),
                    list_rule(80, Some("web")),
                ])])),
            ),
        ]),
    )
    .with_identifier("listener-id");

    let row = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();

    let stored_rules = row.attributes["rules"].as_array().expect("stored rules");
    assert_eq!(stored_rules.len(), 3);
    assert_eq!(stored_rules[0]["port"], 443);
    assert_eq!(stored_rules[1]["port"], 22);
    assert_eq!(stored_rules[2]["port"], 80);
    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("expected resource-root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("expected aligned rules ListElements");
    };
    assert_eq!(elements.len(), stored_rules.len());
    assert_explicit_struct_keys(&elements[0], &["port"]);
    assert_eq!(elements[1], ExplicitFields::Unrecorded);
    assert_explicit_struct_keys(&elements[2], &["port", "description"]);

    let ExplicitFields::ListElements { elements: groups } = &children["groups"] else {
        panic!("expected aligned groups ListElements");
    };
    let ExplicitFields::Struct {
        children: group_children,
    } = &groups[0]
    else {
        panic!("expected group Struct");
    };
    let ExplicitFields::ListElements {
        elements: nested_rules,
    } = &group_children["rules"]
    else {
        panic!("expected nested aligned ListElements");
    };
    assert_explicit_struct_keys(&nested_rules[0], &["port"]);
    assert_explicit_struct_keys(&nested_rules[1], &["port", "description"]);
}

#[test]
fn test_from_provider_state_repairs_unrecorded_from_state_attrs() {
    // carina#3280: when the prior on-disk row carries `Unrecorded` (the
    // legacy-corruption marker, emitted by the v6→v7 migration for rows
    // previously persisted as `Struct { children: {} }`) AND
    // `resource.attributes` is empty AND the freshly-read
    // `state.attributes` is populated, rebuild `explicit` from the
    // fresh state so the next write replaces the corrupt row.
    use carina_core::explicit::ExplicitFields;
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let resource = Resource::with_provider("awscc", "sso.Assignment", "x", None);
    // resource.attributes intentionally left empty — this is the buggy
    // input the old expansion path delivered to state writeback.
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("identifier".to_string()),
        attributes: [
            (
                "principal_type".to_string(),
                Value::Concrete(ConcreteValue::String("GROUP".to_string())),
            ),
            (
                "target_id".to_string(),
                Value::Concrete(ConcreteValue::String("123".to_string())),
            ),
        ]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let mut existing = ResourceState::new("sso.Assignment", "x", "awscc");
    existing.explicit = ExplicitFields::Unrecorded;

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        Some(&existing),
        None,
    )
    .unwrap();

    let ExplicitFields::Struct { children } = &rs.explicit else {
        panic!(
            "expected Struct explicit after repair, got {:?}",
            rs.explicit
        );
    };
    assert!(
        children.contains_key("principal_type"),
        "explicit should be rebuilt from state.attributes when prior on-disk explicit was `Unrecorded`"
    );
    assert!(children.contains_key("target_id"));
}

#[test]
fn test_from_provider_state_emits_unrecorded_for_fresh_empty_body_resource() {
    // carina#3280: a green-field write of a resource with no DSL
    // attributes (e.g. `aws.sts.CallerIdentity {}`, or `carina state
    // import`) must emit `Unrecorded` — NOT `Struct { children: {} }`.
    // Pre-fix resource authoring produced the ambiguous empty
    // Struct shape, which the differ used to interpret as "user
    // authored an empty struct, drop every server-side attribute".
    // The typed signal removes the ambiguity at the source.
    use carina_core::explicit::ExplicitFields;
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let resource = Resource::with_provider("aws", "sts.CallerIdentity", "caller", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("identifier".to_string()),
        attributes: [(
            "account_id".to_string(),
            Value::Concrete(ConcreteValue::String("123456789012".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();

    assert!(
        matches!(rs.explicit, ExplicitFields::Unrecorded),
        "first-apply empty-body resource must emit `Unrecorded`, got {:?}",
        rs.explicit
    );
}

#[test]
fn test_from_provider_state_preserves_populated_struct_when_resource_attrs_empty() {
    // carina#3280 idempotency: after the self-heal path runs once, the
    // on-disk row carries a populated `Struct`. On the next apply (no
    // DSL change), `resource.attributes` is still empty (the user's
    // bodyless DSL hasn't changed), so the aligned builder produces
    // `Struct { children: {} }` again. Without the preservation arm,
    // the empty-Struct collapse would overwrite the populated record
    // with `Unrecorded`, flip-flopping the row on every apply and
    // churning state `serial`. Preserve the existing populated Struct.
    use carina_core::explicit::ExplicitFields;
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let resource = Resource::with_provider("aws", "sts.CallerIdentity", "caller", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("id".to_string()),
        attributes: [(
            "account_id".to_string(),
            Value::Concrete(ConcreteValue::String("123".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    // Prior on-disk: populated Struct (e.g. from a previous self-heal).
    let mut existing = ResourceState::new("sts.CallerIdentity", "caller", "aws");
    let populated = ExplicitFields::Struct {
        children: HashMap::from([("account_id".into(), ExplicitFields::Leaf)]),
    };
    existing.explicit = populated.clone();

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        Some(&existing),
        None,
    )
    .unwrap();

    assert_eq!(
        rs.explicit, populated,
        "populated Struct must be preserved on re-apply with no DSL change; \
         got {:?}",
        rs.explicit
    );
}

#[test]
fn test_from_provider_state_demotes_top_level_list_elements_when_resource_attrs_empty() {
    let resource = Resource::with_provider("mock", "test.Resource", "empty", None);
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([(
            "provider_value".to_string(),
            Value::Concrete(ConcreteValue::String("value".to_string())),
        )]),
    )
    .with_identifier("resource-id");
    let mut existing = ResourceState::new("test.Resource", "empty", "mock");
    let populated = ExplicitFields::ListElements {
        elements: vec![ExplicitFields::Struct {
            children: HashMap::from([("id".to_string(), ExplicitFields::Leaf)]),
        }],
    };
    existing.explicit = populated.clone();

    let row = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        Some(&existing),
        None,
    )
    .unwrap();

    assert_eq!(
        row.explicit,
        ExplicitFields::List {
            element: Box::new(ExplicitFields::Struct {
                children: HashMap::from([("id".to_string(), ExplicitFields::Leaf)]),
            }),
        }
    );
}

#[test]
fn bodyless_refresh_demotes_reordered_list_elements_and_merge_stays_conservative() {
    use carina_core::differ::{Diff, diff};

    let resource = Resource::with_provider("mock", "listener.Listener", "listener", None);
    let reordered_provider = State::existing(
        resource.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(443, Some("provider-default")),
                list_rule(80, Some("web")),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let mut existing = ResourceState::new("listener.Listener", "listener", "mock");
    existing.explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::ListElements {
                elements: vec![
                    ExplicitFields::Struct {
                        children: HashMap::from([
                            ("port".to_string(), ExplicitFields::Leaf),
                            ("description".to_string(), ExplicitFields::Leaf),
                        ]),
                    },
                    ExplicitFields::Struct {
                        children: HashMap::from([("port".to_string(), ExplicitFields::Leaf)]),
                    },
                ],
            },
        )]),
    };

    let row = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &reordered_provider,
        Some(&existing),
        None,
    )
    .unwrap();
    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("expected preserved root Struct");
    };
    let ExplicitFields::List { element } = &children["rules"] else {
        panic!("reordered provider values must demote nested ListElements");
    };
    assert_explicit_struct_keys(element, &["port", "description"]);

    let mut state = StateFile::new();
    state.upsert_resource(row.clone()).unwrap();
    let saved = state.build_saved_attrs();
    let reauthored = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, None),
                list_rule(443, None),
            ])),
        );
    assert!(matches!(
        diff(
            &reauthored,
            &reordered_provider,
            saved.get(&resource.id),
            Some(&row.explicit),
            None,
        ),
        Diff::NoChange(_)
    ));
}

#[test]
fn test_from_provider_state_no_repair_when_state_attrs_also_empty() {
    // carina#3280 case (b): existing is `Unrecorded` AND fresh
    // `state.attributes` is empty (no provider data to promote). The
    // repair cannot rebuild authoring from nothing — emit `Unrecorded`
    // (stable fixed point), do not crash, do not invent attributes.
    use carina_core::explicit::ExplicitFields;
    use carina_core::resource::{Resource, State as ProviderState};

    let resource = Resource::with_provider("awscc", "sso.Assignment", "x", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("id".to_string()),
        attributes: HashMap::new(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let mut existing = ResourceState::new("sso.Assignment", "x", "awscc");
    existing.explicit = ExplicitFields::Unrecorded;

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        Some(&existing),
        None,
    )
    .unwrap();

    assert!(
        matches!(rs.explicit, ExplicitFields::Unrecorded),
        "Unrecorded + empty state.attributes must stay Unrecorded, got {:?}",
        rs.explicit
    );
}

#[test]
fn test_migrate_v6_empty_struct_to_unrecorded() {
    // carina#3280: state files at v6 carry the corrupt-row shape as
    // `Struct { children: {} }`. The v6 → v7 migration rewrites
    // every top-level empty Struct to `Unrecorded` so callers never
    // encounter the ambiguous shape after read.
    use carina_core::explicit::ExplicitFields;
    let v6 = r#"{
        "version": 6,
        "serial": 1,
        "lineage": "test-lineage",
        "carina_version": "0.1.0",
        "resources": [
            {
                "resource_type": "sso.Assignment",
                "name": "x",
                "provider": "awscc",
                "identifier": "id",
                "attributes": { "target_id": "123" },
                "protected": false,
                "directives": {},
                "prefixes": {},
                "name_overrides": {},
                "binding": "x",
                "dependency_bindings": [],
                "explicit": { "kind": "struct", "children": {} }
            }
        ]
    }"#;
    let state = check_and_migrate(v6)
        .expect("migration should succeed")
        .into_state();
    assert_eq!(state.version, StateFile::CURRENT_VERSION);
    let rs = state
        .resources
        .iter()
        .find(|r| r.identity == "x")
        .expect("test resource");
    assert!(
        matches!(rs.explicit, ExplicitFields::Unrecorded),
        "v6 empty-Struct row must migrate to Unrecorded, got {:?}",
        rs.explicit
    );
}

#[test]
fn test_migrate_v6_preserves_populated_explicit() {
    // carina#3280 sibling: the v6 → v7 migration must leave populated
    // `Struct` rows untouched. Only the top-level empty-Struct
    // (corruption) shape is rewritten.
    use carina_core::explicit::ExplicitFields;
    let v6 = r#"{
        "version": 6,
        "serial": 1,
        "lineage": "test-lineage",
        "carina_version": "0.1.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "name": "vpc",
                "provider": "awscc",
                "identifier": "vpc-1",
                "attributes": { "cidr_block": "10.0.0.0/16" },
                "protected": false,
                "directives": {},
                "prefixes": {},
                "name_overrides": {},
                "binding": "vpc",
                "dependency_bindings": [],
                "explicit": {
                    "kind": "struct",
                    "children": { "cidr_block": { "kind": "leaf" } }
                }
            }
        ]
    }"#;
    let state = check_and_migrate(v6)
        .expect("migration should succeed")
        .into_state();
    let rs = state
        .resources
        .iter()
        .find(|r| r.identity == "vpc")
        .unwrap();
    let ExplicitFields::Struct { children } = &rs.explicit else {
        panic!(
            "populated v6 Struct must survive migration, got {:?}",
            rs.explicit
        );
    };
    assert!(children.contains_key("cidr_block"));
}

#[test]
fn test_migrate_v6_does_not_rewrite_nested_empty_struct() {
    // carina#3280: the migration is top-level-only by design. A
    // nested empty `Struct { children: {} }` (the legitimate
    // "user wrote `tags = {}`" shape) must survive the migration
    // unchanged — it is structurally meaningful at that position
    // (recursive `project` correctly drops every field), unlike the
    // top-level legacy-corruption case.
    use carina_core::explicit::ExplicitFields;
    let v6 = r#"{
        "version": 6,
        "serial": 1,
        "lineage": "test-lineage",
        "carina_version": "0.1.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "name": "vpc",
                "provider": "awscc",
                "identifier": "vpc-1",
                "attributes": {},
                "protected": false,
                "directives": {},
                "prefixes": {},
                "name_overrides": {},
                "binding": "vpc",
                "dependency_bindings": [],
                "explicit": {
                    "kind": "struct",
                    "children": {
                        "tags": { "kind": "struct", "children": {} }
                    }
                }
            }
        ]
    }"#;
    let state = check_and_migrate(v6)
        .expect("migration should succeed")
        .into_state();
    let rs = state
        .resources
        .iter()
        .find(|r| r.identity == "vpc")
        .unwrap();
    let ExplicitFields::Struct { children } = &rs.explicit else {
        panic!("expected top-level Struct, got {:?}", rs.explicit);
    };
    let tags = children.get("tags").expect("tags child");
    assert!(
        matches!(tags, ExplicitFields::Struct { children } if children.is_empty()),
        "nested empty Struct must survive migration (legitimate `tags = {{}}` shape); got {:?}",
        tags
    );
}

#[test]
fn test_multi_provider_resources_do_not_collide() {
    use carina_core::resource::Resource;

    let mut state = StateFile::new();

    // Store two resources with the same resource_type and name but different providers
    let aws_resource =
        ResourceState::new("s3.Bucket", "main", "aws").with_identifier("aws-bucket-id");
    let awscc_resource =
        ResourceState::new("s3.Bucket", "main", "awscc").with_identifier("awscc-bucket-id");

    state
        .upsert_resource(aws_resource)
        .expect("test state setup must be valid");
    state
        .upsert_resource(awscc_resource)
        .expect("test state setup must be valid");

    // Both should be stored independently
    assert_eq!(state.resources.len(), 2);

    // find_resource should return the correct one for each provider
    let found_aws = state.find_resource("aws", "s3.Bucket", "main").unwrap();
    assert_eq!(found_aws.identifier, Some("aws-bucket-id".to_string()));

    let found_awscc = state.find_resource("awscc", "s3.Bucket", "main").unwrap();
    assert_eq!(found_awscc.identifier, Some("awscc-bucket-id".to_string()));

    // get_identifier_for_resource should return provider-scoped identifiers
    let aws_res = Resource::with_provider("aws", "s3.Bucket", "main", None);
    assert_eq!(
        state.get_identifier_for_resource(&aws_res),
        Some("aws-bucket-id".to_string())
    );

    let awscc_res = Resource::with_provider("awscc", "s3.Bucket", "main", None);
    assert_eq!(
        state.get_identifier_for_resource(&awscc_res),
        Some("awscc-bucket-id".to_string())
    );

    // Upsert should only update the matching provider's entry
    let updated_aws =
        ResourceState::new("s3.Bucket", "main", "aws").with_identifier("aws-bucket-id-v2");
    state
        .upsert_resource(updated_aws)
        .expect("test state setup must be valid");
    assert_eq!(state.resources.len(), 2);
    assert_eq!(
        state
            .find_resource("aws", "s3.Bucket", "main")
            .unwrap()
            .identifier,
        Some("aws-bucket-id-v2".to_string())
    );
    assert_eq!(
        state
            .find_resource("awscc", "s3.Bucket", "main")
            .unwrap()
            .identifier,
        Some("awscc-bucket-id".to_string())
    );

    // remove_resource should only remove the matching provider's entry
    let removed = state.remove_resource("aws", "s3.Bucket", "main");
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().provider, "aws");
    assert_eq!(state.resources.len(), 1);

    // The awscc entry should still exist
    assert!(state.find_resource("awscc", "s3.Bucket", "main").is_some());
    assert!(state.find_resource("aws", "s3.Bucket", "main").is_none());
}

#[test]
fn test_build_directives_provider_scoped() {
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut aws_rs = ResourceState::new("s3.Bucket", "main", "aws");
    aws_rs.directives.force_delete = true;
    let awscc_rs = ResourceState::new("s3.Bucket", "main", "awscc");

    state
        .upsert_resource(aws_rs)
        .expect("test state setup must be valid");
    state
        .upsert_resource(awscc_rs)
        .expect("test state setup must be valid");

    let directives_map = state.build_directives();
    let aws_id = ResourceId::with_provider_identity("aws", "s3.Bucket", "main", None);
    let awscc_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "main", None);

    assert!(directives_map.get(&aws_id).unwrap().force_delete);
    assert!(!directives_map.get(&awscc_id).unwrap().force_delete);
}

#[test]
fn test_build_saved_attrs_provider_scoped() {
    use carina_core::resource::{ConcreteValue, ResourceId, Value};

    let mut state = StateFile::new();
    let aws_rs = ResourceState::new("s3.Bucket", "main", "aws")
        .with_attribute("region".to_string(), serde_json::json!("us-east-1"));
    let awscc_rs = ResourceState::new("s3.Bucket", "main", "awscc")
        .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"));

    state
        .upsert_resource(aws_rs)
        .expect("test state setup must be valid");
    state
        .upsert_resource(awscc_rs)
        .expect("test state setup must be valid");

    let saved = state.build_saved_attrs();
    let aws_id = ResourceId::with_provider_identity("aws", "s3.Bucket", "main", None);
    let awscc_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "main", None);

    assert_eq!(
        saved.get(&aws_id).unwrap().get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "us-east-1".to_string()
        )))
    );
    assert_eq!(
        saved.get(&awscc_id).unwrap().get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1".to_string()
        )))
    );
}

#[test]
fn test_build_state_for_resource_existing() {
    use carina_core::resource::{ConcreteValue, Resource, Value};

    let mut state = StateFile::new();
    state
        .upsert_resource(
            ResourceState::new("s3.Bucket", "my-bucket", "awscc")
                .with_identifier("my-bucket-id")
                .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1")),
        )
        .expect("test state setup must be valid");

    let resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    let result = state.build_state_for_resource(&resource.id);

    assert!(result.exists);
    assert_eq!(result.identifier, Some("my-bucket-id".to_string()));
    assert_eq!(
        result.attributes.get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1".to_string()
        )))
    );
}

#[test]
fn test_build_state_for_resource_not_found() {
    let state = StateFile::new();
    let resource =
        carina_core::resource::Resource::with_provider("awscc", "s3.Bucket", "missing", None);
    let result = state.build_state_for_resource(&resource.id);

    assert!(!result.exists);
    assert!(result.identifier.is_none());
    assert!(result.attributes.is_empty());
}

#[test]
fn test_build_state_for_resource_without_identifier() {
    let mut state = StateFile::new();
    // Resource in state but without identifier (not yet created)
    state
        .upsert_resource(
            ResourceState::new("s3.Bucket", "pending", "awscc")
                .with_attribute("region".to_string(), serde_json::json!("us-east-1")),
        )
        .expect("test state setup must be valid");

    let resource =
        carina_core::resource::Resource::with_provider("awscc", "s3.Bucket", "pending", None);
    let result = state.build_state_for_resource(&resource.id);

    assert!(!result.exists);
    assert!(result.identifier.is_none());
}

#[test]
fn test_from_provider_state_stores_binding_and_dependencies() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let mut resource = Resource::with_provider("awscc", "ec2.Subnet", "my-subnet", None);
    resource.binding = Some("my_subnet".to_string());
    resource.set_attr(
        "vpc_id".to_string(),
        Value::resource_ref("my_vpc".to_string(), "vpc_id".to_string(), vec![]),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("subnet-123".to_string()),
        attributes: [(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-abc".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();
    assert_eq!(rs.binding, Some("my_subnet".to_string()));
    assert_eq!(
        rs.dependency_bindings,
        BTreeSet::from(["my_vpc".to_string()])
    );
}

#[test]
fn test_build_orphan_states_injects_binding() {
    use carina_core::resource::{ConcreteValue, ResourceId, Value};

    let mut state = StateFile::new();
    let mut rs =
        ResourceState::new("ec2.Subnet", "orphan-subnet", "awscc").with_identifier("subnet-123");
    rs.binding = Some("my_subnet".to_string());
    rs.dependency_bindings = BTreeSet::from(["my_vpc".to_string()]);
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let desired_ids = std::collections::HashSet::new();
    let orphans = state.build_orphan_states(&desired_ids);

    let id = ResourceId::with_provider_identity("awscc", "ec2.Subnet", "orphan-subnet", None);
    let orphan_state = orphans.get(&id).unwrap();
    assert!(orphan_state.exists);
    assert_eq!(
        orphan_state.attributes.get("_binding"),
        Some(&Value::Concrete(ConcreteValue::String(
            "my_subnet".to_string()
        )))
    );
}

#[test]
fn test_build_orphan_dependencies() {
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut rs =
        ResourceState::new("ec2.Subnet", "orphan-subnet", "awscc").with_identifier("subnet-123");
    rs.binding = Some("my_subnet".to_string());
    rs.dependency_bindings = BTreeSet::from(["my_vpc".to_string()]);
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let desired_ids = std::collections::HashSet::new();
    let deps = state.build_orphan_dependencies(&desired_ids);

    let id = ResourceId::with_provider_identity("awscc", "ec2.Subnet", "orphan-subnet", None);
    assert_eq!(
        deps.get(&id).unwrap(),
        &BTreeSet::from(["my_vpc".to_string()])
    );
}

#[test]
fn test_state_file_version_is_v10() {
    let state = StateFile::new();
    assert_eq!(state.version, 10);
}

#[test]
fn v8_state_migrates_to_v10_with_empty_deposed() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 8,
        "serial": 5,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "identity": "vpc",
                "provider": "awscc",
                "identifier": "vpc-current",
                "attributes": { "vpc_id": "vpc-current" }
            }
        ]
    }"#;

    let outcome = check_and_migrate(json).expect("v8 state should migrate to v10");
    let migration = outcome
        .migration
        .expect("v8 read should report a v10 migration");
    assert_eq!(migration.from, 8);
    assert_eq!(migration.to, StateFile::CURRENT_VERSION);
    let row = outcome
        .state
        .find_resource("awscc", "ec2.Vpc", "vpc")
        .expect("resource should survive migration");
    let row_json = serde_json::to_value(row).expect("resource state should serialize");
    assert!(
        row_json.get("deposed").is_none(),
        "empty deposed vec should use serde default and skip serialization"
    );
}

#[test]
fn v9_deposed_entries_round_trip() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 9,
        "serial": 7,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "identity": "vpc",
                "provider": "awscc",
                "identifier": "vpc-new",
                "attributes": { "vpc_id": "vpc-new" },
                "deposed": [
                    {
                        "key": "deposed-1",
                        "identifier": "vpc-old",
                        "provider_instance": "west",
                        "attributes": { "vpc_id": "vpc-old" },
                        "dependency_bindings": ["igw"]
                    }
                ]
            }
        ]
    }"#;

    let state = check_and_migrate(json)
        .expect("v9 state with deposed entries should load")
        .into_state();
    let serialized = serde_json::to_value(&state).expect("state should serialize");
    let reloaded: StateFile =
        serde_json::from_value(serialized.clone()).expect("serialized v9 state should reload");
    assert_eq!(
        serde_json::to_value(reloaded).expect("reloaded state should serialize"),
        serialized
    );
    let row = state
        .find_resource("awscc", "ec2.Vpc", "vpc")
        .expect("resource should exist");
    let row_json = serde_json::to_value(row).expect("resource state should serialize");
    assert_eq!(row_json["deposed"][0]["identifier"], "vpc-old");
    assert_eq!(row_json["deposed"][0]["provider_instance"], "west");
    assert_eq!(row_json["deposed"][0]["dependency_bindings"][0], "igw");
}

#[test]
fn check_and_migrate_retains_identifier_none_rows_only_when_deposed_is_non_empty() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 9,
        "serial": 7,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "identity": "deposed_only",
                "provider": "awscc",
                "identifier": null,
                "attributes": {},
                "deposed": [
                    {
                        "key": "deposed-1",
                        "identifier": "vpc-old",
                        "attributes": { "vpc_id": "vpc-old" },
                        "dependency_bindings": []
                    }
                ]
            },
            {
                "resource_type": "ec2.Vpc",
                "identity": "empty_artifact",
                "provider": "awscc",
                "identifier": null,
                "attributes": {}
            }
        ]
    }"#;

    let state = check_and_migrate(json)
        .expect("v9 deposed-only row should load")
        .into_state();
    assert!(
        state
            .find_resource("awscc", "ec2.Vpc", "deposed_only")
            .is_some(),
        "identifier=None row must survive while deposed entries remain"
    );
    assert!(
        state
            .find_resource("awscc", "ec2.Vpc", "empty_artifact")
            .is_none(),
        "identifier=None row with no deposed entries should still be dropped"
    );
}

#[test]
fn test_build_orphan_dependencies_excludes_desired() {
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut rs =
        ResourceState::new("ec2.Subnet", "kept-subnet", "awscc").with_identifier("subnet-456");
    rs.dependency_bindings = BTreeSet::from(["my_vpc".to_string()]);
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let id = ResourceId::with_provider_identity("awscc", "ec2.Subnet", "kept-subnet", None);
    let mut desired_ids = std::collections::HashSet::new();
    desired_ids.insert(id.clone());

    let deps = state.build_orphan_dependencies(&desired_ids);
    assert!(deps.is_empty());
}

#[test]
fn test_check_and_migrate_current_version() {
    use super::check_and_migrate;

    let state = StateFile::new();
    let json = serde_json::to_string_pretty(&state).unwrap();
    let result = check_and_migrate(&json).unwrap().into_state();
    assert_eq!(result.version, StateFile::CURRENT_VERSION);
    assert_eq!(result.lineage, state.lineage);
}

fn persisted_resource(provider: &str, resource_type: &str, identity: &str) -> serde_json::Value {
    serde_json::json!({
        "resource_type": resource_type,
        "identity": identity,
        "provider": provider,
        "identifier": format!("{provider}-{resource_type}-{identity}"),
        "attributes": {}
    })
}

fn state_json_with_resources(version: u32, resources: Vec<serde_json::Value>) -> String {
    serde_json::json!({
        "version": version,
        "serial": 1,
        "lineage": "duplicate-identity-test",
        "carina_version": "test",
        "resources": resources
    })
    .to_string()
}

fn invalid_state_message(result: Result<MigratedStateFile, BackendError>) -> String {
    match result.expect_err("duplicate identity must be rejected") {
        BackendError::InvalidState(message) => message,
        other => panic!("expected InvalidState, got {other}"),
    }
}

#[test]
fn check_and_migrate_rejects_duplicate_resource_identity() {
    let resource = persisted_resource("aws", "ec2.Vpc", "network");
    let json =
        state_json_with_resources(StateFile::CURRENT_VERSION, vec![resource.clone(), resource]);

    let message = invalid_state_message(check_and_migrate(&json));

    assert!(message.contains("duplicate resource identity"), "{message}");
    assert!(message.contains("aws"), "{message}");
    assert!(message.contains("ec2.Vpc"), "{message}");
    assert!(message.contains("network"), "{message}");
}

#[test]
fn check_and_migrate_accepts_resource_identities_differing_on_each_axis() {
    let json = state_json_with_resources(
        StateFile::CURRENT_VERSION,
        vec![
            persisted_resource("aws", "ec2.Vpc", "network"),
            persisted_resource("awscc", "ec2.Vpc", "network"),
            persisted_resource("aws", "ec2.Subnet", "network"),
            persisted_resource("aws", "ec2.Vpc", "other-network"),
        ],
    );

    let state = check_and_migrate(&json)
        .expect("all three resource identity axes must participate in uniqueness")
        .into_state();

    assert_eq!(state.resources.len(), 4);
}

fn deposed_generation(
    key: &str,
    identifier: &str,
    provider_instance: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "key": key,
        "identifier": identifier,
        "provider_instance": provider_instance,
        "attributes": {},
        "dependency_bindings": []
    })
}

fn resource_with_deposed(deposed: Vec<serde_json::Value>) -> serde_json::Value {
    let mut resource = persisted_resource("awscc", "ec2.Vpc", "network");
    resource["deposed"] = serde_json::Value::Array(deposed);
    resource
}

#[test]
fn check_and_migrate_rejects_duplicate_deposed_key() {
    let json = state_json_with_resources(
        StateFile::CURRENT_VERSION,
        vec![resource_with_deposed(vec![
            deposed_generation("generation-a", "vpc-old-a", Some("west")),
            deposed_generation("generation-a", "vpc-old-b", Some("east")),
        ])],
    );

    let message = invalid_state_message(check_and_migrate(&json));

    assert!(
        message.contains("duplicate deposed generation key"),
        "{message}"
    );
    assert!(message.contains("awscc"), "{message}");
    assert!(message.contains("ec2.Vpc"), "{message}");
    assert!(message.contains("network"), "{message}");
    assert!(message.contains("generation-a"), "{message}");
}

#[test]
fn check_and_migrate_rejects_duplicate_deposed_identifier_and_provider_instance() {
    let json = state_json_with_resources(
        StateFile::CURRENT_VERSION,
        vec![resource_with_deposed(vec![
            deposed_generation("generation-a", "vpc-old", Some("west")),
            deposed_generation("generation-b", "vpc-old", Some("west")),
        ])],
    );

    let message = invalid_state_message(check_and_migrate(&json));

    assert!(
        message.contains("duplicate deposed generation identity"),
        "{message}"
    );
    assert!(message.contains("awscc"), "{message}");
    assert!(message.contains("ec2.Vpc"), "{message}");
    assert!(message.contains("network"), "{message}");
    assert!(message.contains("vpc-old"), "{message}");
    assert!(message.contains("west"), "{message}");
}

#[test]
fn check_and_migrate_loads_legacy_name_alias_and_checks_it_for_duplicates() {
    let mut legacy = persisted_resource("aws", "s3.Bucket", "logs");
    let identity = legacy.as_object_mut().unwrap().remove("identity").unwrap();
    legacy["name"] = identity;
    let valid_json = state_json_with_resources(StateFile::CURRENT_VERSION, vec![legacy.clone()]);

    let state = check_and_migrate(&valid_json)
        .expect("legacy name alias must remain readable")
        .into_state();
    assert_eq!(state.resources[0].identity, "logs");

    let duplicate_json = state_json_with_resources(
        StateFile::CURRENT_VERSION,
        vec![legacy, persisted_resource("aws", "s3.Bucket", "logs")],
    );
    let message = invalid_state_message(check_and_migrate(&duplicate_json));
    assert!(message.contains("duplicate resource identity"), "{message}");
    assert!(message.contains("logs"), "{message}");
}

#[test]
fn check_and_migrate_bytes_rejects_duplicate_resource_identity() {
    let resource = persisted_resource("aws", "ec2.Vpc", "network");
    let json =
        state_json_with_resources(StateFile::CURRENT_VERSION, vec![resource.clone(), resource]);

    let message = invalid_state_message(check_and_migrate_bytes(json.as_bytes()));

    assert!(message.contains("duplicate resource identity"), "{message}");
    assert!(message.contains("network"), "{message}");
}

fn assert_migration_rejects_canonicalized_duplicate(version: u32) {
    let mut legacy = persisted_resource("awscc", "sso.Assignment", "placeholder");
    legacy["name"] = serde_json::json!("_accounts[\"registry_prod\"]");
    legacy.as_object_mut().unwrap().remove("identity");
    let current = persisted_resource("awscc", "sso.Assignment", "_accounts.registry_prod");
    let json = state_json_with_resources(version, vec![legacy, current]);

    let message = invalid_state_message(check_and_migrate(&json));

    assert!(message.contains("duplicate resource identity"), "{message}");
    assert!(message.contains("awscc"), "{message}");
    assert!(message.contains("sso.Assignment"), "{message}");
    assert!(message.contains("_accounts.registry_prod"), "{message}");
}

#[test]
fn v5_migration_rejects_duplicate_created_by_canonicalization() {
    assert_migration_rejects_canonicalized_duplicate(5);
}

#[test]
fn v6_migration_rejects_duplicate_created_by_canonicalization() {
    assert_migration_rejects_canonicalized_duplicate(6);
}

#[test]
fn older_migration_rejects_duplicate_created_by_canonicalization() {
    assert_migration_rejects_canonicalized_duplicate(3);
}

#[test]
fn test_check_and_migrate_future_version_returns_error() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 999,
        "serial": 0,
        "lineage": "test-lineage",
        "carina_version": "0.1.0",
        "resources": []
    }"#;

    let result = check_and_migrate(json);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("999"),
        "error should mention the unsupported version"
    );
    assert!(
        err.contains("Please upgrade Carina"),
        "error should suggest upgrading"
    );
}

#[test]
fn test_check_and_migrate_older_version_migrates() {
    use super::check_and_migrate;

    // v3 state file — should be migrated to current version
    let json = r#"{
        "version": 3,
        "serial": 5,
        "lineage": "old-lineage",
        "carina_version": "0.0.1",
        "resources": []
    }"#;

    let result = check_and_migrate(json).unwrap().into_state();
    assert_eq!(
        result.version,
        StateFile::CURRENT_VERSION,
        "version should be bumped to current"
    );
    assert_eq!(result.serial, 5, "serial should be preserved");
    assert_eq!(result.lineage, "old-lineage", "lineage should be preserved");
}

#[test]
fn test_check_and_migrate_invalid_json_returns_error() {
    use super::check_and_migrate;

    let result = check_and_migrate("not valid json at all");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Failed to parse state version"),
        "error should mention version parsing failure"
    );
}

#[test]
fn test_check_and_migrate_bytes_works() {
    use super::check_and_migrate_bytes;

    let state = StateFile::new();
    let json = serde_json::to_string_pretty(&state).unwrap();
    let result = check_and_migrate_bytes(json.as_bytes())
        .unwrap()
        .into_state();
    assert_eq!(result.version, StateFile::CURRENT_VERSION);
}

#[test]
fn test_check_and_migrate_bytes_invalid_utf8() {
    use super::check_and_migrate_bytes;

    let bytes: &[u8] = &[0xff, 0xfe, 0xfd];
    let result = check_and_migrate_bytes(bytes);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("UTF-8"), "error should mention UTF-8 issue");
}

// carina#3283: check_and_migrate is library-level and must not write to
// stderr. The migration event is returned as a typed value so the caller
// (a backend impl) can decide *when* and *how often* to log it. Without
// this, every read_state call on the same physical state file emits a
// fresh "Migrating state file..." line — for `carina plan` that means
// the warning fires twice (T0 + T1 drift re-read) and even three times
// when the run also crosses the refresh phase (#3283 repro).
#[test]
fn test_check_and_migrate_returns_migration_info_for_older_version() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 6,
        "serial": 14,
        "lineage": "test-lineage",
        "carina_version": "0.0.1",
        "resources": []
    }"#;

    let outcome = check_and_migrate(json).unwrap();
    let migration = outcome
        .migration()
        .expect("migration info should be present for v6 → v7");
    assert_eq!(migration.from, 6);
    assert_eq!(migration.to, StateFile::CURRENT_VERSION);
    assert_eq!(outcome.state().version, StateFile::CURRENT_VERSION);
}

#[test]
fn test_check_and_migrate_no_migration_info_for_current_version() {
    use super::check_and_migrate;

    let state = StateFile::new();
    let json = serde_json::to_string_pretty(&state).unwrap();
    let outcome = check_and_migrate(&json).unwrap();
    assert!(
        outcome.migration().is_none(),
        "current-version reads must not report a migration"
    );
    assert_eq!(outcome.state().version, StateFile::CURRENT_VERSION);
}

// carina#3266: `state.resources` is managed-only by invariant since
// #3181, but pre-#3181 carina releases (and an older `carina state
// refresh` path) persisted `read aws.*` data-source rows here. They
// share one distinguishing shape — `identifier: null` — because a
// data source has no provider-side identity to record. They must be
// pruned at the single read seam so every downstream consumer (apply,
// destroy, state refresh, plan) sees an invariant-respecting state.
// In production, the bug surfaced as `state.exports` for a
// data-source-derived value never converging — the post-apply binding
// overlay was lifting the stale `arns` value out of the artifact row
// and writing it straight back. See the issue body for the full
// repro on `aws/management/identity-center/`.
#[test]
fn check_and_migrate_drops_artifact_rows_with_null_identifier_3266() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 7,
        "serial": 14,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {
                "resource_type": "iam.Roles",
                "name": "admin_access_roles",
                "provider": "aws",
                "identifier": null,
                "attributes": { "arns": ["arn:aws:iam::1:role/OLD"] },
                "binding": "admin_access_roles"
            },
            {
                "resource_type": "s3.Bucket",
                "name": "log",
                "provider": "awscc",
                "identifier": "log-bucket",
                "attributes": { "name": "log-bucket" }
            }
        ]
    }"#;

    let state = check_and_migrate(json).expect("read").into_state();
    let kept_identities: Vec<&str> = state
        .resources
        .iter()
        .map(|r| r.identity.as_str())
        .collect();
    assert_eq!(
        kept_identities,
        vec!["log"],
        "identifier=None artifact rows must be pruned at read time; \
         only managed resources with identifiers survive. Got: {kept_identities:?}",
    );
}

#[test]
fn test_merge_write_only_attributes() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    // Simulate a VPC resource with a write-only attribute (ipv4_netmask_length)
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    resource.set_attr(
        "ipv4_netmask_length".to_string(),
        Value::Concrete(ConcreteValue::Int(16)),
    );

    // Provider returns state without write-only attributes (API doesn't return them)
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let mut rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();

    // Merge write-only attributes
    let write_only_keys = vec!["ipv4_netmask_length".to_string()];
    rs.merge_write_only_attributes(&resource, &write_only_keys);

    // The write-only attribute should be persisted in state
    assert_eq!(
        rs.attributes.get("ipv4_netmask_length"),
        Some(&serde_json::json!(16))
    );
    assert_eq!(rs.write_only_attributes, vec!["ipv4_netmask_length"]);

    // The regular attribute should still be there
    assert_eq!(
        rs.attributes.get("cidr_block"),
        Some(&serde_json::json!("10.0.0.0/16"))
    );
}

#[test]
fn test_merge_write_only_attributes_not_in_desired() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    // Resource without write-only attribute specified
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let mut rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();

    // Try to merge a write-only attribute that the user didn't specify
    let write_only_keys = vec!["ipv4_netmask_length".to_string()];
    rs.merge_write_only_attributes(&resource, &write_only_keys);

    // Should NOT be in state since user didn't specify it
    assert!(!rs.attributes.contains_key("ipv4_netmask_length"));
    assert!(rs.write_only_attributes.is_empty());
}

#[test]
fn test_merge_write_only_skips_if_already_in_provider_state() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    // Resource with a write-only attribute
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "some_attr".to_string(),
        Value::Concrete(ConcreteValue::String("desired".to_string())),
    );

    // Provider happens to return this attribute (unusual for write-only but possible)
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "some_attr".to_string(),
            Value::Concrete(ConcreteValue::String("from-api".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let mut rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();

    let write_only_keys = vec!["some_attr".to_string()];
    rs.merge_write_only_attributes(&resource, &write_only_keys);

    // Should keep the API-returned value, not overwrite with desired
    assert_eq!(
        rs.attributes.get("some_attr"),
        Some(&serde_json::json!("from-api"))
    );
    // Should NOT be recorded as write-only since the API returned it
    assert!(rs.write_only_attributes.is_empty());
}

#[test]
fn test_write_only_attributes_serialization() {
    let mut rs = ResourceState::new("ec2.Vpc", "my-vpc", "awscc")
        .with_identifier("vpc-123")
        .with_attribute("cidr_block".to_string(), serde_json::json!("10.0.0.0/16"))
        .with_attribute("ipv4_netmask_length".to_string(), serde_json::json!(16));
    rs.write_only_attributes = vec!["ipv4_netmask_length".to_string()];

    let json = serde_json::to_string_pretty(&rs).unwrap();
    let deserialized: ResourceState = serde_json::from_str(&json).unwrap();

    assert_eq!(
        deserialized.write_only_attributes,
        vec!["ipv4_netmask_length"]
    );
    assert_eq!(
        deserialized.attributes.get("ipv4_netmask_length"),
        Some(&serde_json::json!(16))
    );
}

#[test]
fn test_write_only_attributes_omitted_when_empty() {
    let rs = ResourceState::new("s3.Bucket", "my-bucket", "awscc");
    let json = serde_json::to_string(&rs).unwrap();

    // write_only_attributes should not appear in JSON when empty
    assert!(
        !json.contains("write_only_attributes"),
        "write_only_attributes should be omitted when empty"
    );
}

#[test]
fn test_from_provider_state_secret_stored_as_hash() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Resource, State as ProviderState, Value,
    };
    use carina_core::value::SECRET_PREFIX;

    let mut resource = Resource::with_provider("awscc", "rds.db_instance", "my-db", None);
    resource.set_attr(
        "master_password".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        )))),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("my-db-id".to_string()),
        // Provider returns the actual password (since secret was unwrapped before sending)
        attributes: [(
            "master_password".to_string(),
            Value::Concrete(ConcreteValue::String("my-password".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();

    // State should store the hash, not the plain password
    let stored = rs
        .attributes
        .get("master_password")
        .unwrap()
        .as_str()
        .unwrap();
    assert!(
        stored.starts_with(SECRET_PREFIX),
        "Expected secret hash, got: {}",
        stored
    );
    assert!(
        !stored.contains("my-password"),
        "State should not contain the plain password"
    );
}

#[test]
fn from_provider_state_rehashes_previous_secret_hash_without_desired_secret() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};
    use carina_core::value::SECRET_PREFIX;

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None);
    let existing = ResourceState::new("db.Instance", "main", "awscc")
        .with_identifier("db-old")
        .with_attribute(
            "password",
            serde_json::json!(format!("{SECRET_PREFIX}previous")),
        );
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("db-old".to_string()),
        attributes: [(
            "password".to_string(),
            Value::Concrete(ConcreteValue::String("plain-secret".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        Some(&existing),
        None,
    )
    .expect("provider state should serialize");
    let stored = rs
        .attributes
        .get("password")
        .and_then(|value| value.as_str())
        .expect("password should remain a stored secret hash");

    assert!(stored.starts_with(SECRET_PREFIX), "got {stored}");
    assert!(!stored.contains("plain-secret"));
}

#[test]
fn lossy_state_json_rehashes_previous_secret_hash_without_desired_secret() {
    use carina_core::resource::{ConcreteValue, Resource, Value};
    use carina_core::value::SECRET_PREFIX;

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None);
    let attrs = HashMap::from([(
        "password".to_string(),
        Value::Concrete(ConcreteValue::String("plain-secret".to_string())),
    )]);
    let existing = HashMap::from([(
        "password".to_string(),
        serde_json::json!(format!("{SECRET_PREFIX}previous")),
    )]);

    let stored = ResourceState::attributes_to_state_json_lossy_for_resource_and_schema(
        &resource,
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );
    let password = stored
        .get("password")
        .and_then(|value| value.as_str())
        .expect("password should remain a stored secret hash");

    assert!(password.starts_with(SECRET_PREFIX), "got {password}");
    assert!(!password.contains("plain-secret"));
}

#[test]
fn lossy_state_json_preserves_hydrated_secret_hash_without_double_hashing() {
    use carina_core::resource::{ConcreteValue, Resource, Value};
    use carina_core::value::SECRET_PREFIX;

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None);
    let previous = serde_json::json!(format!("{SECRET_PREFIX}previous"));
    let attrs = HashMap::from([(
        "password".to_string(),
        Value::Concrete(ConcreteValue::String(
            previous.as_str().unwrap().to_string(),
        )),
    )]);
    let existing = HashMap::from([("password".to_string(), previous.clone())]);

    let stored = ResourceState::attributes_to_state_json_lossy_for_resource_and_schema(
        &resource,
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );

    assert_eq!(stored.get("password"), Some(&previous));
}

#[test]
fn from_provider_state_stores_plain_value_when_secret_is_demoted() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};
    use carina_core::value::SECRET_PREFIX;

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None).with_attribute(
        "password",
        Value::Concrete(ConcreteValue::String("plain-secret".to_string())),
    );
    let existing = ResourceState::new("db.Instance", "main", "awscc")
        .with_identifier("db-old")
        .with_attribute(
            "password",
            serde_json::json!(format!("{SECRET_PREFIX}previous")),
        );
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("db-old".to_string()),
        attributes: [(
            "password".to_string(),
            Value::Concrete(ConcreteValue::String("plain-secret".to_string())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        Some(&existing),
        None,
    )
    .expect("provider state should serialize");

    assert_eq!(
        rs.attributes.get("password"),
        Some(&serde_json::json!("plain-secret"))
    );
}

#[test]
fn write_only_secret_to_plain_demotion_merges_plain_desired_value() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};
    use carina_core::value::SECRET_PREFIX;

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None).with_attribute(
        "password",
        Value::Concrete(ConcreteValue::String("plain-secret".to_string())),
    );
    let existing = ResourceState::new("db.Instance", "main", "awscc")
        .with_identifier("db-old")
        .with_attribute(
            "password",
            serde_json::json!(format!("{SECRET_PREFIX}previous")),
        );
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("db-old".to_string()),
        attributes: HashMap::new(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let mut rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        Some(&existing),
        None,
    )
    .expect("provider state should serialize");
    rs.merge_write_only_attributes(&resource, &["password".to_string()]);

    assert_eq!(
        rs.attributes.get("password"),
        Some(&serde_json::json!("plain-secret"))
    );
    assert_eq!(rs.write_only_attributes, vec!["password"]);
}

#[test]
fn lossy_state_json_merges_previous_nested_secret_hash_per_leaf() {
    use carina_core::resource::{ConcreteValue, Resource, Value};
    use carina_core::value::SECRET_PREFIX;
    use indexmap::IndexMap;

    let resource = Resource::with_provider("awscc", "ec2.Vpc", "main", None);
    let previous_tags = serde_json::json!({
        "Name": "old-name",
        "SecretTag": format!("{SECRET_PREFIX}previous"),
    });
    let mut provider_tags = IndexMap::new();
    provider_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("new-name".to_string())),
    );
    provider_tags.insert(
        "SecretTag".to_string(),
        Value::Concrete(ConcreteValue::String("plain-secret".to_string())),
    );
    let attrs = HashMap::from([(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(provider_tags)),
    )]);
    let existing = HashMap::from([("tags".to_string(), previous_tags.clone())]);

    let stored = ResourceState::attributes_to_state_json_lossy_for_resource_and_schema(
        &resource,
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );
    let stored_tags = stored
        .get("tags")
        .and_then(|value| value.as_object())
        .expect("tags should remain a map");
    let secret = stored_tags
        .get("SecretTag")
        .and_then(|value| value.as_str())
        .expect("secret tag should remain a string");

    assert_eq!(
        stored_tags.get("Name"),
        Some(&serde_json::json!("new-name"))
    );
    assert!(secret.starts_with(SECRET_PREFIX), "got {secret}");
    assert!(!secret.contains("plain-secret"));

    let second_existing = HashMap::from([("tags".to_string(), stored["tags"].clone())]);
    let stored_again = ResourceState::attributes_to_state_json_lossy_for_resource_and_schema(
        &resource,
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&second_existing),
    );
    assert_eq!(stored_again.get("tags"), stored.get("tags"));
}

#[test]
fn lossy_state_json_drops_omitted_non_secret_sibling_but_keeps_missing_hash_leaf() {
    use carina_core::resource::{ConcreteValue, Resource, Value};
    use carina_core::value::SECRET_PREFIX;
    use indexmap::IndexMap;

    let resource = Resource::with_provider("awscc", "ec2.Vpc", "main", None);
    let previous_tags = serde_json::json!({
        "Name": "old-name",
        "SecretTag": format!("{SECRET_PREFIX}previous"),
    });
    let attrs = HashMap::from([(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::new())),
    )]);
    let existing = HashMap::from([("tags".to_string(), previous_tags)]);

    let stored = ResourceState::attributes_to_state_json_lossy_for_resource_and_schema(
        &resource,
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );
    let stored_tags = stored
        .get("tags")
        .and_then(|value| value.as_object())
        .expect("tags should remain a map");
    let secret = stored_tags
        .get("SecretTag")
        .and_then(|value| value.as_str())
        .expect("missing secret tag should keep the previous hash");

    assert!(!stored_tags.contains_key("Name"));
    assert!(secret.starts_with(SECRET_PREFIX), "got {secret}");
}

#[test]
fn lossy_state_json_hashes_whole_array_when_secret_hash_alignment_is_untrusted() {
    use carina_core::resource::{ConcreteValue, Resource, Value};
    use carina_core::value::SECRET_PREFIX;
    use indexmap::IndexMap;

    let resource = Resource::with_provider("awscc", "service.Widget", "main", None);
    let previous_items = serde_json::json!([
        {
            "Name": "one",
            "Token": format!("{SECRET_PREFIX}previous"),
        },
        {
            "Name": "two",
            "Public": "kept",
        }
    ]);
    let mut first_provider_item = IndexMap::new();
    first_provider_item.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("two".to_string())),
    );
    first_provider_item.insert(
        "Public".to_string(),
        Value::Concrete(ConcreteValue::String("kept".to_string())),
    );
    let mut second_provider_item = IndexMap::new();
    second_provider_item.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("one".to_string())),
    );
    second_provider_item.insert(
        "Token".to_string(),
        Value::Concrete(ConcreteValue::String("plain-secret".to_string())),
    );
    let attrs = HashMap::from([(
        "items".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(first_provider_item)),
            Value::Concrete(ConcreteValue::Map(second_provider_item)),
        ])),
    )]);
    let existing = HashMap::from([("items".to_string(), previous_items)]);

    let stored = ResourceState::attributes_to_state_json_lossy_for_resource_and_schema(
        &resource,
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );
    let stored_items = stored
        .get("items")
        .and_then(|value| value.as_str())
        .expect("untrusted array alignment should hash the whole array");

    assert!(
        stored_items.starts_with(SECRET_PREFIX),
        "got {stored_items}"
    );
    assert!(!stored_items.contains("plain-secret"));
}

#[test]
fn test_from_provider_state_secret_in_map_stored_as_hash() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Resource, State as ProviderState, Value,
    };
    use carina_core::value::SECRET_PREFIX;

    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    let mut tags_map = IndexMap::new();
    tags_map.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("test".to_string())),
    );
    tags_map.insert(
        "SecretTag".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("super-secret-value".to_string()),
        )))),
    );
    resource.set_attr(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(tags_map)),
    );

    let mut state_tags = IndexMap::new();
    state_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("test".to_string())),
    );
    state_tags.insert(
        "SecretTag".to_string(),
        Value::Concrete(ConcreteValue::String("super-secret-value".to_string())),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(state_tags)),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();

    // The tags map in state should have the hash for SecretTag
    let tags_json = rs.attributes.get("tags").unwrap();
    let tags_obj = tags_json.as_object().unwrap();

    // Name should be plain
    assert_eq!(tags_obj.get("Name").unwrap().as_str().unwrap(), "test");

    // SecretTag should be stored as a hash, not the plain value
    let secret_stored = tags_obj.get("SecretTag").unwrap().as_str().unwrap();
    assert!(
        secret_stored.starts_with(SECRET_PREFIX),
        "Expected secret hash in map value, got: {}",
        secret_stored
    );
    assert!(
        !secret_stored.contains("super-secret-value"),
        "State should not contain the plain secret value in map"
    );
}

#[test]
fn test_from_provider_state_secret_in_map_preserves_provider_extra_keys() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Resource, State as ProviderState, Value,
    };
    use carina_core::value::SECRET_PREFIX;

    // User specifies only SecretTag in tags
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    let mut tags_map = IndexMap::new();
    tags_map.insert(
        "SecretTag".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("super-secret-value".to_string()),
        )))),
    );
    resource.set_attr(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(tags_map)),
    );

    // Provider returns extra keys (e.g., CloudControl adds Name automatically)
    let mut state_tags = IndexMap::new();
    state_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("test".to_string())),
    );
    state_tags.insert(
        "ExtraTag".to_string(),
        Value::Concrete(ConcreteValue::String("extra-value".to_string())),
    );
    state_tags.insert(
        "SecretTag".to_string(),
        Value::Concrete(ConcreteValue::String("super-secret-value".to_string())),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(state_tags)),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();

    let tags_json = rs.attributes.get("tags").unwrap();
    let tags_obj = tags_json.as_object().unwrap();

    // Provider-only keys should be preserved from the provider state
    assert_eq!(tags_obj.get("Name").unwrap().as_str().unwrap(), "test");
    assert_eq!(
        tags_obj.get("ExtraTag").unwrap().as_str().unwrap(),
        "extra-value"
    );

    // SecretTag should be stored as a hash, not the plain value
    let secret_stored = tags_obj.get("SecretTag").unwrap().as_str().unwrap();
    assert!(
        secret_stored.starts_with(SECRET_PREFIX),
        "Expected secret hash in map value, got: {}",
        secret_stored
    );
    assert!(
        !secret_stored.contains("super-secret-value"),
        "State should not contain the plain secret value in map"
    );
}

#[test]
fn test_from_provider_state_secret_in_list_stored_as_hash() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Resource, State as ProviderState, Value,
    };
    use carina_core::value::SECRET_PREFIX;

    let mut resource = Resource::with_provider("awscc", "test.resource", "my-res", None);
    resource.set_attr(
        "values".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("public".to_string())),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("secret-item".to_string()),
            )))),
        ])),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("res-123".to_string()),
        attributes: [(
            "values".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("public".to_string())),
                Value::Concrete(ConcreteValue::String("secret-item".to_string())),
            ])),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let rs = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap();

    let values_json = rs.attributes.get("values").unwrap();
    let values_arr = values_json.as_array().unwrap();

    // First item should be plain
    assert_eq!(values_arr[0].as_str().unwrap(), "public");

    // Second item should be stored as a hash
    let secret_stored = values_arr[1].as_str().unwrap();
    assert!(
        secret_stored.starts_with(SECRET_PREFIX),
        "Expected secret hash in list value, got: {}",
        secret_stored
    );
}

#[test]
fn build_remote_bindings_returns_exports() {
    let mut state = StateFile::new();
    state.exports.insert(
        "account_id".to_string(),
        serde_json::Value::String("123456789012".to_string()),
    );
    let bindings = state.build_remote_bindings();
    assert_eq!(
        bindings.get("account_id"),
        Some(&Value::Concrete(ConcreteValue::String(
            "123456789012".to_string()
        )))
    );
}

#[test]
fn build_remote_bindings_empty_when_no_exports() {
    let state = StateFile::new();
    let bindings = state.build_remote_bindings();
    assert!(bindings.is_empty());
}

#[test]
fn build_remote_bindings_ignores_resource_bindings() {
    let mut state = StateFile::new();
    // Add a resource with a binding — should NOT appear in remote bindings
    state
        .upsert_resource(ResourceState {
            resource_type: "ec2.Vpc".to_string(),
            identity: "vpc_123".to_string(),
            provider: "awscc".to_string(),
            identifier: Some("vpc-123".to_string()),
            attributes: HashMap::from([(
                "vpc_id".to_string(),
                serde_json::Value::String("vpc-123".to_string()),
            )]),
            deposed: Vec::new(),
            protected: false,
            directives: carina_core::resource::Directives::default(),
            prefixes: HashMap::new(),
            name_overrides: HashMap::new(),
            explicit: ExplicitFields::default(),
            binding: Some("vpc".to_string()),
            dependency_bindings: BTreeSet::new(),
            write_only_attributes: vec![],
            partial_read: None,
        })
        .expect("test state setup must be valid");
    let bindings = state.build_remote_bindings();
    assert!(
        bindings.is_empty(),
        "resource bindings should not be exposed"
    );
}

#[test]
fn check_and_migrate_canonicalizes_legacy_map_key_addresses() {
    // State files written by older Carina builds embed the map key in
    // `binding["key"]` form. After #1903 the canonical address is the
    // dot form for identifier-safe keys; non-identifier-safe keys move
    // from double quotes to single. The `check_and_migrate` load path
    // rewrites these so existing state resolves against new emissions
    // without a `moved` block.
    let json = format!(
        r#"{{
            "version": {ver},
            "serial": 1,
            "lineage": "abc",
            "carina_version": "test",
            "resources": [
                {{
                    "resource_type": "sso.Assignment",
                    "name": "_accounts[\"registry_prod\"]",
                    "provider": "awscc",
                    "identifier": "x",
                    "attributes": {{}},
                    "binding": "_accounts[\"registry_prod\"]",
                    "dependency_bindings": ["other[\"a\"]", "_envs[\"prod-east\"]"]
                }}
            ]
        }}"#,
        ver = StateFile::CURRENT_VERSION,
    );
    let state = check_and_migrate(&json).expect("load state").into_state();
    let r = &state.resources[0];
    assert_eq!(r.identity, "_accounts.registry_prod");
    assert_eq!(r.binding.as_deref(), Some("_accounts.registry_prod"));
    let deps: Vec<&str> = r.dependency_bindings.iter().map(String::as_str).collect();
    assert!(deps.contains(&"other.a"));
    assert!(deps.contains(&"_envs['prod-east']"));
}

/// RFC #2371 #2385: state writeback rejects unresolved `Value` variants
/// surfaced from a buggy provider that returns a `Value::Deferred(DeferredValue::ResourceRef)`
/// in `state.attributes`. Provider-returned states must be concrete; a
/// resolver / provider bug produces a typed `UnresolvedResourceRef`
/// error rather than a debug-formatted string in state JSON.
#[test]
fn from_provider_state_rejects_resource_ref_in_provider_attributes() {
    use carina_core::resource::{
        AccessPath, DeferredValue, Resource, State as ProviderState, Value,
    };

    let resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("my-bucket".to_string()),
        attributes: [(
            "owner".to_string(),
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::with_fields("net", "vpc", vec!["vpc_id".into()]),
            }),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: BTreeSet::new(),

        partial_read: None,
    };

    let err = ResourceState::from_provider_state_for_resource_and_schema(
        &resource,
        &provider_state,
        None,
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("unresolved reference") && err.contains("net.vpc.vpc_id"),
        "expected UnresolvedResourceRef diagnostic in error, got: {err}"
    );
}

#[test]
fn v5_state_read_converts_desired_keys_to_explicit_leaves() {
    // A v5 state file carries a `"desired_keys": ["..."]` array.
    // Reading it under v6 must (a) bump the version to 6 and
    // (b) lift each top-level key to a `Leaf` child of the root
    // `ExplicitFields::Struct`.
    // carina#3266: production v5 rows carry an identifier (the
    // provider returns one on every successful apply). The read
    // path now prunes identifier=None rows as historical artifacts,
    // so this fixture has to mirror real shape.
    let v5 = r#"{
        "version": 5,
        "serial": 0,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "s3.Bucket",
            "name": "my-bucket",
            "provider": "aws",
            "identifier": "my-bucket",
            "attributes": {"region": "ap-northeast-1"},
            "protected": false,
            "directives": {},
            "prefixes": {},
            "name_overrides": {},
            "desired_keys": ["region", "tags"],
            "binding": null,
            "dependency_bindings": []
        }]
    }"#;

    let state = check_and_migrate(v5)
        .expect("migration must succeed")
        .into_state();
    assert_eq!(state.version, StateFile::CURRENT_VERSION);
    assert_eq!(state.resources.len(), 1);

    let ExplicitFields::Struct { children } = &state.resources[0].explicit else {
        panic!(
            "v5 desired_keys should lift to ExplicitFields::Struct, got: {:?}",
            state.resources[0].explicit
        );
    };
    assert_eq!(children.len(), 2);
    assert!(matches!(children["region"], ExplicitFields::Leaf));
    assert!(matches!(children["tags"], ExplicitFields::Leaf));
}

#[test]
fn current_state_writes_and_reads_full_explicit_tree() {
    // A current-version state file with a nested `explicit` tree
    // round-trips through serde without loss.
    let mut state = StateFile::new();
    // carina#3266: `check_and_migrate` prunes identifier=None rows
    // (historical-artifact shape from pre-#3181 data-source writeback).
    // Production rows always carry an identifier from `from_provider_state`.
    let mut rs = ResourceState::new("s3.Bucket", "my-bucket", "aws").with_identifier("my-bucket");
    rs.explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "lifecycle_configuration".into(),
            ExplicitFields::Struct {
                children: HashMap::from([(
                    "rules".into(),
                    ExplicitFields::List {
                        element: Box::new(ExplicitFields::Struct {
                            children: HashMap::from([("id".into(), ExplicitFields::Leaf)]),
                        }),
                    },
                )]),
            },
        )]),
    };
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let json = serde_json::to_string(&state).expect("serialize");
    let back = check_and_migrate(&json).expect("read").into_state();
    assert_eq!(back.version, StateFile::CURRENT_VERSION);
    assert_eq!(back.resources[0].explicit, state.resources[0].explicit);
}

#[test]
fn v10_list_elements_state_round_trip_preserves_empty_and_populated_vectors() {
    let mut state = StateFile::new();
    let mut populated =
        ResourceState::new("listener.Listener", "populated", "mock").with_identifier("one");
    populated.explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::ListElements {
                elements: vec![ExplicitFields::Leaf, ExplicitFields::Unrecorded],
            },
        )]),
    };
    let mut empty = ResourceState::new("listener.Listener", "empty", "mock").with_identifier("two");
    empty.explicit = ExplicitFields::ListElements {
        elements: Vec::new(),
    };
    state.upsert_resource(populated).unwrap();
    state.upsert_resource(empty).unwrap();

    let json = serde_json::to_string(&state).unwrap();
    assert!(json.contains(r#""kind":"list-elements""#));
    assert!(json.contains(r#""elements":[]"#));
    let back = check_and_migrate(&json).unwrap().into_state();

    assert_eq!(back.version, 10);
    assert_eq!(back.resources[0].explicit, state.resources[0].explicit);
    assert_eq!(back.resources[1].explicit, state.resources[1].explicit);
}

#[test]
fn v9_legacy_list_survives_v10_lift_stays_conservative_and_self_heals_on_writeback() {
    use carina_core::differ::{Diff, diff};

    let json = r#"{
        "version": 9,
        "serial": 1,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "listener.Listener",
            "identity": "listener",
            "provider": "mock",
            "identifier": "listener-id",
            "attributes": {
                "rules": [
                    {"port": 80, "description": "web"},
                    {"port": 443, "description": "provider-default"}
                ]
            },
            "explicit": {
                "kind": "struct",
                "children": {
                    "rules": {
                        "kind": "list",
                        "element": {
                            "kind": "struct",
                            "children": {
                                "port": {"kind": "leaf"},
                                "description": {"kind": "leaf"}
                            }
                        }
                    }
                }
            }
        }]
    }"#;
    let outcome = check_and_migrate(json).expect("v9 state must lift without a custom migration");
    assert_eq!(outcome.migration.unwrap().from, 9);
    assert_eq!(outcome.state.version, 10);
    let state = outcome.state;
    let row = state
        .find_resource("mock", "listener.Listener", "listener")
        .expect("legacy row");
    let legacy_explicit = row.explicit.clone();
    let ExplicitFields::Struct { children } = &legacy_explicit else {
        panic!("expected legacy root Struct");
    };
    assert!(matches!(children["rules"], ExplicitFields::List { .. }));

    let serialized = serde_json::to_string(&state).unwrap();
    let round_tripped = check_and_migrate(&serialized).unwrap().into_state();
    assert_eq!(round_tripped.resources[0].explicit, legacy_explicit);

    let desired = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, None),
                list_rule(443, None),
            ])),
        );
    let current_attributes = HashMap::from([(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            list_rule(80, Some("web")),
            list_rule(443, Some("provider-default")),
        ])),
    )]);
    let current = State::existing(desired.id.clone(), current_attributes.clone())
        .with_identifier("listener-id");
    assert!(matches!(
        diff(
            &desired,
            &current,
            Some(&current_attributes),
            Some(&legacy_explicit),
            None,
        ),
        Diff::NoChange(_)
    ));

    let rewritten = ResourceState::from_provider_state_for_resource_and_schema(
        &desired,
        &current,
        Some(row),
        None,
    )
    .unwrap();
    let ExplicitFields::Struct { children } = rewritten.explicit else {
        panic!("expected rewritten root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("v10 writeback must replace legacy List with ListElements");
    };
    assert_eq!(elements.len(), 2);
    assert_explicit_struct_keys(&elements[0], &["port"]);
    assert_explicit_struct_keys(&elements[1], &["port"]);
}

#[test]
fn repeated_writeback_realigns_reordered_elements_and_plan_uses_first_row_alignment() {
    use carina_core::differ::{Diff, diff};

    let authored = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, Some("web")),
                list_rule(443, None),
            ])),
        );
    let first_provider = State::existing(
        authored.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, Some("web")),
                list_rule(443, Some("provider-default")),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let first_row = ResourceState::from_provider_state_for_resource_and_schema(
        &authored,
        &first_provider,
        None,
        None,
    )
    .unwrap();
    let ExplicitFields::Struct {
        children: first_children,
    } = &first_row.explicit
    else {
        panic!("expected first root Struct");
    };
    let ExplicitFields::ListElements {
        elements: first_elements,
    } = &first_children["rules"]
    else {
        panic!("expected first aligned ListElements");
    };
    assert_explicit_struct_keys(&first_elements[0], &["port", "description"]);
    assert_explicit_struct_keys(&first_elements[1], &["port"]);

    let mut first_state = StateFile::new();
    first_state.upsert_resource(first_row.clone()).unwrap();
    let saved = first_state.build_saved_attrs();
    let removed = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, None),
                list_rule(443, None),
            ])),
        );
    let removal = diff(
        &removed,
        &first_provider,
        saved.get(&authored.id),
        Some(&first_row.explicit),
        None,
    );
    let Diff::Update {
        changed_attributes, ..
    } = removal
    else {
        panic!("removing the authored port 80 description must update rules");
    };
    assert_eq!(changed_attributes, vec!["rules".to_string()]);

    // This current view is exactly the effective merged list the plan must compare against: the
    // authored field is gone from port 80, while port 443 keeps its provider default. `NoChange`
    // here discriminates the saved index used for both per-element decisions.
    let expected_effective = State::existing(
        authored.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, None),
                list_rule(443, Some("provider-default")),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let Value::Concrete(ConcreteValue::List(expected_rules)) =
        &expected_effective.attributes["rules"]
    else {
        panic!("expected effective rules list");
    };
    let Value::Concrete(ConcreteValue::Map(port_80)) = &expected_rules[0] else {
        panic!("expected port 80 rule");
    };
    let Value::Concrete(ConcreteValue::Map(port_443)) = &expected_rules[1] else {
        panic!("expected port 443 rule");
    };
    assert!(!port_80.contains_key("description"));
    assert_eq!(
        port_443.get("description"),
        Some(&Value::Concrete(ConcreteValue::String(
            "provider-default".to_string()
        )))
    );
    assert!(matches!(
        diff(
            &removed,
            &expected_effective,
            saved.get(&authored.id),
            Some(&first_row.explicit),
            None,
        ),
        Diff::NoChange(_)
    ));

    let reordered_provider = State::existing(
        authored.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(443, Some("provider-default")),
                list_rule(80, Some("web")),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let second_row = ResourceState::from_provider_state_for_resource_and_schema(
        &authored,
        &reordered_provider,
        Some(&first_row),
        None,
    )
    .unwrap();
    let stored = second_row.attributes["rules"]
        .as_array()
        .expect("stored reordered rules");
    assert_eq!(stored[0]["port"], 443);
    assert_eq!(stored[1]["port"], 80);
    let ExplicitFields::Struct { children } = &second_row.explicit else {
        panic!("expected root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("expected realigned ListElements");
    };
    assert_explicit_struct_keys(&elements[0], &["port"]);
    assert_explicit_struct_keys(&elements[1], &["port", "description"]);
}

#[test]
fn build_explicit_yields_per_resource_trees() {
    let mut state = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "my-bucket", "aws");
    rs.explicit = ExplicitFields::Struct {
        children: HashMap::from([("region".into(), ExplicitFields::Leaf)]),
    };
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let map = state.build_explicit();
    let id = ResourceId::with_provider_identity("aws", "s3.Bucket", "my-bucket", None);
    assert!(map.contains_key(&id));
    assert!(matches!(map[&id], ExplicitFields::Struct { .. }));
}

#[test]
fn build_directives_keys_include_provider_instance() {
    // ResourceState carries `directives.provider_instance` through serde
    // (Phase 3a). The state→ResourceId reconstruction in build_directives
    // must propagate that binding name into the ResourceId so map lookups
    // by `ResourceId` match the freshly-parsed key. Without this, two
    // resources with the same kind/type/name but different instances
    // collide in `HashMap<ResourceId, _>` (Phase 3b-2a).
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "shared-name", "aws");
    rs.directives.provider_instance = Some("us".to_string());
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let map = state.build_directives();
    let expected = ResourceId::with_provider_identity(
        "aws",
        "s3.Bucket",
        "shared-name",
        Some("us".to_string()),
    );
    assert!(
        map.contains_key(&expected),
        "build_directives must construct ResourceId with provider_instance, got keys: {:?}",
        map.keys().collect::<Vec<_>>()
    );
    // Without the instance, the legacy ResourceId must NOT match.
    let legacy = ResourceId::with_provider_identity("aws", "s3.Bucket", "shared-name", None);
    assert!(
        !map.contains_key(&legacy),
        "ResourceId without provider_instance must not match a Some(_) entry"
    );
}

#[test]
fn partial_read_marker_round_trips_through_state_json() {
    let mut state = StateFile::new();
    let mut rs = ResourceState::new("test.resource", "r1", "mock").with_identifier("mock-id");
    rs.partial_read = Some(PartialReadMarker {
        detail: "mock partial create".to_string(),
        missing_attributes: ["a".to_string(), "b".to_string()].into_iter().collect(),
    });
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let json = serde_json::to_string(&state).expect("serialize");
    let back = check_and_migrate(&json).expect("read").into_state();

    assert_eq!(
        back.resources[0].partial_read,
        state.resources[0].partial_read
    );
}

#[test]
fn missing_partial_read_field_defaults_to_none() {
    let json = r#"{
        "version": 7,
        "serial": 0,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "test.resource",
            "name": "r1",
            "provider": "mock",
            "identifier": "mock-id",
            "attributes": {"name": "r1"},
            "protected": false,
            "directives": {},
            "prefixes": {},
            "name_overrides": {},
            "desired_keys": [],
            "explicit": {"kind": "struct", "children": {}},
            "binding": null,
            "dependency_bindings": [],
            "write_only_attributes": []
        }]
    }"#;

    let state = check_and_migrate(json).expect("read").into_state();

    assert_eq!(state.resources.len(), 1);
    assert_eq!(state.resources[0].partial_read, None);
}
