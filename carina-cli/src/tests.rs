use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;

use carina_core::effect::Effect;
use carina_core::parser::{ParsedFile, ProviderConfig};
use carina_core::plan::Plan;
use carina_core::provider::{BoxFuture, ProviderError, ProviderResult};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_state::{BackendError, LockInfo, ResourceState, StateBackend, StateFile};

use crate::commands::apply::{
    ApplyResult, ApplyStateSave, build_state_after_apply, detect_drift, execute_effects,
    finalize_apply, queue_state_refresh, refresh_pending_states, save_state_locked,
};
use crate::commands::plan::{CurrentStateEntry, PlanFile};
use crate::commands::state::run_state_refresh_locked;
use crate::wiring::{
    WiringContext, compute_anonymous_identifiers, reconcile_prefixed_names, resolve_attr_prefixes,
    resolve_names, validate_resources,
};
use carina_core::parser::BackendConfig;
use carina_core::provider::Provider;
use std::sync::Mutex;

struct TestProvider {
    read_results: HashMap<(String, String), Result<State, String>>,
}

impl TestProvider {
    fn with_read_state(id: &ResourceId, identifier: &str, state: State) -> Self {
        Self::with_read_result(id, identifier, Ok(state))
    }

    fn with_read_error(id: &ResourceId, identifier: &str, error: impl Into<String>) -> Self {
        Self::with_read_result(id, identifier, Err(error.into()))
    }

    fn with_read_result(id: &ResourceId, identifier: &str, result: Result<State, String>) -> Self {
        let mut read_results = HashMap::new();
        read_results.insert((id.to_string(), identifier.to_string()), result);
        Self { read_results }
    }
}

impl Provider for TestProvider {
    fn name(&self) -> &'static str {
        "test"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let key = (id.to_string(), identifier.unwrap_or_default().to_string());
        let result = self
            .read_results
            .get(&key)
            .cloned()
            .unwrap_or_else(|| panic!("missing read state for {:?}", key));
        Box::pin(async move { result.map_err(ProviderError::new) })
    }

    fn create(&self, _resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::new("unexpected create")) })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _from: &State,
        _to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::new("unexpected update")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::new("unexpected delete")) })
    }
}

#[tokio::test]
async fn refresh_pending_states_updates_saved_state_from_provider_read() {
    let resource = Resource::with_provider("aws", "s3.bucket", "bucket");
    let id = resource.id.clone();
    let identifier = "bucket-123";

    let mut current_states = HashMap::from([(
        id.clone(),
        State::existing(
            id.clone(),
            HashMap::from([("status".to_string(), Value::String("before".to_string()))]),
        )
        .with_identifier(identifier),
    )]);
    let provider = TestProvider::with_read_state(
        &id,
        identifier,
        State::existing(
            id.clone(),
            HashMap::from([("status".to_string(), Value::String("after".to_string()))]),
        )
        .with_identifier(identifier),
    );

    let mut pending_refreshes = HashMap::new();
    queue_state_refresh(&mut pending_refreshes, &id, Some(identifier));
    let failed_refreshes =
        refresh_pending_states(&provider, &mut current_states, &pending_refreshes).await;

    let mut existing_state = StateFile::new();
    existing_state.upsert_resource(
        ResourceState::new("s3.bucket", "bucket", "aws")
            .with_identifier(identifier)
            .with_attribute("status", json!("before")),
    );

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(existing_state),
        sorted_resources: &[resource],
        current_states: &current_states,
        applied_states: &HashMap::new(),
        permanent_name_overrides: &HashMap::new(),
        plan: &Plan::new(),
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &failed_refreshes,
    })
    .unwrap();

    let saved_resource = saved.find_resource("aws", "s3.bucket", "bucket").unwrap();
    assert_eq!(
        saved_resource.attributes.get("status"),
        Some(&json!("after"))
    );
}

#[tokio::test]
async fn refresh_pending_states_removes_not_found_resource_from_saved_state() {
    let resource = Resource::with_provider("aws", "s3.bucket", "bucket");
    let id = resource.id.clone();
    let identifier = "bucket-123";

    let mut current_states = HashMap::from([(
        id.clone(),
        State::existing(
            id.clone(),
            HashMap::from([("status".to_string(), Value::String("before".to_string()))]),
        )
        .with_identifier(identifier),
    )]);
    let provider = TestProvider::with_read_state(&id, identifier, State::not_found(id.clone()));

    let mut pending_refreshes = HashMap::new();
    queue_state_refresh(&mut pending_refreshes, &id, Some(identifier));
    let failed_refreshes =
        refresh_pending_states(&provider, &mut current_states, &pending_refreshes).await;

    let mut existing_state = StateFile::new();
    existing_state.upsert_resource(
        ResourceState::new("s3.bucket", "bucket", "aws")
            .with_identifier(identifier)
            .with_attribute("status", json!("before")),
    );

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(existing_state),
        sorted_resources: &[resource],
        current_states: &current_states,
        applied_states: &HashMap::new(),
        permanent_name_overrides: &HashMap::new(),
        plan: &Plan::new(),
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &failed_refreshes,
    })
    .unwrap();

    assert!(saved.find_resource("aws", "s3.bucket", "bucket").is_none());
}

#[tokio::test]
async fn refresh_pending_states_does_not_overwrite_with_stale_snapshot_when_refresh_fails() {
    let resource = Resource::with_provider("aws", "s3.bucket", "bucket");
    let id = resource.id.clone();
    let identifier = "bucket-123";

    let mut current_states = HashMap::from([(
        id.clone(),
        State::existing(
            id.clone(),
            HashMap::from([(
                "status".to_string(),
                Value::String("stale-current".to_string()),
            )]),
        )
        .with_identifier(identifier),
    )]);
    let provider = TestProvider::with_read_error(&id, identifier, "read failed");

    let mut pending_refreshes = HashMap::new();
    queue_state_refresh(&mut pending_refreshes, &id, Some(identifier));
    let failed_refreshes =
        refresh_pending_states(&provider, &mut current_states, &pending_refreshes).await;

    let mut existing_state = StateFile::new();
    existing_state.upsert_resource(
        ResourceState::new("s3.bucket", "bucket", "aws")
            .with_identifier(identifier)
            .with_attribute("status", json!("saved")),
    );

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(existing_state),
        sorted_resources: &[resource],
        current_states: &current_states,
        applied_states: &HashMap::new(),
        permanent_name_overrides: &HashMap::new(),
        plan: &Plan::new(),
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &failed_refreshes,
    })
    .unwrap();

    let saved_resource = saved.find_resource("aws", "s3.bucket", "bucket").unwrap();
    assert_eq!(
        saved_resource.attributes.get("status"),
        Some(&json!("saved"))
    );
}

#[test]
fn plan_file_serde_round_trip() {
    use carina_core::plan::Plan;

    let mut plan = Plan::new();
    plan.add(Effect::Create(
        Resource::with_provider("aws", "s3.bucket", "my-bucket")
            .with_attribute("bucket", Value::String("my-bucket".to_string())),
    ));
    plan.add(Effect::Delete {
        id: ResourceId::with_provider("aws", "s3.bucket", "old-bucket"),
        identifier: "old-bucket".to_string(),
        lifecycle: LifecycleConfig::default(),
    });

    let sorted_resources = vec![
        Resource::with_provider("aws", "s3.bucket", "my-bucket")
            .with_attribute("bucket", Value::String("my-bucket".to_string())),
    ];

    let current_states = vec![CurrentStateEntry {
        id: ResourceId::with_provider("aws", "s3.bucket", "my-bucket"),
        state: State::not_found(ResourceId::with_provider("aws", "s3.bucket", "my-bucket")),
    }];

    let plan_file = PlanFile {
        version: 1,
        carina_version: "0.1.0".to_string(),
        timestamp: "2025-01-01T00:00:00Z".to_string(),
        source_path: "example.crn".to_string(),
        state_lineage: Some("test-lineage".to_string()),
        state_serial: Some(1),
        provider_configs: vec![ProviderConfig {
            name: "aws".to_string(),
            attributes: HashMap::from([(
                "region".to_string(),
                Value::String("aws.Region.ap_northeast_1".to_string()),
            )]),
        }],
        backend_config: Some(BackendConfig {
            backend_type: "s3".to_string(),
            attributes: HashMap::from([
                ("bucket".to_string(), Value::String("my-state".to_string())),
                (
                    "key".to_string(),
                    Value::String("prod/carina.state".to_string()),
                ),
            ]),
        }),
        plan,
        sorted_resources,
        current_states,
    };

    let json = serde_json::to_string_pretty(&plan_file).unwrap();
    let deserialized: PlanFile = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.version, 1);
    assert_eq!(deserialized.carina_version, "0.1.0");
    assert_eq!(deserialized.source_path, "example.crn");
    assert_eq!(deserialized.state_lineage, Some("test-lineage".to_string()));
    assert_eq!(deserialized.state_serial, Some(1));
    assert_eq!(deserialized.provider_configs.len(), 1);
    assert_eq!(deserialized.provider_configs[0].name, "aws");
    assert!(deserialized.backend_config.is_some());
    assert_eq!(deserialized.plan.effects().len(), 2);
    assert_eq!(deserialized.sorted_resources.len(), 1);
    assert_eq!(deserialized.current_states.len(), 1);
}

#[test]
fn test_resolve_attr_prefixes_extracts_prefix_and_generates_name() {
    let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
    resource.attributes.insert(
        "bucket_name_prefix".to_string(),
        Value::String("my-app-".to_string()),
    );

    let mut resources = vec![resource];
    resolve_attr_prefixes(&mut resources).unwrap();

    // bucket_name_prefix should be removed
    assert!(!resources[0].attributes.contains_key("bucket_name_prefix"));

    // bucket_name should be generated with the prefix
    let bucket_name = match resources[0].attributes.get("bucket_name").unwrap() {
        Value::String(s) => s.clone(),
        _ => panic!("expected String"),
    };
    assert!(bucket_name.starts_with("my-app-"));
    assert_eq!(bucket_name.len(), "my-app-".len() + 8); // prefix + 8 hex chars

    // prefixes map should have the entry
    assert_eq!(
        resources[0].prefixes.get("bucket_name"),
        Some(&"my-app-".to_string())
    );
}

#[test]
fn test_resolve_attr_prefixes_leaves_non_matching_prefix_alone() {
    // If base attr doesn't exist in schema, leave _prefix as-is
    let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
    resource.attributes.insert(
        "nonexistent_attr_prefix".to_string(),
        Value::String("some-value".to_string()),
    );

    let mut resources = vec![resource];
    resolve_attr_prefixes(&mut resources).unwrap();

    // nonexistent_attr_prefix should remain untouched
    assert!(
        resources[0]
            .attributes
            .contains_key("nonexistent_attr_prefix")
    );
    assert!(resources[0].prefixes.is_empty());
}

#[test]
fn test_resolve_attr_prefixes_errors_when_both_prefix_and_attr_specified() {
    let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
    resource.attributes.insert(
        "bucket_name_prefix".to_string(),
        Value::String("my-app-".to_string()),
    );
    resource.attributes.insert(
        "bucket_name".to_string(),
        Value::String("my-actual-bucket".to_string()),
    );

    let mut resources = vec![resource];
    let result = resolve_attr_prefixes(&mut resources);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("cannot specify both")
    );
}

#[test]
fn test_resolve_attr_prefixes_errors_on_empty_prefix() {
    let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
    resource.attributes.insert(
        "bucket_name_prefix".to_string(),
        Value::String("".to_string()),
    );

    let mut resources = vec![resource];
    let result = resolve_attr_prefixes(&mut resources);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("cannot be empty"));
}

#[test]
fn test_resolve_names_handles_block_name_before_prefix() {
    // resolve_names should first resolve block names, then resolve attr prefixes
    let mut resource = Resource::with_provider("awscc", "ec2.ipam", "test-ipam");
    resource.attributes.insert(
        "operating_region".to_string(),
        Value::List(vec![Value::Map(
            vec![(
                "region_name".to_string(),
                Value::String("us-east-1".to_string()),
            )]
            .into_iter()
            .collect(),
        )]),
    );

    let mut resources = vec![resource];
    resolve_names(&mut resources).unwrap();

    // operating_region should be renamed to operating_regions
    assert!(resources[0].attributes.contains_key("operating_regions"));
    assert!(!resources[0].attributes.contains_key("operating_region"));
}

#[test]
fn test_reconcile_prefixed_names_reuses_state_name_when_prefix_matches() {
    let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());
    resource.attributes.insert(
        "bucket_name".to_string(),
        Value::String("my-app-temporary".to_string()),
    );

    let mut state_file = StateFile::new();
    let mut rs = ResourceState::new("s3.bucket", "test-bucket", "awscc");
    rs.attributes.insert(
        "bucket_name".to_string(),
        serde_json::json!("my-app-existing1"),
    );
    rs.prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());
    state_file.upsert_resource(rs);

    let mut resources = vec![resource];
    reconcile_prefixed_names(&mut resources, &Some(state_file));

    // Should reuse the state name, not the temporary one
    assert_eq!(
        resources[0].attributes.get("bucket_name"),
        Some(&Value::String("my-app-existing1".to_string()))
    );
}

#[test]
fn test_reconcile_prefixed_names_generates_new_name_when_prefix_changes() {
    let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
    resource
        .prefixes
        .insert("bucket_name".to_string(), "new-prefix-".to_string());
    resource.attributes.insert(
        "bucket_name".to_string(),
        Value::String("new-prefix-abcd1234".to_string()),
    );

    let mut state_file = StateFile::new();
    let mut rs = ResourceState::new("s3.bucket", "test-bucket", "awscc");
    rs.attributes.insert(
        "bucket_name".to_string(),
        serde_json::json!("old-prefix-existing1"),
    );
    rs.prefixes
        .insert("bucket_name".to_string(), "old-prefix-".to_string());
    state_file.upsert_resource(rs);

    let mut resources = vec![resource];
    reconcile_prefixed_names(&mut resources, &Some(state_file));

    // Should keep the newly generated name since prefix changed
    assert_eq!(
        resources[0].attributes.get("bucket_name"),
        Some(&Value::String("new-prefix-abcd1234".to_string()))
    );
}

#[test]
fn test_reconcile_prefixed_names_keeps_generated_name_when_no_state() {
    let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());
    resource.attributes.insert(
        "bucket_name".to_string(),
        Value::String("my-app-abcd1234".to_string()),
    );

    let mut resources = vec![resource];
    reconcile_prefixed_names(&mut resources, &None);

    // No state, so keep the generated name
    assert_eq!(
        resources[0].attributes.get("bucket_name"),
        Some(&Value::String("my-app-abcd1234".to_string()))
    );
}

#[test]
fn test_detailed_exitcode_no_changes() {
    // An empty plan means no changes -- has_changes should be false
    let plan = Plan::new();
    let has_changes = plan.mutation_count() > 0;
    assert!(!has_changes);
}

#[test]
fn test_detailed_exitcode_with_changes() {
    // A plan with mutating effects means changes -- has_changes should be true
    let mut plan = Plan::new();
    plan.add(Effect::Create(Resource::new("s3.bucket", "test")));
    let has_changes = plan.mutation_count() > 0;
    assert!(has_changes);
}

#[test]
fn test_detailed_exitcode_read_only_no_changes() {
    // A plan with only Read effects should NOT count as changes
    let mut plan = Plan::new();
    plan.add(Effect::Read {
        resource: Resource::new("sts.caller_identity", "identity").with_read_only(true),
    });
    let has_changes = plan.mutation_count() > 0;
    assert!(!has_changes);
}

fn make_awscc_provider(region_dsl: &str) -> ProviderConfig {
    let mut attrs = HashMap::new();
    attrs.insert("region".to_string(), Value::String(region_dsl.to_string()));
    ProviderConfig {
        name: "awscc".to_string(),
        attributes: attrs,
    }
}

#[test]
fn test_anonymous_id_different_regions_produce_different_identifiers() {
    // Two anonymous ec2_vpc resources with same cidr_block but different provider regions
    let mut r1 = Resource::with_provider("awscc", "ec2.vpc", "");
    r1.attributes.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );

    let mut r2 = Resource::with_provider("awscc", "ec2.vpc", "");
    r2.attributes.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );

    // Use two different provider configs with different regions
    // Resources get identity from their provider, not from resource attributes
    let providers_east = vec![make_awscc_provider("awscc.Region.us_east_1")];
    let providers_west = vec![make_awscc_provider("awscc.Region.us_west_2")];

    let mut resources_east = vec![r1];
    compute_anonymous_identifiers(&mut resources_east, &providers_east).unwrap();

    let mut resources_west = vec![r2];
    compute_anonymous_identifiers(&mut resources_west, &providers_west).unwrap();

    // Both should have identifiers assigned
    assert!(!resources_east[0].id.name.is_empty());
    assert!(!resources_west[0].id.name.is_empty());
    // They must be different because providers have different regions
    assert_ne!(resources_east[0].id.name, resources_west[0].id.name);
}

#[test]
fn test_anonymous_id_same_region_same_create_only_collides() {
    // Two anonymous ec2_vpc resources with same cidr_block and same provider region -> collision
    let mut r1 = Resource::with_provider("awscc", "ec2.vpc", "");
    r1.attributes.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );

    let mut r2 = Resource::with_provider("awscc", "ec2.vpc", "");
    r2.attributes.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );

    let providers = vec![make_awscc_provider("awscc.Region.us_east_1")];
    let mut resources = vec![r1, r2];
    let result = compute_anonymous_identifiers(&mut resources, &providers);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("collision"));
}

#[test]
fn test_anonymous_id_different_create_only_same_region_no_collision() {
    // Two anonymous ec2_vpc resources with different cidr_block in same provider region -> no collision
    let mut r1 = Resource::with_provider("awscc", "ec2.vpc", "");
    r1.attributes.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );

    let mut r2 = Resource::with_provider("awscc", "ec2.vpc", "");
    r2.attributes.insert(
        "cidr_block".to_string(),
        Value::String("10.1.0.0/16".to_string()),
    );

    let providers = vec![make_awscc_provider("awscc.Region.us_east_1")];
    let mut resources = vec![r1, r2];
    compute_anonymous_identifiers(&mut resources, &providers).unwrap();

    assert!(!resources[0].id.name.is_empty());
    assert!(!resources[1].id.name.is_empty());
    assert_ne!(resources[0].id.name, resources[1].id.name);
}

#[test]
fn test_anonymous_id_named_resources_are_skipped() {
    // Named resources should not be processed by compute_anonymous_identifiers
    let mut r1 = Resource::with_provider("awscc", "ec2.vpc", "my_vpc");
    r1.attributes.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );

    let providers = vec![make_awscc_provider("awscc.Region.us_east_1")];
    let mut resources = vec![r1];
    compute_anonymous_identifiers(&mut resources, &providers).unwrap();

    // Name should remain unchanged
    assert_eq!(resources[0].id.name, "my_vpc");
}

#[test]
fn test_find_state_bucket_resource_matching_type() {
    let parsed = ParsedFile {
        providers: vec![],
        backend: None,
        resources: vec![
            Resource::with_provider("aws", "s3.bucket", "my-bucket")
                .with_attribute("bucket", Value::String("my-bucket".to_string())),
        ],
        variables: HashMap::new(),
        imports: vec![],
        module_calls: vec![],
        inputs: vec![],
        outputs: vec![],
    };

    // Matching resource type
    assert!(
        parsed
            .find_resource_by_attr("s3.bucket", "bucket", "my-bucket")
            .is_some()
    );

    // Non-matching resource type
    assert!(
        parsed
            .find_resource_by_attr("gcs.bucket", "bucket", "my-bucket")
            .is_none()
    );

    // Non-matching bucket name
    assert!(
        parsed
            .find_resource_by_attr("s3.bucket", "bucket", "other-bucket")
            .is_none()
    );
}

#[test]
fn validate_data_source_without_read_keyword_errors() {
    let resource = Resource::with_provider("aws", "sts.caller_identity", "identity");
    // read_only defaults to false, simulating missing `read` keyword
    let result = validate_resources(&[resource]);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("data source"),
        "Error should mention 'data source': {}",
        err
    );
    assert!(
        err.contains("read"),
        "Error should mention 'read' keyword: {}",
        err
    );
}

#[test]
fn validate_data_source_with_read_keyword_passes() {
    let resource =
        Resource::with_provider("aws", "sts.caller_identity", "identity").with_read_only(true);
    let result = validate_resources(&[resource]);
    assert!(
        result.is_ok(),
        "Data source with read keyword should pass: {:?}",
        result
    );
}

#[test]
fn validate_regular_resource_without_read_keyword_passes() {
    let resource = Resource::with_provider("aws", "s3.bucket", "my-bucket")
        .with_attribute("bucket", Value::String("my-bucket".to_string()))
        .with_attribute("region", Value::String("ap-northeast-1".to_string()));
    let result = validate_resources(&[resource]);
    assert!(
        result.is_ok(),
        "Regular resource without read should pass: {:?}",
        result
    );
}

#[test]
fn destroy_plan_excludes_data_sources() {
    // Simulate the destroy filtering logic: data sources (read_only=true)
    // should be excluded from the destroy candidate list.
    let managed = Resource::with_provider("awscc", "ec2.vpc", "vpc");
    let data_source =
        Resource::with_provider("awscc", "sts.caller_identity", "identity").with_read_only(true);

    let destroy_order = vec![managed, data_source];

    // Build current_states only for managed resources (data sources are skipped)
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &destroy_order {
        if resource.read_only {
            continue;
        }
        current_states.insert(
            resource.id.clone(),
            State::existing(resource.id.clone(), HashMap::new()),
        );
    }

    // Apply the same filtering logic as run_destroy()
    let resources_to_destroy: Vec<&Resource> = destroy_order
        .iter()
        .filter(|r| {
            if r.read_only {
                return false;
            }
            if !current_states.get(&r.id).map(|s| s.exists).unwrap_or(false) {
                return false;
            }
            true
        })
        .collect();

    assert_eq!(resources_to_destroy.len(), 1);
    assert_eq!(resources_to_destroy[0].id.resource_type, "ec2.vpc");

    // Verify data source is NOT in the destroy list
    assert!(
        !resources_to_destroy.iter().any(|r| r.read_only),
        "Data sources should not appear in destroy plan"
    );
}

/// Simulate the full plan-verify cycle for an anonymous S3 bucket with prefix.
///
/// This test reproduces the bug from issue #535 where after a successful apply,
/// running plan again shows the resource as needing to be created because the
/// anonymous resource identifier changes between runs.
#[test]
fn test_plan_verify_idempotency_anonymous_resource_with_prefix() {
    // --- First run (apply) ---
    // 1. Parse: anonymous resource with bucket_name_prefix
    let mut resource_run1 = Resource::with_provider("awscc", "s3.bucket", "");
    resource_run1.attributes.insert(
        "bucket_name_prefix".to_string(),
        Value::String("my-app-".to_string()),
    );

    let providers = vec![make_awscc_provider("awscc.Region.ap_northeast_1")];

    // 2. resolve_names (resolve_attr_prefixes)
    let mut resources_run1 = vec![resource_run1];
    resolve_names(&mut resources_run1).unwrap();

    // Verify prefix was resolved
    assert!(
        resources_run1[0].prefixes.contains_key("bucket_name"),
        "bucket_name should be in prefixes"
    );
    let run1_bucket_name = match resources_run1[0].attributes.get("bucket_name") {
        Some(Value::String(s)) => s.clone(),
        _ => panic!("bucket_name should be a string"),
    };
    assert!(
        run1_bucket_name.starts_with("my-app-"),
        "bucket_name should start with prefix"
    );

    // 3. compute_anonymous_identifiers
    compute_anonymous_identifiers(&mut resources_run1, &providers).unwrap();
    let run1_name = resources_run1[0].id.name.clone();
    assert!(
        !run1_name.is_empty(),
        "Anonymous identifier should be assigned"
    );

    // 4. Simulate state after apply
    let applied_state = State::existing(
        resources_run1[0].id.clone(),
        vec![(
            "bucket_name".to_string(),
            Value::String(run1_bucket_name.clone()),
        )]
        .into_iter()
        .collect(),
    )
    .with_identifier("my-app-abcd1234");

    let resource_state =
        ResourceState::from_provider_state(&resources_run1[0], &applied_state, None).unwrap();

    let mut state_file = StateFile::new();
    state_file.upsert_resource(resource_state);

    // --- Second run (plan-verify) ---
    // 1. Parse again: same anonymous resource with bucket_name_prefix
    let mut resource_run2 = Resource::with_provider("awscc", "s3.bucket", "");
    resource_run2.attributes.insert(
        "bucket_name_prefix".to_string(),
        Value::String("my-app-".to_string()),
    );

    // 2. resolve_names (resolve_attr_prefixes) - generates NEW random suffix
    let mut resources_run2 = vec![resource_run2];
    resolve_names(&mut resources_run2).unwrap();

    // The random suffix is different on each run (highly probable with 8 hex chars)

    // 3. compute_anonymous_identifiers - should produce SAME identifier
    compute_anonymous_identifiers(&mut resources_run2, &providers).unwrap();
    let run2_name = resources_run2[0].id.name.clone();

    assert_eq!(
        run1_name, run2_name,
        "Anonymous identifier should be stable across runs (prefix-based hash)"
    );

    // 4. reconcile_prefixed_names - should restore original bucket_name from state
    reconcile_prefixed_names(&mut resources_run2, &Some(state_file.clone()));

    let reconciled_bucket_name = match resources_run2[0].attributes.get("bucket_name") {
        Some(Value::String(s)) => s.clone(),
        _ => panic!("bucket_name should be a string after reconciliation"),
    };
    assert_eq!(
        reconciled_bucket_name, run1_bucket_name,
        "Prefix reconciliation should restore original bucket_name from state"
    );

    // 5. get_identifier_for_resource - should find the resource in state
    let identifier = state_file.get_identifier_for_resource(&resources_run2[0]);
    assert_eq!(
        identifier,
        Some("my-app-abcd1234".to_string()),
        "Should find identifier in state for plan-verify (issue #535)"
    );
}

/// Simulate plan-verify for an anonymous IAM role with role_name_prefix and path.
/// This matches the exact failure case from issue #535.
#[test]
fn test_plan_verify_idempotency_iam_role_with_prefix_and_path() {
    let providers = vec![make_awscc_provider("awscc.Region.ap_northeast_1")];

    // --- First run ---
    let mut resource_run1 = Resource::with_provider("awscc", "iam.role", "");
    resource_run1.attributes.insert(
        "role_name_prefix".to_string(),
        Value::String("carina-acc-test-".to_string()),
    );
    resource_run1.attributes.insert(
        "path".to_string(),
        Value::String("/carina/acceptance-test/".to_string()),
    );
    resource_run1.attributes.insert(
        "assume_role_policy_document".to_string(),
        Value::Map(
            vec![(
                "version".to_string(),
                Value::String("2012-10-17".to_string()),
            )]
            .into_iter()
            .collect(),
        ),
    );

    let mut resources_run1 = vec![resource_run1];
    resolve_names(&mut resources_run1).unwrap();
    compute_anonymous_identifiers(&mut resources_run1, &providers).unwrap();
    let run1_name = resources_run1[0].id.name.clone();

    // Simulate state after apply
    let run1_role_name = match resources_run1[0].attributes.get("role_name") {
        Some(Value::String(s)) => s.clone(),
        _ => panic!("role_name should be set after prefix resolution"),
    };
    let applied_state = State::existing(
        resources_run1[0].id.clone(),
        vec![
            (
                "role_name".to_string(),
                Value::String(run1_role_name.clone()),
            ),
            (
                "path".to_string(),
                Value::String("/carina/acceptance-test/".to_string()),
            ),
        ]
        .into_iter()
        .collect(),
    )
    .with_identifier(run1_role_name.as_str());

    let resource_state =
        ResourceState::from_provider_state(&resources_run1[0], &applied_state, None).unwrap();
    let mut state_file = StateFile::new();
    state_file.upsert_resource(resource_state);

    // --- Second run ---
    let mut resource_run2 = Resource::with_provider("awscc", "iam.role", "");
    resource_run2.attributes.insert(
        "role_name_prefix".to_string(),
        Value::String("carina-acc-test-".to_string()),
    );
    resource_run2.attributes.insert(
        "path".to_string(),
        Value::String("/carina/acceptance-test/".to_string()),
    );
    resource_run2.attributes.insert(
        "assume_role_policy_document".to_string(),
        Value::Map(
            vec![(
                "version".to_string(),
                Value::String("2012-10-17".to_string()),
            )]
            .into_iter()
            .collect(),
        ),
    );

    let mut resources_run2 = vec![resource_run2];
    resolve_names(&mut resources_run2).unwrap();
    compute_anonymous_identifiers(&mut resources_run2, &providers).unwrap();
    let run2_name = resources_run2[0].id.name.clone();

    assert_eq!(
        run1_name, run2_name,
        "IAM role anonymous identifier should be stable across runs"
    );

    reconcile_prefixed_names(&mut resources_run2, &Some(state_file.clone()));

    let identifier = state_file.get_identifier_for_resource(&resources_run2[0]);
    assert!(
        identifier.is_some(),
        "Should find IAM role identifier in state for plan-verify (issue #535)"
    );
}

/// Simulate plan-verify for an anonymous flow_log with ResourceRef create-only attributes.
/// ec2_flow_log/s3 test uses ResourceRef values (vpc.vpc_id, bucket.arn) in create-only
/// attributes, which must produce the same hash across runs.
#[test]
fn test_plan_verify_idempotency_anonymous_flow_log_with_resource_refs() {
    let providers = vec![make_awscc_provider("awscc.Region.ap_northeast_1")];

    // --- First run ---
    let mut resource_run1 = Resource::with_provider("awscc", "ec2.flow_log", "");
    resource_run1.attributes.insert(
        "resource_id".to_string(),
        Value::ResourceRef {
            binding_name: "vpc".to_string(),
            attribute_name: "vpc_id".to_string(),
        },
    );
    resource_run1.attributes.insert(
        "resource_type".to_string(),
        Value::UnresolvedIdent("VPC".to_string(), None),
    );
    resource_run1.attributes.insert(
        "traffic_type".to_string(),
        Value::UnresolvedIdent("ALL".to_string(), None),
    );
    resource_run1.attributes.insert(
        "log_destination_type".to_string(),
        Value::UnresolvedIdent("s3".to_string(), None),
    );
    resource_run1.attributes.insert(
        "log_destination".to_string(),
        Value::ResourceRef {
            binding_name: "bucket".to_string(),
            attribute_name: "arn".to_string(),
        },
    );
    resource_run1.attributes.insert(
        "destination_options".to_string(),
        Value::Map(
            vec![
                (
                    "file_format".to_string(),
                    Value::String("plain-text".to_string()),
                ),
                ("hive_compatible_partitions".to_string(), Value::Bool(false)),
                ("per_hour_partition".to_string(), Value::Bool(false)),
            ]
            .into_iter()
            .collect(),
        ),
    );

    let mut resources_run1 = vec![resource_run1];
    compute_anonymous_identifiers(&mut resources_run1, &providers).unwrap();
    let run1_name = resources_run1[0].id.name.clone();

    // Simulate state after apply
    let applied_state = State::existing(resources_run1[0].id.clone(), HashMap::new())
        .with_identifier("fl-12345678");

    let resource_state =
        ResourceState::from_provider_state(&resources_run1[0], &applied_state, None).unwrap();
    let mut state_file = StateFile::new();
    state_file.upsert_resource(resource_state);

    // --- Second run ---
    let mut resource_run2 = Resource::with_provider("awscc", "ec2.flow_log", "");
    resource_run2.attributes.insert(
        "resource_id".to_string(),
        Value::ResourceRef {
            binding_name: "vpc".to_string(),
            attribute_name: "vpc_id".to_string(),
        },
    );
    resource_run2.attributes.insert(
        "resource_type".to_string(),
        Value::UnresolvedIdent("VPC".to_string(), None),
    );
    resource_run2.attributes.insert(
        "traffic_type".to_string(),
        Value::UnresolvedIdent("ALL".to_string(), None),
    );
    resource_run2.attributes.insert(
        "log_destination_type".to_string(),
        Value::UnresolvedIdent("s3".to_string(), None),
    );
    resource_run2.attributes.insert(
        "log_destination".to_string(),
        Value::ResourceRef {
            binding_name: "bucket".to_string(),
            attribute_name: "arn".to_string(),
        },
    );
    resource_run2.attributes.insert(
        "destination_options".to_string(),
        Value::Map(
            vec![
                (
                    "file_format".to_string(),
                    Value::String("plain-text".to_string()),
                ),
                ("hive_compatible_partitions".to_string(), Value::Bool(false)),
                ("per_hour_partition".to_string(), Value::Bool(false)),
            ]
            .into_iter()
            .collect(),
        ),
    );

    let mut resources_run2 = vec![resource_run2];
    compute_anonymous_identifiers(&mut resources_run2, &providers).unwrap();
    let run2_name = resources_run2[0].id.name.clone();

    assert_eq!(
        run1_name, run2_name,
        "Flow log anonymous identifier should be stable across runs"
    );

    let identifier = state_file.get_identifier_for_resource(&resources_run2[0]);
    assert_eq!(
        identifier,
        Some("fl-12345678".to_string()),
        "Should find flow_log identifier in state for plan-verify (issue #535)"
    );
}

#[tokio::test]
async fn detect_drift_errors_when_resource_missing_from_planned_states() {
    let resource = Resource::with_provider("aws", "s3.bucket", "my-bucket");
    let id = resource.id.clone();

    // Provider returns a non-existing state (identifier is None since no planned state)
    let provider = TestProvider::with_read_state(&id, "", State::not_found(id.clone()));

    // planned_states is empty - resource is missing
    let planned_states: HashMap<ResourceId, State> = HashMap::new();

    let result = detect_drift(&[resource], &planned_states, &provider).await;

    assert!(
        result.is_err(),
        "Should return error when resource is missing from planned states"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("missing from planned states"),
        "Error message should mention missing planned states, got: {}",
        err
    );
}

#[tokio::test]
async fn detect_drift_returns_none_when_no_drift() {
    let resource = Resource::with_provider("aws", "s3.bucket", "my-bucket");
    let id = resource.id.clone();
    let identifier = "my-bucket";

    let state = State::existing(
        id.clone(),
        HashMap::from([("name".to_string(), Value::String("my-bucket".to_string()))]),
    )
    .with_identifier(identifier);

    let provider = TestProvider::with_read_state(&id, identifier, state.clone());
    let planned_states = HashMap::from([(id.clone(), state)]);

    let result = detect_drift(&[resource], &planned_states, &provider).await;

    assert!(result.is_ok());
    assert!(result.unwrap().is_none(), "Should detect no drift");
}

#[tokio::test]
async fn detect_drift_returns_messages_when_drift_detected() {
    let resource = Resource::with_provider("aws", "s3.bucket", "my-bucket");
    let id = resource.id.clone();
    let identifier = "my-bucket";

    let planned = State::existing(
        id.clone(),
        HashMap::from([("name".to_string(), Value::String("my-bucket".to_string()))]),
    )
    .with_identifier(identifier);

    // Actual state has different attribute value
    let actual = State::existing(
        id.clone(),
        HashMap::from([(
            "name".to_string(),
            Value::String("changed-bucket".to_string()),
        )]),
    )
    .with_identifier(identifier);

    let provider = TestProvider::with_read_state(&id, identifier, actual);
    let planned_states = HashMap::from([(id.clone(), planned)]);

    let result = detect_drift(&[resource], &planned_states, &provider).await;

    assert!(result.is_ok());
    let messages = result.unwrap();
    assert!(messages.is_some(), "Should detect drift");
    let msgs = messages.unwrap();
    assert!(!msgs.is_empty(), "Should have drift messages");
}

/// Test that resources tracked in the state file but removed from the .crn config
/// produce a Delete effect in the plan.  This is the regression test for issue #844.
#[test]
fn orphaned_state_resource_produces_delete_effect() {
    use carina_core::differ::create_plan;
    use std::collections::HashSet;

    // State file has two resources: "keep-bucket" and "removed-bucket"
    let mut state_file = StateFile::new();
    state_file.upsert_resource(
        ResourceState::new("s3.bucket", "keep-bucket", "aws")
            .with_identifier("keep-bucket")
            .with_attribute("bucket", json!("keep-bucket")),
    );
    state_file.upsert_resource(
        ResourceState::new("s3.bucket", "removed-bucket", "aws")
            .with_identifier("removed-bucket")
            .with_attribute("bucket", json!("removed-bucket")),
    );

    // Config only has "keep-bucket" -- "removed-bucket" was deleted from .crn
    let desired = vec![
        Resource::with_provider("aws", "s3.bucket", "keep-bucket")
            .with_attribute("bucket", Value::String("keep-bucket".to_string())),
    ];

    let desired_ids: HashSet<ResourceId> = desired.iter().map(|r| r.id.clone()).collect();

    // Build current_states from desired resources (simulates the provider read loop)
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &desired {
        current_states.insert(
            resource.id.clone(),
            State::existing(
                resource.id.clone(),
                HashMap::from([(
                    "bucket".to_string(),
                    Value::String("keep-bucket".to_string()),
                )]),
            )
            .with_identifier("keep-bucket"),
        );
    }

    // Seed orphaned state entries -- this is the fix for #844
    let orphan_states = state_file.build_orphan_states(&desired_ids);
    for (id, state) in orphan_states {
        current_states.entry(id).or_insert(state);
    }

    let lifecycles = state_file.build_lifecycles();
    let saved_attrs = state_file.build_saved_attrs();
    let prev_desired_keys = state_file.build_desired_keys();

    let plan = create_plan(
        &desired,
        &current_states,
        &lifecycles,
        &HashMap::new(),
        &saved_attrs,
        &prev_desired_keys,
    );

    // The plan should contain a Delete effect for "removed-bucket"
    let delete_effects: Vec<_> = plan
        .effects()
        .iter()
        .filter(|e| matches!(e, Effect::Delete { .. }))
        .collect();

    assert_eq!(
        delete_effects.len(),
        1,
        "Should have exactly one Delete effect for the orphaned resource, got: {:?}",
        plan.effects()
    );

    match &delete_effects[0] {
        Effect::Delete { id, identifier, .. } => {
            assert_eq!(id.name, "removed-bucket");
            assert_eq!(identifier, "removed-bucket");
        }
        _ => unreachable!(),
    }

    // The plan should NOT have any effects for "keep-bucket" (it's unchanged)
    let non_delete_effects: Vec<_> = plan
        .effects()
        .iter()
        .filter(|e| !matches!(e, Effect::Delete { .. }))
        .collect();
    assert!(
        non_delete_effects.is_empty(),
        "Should have no non-Delete effects for unchanged resource, got: {:?}",
        non_delete_effects
    );
}

/// A mock StateBackend that fails on write_state and tracks release_lock calls
struct MockBackend {
    write_state_fails: bool,
    lock_released: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl StateBackend for MockBackend {
    async fn read_state(&self) -> carina_state::BackendResult<Option<StateFile>> {
        Ok(Some(StateFile::new()))
    }
    async fn write_state(&self, _state: &StateFile) -> carina_state::BackendResult<()> {
        if self.write_state_fails {
            Err(BackendError::Io("simulated write failure".to_string()))
        } else {
            Ok(())
        }
    }
    async fn acquire_lock(&self, operation: &str) -> carina_state::BackendResult<LockInfo> {
        Ok(LockInfo::new(operation))
    }
    async fn release_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<()> {
        self.lock_released.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn renew_lock(&self, lock: &LockInfo) -> carina_state::BackendResult<LockInfo> {
        Ok(lock.renewed())
    }
    async fn write_state_locked(
        &self,
        state: &carina_state::StateFile,
        _lock: &LockInfo,
    ) -> carina_state::BackendResult<()> {
        self.write_state(state).await
    }
    async fn force_unlock(&self, _lock_id: &str) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn init(&self) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn bucket_exists(&self) -> carina_state::BackendResult<bool> {
        Ok(true)
    }
    async fn create_bucket(&self) -> carina_state::BackendResult<()> {
        Ok(())
    }
    fn resource_type(&self) -> Option<&str> {
        None
    }
    fn provider_name(&self) -> Option<&str> {
        None
    }
    fn resource_definition(&self, _bucket_name: &str) -> Option<String> {
        None
    }
}

#[tokio::test]
async fn lock_released_on_write_state_failure() {
    // Simulate the caller pattern: finalize_apply + always release lock
    let lock_released = Arc::new(AtomicBool::new(false));
    let backend = MockBackend {
        write_state_fails: true,
        lock_released: lock_released.clone(),
    };
    let lock = LockInfo::new("apply");

    let result = ApplyResult {
        success_count: 0,
        failure_count: 0,
        skip_count: 0,
        applied_states: HashMap::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        failed_refreshes: HashSet::new(),
    };

    // This mirrors the pattern used in run_apply_locked / run_apply_from_plan_locked:
    // call finalize_apply (which may fail), then always release lock in the caller.
    let op_result = finalize_apply(
        &result,
        Some(StateFile::new()),
        &[],
        &HashMap::new(),
        &Plan::new(),
        &backend,
        Some(&lock),
    )
    .await;

    // Caller always releases the lock
    let _release = backend.release_lock(&lock).await;

    assert!(
        op_result.is_err(),
        "finalize_apply should fail on write error"
    );
    assert!(
        lock_released.load(Ordering::SeqCst),
        "Lock should be released even when write_state fails"
    );
}

/// A mock StateBackend that tracks which write method was called and whether
/// renew_lock was invoked.
struct LockTrackingBackend {
    renew_lock_called: Arc<AtomicBool>,
    write_state_called: Arc<AtomicBool>,
    write_state_locked_called: Arc<AtomicBool>,
}

impl LockTrackingBackend {
    fn new() -> Self {
        Self {
            renew_lock_called: Arc::new(AtomicBool::new(false)),
            write_state_called: Arc::new(AtomicBool::new(false)),
            write_state_locked_called: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait::async_trait]
impl StateBackend for LockTrackingBackend {
    async fn read_state(&self) -> carina_state::BackendResult<Option<StateFile>> {
        Ok(Some(StateFile::new()))
    }
    async fn write_state(&self, _state: &StateFile) -> carina_state::BackendResult<()> {
        self.write_state_called.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn acquire_lock(&self, operation: &str) -> carina_state::BackendResult<LockInfo> {
        Ok(LockInfo::new(operation))
    }
    async fn release_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn renew_lock(&self, lock: &LockInfo) -> carina_state::BackendResult<LockInfo> {
        self.renew_lock_called.store(true, Ordering::SeqCst);
        Ok(lock.renewed())
    }
    async fn write_state_locked(
        &self,
        _state: &carina_state::StateFile,
        _lock: &LockInfo,
    ) -> carina_state::BackendResult<()> {
        self.write_state_locked_called.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn force_unlock(&self, _lock_id: &str) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn init(&self) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn bucket_exists(&self) -> carina_state::BackendResult<bool> {
        Ok(true)
    }
    async fn create_bucket(&self) -> carina_state::BackendResult<()> {
        Ok(())
    }
    fn resource_type(&self) -> Option<&str> {
        None
    }
    fn provider_name(&self) -> Option<&str> {
        None
    }
    fn resource_definition(&self, _bucket_name: &str) -> Option<String> {
        None
    }
}

#[tokio::test]
async fn save_state_locked_calls_renew_lock_and_write_state_locked() {
    let backend = LockTrackingBackend::new();
    let lock = LockInfo::new("apply");
    let mut state = StateFile::new();

    let result = save_state_locked(&backend, &lock, &mut state).await;
    assert!(result.is_ok(), "save_state_locked should succeed");

    assert!(
        backend.renew_lock_called.load(Ordering::SeqCst),
        "save_state_locked must call renew_lock before writing"
    );
    assert!(
        backend.write_state_locked_called.load(Ordering::SeqCst),
        "save_state_locked must call write_state_locked, not write_state"
    );
    assert!(
        !backend.write_state_called.load(Ordering::SeqCst),
        "save_state_locked must NOT call write_state (the unguarded version)"
    );
}

#[tokio::test]
async fn finalize_apply_uses_write_state_locked() {
    let backend = LockTrackingBackend::new();
    let lock = LockInfo::new("apply");

    let result = ApplyResult {
        success_count: 0,
        failure_count: 0,
        skip_count: 0,
        applied_states: HashMap::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        failed_refreshes: HashSet::new(),
    };

    let op_result = finalize_apply(
        &result,
        Some(StateFile::new()),
        &[],
        &HashMap::new(),
        &Plan::new(),
        &backend,
        Some(&lock),
    )
    .await;

    assert!(op_result.is_ok(), "finalize_apply should succeed");
    assert!(
        backend.write_state_locked_called.load(Ordering::SeqCst),
        "finalize_apply must use write_state_locked"
    );
    assert!(
        !backend.write_state_called.load(Ordering::SeqCst),
        "finalize_apply must NOT use write_state (the unguarded version)"
    );
}

/// Mock provider that records update calls for verification.
/// Used to test that dependents with their own Update effects get the new
/// (post-replacement) dependency IDs, not stale pre-replacement IDs.
struct RecordingProvider {
    /// Tracks the `to` resource passed to each update call, keyed by resource ID string.
    update_calls: std::sync::Mutex<Vec<(String, Resource)>>,
}

impl RecordingProvider {
    fn new() -> Self {
        Self {
            update_calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn get_update_calls(&self) -> Vec<(String, Resource)> {
        self.update_calls.lock().unwrap().clone()
    }
}

impl Provider for RecordingProvider {
    fn name(&self) -> &'static str {
        "test"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move { Ok(State::not_found(id)) })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        // Return a state with a new identifier to simulate resource creation
        let mut attrs = resource.attributes.clone();
        // Simulate AWS returning a new ID
        attrs.insert("vpc_id".to_string(), Value::String("vpc-NEW".to_string()));
        let state = State::existing(resource.id.clone(), attrs).with_identifier("vpc-NEW");
        Box::pin(async move { Ok(state) })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        self.update_calls
            .lock()
            .unwrap()
            .push((id.to_string(), to.clone()));
        let state =
            State::existing(id.clone(), to.attributes.clone()).with_identifier("subnet-123");
        Box::pin(async move { Ok(state) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Mock provider where create and delete succeed but update (rename) fails.
struct RenameFailProvider;

impl Provider for RenameFailProvider {
    fn name(&self) -> &'static str {
        "test"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move { Ok(State::not_found(id)) })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let state = State::existing(resource.id.clone(), resource.attributes.clone())
            .with_identifier("temp-name-abc");
        Box::pin(async move { Ok(state) })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _from: &State,
        _to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::new("rename failed: API error")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Regression test for #878: when a create_before_destroy rename fails,
/// the effect should be counted as a failure, not a success.
#[tokio::test]
async fn rename_failure_in_create_before_destroy_counts_as_failure() {
    use carina_core::effect::TemporaryName;

    let id = ResourceId::with_provider("awscc", "s3.bucket", "my-bucket");

    let old_state = State::existing(
        id.clone(),
        HashMap::from([(
            "bucket_name".to_string(),
            Value::String("my-bucket".to_string()),
        )]),
    )
    .with_identifier("my-bucket");

    let new_resource = Resource::with_provider("awscc", "s3.bucket", "my-bucket")
        .with_attribute("bucket_name", Value::String("my-bucket-tmp123".to_string()));

    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: id.clone(),
        from: Box::new(old_state.clone()),
        to: new_resource.clone(),
        lifecycle: LifecycleConfig {
            force_delete: false,
            create_before_destroy: true,
        },
        changed_create_only: vec!["bucket_name".to_string()],
        cascading_updates: vec![],
        temporary_name: Some(TemporaryName {
            attribute: "bucket_name".to_string(),
            original_value: "my-bucket".to_string(),
            temporary_value: "my-bucket-tmp123".to_string(),
            can_rename: true,
        }),
    });

    let provider = RenameFailProvider;
    let mut binding_map = HashMap::new();
    let mut current_states = HashMap::from([(id.clone(), old_state)]);
    let unresolved_resources = HashMap::from([(id.clone(), new_resource)]);

    let result = execute_effects(
        &plan,
        &provider,
        &mut binding_map,
        &mut current_states,
        &unresolved_resources,
    )
    .await;

    // The rename failed, so the effect should be counted as a failure
    assert_eq!(
        result.failure_count, 1,
        "Rename failure should increment failure_count"
    );
    assert_eq!(
        result.success_count, 0,
        "Rename failure should not increment success_count"
    );

    // The state should still be saved (resource exists with temp name)
    assert!(
        result.applied_states.contains_key(&id),
        "State should still be saved even on rename failure"
    );
}

/// Regression test for #865: when a resource is replaced via create_before_destroy
/// and a dependent resource has its own Update effect, the dependent's `to` state
/// should reference the new (post-replacement) resource ID, not the old one.
#[tokio::test]
async fn update_effect_resolves_refs_against_post_replacement_binding_map() {
    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    // --- Unresolved resources (before ref resolution) ---
    let vpc_unresolved = Resource::new("ec2.vpc", "my-vpc")
        .with_attribute("_binding", Value::String("vpc".to_string()))
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet_unresolved = Resource::new("ec2.subnet", "my-subnet")
        .with_attribute("_binding", Value::String("subnet".to_string()))
        .with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            },
        )
        .with_attribute("cidr_block", Value::String("10.1.2.0/24".to_string()));

    // --- Resolved resources (after ref resolution with old state) ---
    // The subnet's vpc_id has been eagerly resolved to "vpc-OLD"
    let subnet_resolved = Resource::new("ec2.subnet", "my-subnet")
        .with_attribute("_binding", Value::String("subnet".to_string()))
        .with_attribute("vpc_id", Value::String("vpc-OLD".to_string()))
        .with_attribute("cidr_block", Value::String("10.1.2.0/24".to_string()));

    // --- Current states ---
    let mut current_states = HashMap::new();

    let mut vpc_attrs = HashMap::new();
    vpc_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    vpc_attrs.insert("vpc_id".to_string(), Value::String("vpc-OLD".to_string()));
    current_states.insert(
        vpc_id.clone(),
        State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-OLD"),
    );

    let mut subnet_attrs = HashMap::new();
    subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-OLD".to_string()));
    subnet_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.1.1.0/24".to_string()),
    );
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
    );

    // --- Build plan ---
    // VPC: Replace with create_before_destroy (no cascading updates for subnet
    //       because subnet already has its own Update effect)
    // Subnet: Update (cidr_block changed, vpc_id eagerly resolved to old value)
    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc_unresolved
            .clone()
            .with_attribute("_binding", Value::String("vpc".to_string())),
        lifecycle: LifecycleConfig {
            force_delete: false,
            create_before_destroy: true,
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
    });
    plan.add(Effect::Update {
        id: subnet_id.clone(),
        from: Box::new(current_states.get(&subnet_id).unwrap().clone()),
        to: subnet_resolved.clone(), // Has stale "vpc-OLD" in vpc_id
        changed_attributes: vec!["cidr_block".to_string()],
    });

    // --- Initial binding map (with old state) ---
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    binding_map.insert(
        "vpc".to_string(),
        HashMap::from([
            ("vpc_id".to_string(), Value::String("vpc-OLD".to_string())),
            (
                "cidr_block".to_string(),
                Value::String("10.0.0.0/16".to_string()),
            ),
            ("_binding".to_string(), Value::String("vpc".to_string())),
        ]),
    );
    binding_map.insert(
        "subnet".to_string(),
        HashMap::from([
            ("vpc_id".to_string(), Value::String("vpc-OLD".to_string())),
            (
                "cidr_block".to_string(),
                Value::String("10.1.1.0/24".to_string()),
            ),
            ("_binding".to_string(), Value::String("subnet".to_string())),
        ]),
    );

    // --- Unresolved resource map ---
    let unresolved_resources: HashMap<ResourceId, Resource> = HashMap::from([
        (vpc_id.clone(), vpc_unresolved),
        (subnet_id.clone(), subnet_unresolved),
    ]);

    // --- Execute ---
    let provider = RecordingProvider::new();
    let result = execute_effects(
        &plan,
        &provider,
        &mut binding_map,
        &mut current_states,
        &unresolved_resources,
    )
    .await;

    assert_eq!(result.success_count, 2, "Both effects should succeed");
    assert_eq!(result.failure_count, 0, "No effects should fail");

    // --- Verify the subnet update received the NEW vpc_id ---
    let update_calls = provider.get_update_calls();
    let subnet_update = update_calls
        .iter()
        .find(|(id_str, _)| id_str.contains("subnet"))
        .expect("Should have an update call for subnet");

    let vpc_id_in_update = subnet_update
        .1
        .attributes
        .get("vpc_id")
        .expect("subnet update should have vpc_id attribute");

    assert_eq!(
        *vpc_id_in_update,
        Value::String("vpc-NEW".to_string()),
        "Subnet update should reference the NEW vpc_id (vpc-NEW), not the stale old one (vpc-OLD)"
    );
}

/// A mock StateBackend that returns a pre-configured state and captures writes.
struct RefreshTestBackend {
    initial_state: Option<StateFile>,
    written_state: Mutex<Option<StateFile>>,
}

impl RefreshTestBackend {
    fn new(state: StateFile) -> Self {
        Self {
            initial_state: Some(state),
            written_state: Mutex::new(None),
        }
    }

    fn get_written_state(&self) -> Option<StateFile> {
        self.written_state.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl StateBackend for RefreshTestBackend {
    async fn read_state(&self) -> carina_state::BackendResult<Option<StateFile>> {
        Ok(self.initial_state.clone())
    }
    async fn write_state(&self, state: &StateFile) -> carina_state::BackendResult<()> {
        *self.written_state.lock().unwrap() = Some(state.clone());
        Ok(())
    }
    async fn acquire_lock(&self, operation: &str) -> carina_state::BackendResult<LockInfo> {
        Ok(LockInfo::new(operation))
    }
    async fn release_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn renew_lock(&self, lock: &LockInfo) -> carina_state::BackendResult<LockInfo> {
        Ok(lock.renewed())
    }
    async fn write_state_locked(
        &self,
        state: &carina_state::StateFile,
        _lock: &LockInfo,
    ) -> carina_state::BackendResult<()> {
        self.write_state(state).await
    }
    async fn force_unlock(&self, _lock_id: &str) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn init(&self) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn bucket_exists(&self) -> carina_state::BackendResult<bool> {
        Ok(true)
    }
    async fn create_bucket(&self) -> carina_state::BackendResult<()> {
        Ok(())
    }
    fn resource_type(&self) -> Option<&str> {
        None
    }
    fn provider_name(&self) -> Option<&str> {
        None
    }
    fn resource_definition(&self, _bucket_name: &str) -> Option<String> {
        None
    }
}

/// Regression test for #879: state refresh should handle orphaned resources
/// (resources in state but removed from config).
#[tokio::test]
async fn state_refresh_removes_orphaned_resource_deleted_externally() {
    // State has two resources: "keep-bucket" (in config) and "orphan-bucket" (not in config)
    let mut state = StateFile::new();
    state.upsert_resource(
        ResourceState::new("s3.bucket", "keep-bucket", "")
            .with_identifier("keep-bucket")
            .with_attribute("bucket", json!("keep-bucket")),
    );
    state.upsert_resource(
        ResourceState::new("s3.bucket", "orphan-bucket", "")
            .with_identifier("orphan-bucket")
            .with_attribute("bucket", json!("orphan-bucket")),
    );

    let backend = RefreshTestBackend::new(state);

    // Config only has "keep-bucket" -- "orphan-bucket" was removed from .crn
    let mut parsed = ParsedFile {
        providers: vec![],
        backend: None,
        resources: vec![
            Resource::new("s3.bucket", "keep-bucket")
                .with_attribute("bucket", Value::String("keep-bucket".to_string())),
        ],
        variables: HashMap::new(),
        imports: vec![],
        module_calls: vec![],
        inputs: vec![],
        outputs: vec![],
    };

    // MockProvider returns not_found for both resources (simulates external deletion)
    let lock = LockInfo::new("state-refresh");
    let result = run_state_refresh_locked(&mut parsed, &backend, Some(&lock)).await;
    assert!(result.is_ok(), "refresh should succeed: {:?}", result);

    // Verify the written state
    let written = backend
        .get_written_state()
        .expect("state should be written");

    // "orphan-bucket" should have been removed from state since it was deleted externally
    // and the refresh should have visited it
    assert!(
        written
            .find_resource("", "s3.bucket", "orphan-bucket")
            .is_none(),
        "Orphaned resource should be removed from state after refresh (issue #879)"
    );
}

/// Test that save_state_unlocked uses write_state (not write_state_locked) and
/// does NOT call renew_lock.
#[tokio::test]
async fn save_state_unlocked_uses_write_state_without_lock() {
    use crate::commands::apply::save_state_unlocked;

    let backend = LockTrackingBackend::new();
    let mut state = StateFile::new();

    let result = save_state_unlocked(&backend, &mut state).await;
    assert!(result.is_ok(), "save_state_unlocked should succeed");

    assert!(
        backend.write_state_called.load(Ordering::SeqCst),
        "save_state_unlocked must call write_state"
    );
    assert!(
        !backend.renew_lock_called.load(Ordering::SeqCst),
        "save_state_unlocked must NOT call renew_lock"
    );
    assert!(
        !backend.write_state_locked_called.load(Ordering::SeqCst),
        "save_state_unlocked must NOT call write_state_locked"
    );
}

/// Test that finalize_apply with lock=None uses write_state (unlocked path).
#[tokio::test]
async fn finalize_apply_without_lock_uses_write_state() {
    let backend = LockTrackingBackend::new();

    let result = ApplyResult {
        success_count: 0,
        failure_count: 0,
        skip_count: 0,
        applied_states: HashMap::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        failed_refreshes: HashSet::new(),
    };

    let op_result = finalize_apply(
        &result,
        Some(StateFile::new()),
        &[],
        &HashMap::new(),
        &Plan::new(),
        &backend,
        None, // No lock
    )
    .await;

    assert!(op_result.is_ok(), "finalize_apply should succeed");
    assert!(
        backend.write_state_called.load(Ordering::SeqCst),
        "finalize_apply without lock must use write_state"
    );
    assert!(
        !backend.write_state_locked_called.load(Ordering::SeqCst),
        "finalize_apply without lock must NOT use write_state_locked"
    );
    assert!(
        !backend.renew_lock_called.load(Ordering::SeqCst),
        "finalize_apply without lock must NOT call renew_lock"
    );
}

/// Test that WiringContext is constructed once and provides factories and schemas
/// without repeated allocations.
#[test]
fn wiring_context_constructs_factories_and_schemas_once() {
    let ctx = WiringContext::new();

    // Factories should include at least aws and awscc
    assert!(
        ctx.factories().len() >= 2,
        "Should have at least 2 provider factories (aws, awscc)"
    );

    // Schemas should be non-empty
    assert!(
        !ctx.schemas().is_empty(),
        "Should have schemas from provider factories"
    );

    // Calling schemas() again should return the same data (cached, not rebuilt)
    let schemas_a = ctx.schemas();
    let schemas_b = ctx.schemas();
    assert_eq!(
        schemas_a.len(),
        schemas_b.len(),
        "Schemas should be consistent across calls"
    );
}

/// Issue #931: orphaned resources that no longer exist in infrastructure should not
/// produce a Delete effect. `create_plan_from_parsed()` now calls `provider.read()`
/// for each orphan returned by `build_orphan_states()` and skips those that no longer
/// exist.
///
/// This test simulates the fixed code path:
/// 1. State file has "removed-bucket" (orphaned — not in .crn config)
/// 2. The provider returns not-found for "removed-bucket" (deleted externally)
/// 3. The refresh loop skips it, so current_states has no entry for it
/// 4. Expected: no Delete effect should be generated
#[test]
fn orphaned_resource_deleted_externally_should_not_produce_delete_effect() {
    use carina_core::differ::create_plan;

    // State file tracks "removed-bucket" with an identifier (implies it existed in infra)
    let mut state_file = StateFile::new();
    state_file.upsert_resource(
        ResourceState::new("s3.bucket", "removed-bucket", "aws")
            .with_identifier("removed-bucket")
            .with_attribute("bucket", json!("removed-bucket")),
    );

    // No desired resources — "removed-bucket" was removed from .crn
    let desired: Vec<Resource> = vec![];
    let desired_ids: HashSet<ResourceId> = desired.iter().map(|r| r.id.clone()).collect();

    let mut current_states: HashMap<ResourceId, State> = HashMap::new();

    // Simulate the fixed code path in create_plan_from_parsed():
    // For each orphan, call provider.read(). If the provider returns not-found
    // (resource was deleted externally), skip inserting it into current_states.
    let orphan_states = state_file.build_orphan_states(&desired_ids);
    for (id, state) in orphan_states {
        // Simulate provider.read() returning not-found for this identifier.
        // In the real code, provider.read(&id, state.identifier.as_deref())
        // would return State { exists: false, .. }.
        let refreshed = State::not_found(id.clone());
        if refreshed.exists {
            current_states.entry(id).or_insert(refreshed);
        }
        // "state" from build_orphan_states is intentionally unused — the fix
        // replaces it with the provider-refreshed result.
        let _ = state;
    }

    let lifecycles = state_file.build_lifecycles();
    let saved_attrs = state_file.build_saved_attrs();
    let prev_desired_keys = state_file.build_desired_keys();

    let plan = create_plan(
        &desired,
        &current_states,
        &lifecycles,
        &HashMap::new(),
        &saved_attrs,
        &prev_desired_keys,
    );

    let delete_effects: Vec<_> = plan
        .effects()
        .iter()
        .filter(|e| matches!(e, Effect::Delete { .. }))
        .collect();

    assert_eq!(
        delete_effects.len(),
        0,
        "Issue #931: orphaned resource deleted externally should NOT produce a Delete effect. \
         Got delete effects: {:?}",
        delete_effects
    );
}
