use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use indexmap::IndexMap;

use serde_json::json;

use carina_core::data_source_value::{DataSourceValue, DataSourceValues};
use carina_core::effect::Effect;
use carina_core::executor::{AcceptedRequest, Applied};
use carina_core::parser::{ParsedFile, ProviderConfig, ProviderContext};
use carina_core::plan::Plan;
use carina_core::provider::{
    AppliedIdentifier, BoxFuture, ProviderError, ProviderResult, ReadOutcome,
};
use carina_core::resource::{
    ConcreteValue, Current, DataSource, DeferredValue, Directives, ResolvedDataSource,
    ResolvedResource, Resource, ResourceId, State, Value,
};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
use carina_core::value::PersistedValue;
use carina_state::{
    BackendError, DeposedInstance, DeposedKey, LoadedState, LockInfo, ResourceState, RowBasis,
    StateBackend, StateFile,
};

use crate::commands::apply::{
    ApplyResult, ApplyTarget, detect_drift, finalize_apply, refresh_pending_states,
    save_state_locked,
};
use crate::commands::plan::{
    CurrentStateEntry, MovedOrigins, PlanFile, PlanFileContents, RecordedProviderArtifact,
    ReloadedCurrentStateEntry, SavedDataSourceValue,
};
use crate::commands::shared::finalize::StatePersistence;
use crate::commands::shared::lock_heartbeat::{StateLock, StateLockRef};
use crate::commands::shared::state_writeback::{
    ApplyStateSave, ApplyStateSaveInputs, FinalizeApplyInput, PostApplyStates,
    build_state_after_apply, queue_state_refresh,
};
use crate::commands::state::run_state_refresh_locked;
use crate::fixture_read_outcome;
use crate::wiring::{
    WiringContext, compute_anonymous_identifiers, reconcile_prefixed_names, resolve_attr_prefixes,
    resolve_names, validate_resources,
};
use carina_core::parser::BackendConfig;
use carina_core::provider::{Provider, ProviderFactory};
use std::sync::Mutex;

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

struct EvidenceTestFactory {
    name: String,
}

impl ProviderFactory for EvidenceTestFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        "saved-plan evidence test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async {
            Ok(Box::new(carina_provider_mock::MockProvider::new()) as Box<dyn Provider>)
        })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        Vec::new()
    }
}

fn provider_config_for_evidence_test(name: &str) -> ProviderConfig {
    ProviderConfig {
        name: name.to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: Some(format!("file:///{name}.wasm").parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }
}

fn record_test_plan(mut contents: PlanFileContents, providers: &[&str]) -> PlanFile {
    if contents.provider_configs.as_provider_configs().is_empty() {
        contents.provider_configs = saved_provider_configs(
            providers
                .iter()
                .map(|provider| provider_config_for_evidence_test(provider))
                .collect(),
        );
    }
    let factories = providers
        .iter()
        .map(|provider| {
            Box::new(EvidenceTestFactory {
                name: (*provider).to_string(),
            }) as Box<dyn ProviderFactory>
        })
        .collect();
    let ctx =
        WiringContext::new_for_test(factories, contents.provider_configs.as_provider_configs());
    contents
        .record(&ctx)
        .expect("test plan provider evidence must be valid")
}

fn final_state_save_inputs(sorted_resources: &[ResolvedResource]) -> Arc<ApplyStateSaveInputs> {
    Arc::new(ApplyStateSaveInputs::new(
        sorted_resources.to_vec(),
        Plan::new(),
        SchemaRegistry::new(),
        None,
    ))
}

fn complete_create(state: State) -> Applied {
    Applied::Complete {
        state,
        request: AcceptedRequest::create_for_test(),
    }
}

fn complete_update(state: State, sent: &[&str], removed: &[&str]) -> Applied {
    Applied::Complete {
        state,
        request: AcceptedRequest::update_for_test(sent, removed),
    }
}

fn resolved_data_source(resource: DataSource) -> ResolvedDataSource {
    ResolvedDataSource::new(resource)
}

fn checked_exports(
    params: &[carina_core::parser::InferredExportParam],
) -> carina_core::validation::ExportsCheckedPreApply<carina_core::parser::InferredExportParam> {
    carina_core::validation::check_export_values_pre_apply(
        params,
        &carina_core::binding_index::ResolvedBindings::default(),
        &ProviderContext::default(),
    )
    .expect("test exports must pass the secret boundary")
}

fn saved_provider_configs(
    configs: Vec<ProviderConfig>,
) -> carina_core::parser::SavedProviderConfigs {
    let parsed = carina_core::parser::InferredFile {
        providers: configs,
        ..Default::default()
    };
    let resolved = carina_core::parser::resolve_provider_attributes_with_remote(
        &parsed,
        &HashMap::new(),
        &ProviderContext::default(),
    )
    .expect("test provider configs must resolve");
    carina_core::parser::SavedProviderConfigs::from(&resolved)
}

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
    fn name(&self) -> &str {
        "test"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: carina_core::provider::ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let key = (id.to_string(), identifier.as_str().to_string());
        let result = self
            .read_results
            .get(&key)
            .cloned()
            .unwrap_or_else(|| panic!("missing read state for {:?}", key));
        Box::pin(async move {
            result
                .map(fixture_read_outcome)
                .map_err(ProviderError::internal)
        })
    }

    fn read_data_source(
        &self,
        _resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("unexpected read_data_source")) })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: carina_core::provider::CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        Box::pin(
            async move { Err(attempt.untracked(ProviderError::internal("unexpected create"))) },
        )
    }

    fn resume_create(
        &self,
        _request: carina_core::provider::ResumeCreateRequest,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
    > {
        Box::pin(async { panic!("unexpected resume_create") })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("unexpected update")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("unexpected delete")) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn refresh_pending_states_updates_saved_state_from_provider_read() {
    let resource = Resource::with_provider("aws", "s3.Bucket", "bucket", None);
    let id = resource.id.clone();
    let identifier = "bucket-123";

    let mut current_states = HashMap::from([(
        id.clone(),
        Current::from_state(
            State::existing(
                id.clone(),
                HashMap::from([(
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("before".into())),
                )]),
            )
            .with_identifier(identifier),
        ),
    )]);
    let provider = TestProvider::with_read_state(
        &id,
        identifier,
        State::existing(
            id.clone(),
            HashMap::from([(
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("after".into())),
            )]),
        )
        .with_identifier(identifier),
    );

    let mut pending_refreshes = HashMap::new();
    let pending_identifier =
        AppliedIdentifier::new(identifier).expect("fixture identifier is non-empty");
    queue_state_refresh(&mut pending_refreshes, &id, &pending_identifier);
    let failed_refreshes =
        refresh_pending_states(&provider, &mut current_states, &pending_refreshes).await;

    let mut existing_state = StateFile::new();
    existing_state
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "bucket", "aws", identifier)
                .expect("fixture identifier is non-empty")
                .with_attribute("status", json!("before")),
        )
        .expect("test state setup must be valid");

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file: existing_state,
            sorted_resources: &[resolved(resource)],
            runtime_synthesized_resources: &[],
            managed_current: &current_states,
            applied_states: &HashMap::new(),
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &failed_refreshes,
            schemas: &SchemaRegistry::new(),
        },
        None,
    )
    .unwrap();

    let saved_resource = saved.find_resource("aws", "s3.Bucket", "bucket").unwrap();
    assert_eq!(
        saved_resource
            .persisted_attributes()
            .expect("saved resource has applied attributes")
            .get("status"),
        carina_core::value::json_to_dsl_value(&json!("after"))
    );
}

#[tokio::test]
async fn refresh_pending_states_removes_not_found_resource_from_saved_state() {
    let resource = Resource::with_provider("aws", "s3.Bucket", "bucket", None);
    let id = resource.id.clone();
    let identifier = "bucket-123";

    let mut current_states = HashMap::from([(
        id.clone(),
        Current::from_state(
            State::existing(
                id.clone(),
                HashMap::from([(
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("before".into())),
                )]),
            )
            .with_identifier(identifier),
        ),
    )]);
    let provider = TestProvider::with_read_state(&id, identifier, State::not_found(id.clone()));

    let mut pending_refreshes = HashMap::new();
    let pending_identifier =
        AppliedIdentifier::new(identifier).expect("fixture identifier is non-empty");
    queue_state_refresh(&mut pending_refreshes, &id, &pending_identifier);
    let failed_refreshes =
        refresh_pending_states(&provider, &mut current_states, &pending_refreshes).await;

    let mut existing_state = StateFile::new();
    existing_state
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "bucket", "aws", identifier)
                .expect("fixture identifier is non-empty")
                .with_attribute("status", json!("before")),
        )
        .expect("test state setup must be valid");

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file: existing_state,
            sorted_resources: &[resolved(resource)],
            runtime_synthesized_resources: &[],
            managed_current: &current_states,
            applied_states: &HashMap::new(),
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &failed_refreshes,
            schemas: &SchemaRegistry::new(),
        },
        None,
    )
    .unwrap();

    assert!(saved.find_resource("aws", "s3.Bucket", "bucket").is_none());
}

#[tokio::test]
async fn refresh_pending_states_does_not_overwrite_with_stale_snapshot_when_refresh_fails() {
    let resource = Resource::with_provider("aws", "s3.Bucket", "bucket", None);
    let id = resource.id.clone();
    let identifier = "bucket-123";

    let mut current_states = HashMap::from([(
        id.clone(),
        Current::from_state(
            State::existing(
                id.clone(),
                HashMap::from([(
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("stale-current".into())),
                )]),
            )
            .with_identifier(identifier),
        ),
    )]);
    let provider = TestProvider::with_read_error(&id, identifier, "read failed");

    let mut pending_refreshes = HashMap::new();
    let pending_identifier =
        AppliedIdentifier::new(identifier).expect("fixture identifier is non-empty");
    queue_state_refresh(&mut pending_refreshes, &id, &pending_identifier);
    let failed_refreshes =
        refresh_pending_states(&provider, &mut current_states, &pending_refreshes).await;

    let mut existing_state = StateFile::new();
    existing_state
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "bucket", "aws", identifier)
                .expect("fixture identifier is non-empty")
                .with_attribute("status", json!("saved")),
        )
        .expect("test state setup must be valid");

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file: existing_state,
            sorted_resources: &[resolved(resource)],
            runtime_synthesized_resources: &[],
            managed_current: &current_states,
            applied_states: &HashMap::new(),
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &failed_refreshes,
            schemas: &SchemaRegistry::new(),
        },
        None,
    )
    .unwrap();

    let saved_resource = saved.find_resource("aws", "s3.Bucket", "bucket").unwrap();
    assert_eq!(
        saved_resource
            .persisted_attributes()
            .expect("saved resource has applied attributes")
            .get("status"),
        carina_core::value::json_to_dsl_value(&json!("saved"))
    );
}

#[test]
fn plan_file_serde_round_trip() {
    use carina_core::plan::Plan;

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(
        Resource::with_provider("aws", "s3.Bucket", "my-bucket", None).with_attribute(
            "bucket",
            Value::Concrete(ConcreteValue::String("my-bucket".into())),
        ),
    )));
    plan.add(Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "aws",
            "s3.Bucket",
            "old-bucket",
            None,
        )),
        identifier: crate::fixture_applied_identifier("old-bucket"),
        generation: carina_core::effect::EffectGeneration::Current,
        directives: Directives::default(),
        binding: None,
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });

    let sorted_resources = vec![
        Resource::with_provider("aws", "s3.Bucket", "my-bucket", None).with_attribute(
            "bucket",
            Value::Concrete(ConcreteValue::String("my-bucket".into())),
        ),
    ];

    let data_source_id =
        ResourceId::with_provider_identity("aws", "sts.CallerIdentity", "caller", None);
    let absent_id = ResourceId::with_provider_identity("aws", "s3.Bucket", "absent-bucket", None);
    let present_id = ResourceId::with_provider_identity("aws", "s3.Bucket", "present-bucket", None);
    let missing_id = ResourceId::with_provider_identity("aws", "s3.Bucket", "missing-bucket", None);
    let current_states = vec![
        CurrentStateEntry::managed(absent_id.clone(), State::not_found(absent_id.clone())),
        CurrentStateEntry::managed(
            present_id.clone(),
            State::existing(present_id.clone(), HashMap::new())
                .with_identifier("present-provider-id"),
        ),
        CurrentStateEntry::managed(
            missing_id.clone(),
            State::existing(missing_id.clone(), HashMap::new()),
        ),
    ];
    let data_source = DataSource::with_provider("aws", "sts.CallerIdentity", "caller", None);

    let plan_file = record_test_plan(
        PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: "0.1.0".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            source_path: "example.crn".to_string(),
            state_lineage: Some("test-lineage".to_string()),
            state_serial: Some(1),
            provider_configs: saved_provider_configs(vec![ProviderConfig {
                name: "aws".to_string(),
                attributes: IndexMap::from([(
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::String("aws.Region.ap_northeast_1".into())),
                )]),
                default_tags: IndexMap::new(),
                source: Some("file:///aws.wasm".parse().unwrap()),
                version: None,
                revision: None,
                unresolved_attributes: IndexMap::new(),
                binding: None,
            }]),
            backend_config: Some(BackendConfig {
                backend_type: "s3".to_string(),
                attributes: HashMap::from([
                    (
                        "bucket".to_string(),
                        Value::Concrete(ConcreteValue::String("my-state".into())),
                    ),
                    (
                        "key".to_string(),
                        Value::Concrete(ConcreteValue::String("prod/carina.state".into())),
                    ),
                ]),
                attribute_spans: HashMap::new(),
            }),
            plan,
            sorted_resources: sorted_resources.clone(),
            unresolved_resources: sorted_resources,
            current_states,
            compositions: vec![],
            data_sources: vec![data_source],
            data_source_origins: vec![],
            data_source_values: vec![SavedDataSourceValue {
                id: data_source_id.clone(),
                value: DataSourceValue::read(State::existing(
                    data_source_id.clone(),
                    HashMap::new(),
                )),
            }],
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: vec![],
        },
        &["aws"],
    );

    let json = serde_json::to_string_pretty(&plan_file).unwrap();
    let wire: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(wire["version"], serde_json::json!(26));
    assert_eq!(wire["current_states"][0]["kind"], "managed");
    assert_eq!(wire["current_states"][0]["current"]["status"], "absent");
    assert_eq!(wire["current_states"][1]["kind"], "managed");
    assert_eq!(wire["current_states"][1]["current"]["status"], "present");
    assert_eq!(wire["current_states"][2]["kind"], "managed");
    assert_eq!(
        wire["current_states"][2]["current"]["status"],
        "missing_identifier"
    );
    assert_eq!(wire["current_states"].as_array().unwrap().len(), 3);
    assert_eq!(wire["data_source_values"][0]["value"]["kind"], "read");
    let mut invalid_present = wire.clone();
    invalid_present["current_states"][1]["current"]["state"]["identifier"] =
        serde_json::Value::Null;
    let error = serde_json::from_value::<PlanFile>(invalid_present)
        .expect_err("a present tag must not bypass provider-identifier validation");
    assert!(
        error
            .to_string()
            .contains("does not match classified state"),
        "{error}"
    );
    let deserialized: PlanFile = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.version, PlanFile::CURRENT_VERSION);
    assert_eq!(deserialized.carina_version, "0.1.0");
    assert_eq!(deserialized.source_path, "example.crn");
    assert_eq!(deserialized.state_lineage, Some("test-lineage".to_string()));
    assert_eq!(deserialized.state_serial, Some(1));
    assert_eq!(deserialized.provider_configs.as_provider_configs().len(), 1);
    assert_eq!(
        deserialized.provider_configs.as_provider_configs()[0].name,
        "aws"
    );
    assert!(deserialized.backend_config.is_some());
    assert_eq!(deserialized.plan.effects().len(), 2);
    assert_eq!(deserialized.sorted_resources.len(), 1);
    assert_eq!(deserialized.unresolved_resources.len(), 1);
    let reloaded = deserialized
        .reload_in_flight_from_locked_state(None)
        .expect("a plan without in-flight markers reloads");
    match reloaded.current_states() {
        [
            ReloadedCurrentStateEntry::Managed {
                id: loaded_absent_id,
                current: loaded_absent,
            },
            ReloadedCurrentStateEntry::Managed {
                id: loaded_present_id,
                current: loaded_present,
            },
            ReloadedCurrentStateEntry::Managed {
                id: loaded_missing_id,
                current: loaded_missing,
            },
        ] => {
            assert!(matches!(loaded_absent.as_ref(), Current::Absent(_absent)));
            let Current::Present(present) = loaded_present.as_ref() else {
                panic!("present fixture did not deserialize as Present");
            };
            assert!(matches!(
                loaded_missing.as_ref(),
                Current::MissingIdentifier(_missing)
            ));
            assert_eq!(loaded_absent_id, &absent_id);
            assert_eq!(loaded_present_id, &present_id);
            assert_eq!(present.identifier().as_str(), "present-provider-id");
            assert_eq!(loaded_missing_id, &missing_id);
        }
        entries => panic!("unexpected typed current-state entries: {entries:?}"),
    }
}

#[test]
#[ignore = "requires provider binary for schema-based prefix resolution"]
fn test_resolve_attr_prefixes_extracts_prefix_and_generates_name() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource.set_attr(
        "bucket_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-".into())),
    );

    let mut resources = vec![resource];
    resolve_attr_prefixes(&mut resources).unwrap();

    // bucket_name_prefix should be removed
    assert!(!resources[0].attributes.contains_key("bucket_name_prefix"));

    // bucket_name should be generated with the prefix
    let bucket_name = match resources[0].get_attr("bucket_name").unwrap() {
        Value::Concrete(ConcreteValue::String(s)) => s.clone(),
        _ => panic!("expected String"),
    };
    assert!(bucket_name.expose_plaintext().starts_with("my-app-"));
    assert_eq!(bucket_name.expose_plaintext().len(), "my-app-".len() + 8); // prefix + 8 hex chars

    // prefixes map should have the entry
    assert_eq!(
        resources[0].prefixes.get("bucket_name"),
        Some(&"my-app-".to_string())
    );
}

#[test]
fn test_resolve_attr_prefixes_leaves_non_matching_prefix_alone() {
    // If base attr doesn't exist in schema, leave _prefix as-is
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource.set_attr(
        "nonexistent_attr_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("some-value".into())),
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
#[ignore = "requires provider binary for schema-based prefix resolution"]
fn test_resolve_attr_prefixes_errors_when_both_prefix_and_attr_specified() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource.set_attr(
        "bucket_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-".into())),
    );
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-actual-bucket".into())),
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
#[ignore = "requires provider binary for schema-based prefix resolution"]
fn test_resolve_attr_prefixes_errors_on_empty_prefix() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource.set_attr(
        "bucket_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("".into())),
    );

    let mut resources = vec![resource];
    let result = resolve_attr_prefixes(&mut resources);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("cannot be empty"));
}

#[test]
#[ignore = "requires provider binary for schema-based name resolution"]
fn test_resolve_names_handles_block_name_before_prefix() {
    // resolve_names should first resolve block names, then resolve attr prefixes
    let mut resource = Resource::with_provider("awscc", "ec2.ipam", "test-ipam", None);
    resource.set_attr(
        "operating_region".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![(
                    "region_name".to_string(),
                    Value::Concrete(ConcreteValue::String("us-east-1".into())),
                )]
                .into_iter()
                .collect(),
            ),
        )])),
    );

    let mut resources = vec![resource];
    resolve_names(&mut resources).unwrap();

    // operating_region should be renamed to operating_regions
    assert!(resources[0].attributes.contains_key("operating_regions"));
    assert!(!resources[0].attributes.contains_key("operating_region"));
}

#[test]
fn test_reconcile_prefixed_names_reuses_state_name_when_prefix_matches() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-temporary".into())),
    );

    let mut state_file = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "test-bucket", "awscc");
    rs.set_persisted_attribute(
        "bucket_name".to_string(),
        serde_json::json!("my-app-existing1"),
    );
    rs.prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());
    state_file
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let mut resources = vec![resource];
    reconcile_prefixed_names(&mut resources, &Some(state_file));

    // Should reuse the state name, not the temporary one
    assert_eq!(
        resources[0].get_attr("bucket_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "my-app-existing1".into()
        )))
    );
}

#[test]
fn test_reconcile_prefixed_names_generates_new_name_when_prefix_changes() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource
        .prefixes
        .insert("bucket_name".to_string(), "new-prefix-".to_string());
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("new-prefix-abcd1234".into())),
    );

    let mut state_file = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "test-bucket", "awscc");
    rs.set_persisted_attribute(
        "bucket_name".to_string(),
        serde_json::json!("old-prefix-existing1"),
    );
    rs.prefixes
        .insert("bucket_name".to_string(), "old-prefix-".to_string());
    state_file
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let mut resources = vec![resource];
    reconcile_prefixed_names(&mut resources, &Some(state_file));

    // Should keep the newly generated name since prefix changed
    assert_eq!(
        resources[0].get_attr("bucket_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "new-prefix-abcd1234".into()
        )))
    );
}

#[test]
fn test_reconcile_prefixed_names_keeps_generated_name_when_no_state() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-abcd1234".into())),
    );

    let mut resources = vec![resource];
    reconcile_prefixed_names(&mut resources, &None);

    // No state, so keep the generated name
    assert_eq!(
        resources[0].get_attr("bucket_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "my-app-abcd1234".into()
        )))
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
    plan.add(Effect::Create(resolved(Resource::new("s3.Bucket", "test"))));
    let has_changes = plan.mutation_count() > 0;
    assert!(has_changes);
}

#[test]
fn test_detailed_exitcode_read_only_no_changes() {
    let mut plan = Plan::new();
    plan.add(Effect::Read {
        resource: resolved_data_source(DataSource::new("sts.caller_identity", "identity")),
    });
    let has_changes = plan.mutation_count() > 0;
    assert!(!has_changes);
}

#[test]
fn test_detailed_exitcode_wait_only_no_changes() {
    use std::time::Duration;

    use carina_core::wait::ManagedWaitTarget;
    use carina_core::wait::predicate::{AttrPath, WaitPredicate};

    let mut plan = Plan::new();
    plan.add(Effect::Wait {
        identity: "cert_issued".into(),
        target_id: ManagedWaitTarget::for_test(&resolved(Resource::new("acm.Certificate", "cert"))),
        until: WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("ISSUED".into())),
        )
        .unwrap(),
        until_surface: "cert.status == ISSUED".to_string(),
        timeout: Duration::from_secs(60),
        interval: Duration::from_secs(5),
        explicit_dependencies: HashSet::new(),
    });

    let has_changes = plan.mutation_count() > 0;
    assert!(!has_changes);
}

fn make_awscc_provider(region_dsl: &str) -> ProviderConfig {
    let mut attrs = IndexMap::new();
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String(region_dsl.to_string().into())),
    );
    ProviderConfig {
        name: "awscc".to_string(),
        attributes: attrs,
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }
}

#[test]
#[ignore = "requires provider binary for identity_attributes"]
fn test_anonymous_id_different_regions_produce_different_identifiers() {
    // Two anonymous ec2_vpc resources with same cidr_block but different provider regions
    let mut r1 = Resource::pending_with_provider("awscc", "ec2.Vpc", None);
    r1.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );

    let mut r2 = Resource::pending_with_provider("awscc", "ec2.Vpc", None);
    r2.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
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
    assert!(
        !resources_east[0]
            .id
            .identity_str()
            .expect("resolved identity")
            .is_empty()
    );
    assert!(
        !resources_west[0]
            .id
            .identity_str()
            .expect("resolved identity")
            .is_empty()
    );
    // They must be different because providers have different regions
    assert_ne!(
        resources_east[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        resources_west[0]
            .id
            .identity_str()
            .expect("resolved identity")
    );
}

#[test]
#[ignore = "requires provider binary for identity_attributes"]
fn test_anonymous_id_same_region_same_create_only_collides() {
    // Two anonymous ec2_vpc resources with same cidr_block and same provider region -> collision
    let mut r1 = Resource::pending_with_provider("awscc", "ec2.Vpc", None);
    r1.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );

    let mut r2 = Resource::pending_with_provider("awscc", "ec2.Vpc", None);
    r2.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );

    let providers = vec![make_awscc_provider("awscc.Region.us_east_1")];
    let mut resources = vec![r1, r2];
    let result = compute_anonymous_identifiers(&mut resources, &providers);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("collision"));
}

#[test]
#[ignore = "requires provider binary for identity_attributes"]
fn test_anonymous_id_different_create_only_same_region_no_collision() {
    // Two anonymous ec2_vpc resources with different cidr_block in same provider region -> no collision
    let mut r1 = Resource::pending_with_provider("awscc", "ec2.Vpc", None);
    r1.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );

    let mut r2 = Resource::pending_with_provider("awscc", "ec2.Vpc", None);
    r2.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
    );

    let providers = vec![make_awscc_provider("awscc.Region.us_east_1")];
    let mut resources = vec![r1, r2];
    compute_anonymous_identifiers(&mut resources, &providers).unwrap();

    assert!(
        !resources[0]
            .id
            .identity_str()
            .expect("resolved identity")
            .is_empty()
    );
    assert!(
        !resources[1]
            .id
            .identity_str()
            .expect("resolved identity")
            .is_empty()
    );
    assert_ne!(
        resources[0].id.identity_str().expect("resolved identity"),
        resources[1].id.identity_str().expect("resolved identity")
    );
}

#[test]
fn test_anonymous_id_named_resources_are_skipped() {
    // Named resources should not be processed by compute_anonymous_identifiers
    let mut r1 = Resource::with_provider("awscc", "ec2.Vpc", "my_vpc", None);
    r1.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );

    let providers = vec![make_awscc_provider("awscc.Region.us_east_1")];
    let mut resources = vec![r1];
    compute_anonymous_identifiers(&mut resources, &providers).unwrap();

    // Name should remain unchanged
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        "my_vpc"
    );
}

#[test]
fn test_find_state_bucket_resource_matching_type() {
    let parsed = ParsedFile {
        providers: vec![],
        backend: None,
        resources: vec![
            Resource::with_provider("aws", "s3.Bucket", "my-bucket", None).with_attribute(
                "bucket",
                Value::Concrete(ConcreteValue::String("my-bucket".into())),
            ),
        ],
        data_sources: vec![],
        compositions: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![],
        attribute_params: vec![],
        export_params: vec![],
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        warnings: vec![],
        deferred_for_expressions: vec![],
        expansion_trace: carina_core::resource::ExpansionTrace::new(),
    };

    // Matching resource type
    assert!(
        parsed
            .find_resource_by_attr("s3.Bucket", "bucket", "my-bucket")
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
            .find_resource_by_attr("s3.Bucket", "bucket", "other-bucket")
            .is_none()
    );
}

#[test]
#[ignore = "requires provider binary for resource type validation"]
fn validate_data_source_without_read_keyword_errors() {
    let resource = Resource::with_provider("aws", "sts.caller_identity", "identity", None);
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

// carina#3181: the former `validate_data_source_with_read_keyword_passes`
// test is obsolete — `validate_resources` takes `&[Resource]`, so a
// data source cannot be passed into the managed-resource validation path
// at all (the typestate split enforces this).

#[test]
#[ignore = "requires provider binary for resource type validation"]
fn validate_regular_resource_without_read_keyword_passes() {
    let resource = Resource::with_provider("aws", "s3.Bucket", "my-bucket", None)
        .with_attribute(
            "bucket",
            Value::Concrete(ConcreteValue::String("my-bucket".into())),
        )
        .with_attribute(
            "region",
            Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
        );
    let result = validate_resources(&[resource]);
    assert!(
        result.is_ok(),
        "Regular resource without read should pass: {:?}",
        result
    );
}

// carina#3181: the former `destroy_plan_excludes_data_sources` test is
// obsolete. The destroy path operates on `&[Resource]`, and data
// sources are a distinct typestate (`DataSource`) — they can no longer
// be mixed into the destroy candidate list, so "data sources are
// excluded from destroy" is enforced by the type system rather than a
// runtime `is_data_source()` filter.

/// Simulate the full plan-verify cycle for an anonymous S3 bucket with prefix.
///
/// This test reproduces the bug from issue #535 where after a successful apply,
/// running plan again shows the resource as needing to be created because the
/// anonymous resource identifier changes between runs.
#[test]
#[ignore = "requires provider binary for schema-based prefix resolution"]
fn test_plan_verify_idempotency_anonymous_resource_with_prefix() {
    // --- First run (apply) ---
    // 1. Parse: anonymous resource with bucket_name_prefix
    let mut resource_run1 = Resource::pending_with_provider("awscc", "s3.Bucket", None);
    resource_run1.set_attr(
        "bucket_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-".into())),
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
    let run1_bucket_name = match resources_run1[0].get_attr("bucket_name") {
        Some(Value::Concrete(ConcreteValue::String(s))) => s.clone(),
        _ => panic!("bucket_name should be a string"),
    };
    assert!(
        run1_bucket_name.expose_plaintext().starts_with("my-app-"),
        "bucket_name should start with prefix"
    );

    // 3. compute_anonymous_identifiers
    compute_anonymous_identifiers(&mut resources_run1, &providers).unwrap();
    let run1_name = resources_run1[0]
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();
    assert!(
        !run1_name.is_empty(),
        "Anonymous identifier should be assigned"
    );

    // 4. Simulate state after apply
    let applied_state = State::existing(
        resources_run1[0].id.clone(),
        vec![(
            "bucket_name".to_string(),
            Value::Concrete(ConcreteValue::String(run1_bucket_name.clone())),
        )]
        .into_iter()
        .collect(),
    )
    .with_identifier("my-app-abcd1234");

    let resource_state = ResourceState::from_provider_state_for_resource_and_schema(
        &ResolvedResource::new(resources_run1[0].clone()),
        &applied_state,
        None,
        None,
        RowBasis::Applied(&AcceptedRequest::create_for_test()),
        None,
    )
    .unwrap();

    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(resource_state)
        .expect("test state setup must be valid");

    // --- Second run (plan-verify) ---
    // 1. Parse again: same anonymous resource with bucket_name_prefix
    let mut resource_run2 = Resource::pending_with_provider("awscc", "s3.Bucket", None);
    resource_run2.set_attr(
        "bucket_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-".into())),
    );

    // 2. resolve_names (resolve_attr_prefixes) - generates NEW random suffix
    let mut resources_run2 = vec![resource_run2];
    resolve_names(&mut resources_run2).unwrap();

    // The random suffix is different on each run (highly probable with 8 hex chars)

    // 3. compute_anonymous_identifiers - should produce SAME identifier
    compute_anonymous_identifiers(&mut resources_run2, &providers).unwrap();
    let run2_name = resources_run2[0]
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();

    assert_eq!(
        run1_name, run2_name,
        "Anonymous identifier should be stable across runs (prefix-based hash)"
    );

    // 4. reconcile_prefixed_names - should restore original bucket_name from state
    reconcile_prefixed_names(&mut resources_run2, &Some(state_file.clone()));

    let reconciled_bucket_name = match resources_run2[0].get_attr("bucket_name") {
        Some(Value::Concrete(ConcreteValue::String(s))) => s.clone(),
        _ => panic!("bucket_name should be a string after reconciliation"),
    };
    assert_eq!(
        reconciled_bucket_name, run1_bucket_name,
        "Prefix reconciliation should restore original bucket_name from state"
    );

    // 5. The typed state loader should find the resource identifier.
    let identifier = match state_file.build_state_for_resource(&resources_run2[0].id) {
        carina_core::resource::Current::Present(present) => {
            Some(present.identifier().as_str().to_owned())
        }
        carina_core::resource::Current::Absent(_state) => None,
        carina_core::resource::Current::MissingIdentifier(_state) => None,
        carina_core::resource::Current::CreateInFlight(_pending) => None,
    };
    assert_eq!(
        identifier,
        Some("my-app-abcd1234".to_string()),
        "Should find identifier in state for plan-verify (issue #535)"
    );
}

/// Simulate plan-verify for an anonymous IAM role with role_name_prefix and path.
/// This matches the exact failure case from issue #535.
#[test]
#[ignore = "requires provider binary for schema-based prefix resolution"]
fn test_plan_verify_idempotency_iam_role_with_prefix_and_path() {
    let providers = vec![make_awscc_provider("awscc.Region.ap_northeast_1")];

    // --- First run ---
    let mut resource_run1 = Resource::pending_with_provider("awscc", "iam.role", None);
    resource_run1.set_attr(
        "role_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("carina-acc-test-".into())),
    );
    resource_run1.set_attr(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String("/carina/acceptance-test/".into())),
    );
    resource_run1.set_attr(
        "assume_role_policy_document".to_string(),
        Value::Concrete(ConcreteValue::Map(
            vec![(
                "version".to_string(),
                Value::Concrete(ConcreteValue::String("2012-10-17".into())),
            )]
            .into_iter()
            .collect(),
        )),
    );

    let mut resources_run1 = vec![resource_run1];
    resolve_names(&mut resources_run1).unwrap();
    compute_anonymous_identifiers(&mut resources_run1, &providers).unwrap();
    let run1_name = resources_run1[0]
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();

    // Simulate state after apply
    let run1_role_name = match resources_run1[0].get_attr("role_name") {
        Some(Value::Concrete(ConcreteValue::String(s))) => s.clone(),
        _ => panic!("role_name should be set after prefix resolution"),
    };
    let applied_state = State::existing(
        resources_run1[0].id.clone(),
        vec![
            (
                "role_name".to_string(),
                Value::Concrete(ConcreteValue::String(run1_role_name.clone())),
            ),
            (
                "path".to_string(),
                Value::Concrete(ConcreteValue::String("/carina/acceptance-test/".into())),
            ),
        ]
        .into_iter()
        .collect(),
    )
    .with_identifier(run1_role_name.expose_plaintext());

    let resource_state = ResourceState::from_provider_state_for_resource_and_schema(
        &ResolvedResource::new(resources_run1[0].clone()),
        &applied_state,
        None,
        None,
        RowBasis::Applied(&AcceptedRequest::create_for_test()),
        None,
    )
    .unwrap();
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(resource_state)
        .expect("test state setup must be valid");

    // --- Second run ---
    let mut resource_run2 = Resource::pending_with_provider("awscc", "iam.role", None);
    resource_run2.set_attr(
        "role_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("carina-acc-test-".into())),
    );
    resource_run2.set_attr(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String("/carina/acceptance-test/".into())),
    );
    resource_run2.set_attr(
        "assume_role_policy_document".to_string(),
        Value::Concrete(ConcreteValue::Map(
            vec![(
                "version".to_string(),
                Value::Concrete(ConcreteValue::String("2012-10-17".into())),
            )]
            .into_iter()
            .collect(),
        )),
    );

    let mut resources_run2 = vec![resource_run2];
    resolve_names(&mut resources_run2).unwrap();
    compute_anonymous_identifiers(&mut resources_run2, &providers).unwrap();
    let run2_name = resources_run2[0]
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();

    assert_eq!(
        run1_name, run2_name,
        "IAM role anonymous identifier should be stable across runs"
    );

    reconcile_prefixed_names(&mut resources_run2, &Some(state_file.clone()));

    let identifier = match state_file.build_state_for_resource(&resources_run2[0].id) {
        carina_core::resource::Current::Present(present) => {
            Some(present.identifier().as_str().to_owned())
        }
        carina_core::resource::Current::Absent(_state) => None,
        carina_core::resource::Current::MissingIdentifier(_state) => None,
        carina_core::resource::Current::CreateInFlight(_pending) => None,
    };
    assert!(
        identifier.is_some(),
        "Should find IAM role identifier in state for plan-verify (issue #535)"
    );
}

#[tokio::test]
async fn detect_drift_errors_when_resource_missing_from_planned_states() {
    let resource = Resource::with_provider("aws", "s3.Bucket", "my-bucket", None);
    let id = resource.id.clone();

    // Provider returns a non-existing state (identifier is None since no planned state)
    let provider = TestProvider::with_read_state(&id, "", State::not_found(id.clone()));

    // planned_states is empty - resource is missing
    let planned_states = Vec::new();

    let result = detect_drift(
        &[resource],
        &planned_states,
        &MovedOrigins::from_plan(&Plan::new()),
        &provider,
        None,
        &carina_core::schema::SchemaRegistry::new(),
        ApplyTarget::Project(std::path::Path::new(".")),
    )
    .await;

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
    let resource = Resource::with_provider("aws", "s3.Bucket", "my-bucket", None);
    let id = resource.id.clone();
    let identifier = "my-bucket";

    let state = State::existing(
        id.clone(),
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("my-bucket".into())),
        )]),
    )
    .with_identifier(identifier);

    let provider = TestProvider::with_read_state(&id, identifier, state.clone());
    let planned_states = vec![ReloadedCurrentStateEntry::managed(id.clone(), state)];

    let result = detect_drift(
        &[resource],
        &planned_states,
        &MovedOrigins::from_plan(&Plan::new()),
        &provider,
        None,
        &carina_core::schema::SchemaRegistry::new(),
        ApplyTarget::Project(std::path::Path::new(".")),
    )
    .await
    .expect("drift check succeeds");

    assert!(result.drift_messages().is_none(), "Should detect no drift");
}

#[tokio::test]
async fn detect_drift_restores_legacy_unlisted_write_only_secret_from_state_file() {
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("secret-value".into()),
    ))));
    let persisted_secret = PersistedValue::encode_desired_value(&secret, None)
        .expect("secret is persistable")
        .as_json()
        .clone();
    let resource = Resource::with_provider("mock", "test.resource", "r1", None)
        .with_attribute("name", Value::Concrete(ConcreteValue::String("r1".into())))
        .with_attribute("password", secret);
    let id = resource.id.clone();
    let identifier = "provider-id";
    let row = ResourceState::try_applied("test.resource", "r1", "mock", identifier)
        .expect("fixture identifier is non-empty")
        .with_attribute("name", json!("r1"))
        .with_attribute("password", persisted_secret);
    assert!(
        row.write_only_attributes().is_empty(),
        "fixture must model a legacy-shaped row without write-only metadata"
    );
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(row)
        .expect("fixture row should be valid");
    let planned = match state_file.build_state_for_resource(&id) {
        Current::Present(present) => present.into_parts().0,
        other => panic!("fixture must load as present, got {other:?}"),
    };
    let provider = TestProvider::with_read_state(
        &id,
        identifier,
        State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("r1".into())),
            )]),
        )
        .with_identifier(identifier),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let result = detect_drift(
        &[resource],
        &[ReloadedCurrentStateEntry::managed(id.clone(), planned)],
        &MovedOrigins::from_plan(&Plan::new()),
        &provider,
        Some(&state_file),
        &schemas,
        ApplyTarget::Project(std::path::Path::new(".")),
    )
    .await
    .expect("drift check succeeds");

    assert_eq!(result.drift_messages(), None);
}

#[tokio::test]
async fn detect_drift_uses_identifier_carried_by_present_current() {
    let resource = Resource::with_provider("aws", "s3.Bucket", "my-bucket", None);
    let id = resource.id.clone();
    let identifier =
        AppliedIdentifier::new("typed-provider-id").expect("fixture identifier is non-empty");
    let planned = State::existing(id.clone(), HashMap::new());
    let actual = State::existing(id.clone(), HashMap::new()).with_identifier("typed-provider-id");
    let provider = TestProvider::with_read_state(&id, "typed-provider-id", actual);
    let planned_states = vec![ReloadedCurrentStateEntry::managed_current(
        id.clone(),
        carina_core::resource::fixture_present_current(planned, identifier),
    )];

    let result = detect_drift(
        &[resource],
        &planned_states,
        &MovedOrigins::from_plan(&Plan::new()),
        &provider,
        None,
        &carina_core::schema::SchemaRegistry::new(),
        ApplyTarget::Project(std::path::Path::new(".")),
    )
    .await
    .expect("typed present identifier must drive the provider read");

    assert!(result.drift_messages().is_none());
}

#[tokio::test]
async fn detect_drift_returns_messages_when_drift_detected() {
    let resource = Resource::with_provider("aws", "s3.Bucket", "my-bucket", None);
    let id = resource.id.clone();
    let identifier = "my-bucket";

    let planned = State::existing(
        id.clone(),
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("my-bucket".into())),
        )]),
    )
    .with_identifier(identifier);

    // Actual state has different attribute value
    let actual = State::existing(
        id.clone(),
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("changed-bucket".into())),
        )]),
    )
    .with_identifier(identifier);

    let provider = TestProvider::with_read_state(&id, identifier, actual);
    let planned_states = vec![ReloadedCurrentStateEntry::managed(id.clone(), planned)];

    let result = detect_drift(
        &[resource],
        &planned_states,
        &MovedOrigins::from_plan(&Plan::new()),
        &provider,
        None,
        &carina_core::schema::SchemaRegistry::new(),
        ApplyTarget::Project(std::path::Path::new(".")),
    )
    .await
    .expect("drift check succeeds");

    assert!(result.drift_messages().is_some(), "Should detect drift");
    let msgs = result.drift_messages().unwrap();
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
    state_file
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "keep-bucket", "aws", "keep-bucket")
                .expect("fixture identifier is non-empty")
                .with_attribute("bucket", json!("keep-bucket")),
        )
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "removed-bucket", "aws", "removed-bucket")
                .expect("fixture identifier is non-empty")
                .with_attribute("bucket", json!("removed-bucket")),
        )
        .expect("test state setup must be valid");

    // Config only has "keep-bucket" -- "removed-bucket" was deleted from .crn
    let desired = vec![
        Resource::with_provider("aws", "s3.Bucket", "keep-bucket", None).with_attribute(
            "bucket",
            Value::Concrete(ConcreteValue::String("keep-bucket".into())),
        ),
    ];

    let desired_ids: HashSet<ResourceId> = desired.iter().map(|r| r.id.clone()).collect();

    // Build managed current state from desired resources (simulates the provider read loop)
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &desired {
        current_states.insert(
            resource.id.clone(),
            State::existing(
                resource.id.clone(),
                HashMap::from([(
                    "bucket".to_string(),
                    Value::Concrete(ConcreteValue::String("keep-bucket".into())),
                )]),
            )
            .with_identifier("keep-bucket"),
        );
    }

    // Orphans remain in their typed collection rather than being duplicated
    // into the desired-resource current-state map.
    let orphans = state_file.build_orphans(&desired_ids).into_plan_orphans();

    let directives_map = state_file.build_directives();
    let saved_attrs = state_file.build_saved_attrs().lift(&SchemaRegistry::new());
    let prev_explicit = state_file.build_explicit();

    let plan = create_plan(
        &ResolvedResource::collect_resolved(desired.clone()).unwrap(),
        &[],
        &carina_core::provider::ProviderRouter::new(),
        carina_core::differ::PlanPriorStates::new(
            &crate::plan_input_current_from_states(
                current_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &[],
            ),
            &orphans,
        ),
        &directives_map,
        &SchemaRegistry::new(),
        &saved_attrs,
        &prev_explicit,
        &[],
    )
    .expect("test provider hints should succeed");

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
            assert_eq!(id.identity_str(), "removed-bucket");
            assert_eq!(identifier.as_str(), "removed-bucket");
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
    async fn read_state(&self) -> carina_state::BackendResult<Option<LoadedState>> {
        Ok(Some(LoadedState::Pristine(StateFile::new())))
    }
    async fn read_state_bytes(
        &self,
    ) -> carina_state::BackendResult<Option<carina_state::RawState>> {
        carina_state::RawState::from_state(&StateFile::new(), "mock backend state").map(Some)
    }
    async fn write_state(&self, _state: &StateFile) -> carina_state::BackendResult<()> {
        if self.write_state_fails {
            Err(BackendError::Io("simulated write failure".to_string()))
        } else {
            Ok(())
        }
    }
    async fn acquire_lock(&self, operation: &str) -> carina_state::BackendResult<LockInfo> {
        Ok(LockInfo::for_test(operation))
    }
    async fn release_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<()> {
        self.lock_released.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn renew_lock(&self, lock: &LockInfo) -> carina_state::BackendResult<LockInfo> {
        Ok(lock.renewed_for_test())
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
    let lock = LockInfo::for_test("apply");

    let result = ApplyResult {
        success_count: 0,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        managed_current: HashMap::new(),
        data_source_states: HashMap::new(),
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    };

    // This mirrors the pattern used in run_apply_locked / run_apply_from_plan_locked:
    // call finalize_apply (which may fail), then always release lock in the caller.
    let op_result = finalize_apply(
        FinalizeApplyInput {
            result: &result,
            state_base: crate::commands::shared::incremental_state_saver::FinalStateBase::for_test(
                Some(StateFile::new()),
            ),
            state_save_inputs: final_state_save_inputs(&[]),
            data_source_values: &DataSourceValues::empty(),
            backend: &backend,
            lock: StateLockRef::Held(&lock),
            export_params: Some(&checked_exports(&[])),
            provider_context: &ProviderContext::default(),
            wait_aliases: &[],
            pre_resolve_compositions: &[],
        },
        StatePersistence::Normal,
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
    async fn read_state(&self) -> carina_state::BackendResult<Option<LoadedState>> {
        Ok(Some(LoadedState::Pristine(StateFile::new())))
    }
    async fn read_state_bytes(
        &self,
    ) -> carina_state::BackendResult<Option<carina_state::RawState>> {
        carina_state::RawState::from_state(&StateFile::new(), "lock tracking test state").map(Some)
    }
    async fn write_state(&self, _state: &StateFile) -> carina_state::BackendResult<()> {
        self.write_state_called.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn acquire_lock(&self, operation: &str) -> carina_state::BackendResult<LockInfo> {
        Ok(LockInfo::for_test(operation))
    }
    async fn release_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn renew_lock(&self, lock: &LockInfo) -> carina_state::BackendResult<LockInfo> {
        self.renew_lock_called.store(true, Ordering::SeqCst);
        Ok(lock.renewed_for_test())
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
    let lock = LockInfo::for_test("apply");
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
    let lock = LockInfo::for_test("apply");

    let result = ApplyResult {
        success_count: 0,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        managed_current: HashMap::new(),
        data_source_states: HashMap::new(),
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    };

    let op_result = finalize_apply(
        FinalizeApplyInput {
            result: &result,
            state_base: crate::commands::shared::incremental_state_saver::FinalStateBase::for_test(
                Some(StateFile::new()),
            ),
            state_save_inputs: final_state_save_inputs(&[]),
            data_source_values: &DataSourceValues::empty(),
            backend: &backend,
            lock: StateLockRef::Held(&lock),
            export_params: Some(&checked_exports(&[])),
            provider_context: &ProviderContext::default(),
            wait_aliases: &[],
            pre_resolve_compositions: &[],
        },
        StatePersistence::Normal,
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
    async fn read_state(&self) -> carina_state::BackendResult<Option<LoadedState>> {
        Ok(self.initial_state.clone().map(LoadedState::Pristine))
    }
    async fn read_state_bytes(
        &self,
    ) -> carina_state::BackendResult<Option<carina_state::RawState>> {
        self.initial_state
            .as_ref()
            .map(|state| carina_state::RawState::from_state(state, "refresh test state"))
            .transpose()
    }
    async fn write_state(&self, state: &StateFile) -> carina_state::BackendResult<()> {
        *self.written_state.lock().unwrap() = Some(state.clone());
        Ok(())
    }
    async fn acquire_lock(&self, operation: &str) -> carina_state::BackendResult<LockInfo> {
        Ok(LockInfo::for_test(operation))
    }
    async fn release_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn renew_lock(&self, lock: &LockInfo) -> carina_state::BackendResult<LockInfo> {
        Ok(lock.renewed_for_test())
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
    state
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "keep-bucket", "", "keep-bucket")
                .expect("fixture identifier is non-empty")
                .with_attribute("bucket", json!("keep-bucket")),
        )
        .expect("test state setup must be valid");
    state
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "orphan-bucket", "", "orphan-bucket")
                .expect("fixture identifier is non-empty")
                .with_attribute("bucket", json!("orphan-bucket")),
        )
        .expect("test state setup must be valid");

    let backend = RefreshTestBackend::new(state);

    // Config only has "keep-bucket" -- "orphan-bucket" was removed from .crn
    let mut parsed = carina_core::parser::InferredFile {
        resources: vec![Resource::new("s3.Bucket", "keep-bucket").with_attribute(
            "bucket",
            Value::Concrete(ConcreteValue::String("keep-bucket".into())),
        )],
        ..carina_core::parser::InferredFile::default()
    };

    // MockProvider returns not_found for both resources (simulates external deletion)
    let lock = LockInfo::for_test("state-refresh");
    let result = run_state_refresh_locked(
        &mut parsed,
        &backend,
        StateLockRef::Held(&lock),
        std::path::Path::new("."),
        &ProviderContext::default(),
        carina_core::shutdown::ShutdownToken::running(),
    )
    .await;
    assert!(result.is_ok(), "refresh should succeed: {:?}", result);

    // Verify the written state
    let written = backend
        .get_written_state()
        .expect("state should be written");

    // "orphan-bucket" should have been removed from state since it was deleted externally
    // and the refresh should have visited it
    assert!(
        written
            .find_resource("", "s3.Bucket", "orphan-bucket")
            .is_none(),
        "Orphaned resource should be removed from state after refresh (issue #879)"
    );
}

#[tokio::test]
async fn state_refresh_locked_reconciles_deposed_generation_deleted_externally() {
    let mut state = StateFile::new();
    let mut row = ResourceState::new("s3.Bucket", "deposed-bucket", "");
    row.deposed.push(DeposedInstance::from_persisted_json(
        DeposedKey::new_unique(),
        "old-bucket".to_string(),
        None,
        HashMap::from([("bucket".to_string(), json!("old-bucket"))]),
        BTreeSet::new(),
    ));
    state
        .upsert_resource(row)
        .expect("test state setup must be valid");

    let backend = RefreshTestBackend::new(state);
    let mut parsed = carina_core::parser::InferredFile::default();
    let lock = LockInfo::for_test("state-refresh");

    let result = run_state_refresh_locked(
        &mut parsed,
        &backend,
        StateLockRef::Held(&lock),
        std::path::Path::new("."),
        &ProviderContext::default(),
        carina_core::shutdown::ShutdownToken::running(),
    )
    .await;
    assert!(result.is_ok(), "refresh should succeed: {:?}", result);

    let written = backend
        .get_written_state()
        .expect("state should be written");
    assert!(
        written
            .find_resource("", "s3.Bucket", "deposed-bucket")
            .is_none(),
        "shell row should be removed after deposed generation is reconciled as gone"
    );
}

#[tokio::test]
async fn state_refresh_locked_updates_alive_deposed_generation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mock_state_path = tmp.path().join("mock-provider-state.json");
    std::fs::write(
        &mock_state_path,
        serde_json::json!({
            "s3.Bucket.alive-deposed-bucket": {
                "bucket": "old-bucket",
                "status": "alive"
            }
        })
        .to_string(),
    )
    .expect("mock provider state");

    let previous_mock_state = std::env::var_os("CARINA_MOCK_STATE_FILE");
    unsafe {
        std::env::set_var("CARINA_MOCK_STATE_FILE", &mock_state_path);
    }

    let mut state = StateFile::new();
    let mut row = ResourceState::new("s3.Bucket", "alive-deposed-bucket", "");
    row.deposed.push(DeposedInstance::from_persisted_json(
        DeposedKey::new_unique(),
        "old-bucket".to_string(),
        None,
        HashMap::from([
            ("bucket".to_string(), json!("old-bucket")),
            ("status".to_string(), json!("stale")),
        ]),
        BTreeSet::new(),
    ));
    state
        .upsert_resource(row)
        .expect("test state setup must be valid");

    let backend = RefreshTestBackend::new(state);
    let mut parsed = carina_core::parser::InferredFile::default();
    let lock = LockInfo::for_test("state-refresh");

    let result = run_state_refresh_locked(
        &mut parsed,
        &backend,
        StateLockRef::Held(&lock),
        std::path::Path::new("."),
        &ProviderContext::default(),
        carina_core::shutdown::ShutdownToken::running(),
    )
    .await;

    unsafe {
        match previous_mock_state {
            Some(value) => std::env::set_var("CARINA_MOCK_STATE_FILE", value),
            None => std::env::remove_var("CARINA_MOCK_STATE_FILE"),
        }
    }

    assert!(result.is_ok(), "refresh should succeed: {:?}", result);
    let written = backend
        .get_written_state()
        .expect("state should be written");
    let row = written
        .find_resource("", "s3.Bucket", "alive-deposed-bucket")
        .expect("shell row should remain while the deposed generation is alive");
    assert_eq!(row.deposed.len(), 1);
    assert_eq!(
        row.deposed[0].persisted_attributes().get("status"),
        carina_core::value::json_to_dsl_value(&json!("alive"))
    );
}

/// Test that save_state_unlocked uses write_state (not write_state_locked) and
/// does NOT call renew_lock.
#[tokio::test]
async fn save_state_unlocked_uses_write_state_without_lock() {
    use crate::commands::apply::save_state_unlocked;

    let backend = LockTrackingBackend::new();
    let mut state = StateFile::new();
    let state_lock = StateLock::disabled_for_test();
    let StateLockRef::Disabled(disabled) = state_lock.as_ref() else {
        unreachable!("test lock must be disabled");
    };

    let result = save_state_unlocked(&backend, disabled, &mut state).await;
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
    let state_lock = StateLock::disabled_for_test();

    let result = ApplyResult {
        success_count: 0,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        managed_current: HashMap::new(),
        data_source_states: HashMap::new(),
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    };

    let op_result = finalize_apply(
        FinalizeApplyInput {
            result: &result,
            state_base: crate::commands::shared::incremental_state_saver::FinalStateBase::for_test(
                Some(StateFile::new()),
            ),
            state_save_inputs: final_state_save_inputs(&[]),
            data_source_values: &DataSourceValues::empty(),
            backend: &backend,
            lock: state_lock.as_ref(),
            export_params: Some(&checked_exports(&[])),
            provider_context: &ProviderContext::default(),
            wait_aliases: &[],
            pre_resolve_compositions: &[],
        },
        StatePersistence::Normal,
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

#[tokio::test]
async fn finalize_apply_resolves_exports_from_execution_result_data_sources() {
    use carina_core::parser::{InferredExportParam, TypeExpr};
    use carina_core::resource::AccessPath;

    let captured = Arc::new(Mutex::new(None));
    let backend = CapturingBackend {
        captured: captured.clone(),
    };
    let lock = LockInfo::for_test("apply");

    let data_source_id = ResourceId::with_provider_identity("mock", "test.lookup", "lookup", None);
    let mut data_source = DataSource::with_provider("mock", "test.lookup", "lookup", None);
    data_source.binding = Some("lookup".to_string());
    let data_sources = vec![data_source];
    let data_source_state = State::existing(
        data_source_id.clone(),
        HashMap::from([(
            "answer".to_string(),
            Value::Concrete(ConcreteValue::String("from-execution-result".into())),
        )]),
    );
    let result = ApplyResult {
        success_count: 1,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        managed_current: HashMap::new(),
        data_source_states: HashMap::from([(data_source_id, data_source_state)]),
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    };
    let export_params = vec![InferredExportParam {
        name: "answer".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("lookup", "answer"),
        })),
    }];
    let data_source_values = DataSourceValues::from_data_sources(&data_sources, |data_source| {
        DataSourceValue::read(result.data_source_states[&data_source.id].clone())
    });

    let _ = finalize_apply(
        FinalizeApplyInput {
            result: &result,
            state_base: crate::commands::shared::incremental_state_saver::FinalStateBase::for_test(
                Some(StateFile::new()),
            ),
            state_save_inputs: final_state_save_inputs(&[]),
            data_source_values: &data_source_values,
            backend: &backend,
            lock: StateLockRef::Held(&lock),
            export_params: Some(&checked_exports(&export_params)),
            provider_context: &ProviderContext::default(),
            wait_aliases: &[],
            pre_resolve_compositions: &[],
        },
        StatePersistence::Normal,
    )
    .await
    .expect("finalize apply must save data-source-derived exports");

    let written = captured.lock().unwrap();
    let state = written.as_ref().expect("state should be written");
    assert_eq!(
        state
            .exports
            .get("answer")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::json!("from-execution-result"))
    );
}

/// Test that WiringContext activates the built-in mock catalog when the
/// provider is declared but no factory was loaded.
#[test]
fn wiring_context_constructs_factories_and_schemas_once() {
    let parsed = carina_core::parser::parse_file(
        "provider mock {}\n",
        std::path::Path::new("providers.crn"),
        &carina_core::parser::ProviderContext::default(),
    )
    .unwrap();
    let ctx = WiringContext::new_for_test(vec![], &parsed.providers);

    assert!(
        ctx.factories().is_empty(),
        "Should have no factories when none are provided"
    );
    assert!(
        ctx.schemas().has_managed("mock", "test.resource")
            && ctx.schemas().has_managed("mock", "compute.Instance")
            && ctx.schemas().has_data_source("mock", "iam.Roles"),
        "built-in mock schemas must be registered for a declared mock provider without a factory"
    );
    assert!(ctx.known_providers().contains("mock"));

    // Calling schemas() again should return the same data (cached, not rebuilt)
    let schemas_a = ctx.schemas();
    let schemas_b = ctx.schemas();
    assert_eq!(
        schemas_a.len(),
        schemas_b.len(),
        "Schemas should be consistent across calls"
    );
}

#[test]
fn wiring_context_activates_builtin_mock_for_implicit_fallback() {
    let ctx = WiringContext::new_for_test(vec![], &[]);

    assert!(ctx.known_providers().contains("mock"));
    assert!(ctx.schemas().has_managed("mock", "test.resource"));
    assert!(ctx.schemas().has_data_source("mock", "iam.Roles"));
}

/// Issue #931: orphaned resources that no longer exist in infrastructure should not
/// produce a Delete effect. The plan path now calls `provider.read()`
/// for each orphan returned by `build_orphans()` and skips those that no longer
/// exist.
///
/// This test simulates the fixed code path:
/// 1. State file has "removed-bucket" (orphaned — not in .crn config)
/// 2. The provider returns not-found for "removed-bucket" (deleted externally)
/// 3. The refresh loop skips it, so managed current state has no entry for it
/// 4. Expected: no Delete effect should be generated
#[test]
fn orphaned_resource_deleted_externally_should_not_produce_delete_effect() {
    use carina_core::differ::create_plan;

    // State file tracks "removed-bucket" with an identifier (implies it existed in infra)
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "removed-bucket", "aws", "removed-bucket")
                .expect("fixture identifier is non-empty")
                .with_attribute("bucket", json!("removed-bucket")),
        )
        .expect("test state setup must be valid");

    // No desired resources — "removed-bucket" was removed from .crn
    let desired: Vec<Resource> = vec![];
    let desired_ids: HashSet<ResourceId> = desired.iter().map(|r| r.id.clone()).collect();

    let current_states: HashMap<ResourceId, State> = HashMap::new();

    // Simulate provider refresh replacing only the provider-observed state
    // inside each typed orphan.
    let orphans = state_file.build_orphans(&desired_ids);
    let orphans = orphans
        .into_iter()
        .map(|(id, orphan)| {
            // Simulate provider.read() returning not-found for this identifier.
            // In the real code the typed orphan supplies `AppliedIdentifier`
            // and the provider returns `ReadOutcome::NotFound`.
            let refreshed = State::not_found(id.clone());
            orphan.with_refreshed_state(refreshed)
        })
        .collect::<carina_state::LoadedOrphans>()
        .into_plan_orphans();

    let directives_map = state_file.build_directives();
    let saved_attrs = state_file.build_saved_attrs().lift(&SchemaRegistry::new());
    let prev_explicit = state_file.build_explicit();

    let plan = create_plan(
        &ResolvedResource::collect_resolved(desired.clone()).unwrap(),
        &[],
        &carina_core::provider::ProviderRouter::new(),
        carina_core::differ::PlanPriorStates::new(
            &crate::plan_input_current_from_states(
                current_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &[],
            ),
            &orphans,
        ),
        &directives_map,
        &SchemaRegistry::new(),
        &saved_attrs,
        &prev_explicit,
        &[],
    )
    .expect("test provider hints should succeed");

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

/// Issue #950: --refresh=false should use cached state from state file
/// instead of calling provider.read().
///
/// This test simulates the refresh=false code path:
/// 1. State file has "my-bucket" with known attributes
/// 2. With refresh=false, managed current state is built from the state file
/// 3. No provider.read() calls are made
#[test]
fn refresh_false_uses_cached_state_from_state_file() {
    use carina_core::differ::create_plan;

    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "my-bucket", "awscc", "my-bucket-id")
                .expect("fixture identifier is non-empty")
                .with_attribute("bucket_name", json!("my-bucket"))
                .with_attribute("region", json!("ap-northeast-1")),
        )
        .expect("test state setup must be valid");

    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    );
    resource.set_attr(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );

    let desired = vec![resource];

    // Simulate refresh=false: build managed current state from the state file.
    let mut current_states: HashMap<ResourceId, Current> = HashMap::new();
    for res in &desired {
        let current = state_file.build_state_for_resource(&res.id);
        current_states.insert(res.id.clone(), current);
    }

    // Verify state was loaded from state file
    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "my-bucket", None);
    let cached_state = current_states.get(&id).unwrap();
    let cached_state = match cached_state {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::MissingIdentifier(state) => state.state(),
        Current::CreateInFlight(pending) => {
            panic!("fixture unexpectedly loaded an in-flight create: {pending:?}")
        }
    };
    assert!(cached_state.exists, "State should exist from state file");
    assert_eq!(
        cached_state.identifier,
        Some("my-bucket-id".to_string()),
        "Identifier should come from state file"
    );
    assert_eq!(
        cached_state.attributes.get("bucket_name"),
        Some(&Value::Concrete(ConcreteValue::String("my-bucket".into()))),
        "Attributes should come from state file"
    );

    let directives_map = state_file.build_directives();
    let saved_attrs = state_file.build_saved_attrs().lift(&SchemaRegistry::new());
    let prev_explicit = state_file.build_explicit();

    let plan = create_plan(
        &ResolvedResource::collect_resolved(desired.clone()).unwrap(),
        &[],
        &carina_core::provider::ProviderRouter::new(),
        carina_core::differ::PlanPriorStates::new(
            &carina_core::resource::into_plan_input_current_map(
                current_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &[],
            ),
            &carina_core::differ::Orphans::new(),
        ),
        &directives_map,
        &SchemaRegistry::new(),
        &saved_attrs,
        &prev_explicit,
        &[],
    )
    .expect("test provider hints should succeed");

    // No changes expected since desired matches cached state
    assert_eq!(
        plan.mutation_count(),
        0,
        "No changes expected when desired state matches cached state"
    );
}

/// Issue #950: --refresh=false with orphaned resources should use state file data
/// directly without calling provider.read().
#[test]
fn refresh_false_includes_orphaned_resources_from_state_file() {
    use carina_core::differ::create_plan;

    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(
            ResourceState::try_applied(
                "s3.Bucket",
                "orphaned-bucket",
                "awscc",
                "orphaned-bucket-id",
            )
            .expect("fixture identifier is non-empty")
            .with_attribute("bucket_name", json!("orphaned-bucket")),
        )
        .expect("test state setup must be valid");

    // No desired resources — the bucket was removed from .crn
    let desired: Vec<Resource> = vec![];
    let desired_ids: HashSet<ResourceId> = desired.iter().map(|r| r.id.clone()).collect();

    let current_states: HashMap<ResourceId, State> = HashMap::new();

    // Simulate refresh=false: the typed orphan state is the cached state-file
    // value and is not duplicated into the managed-current map.
    let orphans = state_file.build_orphans(&desired_ids).into_plan_orphans();

    let directives_map = state_file.build_directives();
    let saved_attrs = state_file.build_saved_attrs().lift(&SchemaRegistry::new());
    let prev_explicit = state_file.build_explicit();

    let plan = create_plan(
        &ResolvedResource::collect_resolved(desired.clone()).unwrap(),
        &[],
        &carina_core::provider::ProviderRouter::new(),
        carina_core::differ::PlanPriorStates::new(
            &crate::plan_input_current_from_states(
                current_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &[],
            ),
            &orphans,
        ),
        &directives_map,
        &SchemaRegistry::new(),
        &saved_attrs,
        &prev_explicit,
        &[],
    )
    .expect("test provider hints should succeed");

    // With refresh=false, orphaned resources are assumed to still exist,
    // so a Delete effect should be generated
    let delete_effects: Vec<_> = plan
        .effects()
        .iter()
        .filter(|e| matches!(e, Effect::Delete { .. }))
        .collect();

    assert_eq!(
        delete_effects.len(),
        1,
        "Issue #950: orphaned resource with refresh=false should produce Delete effect"
    );
}

/// Issue #950: --refresh=false without a state file should treat all resources as new.
#[test]
fn refresh_false_without_state_file_treats_resources_as_new() {
    use carina_core::differ::create_plan;

    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "new-bucket", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("new-bucket".into())),
    );

    let desired = vec![resource];

    // Simulate refresh=false with no state file
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for res in &desired {
        current_states.insert(res.id.clone(), State::not_found(res.id.clone()));
    }

    let plan = create_plan(
        &ResolvedResource::collect_resolved(desired.clone()).unwrap(),
        &[],
        &carina_core::provider::ProviderRouter::new(),
        carina_core::differ::PlanPriorStates::new(
            &crate::plan_input_current_from_states(
                current_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &[],
            ),
            &carina_core::differ::Orphans::new(),
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &crate::empty_lifted_saved_attrs(),
        &HashMap::new(),
        &[],
    )
    .expect("test provider hints should succeed");

    let create_effects: Vec<_> = plan
        .effects()
        .iter()
        .filter(|e| matches!(e, Effect::Create { .. }))
        .collect();

    assert_eq!(
        create_effects.len(),
        1,
        "Without state file, all resources should be created"
    );
}

#[test]
fn import_effect_preserves_resource_metadata_in_state() {
    // Regression test: the Effect::Import arm in build_state_after_apply used to
    // overwrite the ResourceState with a bare ResourceState::new(), stripping
    // directives, prefixes, desired_keys, binding, and dependency_bindings.

    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None)
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        )
        .with_binding("my_vpc_binding");
    resource.directives.force_delete = true;
    resource
        .prefixes
        .insert("vpc_name".to_string(), "prod-".to_string());

    let id = resource.id.clone();
    let identifier = "vpc-0abc123";

    // Simulate the imported state returned by the provider read
    let imported_state = State::existing(
        id.clone(),
        HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        )]),
    )
    .with_identifier(identifier);

    let mut applied_states = HashMap::new();
    applied_states.insert(id.clone(), Applied::Imported(imported_state));

    // Build a plan with an Import effect
    let mut plan = Plan::new();
    plan.add(Effect::Import {
        id: carina_core::resource::ResolvedResourceId::new(id.clone()),
        identifier: Value::Concrete(ConcreteValue::String(identifier.to_string().into())),
    });

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &[resolved(resource)],
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        },
        None,
    )
    .unwrap();

    let saved_resource = saved.find_resource("awscc", "ec2.Vpc", "my-vpc").unwrap();

    // Metadata must survive the import
    assert!(
        saved_resource.directives.force_delete,
        "directives.force_delete should be preserved after import"
    );
    assert_eq!(
        saved_resource.prefixes.get("vpc_name"),
        Some(&"prod-".to_string()),
        "prefixes should be preserved after import"
    );
    let carina_core::explicit::ExplicitFields::Struct {
        children: explicit_children,
    } = &saved_resource.explicit
    else {
        panic!(
            "saved_resource.explicit must be Struct, got: {:?}",
            saved_resource.explicit
        );
    };
    assert!(
        explicit_children.contains_key("cidr_block"),
        "explicit children should be preserved after import"
    );
    assert_eq!(
        saved_resource.binding,
        Some("my_vpc_binding".to_string()),
        "binding should be preserved after import"
    );
    assert_eq!(
        saved_resource.identifier().map(|value| value.as_str()),
        Some("vpc-0abc123"),
        "identifier should be preserved after import"
    );
}

#[test]
fn build_state_after_apply_persists_write_only_attributes() {
    use carina_core::schema::{AttributeSchema, AttributeType};

    // Set up a resource with a write-only attribute (ipv4_netmask_length)
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    resource.set_attr(
        "ipv4_netmask_length".to_string(),
        Value::Concrete(ConcreteValue::Int(16)),
    );

    let id = resource.id.clone();

    // Provider returns state WITHOUT the write-only attribute (API doesn't return it)
    let applied_states = HashMap::from([(
        id.clone(),
        complete_create(State {
            id: id.clone(),
            identifier: Some("vpc-123".to_string()),
            attributes: HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )]),
            exists: true,
            dependency_bindings: BTreeSet::new(),
            partial_read: None,
        }),
    )]);

    // Build schema with write-only attribute.
    // Schema keys include the provider prefix (e.g., "awscc.ec2.Vpc").
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("ipv4_netmask_length", AttributeType::int())
                    .write_only()
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &[resolved(resource)],
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied_states,
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    )
    .unwrap();

    let saved_resource = saved.find_resource("awscc", "ec2.Vpc", "my-vpc").unwrap();

    // The write-only attribute should be persisted from the desired resource
    assert_eq!(
        saved_resource
            .persisted_attributes()
            .expect("saved resource has applied attributes")
            .get("ipv4_netmask_length"),
        carina_core::value::json_to_dsl_value(&json!(16)),
        "write-only attribute should be persisted in state"
    );
    assert_eq!(
        saved_resource.write_only_attributes(),
        ["ipv4_netmask_length"],
        "write-only attribute names should be recorded"
    );

    // The regular attribute should also be there
    assert_eq!(
        saved_resource
            .persisted_attributes()
            .expect("saved resource has applied attributes")
            .get("cidr_block"),
        carina_core::value::json_to_dsl_value(&json!("10.0.0.0/16"))
    );
}

#[test]
fn write_only_canonical_enum_state_roundtrip_converges_without_diff() {
    use carina_core::differ::create_plan;
    use carina_core::resource::EnumValueResolver;
    use carina_core::schema::{AttributeSchema, AttributeType, TypeIdentity};

    let domain_type = AttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
        Some(vec!["vpc".to_string(), "standard".to_string()]),
        Vec::new(),
        None,
        None,
    );
    let domain = EnumValueResolver::new(&domain_type)
        .resolve_state_text("vpc")
        .unwrap();
    let mut resource = Resource::with_provider("aws", "ec2.Eip", "main", None);
    resource.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::CanonicalEnum(domain)),
    );
    let id = resource.id.clone();
    let applied_states = HashMap::from([(
        id.clone(),
        complete_create(State::existing(
            id,
            HashMap::from([(
                "allocation_id".to_string(),
                Value::Concrete(ConcreteValue::String("eipalloc-123".into())),
            )]),
        )),
    )]);
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (ResourceSchema::new("ec2.Eip")
            .attribute(AttributeSchema::optional("allocation_id", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("domain", domain_type)
                    .write_only()
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &[resolved(resource.clone())],
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied_states,
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    )
    .unwrap();
    let reloaded = PostApplyStates::from_current_and_state(
        &HashMap::new(),
        &DataSourceValues::empty(),
        &saved,
    );

    let resources = [resource];
    let plan = create_plan(
        &ResolvedResource::collect_resolved(resources.iter().cloned()).unwrap(),
        &[],
        &carina_core::provider::ProviderRouter::new(),
        carina_core::differ::PlanPriorStates::new(
            &carina_core::resource::into_plan_input_current_map(
                reloaded.managed_current().clone(),
                &schemas,
                &resources,
            ),
            &carina_core::differ::Orphans::new(),
        ),
        &HashMap::new(),
        &schemas,
        &crate::empty_lifted_saved_attrs(),
        &HashMap::new(),
        &[],
    )
    .expect("test provider hints should succeed");
    assert!(
        plan.effects().is_empty(),
        "typed enum state JSON must reload as CanonicalEnum, got effects: {:?}",
        plan.effects()
    );
}

#[test]
fn build_state_after_apply_write_only_detects_value_change() {
    use carina_core::schema::{AttributeSchema, AttributeType};

    // Simulate: user changed ipv4_netmask_length from 16 to 24
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/24".into())),
    );
    resource.set_attr(
        "ipv4_netmask_length".to_string(),
        Value::Concrete(ConcreteValue::Int(24)),
    );

    let id = resource.id.clone();

    // Provider returns updated state (after apply) without write-only attribute
    let applied_states = HashMap::from([(
        id.clone(),
        complete_update(
            State {
                id: id.clone(),
                identifier: Some("vpc-123".to_string()),
                attributes: HashMap::from([(
                    "cidr_block".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.0.0/24".into())),
                )]),
                exists: true,
                dependency_bindings: BTreeSet::new(),
                partial_read: None,
            },
            &["cidr_block", "ipv4_netmask_length"],
            &[],
        ),
    )]);

    // Previous state had old write-only value
    let mut existing_state = StateFile::new();
    let mut old_rs = ResourceState::try_applied("ec2.Vpc", "my-vpc", "awscc", "vpc-123")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block".to_string(), json!("10.0.0.0/16"))
        .with_attribute("ipv4_netmask_length".to_string(), json!(16));
    old_rs = crate::fixture_resource_state_metadata(
        old_rs,
        Some(vec!["ipv4_netmask_length".to_string()]),
        None,
    );
    existing_state
        .upsert_resource(old_rs)
        .expect("test state setup must be valid");

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("ipv4_netmask_length", AttributeType::int())
                    .write_only()
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file: existing_state,
            sorted_resources: &[resolved(resource)],
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied_states,
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    )
    .unwrap();

    let saved_resource = saved.find_resource("awscc", "ec2.Vpc", "my-vpc").unwrap();

    // The write-only attribute should have the NEW value (24, not 16)
    assert_eq!(
        saved_resource
            .persisted_attributes()
            .expect("saved resource has applied attributes")
            .get("ipv4_netmask_length"),
        carina_core::value::json_to_dsl_value(&json!(24)),
        "write-only attribute should reflect the updated desired value"
    );
}

#[test]
fn plan_file_serialization_redacts_secrets() {
    use carina_core::value::{
        redact_secrets_in_plan, redact_secrets_in_resource, redact_secrets_in_state,
    };

    // Build a plan with secrets in resources, states, and effects
    let secret_password = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("super-secret-pw".into()),
    ))));

    let resource_with_secret = Resource::with_provider("awscc", "rds.db_instance", "my-db", None)
        .with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("my-db".into())),
        )
        .with_attribute("master_password", secret_password.clone());

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(resource_with_secret.clone())));

    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("my-db".into())),
    );
    state_attrs.insert("master_password".to_string(), secret_password.clone());
    let state_with_secret = State::existing(
        ResourceId::with_provider_identity("awscc", "rds.db_instance", "my-db", None),
        state_attrs,
    );

    // Redact before building PlanFile (same as production code does)
    let plan_file = record_test_plan(
        PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: "0.1.0".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            source_path: "example.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: saved_provider_configs(Vec::new()),
            backend_config: None,
            plan: redact_secrets_in_plan(&plan).unwrap(),
            sorted_resources: vec![redact_secrets_in_resource(&resource_with_secret).unwrap()],
            unresolved_resources: vec![redact_secrets_in_resource(&resource_with_secret).unwrap()],
            compositions: vec![],
            data_sources: vec![],
            data_source_origins: vec![],
            data_source_values: vec![],
            current_states: vec![CurrentStateEntry::managed(
                ResourceId::with_provider_identity("awscc", "rds.db_instance", "my-db", None),
                redact_secrets_in_state(&state_with_secret).unwrap(),
            )],
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: vec![],
        },
        &["awscc"],
    );

    let json = serde_json::to_string_pretty(&plan_file).unwrap();

    // The plaintext secret must never appear in the serialized output
    assert!(
        !json.contains("super-secret-pw"),
        "Plan file JSON must not contain plaintext secret value.\nJSON:\n{}",
        json
    );

    // The hash is carried only by the dedicated placeholder, never by an
    // ordinary string that could cross the provider boundary.
    assert!(
        json.contains("RedactedSecret") && json.contains("\"hash\""),
        "Plan file JSON should contain a typed secret placeholder.\nJSON:\n{}",
        json
    );

    // Non-secret values should still be present
    assert!(json.contains("my-db"));
}

#[test]
fn plan_file_serialization_rejects_pending_resource_identity() {
    let plan_file = record_test_plan(
        PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: "test".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            source_path: "main.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: saved_provider_configs(Vec::new()),
            backend_config: None,
            plan: Plan::new(),
            sorted_resources: vec![Resource::pending_with_provider(
                "mock",
                "test.resource",
                None,
            )],
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            data_source_origins: Vec::new(),
            data_source_values: Vec::new(),
            current_states: Vec::new(),
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        },
        &["mock"],
    );

    let error = serde_json::to_string(&plan_file)
        .expect_err("a saved plan must never persist a pending resource identity");
    assert!(
        error
            .to_string()
            .contains("pending resource identity cannot be serialized")
    );
}

#[test]
fn plan_file_validation_rejects_missing_resource_identity() {
    let plan_file = record_test_plan(
        PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: "test".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            source_path: "main.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: saved_provider_configs(Vec::new()),
            backend_config: None,
            plan: Plan::new(),
            sorted_resources: vec![Resource::with_provider(
                "mock",
                "test.resource",
                "alpha",
                None,
            )],
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            data_source_origins: Vec::new(),
            data_source_values: Vec::new(),
            current_states: Vec::new(),
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        },
        &["mock"],
    );
    let mut json = serde_json::to_value(plan_file).unwrap();
    json["sorted_resources"][0]["id"]
        .as_object_mut()
        .unwrap()
        .remove("identity");

    let decoded: PlanFile = serde_json::from_value(json).unwrap();
    let error = decoded
        .validate_resource_identities()
        .expect_err("a saved plan must reject a missing resource identity");
    assert!(error.contains("sorted_resources[0]"), "got: {error}");
}

fn plan_file_with_unchanged_managed_wait_target() -> PlanFile {
    use std::time::Duration;

    use carina_core::wait::ManagedWaitTarget;
    use carina_core::wait::predicate::{AttrPath, WaitPredicate};

    let managed_target = resolved(Resource::with_provider(
        "mock",
        "test.resource",
        "managed",
        None,
    ));
    let target_id = managed_target.resolved_id().as_inner().clone();
    let mut plan = Plan::new();
    plan.add(Effect::Wait {
        identity: "lookup_ready".into(),
        target_id: ManagedWaitTarget::for_test(&managed_target),
        until: WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("ready".into())),
        )
        .unwrap(),
        until_surface: "managed.status == \"ready\"".to_string(),
        timeout: Duration::from_secs(60),
        interval: Duration::from_secs(5),
        explicit_dependencies: HashSet::new(),
    });
    record_test_plan(
        PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: "test".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            source_path: "main.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: saved_provider_configs(Vec::new()),
            backend_config: None,
            plan,
            sorted_resources: vec![managed_target.into_inner()],
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            data_source_origins: Vec::new(),
            data_source_values: Vec::new(),
            current_states: vec![CurrentStateEntry::managed(
                target_id.clone(),
                State::existing(target_id, HashMap::new()).with_identifier("managed-id"),
            )],
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        },
        &["mock"],
    )
}

fn replace_saved_wait_target(plan_file: PlanFile, target: &ResourceId) -> PlanFile {
    let mut json = serde_json::to_value(plan_file).unwrap();
    json["plan"]["effects"][0]["Wait"]["target_id"] = serde_json::to_value(target).unwrap();
    let schemas = json["schema_fingerprints"]["providers"]
        .as_object_mut()
        .expect("provider fingerprint inventory")
        .get_mut(&target.provider)
        .expect("target provider fingerprint")
        .get_mut("schemas")
        .and_then(serde_json::Value::as_array_mut)
        .expect("target provider schemas");
    let already_recorded = schemas.iter().any(|schema| {
        schema["kind"] == "resource" && schema["resource_type"] == target.resource_type.as_str()
    });
    if !already_recorded {
        schemas.push(serde_json::json!({
            "kind": "resource",
            "resource_type": target.resource_type.as_str(),
            "fingerprint": "absent"
        }));
    }
    serde_json::from_value(json)
        .expect("the transparent wait target should deserialize before load validation")
}

#[test]
fn plan_file_validation_rejects_wait_target_present_only_in_data_sources() {
    let data_source =
        DataSource::with_provider("mock", "test.lookup", "lookup", None).with_binding("lookup");
    let mut contents = plan_file_with_unchanged_managed_wait_target().into_contents();
    contents.data_sources.push(data_source.clone());
    contents.data_source_values.push(SavedDataSourceValue {
        id: data_source.id.clone(),
        value: DataSourceValue::deferred_to_apply(Vec::new()),
    });
    assert!(contents.data_source_origins.is_empty());

    let plan_file = record_test_plan(contents, &["mock"]);
    let decoded = replace_saved_wait_target(plan_file, &data_source.id);
    let error = decoded
        .validate_resource_identities()
        .expect_err("a saved plan must reject a wait targeting a data source");
    assert_eq!(
        error,
        format!(
            "Invalid saved plan: wait `lookup_ready` targets `{}`, which is not a managed resource in this plan",
            data_source.id
        )
    );
}

#[test]
fn plan_file_validation_rejects_wait_target_missing_from_managed_resources() {
    let unknown = ResourceId::with_provider_identity("mock", "test.resource", "unknown", None);
    let decoded =
        replace_saved_wait_target(plan_file_with_unchanged_managed_wait_target(), &unknown);

    let error = decoded
        .validate_resource_identities()
        .expect_err("a saved plan must reject an unknown wait target");
    assert_eq!(
        error,
        format!(
            "Invalid saved plan: wait `lookup_ready` targets `{unknown}`, which is not a managed resource in this plan"
        )
    );
}

#[test]
fn plan_file_validation_accepts_unchanged_managed_wait_target() {
    let encoded = serde_json::to_value(plan_file_with_unchanged_managed_wait_target()).unwrap();
    let decoded: PlanFile = serde_json::from_value(encoded).unwrap();

    decoded
        .validate_resource_identities()
        .expect("a wait-only plan may target an unchanged managed resource");
}

#[test]
fn plan_file_validation_rejects_blocked_data_source_without_plan_errors() {
    let data_source =
        DataSource::with_provider("mock", "iam.Roles", "roles", None).with_binding("roles");
    let mut plan = Plan::new();
    plan.add(Effect::Read {
        resource: resolved_data_source(data_source.clone()),
    });
    let plan_file = record_test_plan(
        PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: "test".to_string(),
            timestamp: "2026-10-28T00:00:00Z".to_string(),
            source_path: "main.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: saved_provider_configs(Vec::new()),
            backend_config: None,
            plan,
            sorted_resources: Vec::new(),
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: vec![data_source.clone()],
            data_source_origins: Vec::new(),
            data_source_values: vec![SavedDataSourceValue {
                id: data_source.id,
                value: DataSourceValue::blocked_by_plan_error(),
            }],
            current_states: Vec::new(),
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        },
        &["mock"],
    );

    let error = plan_file
        .validate_resource_identities()
        .expect_err("a blocked outcome cannot enter execution without its plan error");
    assert!(
        error.contains("blocked_by_plan_error") && error.contains("no plan errors"),
        "got: {error}"
    );
}

#[test]
fn plan_file_decode_requires_data_source_values_field() {
    let mut encoded = serde_json::to_value(plan_file_with_unchanged_managed_wait_target()).unwrap();
    encoded
        .as_object_mut()
        .expect("saved plan object")
        .remove("data_source_values");

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("format 25 must require the total data-source outcome field");
    assert!(error.to_string().contains("data_source_values"), "{error}");
}

#[test]
fn plan_file_decode_requires_provider_artifacts_field() {
    let mut encoded = serde_json::to_value(plan_file_with_unchanged_managed_wait_target()).unwrap();
    encoded
        .as_object_mut()
        .expect("saved plan object")
        .remove("provider_artifacts");

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("format 26 must require provider artifact identities");
    assert!(error.to_string().contains("provider_artifacts"), "{error}");
}

#[test]
fn plan_file_decode_requires_schema_fingerprints_field() {
    let mut encoded = serde_json::to_value(plan_file_with_unchanged_managed_wait_target()).unwrap();
    encoded
        .as_object_mut()
        .expect("saved plan object")
        .remove("schema_fingerprints");

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("format 23 and later must require schema fingerprints");
    assert!(error.to_string().contains("schema_fingerprints"), "{error}");
}

fn encoded_plan_with_mock_provider_evidence() -> serde_json::Value {
    let mut encoded = serde_json::to_value(plan_file_with_unchanged_managed_wait_target()).unwrap();
    encoded["schema_fingerprints"] = serde_json::json!({
        "providers": {
            "mock": {
                "schemas": [{
                    "kind": "resource",
                    "resource_type": "test.resource",
                    "fingerprint": "absent"
                }]
            }
        }
    });
    encoded["provider_artifacts"] = serde_json::json!({
        "mock": {
            "kind": "wasm",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "origin": { "kind": "file" }
        }
    });
    encoded
}

#[test]
fn plan_file_decode_accepts_base_mock_provider_evidence() {
    serde_json::from_value::<PlanFile>(encoded_plan_with_mock_provider_evidence())
        .expect("base provider evidence fixture must decode");
}

#[test]
fn plan_file_decode_rejects_empty_schema_fingerprints_for_referenced_provider() {
    let mut encoded = encoded_plan_with_mock_provider_evidence();
    encoded["schema_fingerprints"]["providers"] = serde_json::json!({});

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("referenced provider schemas must be recorded");
    assert_eq!(
        error.to_string(),
        "invalid saved-plan provider evidence: schema_fingerprints is missing provider `mock`, managed type `test.resource`"
    );
}

#[test]
fn plan_file_decode_rejects_provider_with_empty_schema_inventory() {
    let mut encoded = encoded_plan_with_mock_provider_evidence();
    encoded["schema_fingerprints"]["providers"]["mock"]["schemas"] = serde_json::json!([]);

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("a provider fingerprint entry must contain schemas");
    assert_eq!(
        error.to_string(),
        "invalid saved-plan provider evidence: schema_fingerprints provider `mock` has an empty schemas list"
    );
}

#[test]
fn plan_file_decode_rejects_missing_artifact_for_configured_provider() {
    let mut encoded = encoded_plan_with_mock_provider_evidence();
    encoded["provider_artifacts"] = serde_json::json!({});

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("referenced providers must have artifact evidence");
    assert_eq!(
        error.to_string(),
        "invalid saved-plan provider evidence: provider_artifacts is missing expected provider `mock`"
    );
}

#[test]
fn plan_file_decode_rejects_artifact_for_unconfigured_provider() {
    let mut encoded = encoded_plan_with_mock_provider_evidence();
    encoded["provider_artifacts"]["retired"] = serde_json::json!({ "kind": "built_in" });

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("artifact keys must exactly match configured provider kinds");
    assert_eq!(
        error.to_string(),
        "invalid saved-plan provider evidence: provider_artifacts contains unexpected provider `retired`"
    );
}

#[test]
fn plan_file_decode_rejects_wrong_artifact_kind_for_configured_provider() {
    let mut encoded = encoded_plan_with_mock_provider_evidence();
    encoded["provider_artifacts"]["mock"] = serde_json::json!({ "kind": "built_in" });

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("artifact kind must match the configured provider implementation");
    assert_eq!(
        error.to_string(),
        "invalid saved-plan provider evidence: provider_artifacts records provider `mock` as built-in, expected wasm"
    );
}

#[test]
fn plan_file_decode_rejects_extra_schema_fingerprint() {
    let mut encoded = encoded_plan_with_mock_provider_evidence();
    encoded["schema_fingerprints"]["providers"]["mock"]["schemas"]
        .as_array_mut()
        .expect("mock schema inventory")
        .push(serde_json::json!({
            "kind": "resource",
            "resource_type": "test.extra",
            "fingerprint": "absent"
        }));

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("unreferenced schema evidence must be rejected");
    assert_eq!(
        error.to_string(),
        "invalid saved-plan provider evidence: schema_fingerprints contains unreferenced provider `mock`, managed type `test.extra`"
    );
}

#[test]
fn plan_file_decode_rejects_duplicate_schema_fingerprint() {
    let mut encoded = encoded_plan_with_mock_provider_evidence();
    let duplicate = encoded["schema_fingerprints"]["providers"]["mock"]["schemas"][0].clone();
    encoded["schema_fingerprints"]["providers"]["mock"]["schemas"]
        .as_array_mut()
        .expect("mock schema inventory")
        .push(duplicate);

    let error = serde_json::from_value::<PlanFile>(encoded)
        .expect_err("duplicate schema evidence must be rejected");
    assert_eq!(
        error.to_string(),
        "invalid saved-plan provider evidence: duplicate schema fingerprint for provider `mock`, managed type `test.resource`"
    );
}

fn builtin_mock_provider_config() -> ProviderConfig {
    ProviderConfig {
        name: "mock".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }
}

fn builtin_mock_recording_context() -> WiringContext {
    WiringContext::new_for_test(Vec::new(), &[builtin_mock_provider_config()])
}

#[test]
fn recorded_builtin_only_plan_round_trips_with_format_26_top_level_keys() {
    let mut contents = plan_file_with_unchanged_managed_wait_target().into_contents();
    contents.provider_configs = saved_provider_configs(vec![builtin_mock_provider_config()]);
    let recorded = contents
        .record(&builtin_mock_recording_context())
        .expect("built-in provider evidence must record");
    assert_eq!(
        recorded.provider_artifacts().get("mock"),
        Some(&RecordedProviderArtifact::BuiltIn)
    );

    let encoded = serde_json::to_value(&recorded).expect("recorded plan serializes");
    let actual_keys = encoded
        .as_object()
        .expect("saved plan is a top-level object")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let expected_keys = BTreeSet::from([
        "backend_config",
        "carina_version",
        "compositions",
        "current_states",
        "data_source_origins",
        "data_source_values",
        "data_sources",
        "plan",
        "provider_artifacts",
        "provider_configs",
        "schema_fingerprints",
        "sorted_resources",
        "source_path",
        "state_lineage",
        "state_serial",
        "timestamp",
        "unresolved_resources",
        "upstream_snapshot",
        "upstream_sources",
        "version",
        "wait_bindings",
    ]);
    assert_eq!(actual_keys, expected_keys);

    struct TopLevelKeys;

    impl<'de> serde::de::Visitor<'de> for TopLevelKeys {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a saved-plan object")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut keys = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                keys.push(key);
                map.next_value::<serde::de::IgnoredAny>()?;
            }
            Ok(keys)
        }
    }

    let serialized = serde_json::to_vec(&recorded).expect("recorded plan serializes in order");
    let mut deserializer = serde_json::Deserializer::from_slice(&serialized);
    let actual_order =
        serde::de::Deserializer::deserialize_map(&mut deserializer, TopLevelKeys).unwrap();
    assert_eq!(
        actual_order,
        [
            "version",
            "carina_version",
            "timestamp",
            "source_path",
            "state_lineage",
            "state_serial",
            "provider_configs",
            "backend_config",
            "plan",
            "sorted_resources",
            "unresolved_resources",
            "compositions",
            "data_sources",
            "data_source_origins",
            "data_source_values",
            "current_states",
            "upstream_snapshot",
            "upstream_sources",
            "wait_bindings",
            "schema_fingerprints",
            "provider_artifacts",
        ]
    );

    let decoded: PlanFile = serde_json::from_value(encoded).expect("recorded plan round-trips");
    assert_eq!(
        decoded.schema_fingerprints(),
        recorded.schema_fingerprints()
    );
    assert_eq!(decoded.provider_artifacts(), recorded.provider_artifacts());
}

#[test]
fn zero_referenced_provider_plan_round_trips() {
    let mut contents = plan_file_with_unchanged_managed_wait_target().into_contents();
    contents.provider_configs = saved_provider_configs(Vec::new());
    contents.plan = Plan::new();
    contents.sorted_resources.clear();
    contents.current_states.clear();

    let recorded = contents
        .record(&WiringContext::new_for_test(Vec::new(), &[]))
        .expect("providerless evidence must record the built-in mock");
    assert!(recorded.schema_fingerprints().providers.is_empty());
    assert_eq!(
        recorded.provider_artifacts().get("mock"),
        Some(&RecordedProviderArtifact::BuiltIn),
        "the implicit built-in provider remains explicit artifact evidence"
    );

    let encoded = serde_json::to_value(&recorded).expect("zero-provider plan serializes");
    let decoded: PlanFile =
        serde_json::from_value(encoded).expect("zero-provider plan round-trips");
    assert_eq!(
        decoded.schema_fingerprints(),
        recorded.schema_fingerprints()
    );
    assert_eq!(decoded.provider_artifacts(), recorded.provider_artifacts());
}

#[test]
fn state_only_unloaded_provider_records_absent_schema_and_round_trips() {
    let retired = ResourceId::with_provider_identity("retired", "legacy.Widget", "gone", None);
    let mut contents = plan_file_with_unchanged_managed_wait_target().into_contents();
    contents.provider_configs = saved_provider_configs(Vec::new());
    contents.plan = Plan::new();
    contents.plan.add(Effect::Remove {
        id: carina_core::resource::ResolvedResourceId::new(retired.clone()),
    });
    contents.sorted_resources.clear();
    contents.current_states = vec![CurrentStateEntry::managed(
        retired,
        State::not_found(ResourceId::with_provider_identity(
            "retired",
            "legacy.Widget",
            "gone",
            None,
        )),
    )];

    let ctx = WiringContext::new_for_test(Vec::new(), &[]);
    let recorded = contents
        .record(&ctx)
        .expect("a state-only type from an unloaded provider must record");

    assert_eq!(
        recorded.schema_fingerprints().providers["retired"].schemas[0].fingerprint,
        crate::commands::plan::TypeSchemaFingerprint::Absent
    );
    assert!(!recorded.provider_artifacts().contains_key("retired"));
    assert_eq!(
        recorded.provider_artifacts().get("mock"),
        Some(&RecordedProviderArtifact::BuiltIn)
    );

    let encoded = serde_json::to_vec(&recorded).expect("state-only plan serializes");
    let decoded: PlanFile =
        serde_json::from_slice(&encoded).expect("state-only plan evidence round-trips");
    assert_eq!(
        decoded.schema_fingerprints(),
        recorded.schema_fingerprints()
    );
    assert_eq!(decoded.provider_artifacts(), recorded.provider_artifacts());
}

#[test]
fn recording_invalid_provider_artifact_inventory_does_not_panic() {
    let configured = ProviderConfig {
        name: "aws".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: Some("file:///missing/aws.wasm".parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    };
    let mut contents = plan_file_with_unchanged_managed_wait_target().into_contents();
    contents.provider_configs = saved_provider_configs(vec![configured.clone()]);
    contents.plan = Plan::new();
    contents.plan.add(Effect::Remove {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "aws",
            "legacy.Widget",
            "gone",
            None,
        )),
    });
    contents.sorted_resources.clear();
    contents.current_states.clear();

    let ctx = WiringContext::new_for_test(Vec::new(), &[configured]);
    let error = contents
        .record(&ctx)
        .expect_err("a configured provider without a loaded artifact must be rejected");
    assert_eq!(
        error,
        crate::commands::plan::ProviderEvidenceError::MissingProviderArtifact {
            provider: "aws".to_string()
        }
    );
}

/// Backend that captures the state written to it.
struct CapturingBackend {
    captured: Arc<Mutex<Option<StateFile>>>,
}

#[async_trait::async_trait]
impl StateBackend for CapturingBackend {
    async fn read_state(&self) -> carina_state::BackendResult<Option<LoadedState>> {
        Ok(Some(LoadedState::Pristine(StateFile::new())))
    }
    async fn read_state_bytes(
        &self,
    ) -> carina_state::BackendResult<Option<carina_state::RawState>> {
        carina_state::RawState::from_state(&StateFile::new(), "capturing test state").map(Some)
    }
    async fn write_state(&self, state: &StateFile) -> carina_state::BackendResult<()> {
        *self.captured.lock().unwrap() = Some(state.clone());
        Ok(())
    }
    async fn acquire_lock(&self, operation: &str) -> carina_state::BackendResult<LockInfo> {
        Ok(LockInfo::for_test(operation))
    }
    async fn release_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<()> {
        Ok(())
    }
    async fn renew_lock(&self, lock: &LockInfo) -> carina_state::BackendResult<LockInfo> {
        Ok(lock.renewed_for_test())
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
async fn persist_exports_only_clears_state_exports_when_params_empty() {
    let captured = Arc::new(Mutex::new(None));
    let backend = CapturingBackend {
        captured: captured.clone(),
    };
    let lock = LockInfo::for_test("apply");

    // Pre-existing state with stale exports
    let mut state_in = StateFile::new();
    state_in.insert_export_json_for_test("old", serde_json::json!("stale"));

    let result = crate::commands::apply::persist_exports_only(
        &backend,
        StateLockRef::Held(&lock),
        Some(state_in),
        &[],
        &DataSourceValues::empty(),
        &[],
        &checked_exports(&[]),
        &carina_core::schema::SchemaRegistry::new(),
        &[],
        &std::collections::HashMap::new(),
        &ProviderContext::default(),
    )
    .await;

    assert!(result.is_ok(), "persist_exports_only failed: {:?}", result);

    let written = captured.lock().unwrap();
    let state = written.as_ref().expect("state should be written");
    assert!(
        state.exports.is_empty(),
        "stale exports should be cleared when export_params is empty, got: {:?}",
        state.exports
    );
}

#[tokio::test]
async fn persist_exports_only_writes_state_with_new_exports() {
    use carina_core::parser::{InferredExportParam, TypeExpr};
    use carina_core::resource::{ConcreteValue, Value};

    let captured = Arc::new(Mutex::new(None));
    let backend = CapturingBackend {
        captured: captured.clone(),
    };
    let lock = LockInfo::for_test("apply");

    let export_params = vec![InferredExportParam {
        name: "account_id".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Concrete(ConcreteValue::String(
            "123456789012".into(),
        ))),
    }];

    let result = crate::commands::apply::persist_exports_only(
        &backend,
        StateLockRef::Held(&lock),
        Some(StateFile::new()),
        &[],
        &DataSourceValues::empty(),
        &[],
        &checked_exports(&export_params),
        &carina_core::schema::SchemaRegistry::new(),
        &[],
        &std::collections::HashMap::new(),
        &ProviderContext::default(),
    )
    .await;

    assert!(result.is_ok(), "persist_exports_only failed: {:?}", result);

    let written = captured.lock().unwrap();
    let state = written.as_ref().expect("state should be written");
    assert_eq!(state.exports.len(), 1);
    assert_eq!(
        state
            .exports
            .get("account_id")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::json!("123456789012"))
    );
}

#[tokio::test]
async fn persist_exports_only_saves_state_then_errors_for_unresolved_export() {
    use carina_core::parser::{InferredExportParam, TypeExpr};
    use carina_core::resource::AccessPath;

    let captured = Arc::new(Mutex::new(None));
    let backend = CapturingBackend {
        captured: captured.clone(),
    };
    let lock = LockInfo::for_test("apply");
    let mut state_in = StateFile::new();
    state_in.insert_export_json_for_test("role_arn", serde_json::json!("prior-arn"));
    let export_params = vec![InferredExportParam {
        name: "role_arn".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("role", "web_acl_arn"),
        })),
    }];

    let result = crate::commands::apply::persist_exports_only(
        &backend,
        StateLockRef::Held(&lock),
        Some(state_in),
        &[],
        &DataSourceValues::empty(),
        &[],
        &checked_exports(&export_params),
        &carina_core::schema::SchemaRegistry::new(),
        &[],
        &HashMap::new(),
        &ProviderContext::default(),
    )
    .await;

    assert!(
        result.is_err(),
        "an unresolved export must fail only after state persistence"
    );
    let written = captured.lock().unwrap();
    let state = written.as_ref().expect("state should be written");
    assert_eq!(state.serial, 1, "state save must happen before the error");
    assert_eq!(
        state
            .exports
            .get("role_arn")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::json!("prior-arn")),
        "a skipped export must preserve its prior persisted value",
    );
}

/// Regression test for #2932: when the user removes the entire
/// `exports {}` block (so `parsed.export_params` is empty) but the
/// plan has resource changes, `finalize_apply` must still drop
/// stale entries from `state.exports`. Previously a guard
/// `if !input.export_params.is_empty()` skipped the assignment,
/// so subsequent plans kept re-emitting the same `- exports`
/// removal lines and the state never converged.
#[tokio::test]
async fn finalize_apply_clears_state_exports_when_params_empty() {
    let captured = Arc::new(Mutex::new(None));
    let backend = CapturingBackend {
        captured: captured.clone(),
    };
    let lock = LockInfo::for_test("apply");

    // Pre-existing state with exports left over from a previous
    // apply whose `exports {}` block has since been removed.
    let mut state_in = StateFile::new();
    state_in.insert_export_json_for_test(
        "zone_id".to_string(),
        serde_json::json!("Z0788351HTU0H3UKV38I"),
    );
    state_in.insert_export_json_for_test(
        "nameservers".to_string(),
        serde_json::json!(["ns-1572.awsdns-04.co.uk"]),
    );

    let result = ApplyResult {
        success_count: 0,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        managed_current: HashMap::new(),
        data_source_states: HashMap::new(),
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    };

    let op_result = finalize_apply(
        FinalizeApplyInput {
            result: &result,
            state_base: crate::commands::shared::incremental_state_saver::FinalStateBase::for_test(
                Some(state_in),
            ),
            state_save_inputs: final_state_save_inputs(&[]),
            data_source_values: &DataSourceValues::empty(),
            backend: &backend,
            lock: StateLockRef::Held(&lock),
            export_params: Some(&checked_exports(&[])),
            provider_context: &ProviderContext::default(),
            wait_aliases: &[],
            pre_resolve_compositions: &[],
        },
        StatePersistence::Normal,
    )
    .await;

    assert!(op_result.is_ok(), "finalize_apply failed: {:?}", op_result);

    let written = captured.lock().unwrap();
    let state = written.as_ref().expect("state should be written");
    assert!(
        state.exports.is_empty(),
        "stale exports must be cleared when export_params is empty, got: {:?}",
        state.exports
    );
}

/// `apply --plan plan.json` does not parse the source `.crn`, so it
/// has no `export_params` to feed `finalize_apply` and passes `None`
/// instead. In that case `state.exports` must be **preserved** — the
/// previous source-driven apply already populated them, and clearing
/// here would silently drop the user's exports on every apply-from-plan
/// (regression of the #2932 fix). The next source-driven `carina
/// apply` reconciles them.
#[tokio::test]
async fn finalize_apply_preserves_state_exports_when_params_none() {
    let captured = Arc::new(Mutex::new(None));
    let backend = CapturingBackend {
        captured: captured.clone(),
    };
    let lock = LockInfo::for_test("apply");

    let mut state_in = StateFile::new();
    state_in.insert_export_json_for_test("vpc_id", serde_json::json!("vpc-0abc123"));

    let result = ApplyResult {
        success_count: 0,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        managed_current: HashMap::new(),
        data_source_states: HashMap::new(),
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    };

    let op_result = finalize_apply(
        FinalizeApplyInput {
            result: &result,
            state_base: crate::commands::shared::incremental_state_saver::FinalStateBase::for_test(
                Some(state_in),
            ),
            state_save_inputs: final_state_save_inputs(&[]),
            data_source_values: &DataSourceValues::empty(),
            backend: &backend,
            lock: StateLockRef::Held(&lock),
            export_params: None,
            provider_context: &ProviderContext::default(),
            wait_aliases: &[],
            pre_resolve_compositions: &[],
        },
        StatePersistence::Normal,
    )
    .await;

    assert!(op_result.is_ok(), "finalize_apply failed: {:?}", op_result);

    let written = captured.lock().unwrap();
    let state = written.as_ref().expect("state should be written");
    assert_eq!(
        state
            .exports
            .get("vpc_id")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::json!("vpc-0abc123")),
        "state.exports must survive apply-from-plan (export_params: None), got: {:?}",
        state.exports
    );
}

#[tokio::test]
async fn finalize_apply_persists_successful_state_when_one_export_is_unresolved() {
    use carina_core::parser::{InferredExportParam, TypeExpr};
    use carina_core::resource::AccessPath;

    let captured = Arc::new(Mutex::new(None));
    let backend = CapturingBackend {
        captured: captured.clone(),
    };
    let lock = LockInfo::for_test("apply");

    let mut resource_a = Resource::with_provider("mock", "test.resource", "a", None);
    resource_a.binding = Some("a".to_string());
    let mut resource_b = Resource::with_provider("mock", "test.resource", "b", None);
    resource_b.binding = Some("b".to_string());
    let sorted_resources = vec![resolved(resource_a.clone()), resolved(resource_b)];

    let a_id = resource_a.id.clone();
    let mut a_attrs = HashMap::new();
    a_attrs.insert(
        "id".to_string(),
        Value::Concrete(ConcreteValue::String("a-id".into())),
    );
    let a_state = State::existing(a_id.clone(), a_attrs).with_identifier("a-id");

    let result = ApplyResult {
        success_count: 1,
        failure_count: 1,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: HashMap::from([(a_id.clone(), complete_create(a_state.clone()))]),
        runtime_synthesized_resources: Vec::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        managed_current: HashMap::from([(a_id, Current::from_state(a_state))]),
        data_source_states: HashMap::new(),
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    };

    let export_params = vec![
        InferredExportParam {
            name: "ax".to_string(),
            type_expr: TypeExpr::Unknown,
            value: Some(Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("a", "id"),
            })),
        },
        InferredExportParam {
            name: "bx".to_string(),
            type_expr: TypeExpr::Unknown,
            value: Some(Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("b", "id"),
            })),
        },
    ];

    let op_result = finalize_apply(
        FinalizeApplyInput {
            result: &result,
            state_base: crate::commands::shared::incremental_state_saver::FinalStateBase::for_test(
                Some(StateFile::new()),
            ),
            state_save_inputs: final_state_save_inputs(&sorted_resources),
            data_source_values: &DataSourceValues::empty(),
            backend: &backend,
            lock: StateLockRef::Held(&lock),
            export_params: Some(&checked_exports(&export_params)),
            provider_context: &ProviderContext::default(),
            wait_aliases: &[],
            pre_resolve_compositions: &[],
        },
        StatePersistence::Normal,
    )
    .await;

    let skipped =
        op_result.expect("unresolved export from failed resource must not abort state writeback");
    assert_eq!(
        skipped.len(),
        1,
        "the caller must receive the skipped-export summary after state save",
    );
    assert!(
        result.failure_count > 0,
        "the modeled apply result must still represent a partial failure"
    );

    let written = captured.lock().unwrap();
    let state = written.as_ref().expect("state should be written");
    assert!(
        state.find_resource("mock", "test.resource", "a").is_some(),
        "successful resource A must be persisted"
    );
    assert!(
        state.find_resource("mock", "test.resource", "b").is_none(),
        "failed resource B must not be persisted"
    );
    assert_eq!(
        state
            .exports
            .get("ax")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::json!("a-id"))
    );
    assert!(
        !state.exports.contains_key("bx"),
        "unresolved export bx must be omitted"
    );
    assert_eq!(
        state.serial, 1,
        "writeback should advance serial after persisting partial apply state"
    );
}

#[tokio::test]
async fn finalize_apply_persists_resource_state_before_reporting_export_error() {
    use carina_core::parser::{InferredExportParam, TypeExpr};

    let captured = Arc::new(Mutex::new(None));
    let backend = CapturingBackend {
        captured: captured.clone(),
    };
    let lock = LockInfo::for_test("apply");

    let mut resource = Resource::with_provider("mock", "test.resource", "created", None);
    resource.binding = Some("created".to_string());
    let sorted_resources = vec![resolved(resource.clone())];
    let resource_id = resource.id.clone();
    let state = State::existing(
        resource_id.clone(),
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("created".into())),
        )]),
    )
    .with_identifier("created-id");
    let result = ApplyResult {
        success_count: 1,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: HashMap::from([(resource_id.clone(), complete_create(state.clone()))]),
        runtime_synthesized_resources: Vec::new(),
        permanent_name_overrides: HashMap::new(),
        successfully_deleted: HashSet::new(),
        managed_current: HashMap::from([(resource_id, Current::from_state(state))]),
        data_source_states: HashMap::new(),
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    };
    let export_params = vec![InferredExportParam {
        name: "broken".to_string(),
        type_expr: TypeExpr::Float,
        value: Some(Value::Concrete(ConcreteValue::Float(f64::NAN))),
    }];

    let error = finalize_apply(
        FinalizeApplyInput {
            result: &result,
            state_base: crate::commands::shared::incremental_state_saver::FinalStateBase::for_test(
                Some(StateFile::new()),
            ),
            state_save_inputs: final_state_save_inputs(&sorted_resources),
            data_source_values: &DataSourceValues::empty(),
            backend: &backend,
            lock: StateLockRef::Held(&lock),
            export_params: Some(&checked_exports(&export_params)),
            provider_context: &ProviderContext::default(),
            wait_aliases: &[],
            pre_resolve_compositions: &[],
        },
        StatePersistence::Normal,
    )
    .await
    .expect_err("non-finite export must still be reported");
    assert!(error.to_string().contains("non-finite"), "{error}");

    let written = captured.lock().unwrap();
    let state = written
        .as_ref()
        .expect("successful resource state must be persisted despite the export error");
    assert!(
        state
            .find_resource("mock", "test.resource", "created")
            .is_some(),
        "created resource missing from persisted state: {state:#?}",
    );
}
