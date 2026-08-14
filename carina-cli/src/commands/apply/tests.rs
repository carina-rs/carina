use super::*;
use carina_core::binding_index::PreApplyInputs;
use carina_core::override_aware::OverrideAwareResources;
use carina_core::provider::{
    BoxFuture, CreateRequest, DeleteRequest, NoopNormalizer, Provider, ProviderError,
    ProviderFactory, ProviderNormalizer, ProviderResult, ReadRequest, UpdateRequest,
};
use carina_core::resource::{
    DataSource, DeferredValue, ResolvedDataSource, ResolvedResource, Resource, ResourceId,
};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
use carina_state::{DeposedInstance, DeposedKey, NameOverride, ResourceState};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "tests/cancellation_fixture.rs"]
mod cancellation_fixture;

use cancellation_fixture::ApplyCancellationFixture;

use crate::commands::shared::cancellation_test_support::MOCK_PROVIDER_ENV_LOCK;

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

fn state_file_from_json(json: serde_json::Value) -> StateFile {
    carina_state::check_and_migrate(&json.to_string())
        .expect("test state should pass the version gate")
        .into_state()
}

struct FailBCreateFactory;

impl ProviderFactory for FailBCreateFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Mock provider with B create failure"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { Ok(Box::new(FailBCreateProvider) as Box<dyn Provider>) })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
        Box::pin(async { Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer> })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![
            ResourceSchema::new("test.resource")
                .attribute(AttributeSchema::new("id", AttributeType::string()))
                .attribute(AttributeSchema::new("name", AttributeType::string())),
        ]
    }
}

struct FailBCreateProvider;

impl Provider for FailBCreateProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move { Ok(State::not_found(id)) })
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(State::not_found(id)) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::CreateOutcome>> {
        let id = id.clone();
        Box::pin(async move {
            if id.identity_or_empty() == "b" {
                return Err(ProviderError::api_error("create failed").for_resource(id));
            }
            let resource = request.resource.as_resource().clone();
            Ok(carina_core::provider::CreateOutcome::Success {
                state: State::existing(id, resource.resolved_attributes())
                    .with_identifier("mock-id"),
            })
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        let id = id.clone();
        Box::pin(async move { Err(ProviderError::internal("unexpected update").for_resource(id)) })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move { Err(ProviderError::internal("unexpected delete").for_resource(id)) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Clone, Default)]
struct ApplyTimeReadShared {
    created_roles: Arc<Mutex<HashMap<String, HashMap<String, Value>>>>,
    operations: Arc<Mutex<Vec<String>>>,
    consumer_description: Arc<Mutex<Option<Value>>>,
    read_calls: Arc<AtomicUsize>,
}

struct ApplyTimeReadFactory {
    shared: ApplyTimeReadShared,
}

impl ProviderFactory for ApplyTimeReadFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Mock provider for apply-time data-source reads"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        let shared = self.shared.clone();
        Box::pin(async move { Ok(Box::new(ApplyTimeReadProvider { shared }) as Box<dyn Provider>) })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
        Box::pin(async { Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer> })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![
            ResourceSchema::new("iam.Role")
                .attribute(AttributeSchema::new("role_name", AttributeType::string()).create_only())
                .attribute(AttributeSchema::new("path", AttributeType::string()).create_only())
                .attribute(
                    AttributeSchema::new("max_session_duration", AttributeType::int())
                        .create_only(),
                )
                .attribute(AttributeSchema::new("description", AttributeType::string())),
            ResourceSchema::new("iam.Roles")
                .attribute(AttributeSchema::new("name_regex", AttributeType::string()))
                .attribute(AttributeSchema::new(
                    "names",
                    AttributeType::list(AttributeType::string()),
                ))
                .as_data_source(),
        ]
    }
}

struct ApplyTimeReadProvider {
    shared: ApplyTimeReadShared,
}

#[derive(Clone, Default)]
struct ApplyCascadeShared {
    creates: Arc<Mutex<ApplyCascadeCreateCalls>>,
    deletes: Arc<Mutex<ApplyCascadeDeleteCalls>>,
    fail_delete_identifiers: Arc<Mutex<HashSet<String>>>,
}

type ApplyCascadeCreateCalls = Vec<(String, HashMap<String, Value>)>;
type ApplyCascadeDeleteCalls = Vec<(String, String)>;

impl ApplyCascadeShared {
    fn create_calls(&self) -> ApplyCascadeCreateCalls {
        self.creates.lock().unwrap().clone()
    }

    fn delete_calls(&self) -> ApplyCascadeDeleteCalls {
        self.deletes.lock().unwrap().clone()
    }

    fn fail_delete_identifier(&self, identifier: &str) {
        self.fail_delete_identifiers
            .lock()
            .unwrap()
            .insert(identifier.to_string());
    }
}

struct ApplyCascadeAwsccFactory {
    shared: ApplyCascadeShared,
}

impl ProviderFactory for ApplyCascadeAwsccFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWSCC apply cascade test provider"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        let shared = self.shared.clone();
        Box::pin(async move { Ok(Box::new(ApplyCascadeProvider { shared }) as Box<dyn Provider>) })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
        Box::pin(async { Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer> })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![
            ResourceSchema::new("ec2.Vpc")
                .attribute(
                    AttributeSchema::new("cidr_block", AttributeType::string()).create_only(),
                )
                .attribute(AttributeSchema::new("vpc_id", AttributeType::string()).read_only())
                .with_coexisting_replacement(),
            ResourceSchema::new("ec2.Subnet")
                .attribute(AttributeSchema::new("vpc_id", AttributeType::string()).create_only())
                .attribute(
                    AttributeSchema::new("cidr_block", AttributeType::string()).create_only(),
                )
                .attribute(
                    AttributeSchema::new("availability_zone", AttributeType::string())
                        .create_only(),
                )
                .attribute(AttributeSchema::new("subnet_id", AttributeType::string()).read_only())
                .with_coexisting_replacement(),
        ]
    }
}

struct ApplyCascadeProvider {
    shared: ApplyCascadeShared,
}

impl Provider for ApplyCascadeProvider {
    fn name(&self) -> &str {
        "awscc"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.map(ToOwned::to_owned);
        Box::pin(async move {
            match (id.resource_type.as_str(), identifier.as_deref()) {
                ("ec2.Vpc", Some("vpc-new")) => Ok(State::existing(
                    id,
                    HashMap::from([
                        ("cidr_block".to_string(), string_value("10.221.0.0/16")),
                        ("vpc_id".to_string(), string_value("vpc-new")),
                    ]),
                )
                .with_identifier("vpc-new")),
                ("ec2.Vpc", Some("vpc-old" | "vpc-older" | "vpc-oldest")) => Ok(State::existing(
                    id,
                    HashMap::from([
                        ("cidr_block".to_string(), string_value("10.220.0.0/16")),
                        (
                            "vpc_id".to_string(),
                            string_value(identifier.as_deref().unwrap()),
                        ),
                    ]),
                )
                .with_identifier(identifier.as_deref().unwrap())),
                ("ec2.Subnet", Some("subnet-old")) => Ok(State::existing(
                    id,
                    HashMap::from([
                        ("vpc_id".to_string(), string_value("vpc-old")),
                        ("cidr_block".to_string(), string_value("10.220.1.0/24")),
                        (
                            "availability_zone".to_string(),
                            string_value("ap-northeast-1c"),
                        ),
                    ]),
                )
                .with_identifier("subnet-old")),
                _ => Ok(State::not_found(id)),
            }
        })
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(State::not_found(id)) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::CreateOutcome>> {
        let id = id.clone();
        let attrs = request.resource.as_resource().resolved_attributes();
        self.shared
            .creates
            .lock()
            .unwrap()
            .push((id.to_string(), attrs.clone()));

        Box::pin(async move {
            match id.resource_type.as_str() {
                "ec2.Vpc" => Ok(carina_core::provider::CreateOutcome::Success {
                    state: State::existing(
                        id,
                        HashMap::from([("vpc_id".to_string(), string_value("vpc-new"))]),
                    )
                    .with_identifier("vpc-new"),
                }),
                "ec2.Subnet" => Ok(carina_core::provider::CreateOutcome::Success {
                    state: State::existing(
                        id,
                        HashMap::from([("subnet_id".to_string(), string_value("subnet-new"))]),
                    )
                    .with_identifier("subnet-new"),
                }),
                _ => Err(ProviderError::internal(format!("unexpected create {id}"))),
            }
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        let id = id.clone();
        Box::pin(async move { Err(ProviderError::internal("unexpected update").for_resource(id)) })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let shared = self.shared.clone();
        Box::pin(async move {
            shared
                .deletes
                .lock()
                .unwrap()
                .push((id.to_string(), identifier.clone()));
            if shared
                .fail_delete_identifiers
                .lock()
                .unwrap()
                .contains(&identifier)
            {
                Err(
                    ProviderError::api_error(format!("delete failed for {identifier}"))
                        .for_resource(id),
                )
            } else {
                Ok(())
            }
        })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> Vec<String> {
        Vec::new()
    }
}

fn string_value(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.to_string()))
}

fn apply_cascade_crn(state_path: &std::path::Path, include_subnet: bool) -> String {
    let subnet = if include_subnet {
        r#"
awscc.ec2.Subnet {
  vpc_id = vpc.vpc_id
  cidr_block = "10.221.1.0/24"
  availability_zone = "ap-northeast-1c"
  directives {
    create_before_destroy = true
  }
}
"#
    } else {
        ""
    };
    format!(
        r#"backend local {{ path = "{}" }}
provider awscc {{}}

let vpc = awscc.ec2.Vpc {{
  cidr_block = "10.221.0.0/16"
  directives {{
    create_before_destroy = true
  }}
}}
{subnet}"#,
        state_path.display()
    )
}

fn concrete_subnet_identity(ctx: &WiringContext, providers: &[ProviderConfig]) -> String {
    let mut resources = vec![
        Resource::with_provider("awscc", "ec2.Subnet", "", None)
            .with_attribute("vpc_id", string_value("vpc-old"))
            .with_attribute("cidr_block", string_value("10.220.1.0/24"))
            .with_attribute("availability_zone", string_value("ap-northeast-1c")),
    ];
    let canonical =
        carina_core::value::canonicalize_resources_with_schemas(&mut resources, ctx.schemas());
    let errors = crate::wiring::compute_anonymous_identifiers_with_ctx(ctx, canonical, providers);
    assert!(errors.is_empty(), "state id setup failed: {errors:?}");
    resources[0].id.identity_or_empty().to_string()
}

fn seed_apply_cascade_state(
    fixture: &ApplyCancellationFixture,
    ctx: &WiringContext,
    providers: &[ProviderConfig],
    include_subnet: bool,
) {
    let mut vpc_state = ResourceState::new("ec2.Vpc", "vpc", "awscc")
        .with_identifier("vpc-old")
        .with_attribute("cidr_block", serde_json::json!("10.220.0.0/16"))
        .with_attribute("vpc_id", serde_json::json!("vpc-old"));
    vpc_state.binding = Some("vpc".to_string());

    let mut state_file = carina_state::StateFile::new();
    state_file
        .upsert_resource(vpc_state)
        .expect("test state setup must be valid");

    if include_subnet {
        let mut subnet_state = ResourceState::new(
            "ec2.Subnet",
            concrete_subnet_identity(ctx, providers),
            "awscc",
        )
        .with_identifier("subnet-old")
        .with_attribute("vpc_id", serde_json::json!("vpc-old"))
        .with_attribute("cidr_block", serde_json::json!("10.220.1.0/24"))
        .with_attribute("availability_zone", serde_json::json!("ap-northeast-1c"));
        subnet_state.dependency_bindings.insert("vpc".to_string());
        state_file
            .upsert_resource(subnet_state)
            .expect("test state setup must be valid");
    }

    fixture.write_state(&state_file);
}

fn seed_apply_cascade_state_with_deposed(
    fixture: &ApplyCancellationFixture,
    deposed_identifiers: &[&str],
) {
    let mut vpc_state = ResourceState::new("ec2.Vpc", "vpc", "awscc")
        .with_identifier("vpc-new")
        .with_attribute("cidr_block", serde_json::json!("10.221.0.0/16"))
        .with_attribute("vpc_id", serde_json::json!("vpc-new"));
    vpc_state.binding = Some("vpc".to_string());
    for identifier in deposed_identifiers {
        vpc_state.deposed.push(DeposedInstance {
            key: DeposedKey::new_unique(),
            identifier: (*identifier).to_string(),
            provider_instance: None,
            attributes: HashMap::from([
                ("cidr_block".to_string(), serde_json::json!("10.220.0.0/16")),
                ("vpc_id".to_string(), serde_json::json!(identifier)),
            ]),
            dependency_bindings: BTreeSet::new(),
        });
    }

    let mut state_file = carina_state::StateFile::new();
    state_file
        .upsert_resource(vpc_state)
        .expect("test state setup must be valid");
    fixture.write_state(&state_file);
}

fn deposed_identifiers(state_path: &std::path::Path) -> Vec<String> {
    let raw = std::fs::read_to_string(state_path).expect("state file must be written");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("state must be valid JSON");
    let vpc = state_json_resource(&state, "awscc", "ec2.Vpc", "vpc");
    vpc.get("deposed")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|entry| {
            entry["identifier"]
                .as_str()
                .expect("deposed identifier must be a string")
                .to_string()
        })
        .collect()
}

async fn run_apply_cascade_fixture(
    fixture: &ApplyCancellationFixture,
    shared: &ApplyCascadeShared,
) -> Result<Option<Duration>, AppError> {
    let ctx = WiringContext::new(vec![Box::new(ApplyCascadeAwsccFactory {
        shared: shared.clone(),
    })]);
    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let observer_factory = fixture.observer_factory();
    run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        true,
        fixture.backend(),
        None,
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
}

fn state_json_resource<'a>(
    state: &'a serde_json::Value,
    provider: &str,
    resource_type: &str,
    identity: &str,
) -> &'a serde_json::Value {
    state["resources"]
        .as_array()
        .expect("state resources must be an array")
        .iter()
        .find(|row| {
            row["provider"] == provider
                && row["resource_type"] == resource_type
                && row["identity"] == identity
        })
        .unwrap_or_else(|| panic!("missing state row {provider}.{resource_type}.{identity}"))
}

fn assert_vpc_new_current_and_old_deposed(state_path: &std::path::Path) {
    let raw = std::fs::read_to_string(state_path).expect("state file must be written");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("state must be valid JSON");
    let vpc = state_json_resource(&state, "awscc", "ec2.Vpc", "vpc");
    assert_eq!(vpc["identifier"], serde_json::json!("vpc-new"));

    let deposed = vpc
        .get("deposed")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("expected vpc-old to be recorded as deposed; row: {vpc:#?}"));
    assert_eq!(deposed.len(), 1, "expected exactly one deposed VPC");
    assert_eq!(deposed[0]["identifier"], serde_json::json!("vpc-old"));
}

impl Provider for ApplyTimeReadProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let shared = self.shared.clone();
        Box::pin(async move {
            let roles = shared.created_roles.lock().unwrap();
            if let Some(attrs) = roles.get(id.identity_or_empty()) {
                Ok(State::existing(id, attrs.clone()).with_identifier("mock-id"))
            } else {
                Ok(State::not_found(id))
            }
        })
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        let shared = self.shared.clone();
        Box::pin(async move {
            shared.read_calls.fetch_add(1, Ordering::SeqCst);
            shared
                .operations
                .lock()
                .unwrap()
                .push(format!("read:{}", resource.id.identity_or_empty()));

            let expected = resource
                .attributes
                .get("name_regex")
                .and_then(value_string)
                .and_then(exact_regex_literal)
                .unwrap_or_default();

            let names = shared
                .created_roles
                .lock()
                .unwrap()
                .values()
                .filter_map(|attrs| attrs.get("role_name").and_then(value_string))
                .filter(|role_name| role_name == &expected)
                .map(|role_name| Value::Concrete(ConcreteValue::String(role_name)))
                .collect();

            Ok(State::existing(
                resource.id,
                HashMap::from([(
                    "names".to_string(),
                    Value::Concrete(ConcreteValue::List(names)),
                )]),
            )
            .with_identifier("mock-id"))
        })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::CreateOutcome>> {
        let id = id.clone();
        let shared = self.shared.clone();
        Box::pin(async move {
            let mut attrs = request.resource.as_resource().resolved_attributes();
            if id.resource_type == "iam.Role" {
                attrs
                    .entry("path".to_string())
                    .or_insert_with(|| Value::Concrete(ConcreteValue::String("/".to_string())));
                attrs
                    .entry("max_session_duration".to_string())
                    .or_insert_with(|| Value::Concrete(ConcreteValue::Int(3600)));
            }
            if id.identity_or_empty() == "consumer" {
                *shared.consumer_description.lock().unwrap() = attrs.get("description").cloned();
            }
            shared
                .created_roles
                .lock()
                .unwrap()
                .insert(id.identity_or_empty().to_string(), attrs.clone());
            shared
                .operations
                .lock()
                .unwrap()
                .push(format!("create:{}", id.identity_or_empty()));

            Ok(carina_core::provider::CreateOutcome::Success {
                state: State::existing(id, attrs).with_identifier("mock-id"),
            })
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        let id = id.clone();
        Box::pin(async move { Err(ProviderError::internal("unexpected update").for_resource(id)) })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move { Err(ProviderError::internal("unexpected delete").for_resource(id)) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> Vec<String> {
        Vec::new()
    }
}

fn value_string(value: &Value) -> Option<String> {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn exact_regex_literal(value: String) -> Option<String> {
    value
        .strip_prefix('^')
        .and_then(|value| value.strip_suffix('$'))
        .map(ToString::to_string)
}

fn apply_time_read_crn(state_path: &std::path::Path) -> String {
    format!(
        r#"backend local {{ path = "{}" }}
provider mock {{}}

let target_role = mock.iam.Role {{
  role_name = "carina-acc-test-3666-target"
}}

let roles = read mock.iam.Roles {{
  name_regex = "^${{target_role.role_name}}$"
}}

let consumer = mock.iam.Role {{
  role_name = "carina-acc-test-3666-consumer"
  description = roles.names[0]
}}
"#,
        state_path.display()
    )
}

fn module_data_source_fixture() -> (ApplyCancellationFixture, ApplyTimeReadShared) {
    let fixture = ApplyCancellationFixture::new();
    let module_dir = fixture.config_path().join("registry");
    std::fs::create_dir_all(&module_dir).expect("create module fixture directory");
    std::fs::write(
        module_dir.join("read.crn"),
        r#"
arguments {
  consumer_name: String
}

let caller = read mock.iam.Roles {
  name_regex = "^existing$"
}
"#,
    )
    .expect("write module data-source fixture");
    std::fs::write(
        module_dir.join("resource.crn"),
        r#"
let consumer = mock.iam.Role {
  role_name  = consumer_name
  description = "role:${caller.names[0]}"
}
"#,
    )
    .expect("write module resource fixture");

    let root = format!(
        r#"backend local {{ path = "{}" }}
provider mock {{}}

let registry = use {{
  source = "./registry"
}}

let registry_publish = registry {{
  consumer_name = "module-consumer"
}}
"#,
        fixture.state_path().display()
    );
    let fixture = fixture.with_raw_config(root);

    let shared = ApplyTimeReadShared::default();
    shared.created_roles.lock().unwrap().insert(
        "existing".to_string(),
        HashMap::from([("role_name".to_string(), string_value("existing"))]),
    );

    (fixture, shared)
}

fn load_and_resolve_module_data_source_fixture(
    fixture: &ApplyCancellationFixture,
    shared: &ApplyTimeReadShared,
) -> (
    carina_core::parser::InferredFile,
    carina_core::parser::ParsedFile,
) {
    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("module data-source fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        get_base_dir(fixture.config_path()),
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "module data-source fixture must validate, got: {validation_errors:?}"
    );

    (parsed, unresolved_parsed)
}

#[tokio::test]
async fn plan_reads_module_data_source_and_resolves_consumer_interpolation() {
    use carina_core::effect::Effect;

    let (fixture, shared) = module_data_source_fixture();
    let (parsed, unresolved_parsed) =
        load_and_resolve_module_data_source_fixture(&fixture, &shared);

    assert!(
        unresolved_parsed.data_sources.is_empty(),
        "the pre-expansion snapshot must not contain module declarations"
    );
    assert_eq!(
        parsed.data_sources[0].binding.as_deref(),
        Some("registry_publish.caller"),
        "module resolution must prefix the data-source binding"
    );

    let ctx = WiringContext::new(vec![Box::new(ApplyTimeReadFactory {
        shared: shared.clone(),
    })]);
    let plan_ctx = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        &ctx,
        &parsed,
        &unresolved_parsed.resources,
        &None,
        true,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
        get_base_dir(fixture.config_path()),
    )
    .await
    .expect("module data-source plan should build");

    assert_eq!(
        plan_ctx
            .data_sources
            .iter()
            .map(|data_source| data_source.id.identity_or_empty())
            .collect::<Vec<_>>(),
        ["registry_publish.caller"],
        "the expanded module data source must be in the planning set"
    );
    let consumer = plan_ctx
        .plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::Create(resource)
                if resource.id.identity_or_empty() == "registry_publish.consumer" =>
            {
                Some(resource)
            }
            _ => None,
        })
        .expect("module consumer must have a create effect");
    assert_eq!(
        consumer.attributes.get("description"),
        Some(&string_value("role:existing")),
        "the module-scoped interpolation must resolve before planning"
    );
    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        1,
        "planning must read the module data source exactly once"
    );
}

#[tokio::test]
async fn apply_reads_module_data_source_and_resolves_consumer_interpolation() {
    let (fixture, shared) = module_data_source_fixture();
    let (mut parsed, unresolved_parsed) =
        load_and_resolve_module_data_source_fixture(&fixture, &shared);
    let ctx = WiringContext::new(vec![Box::new(ApplyTimeReadFactory {
        shared: shared.clone(),
    })]);
    let observer_factory = fixture.observer_factory();

    run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        true,
        fixture.backend(),
        None,
        get_base_dir(fixture.config_path()),
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect("apply must read the module data source before creating its consumer");

    assert_eq!(
        shared.operations.lock().unwrap().as_slice(),
        [
            "read:registry_publish.caller",
            "create:registry_publish.consumer"
        ],
        "apply must include the prefixed read before its module consumer"
    );
    let created_roles = shared.created_roles.lock().unwrap();
    let consumer = created_roles
        .get("registry_publish.consumer")
        .expect("module consumer must be created");
    assert_eq!(
        consumer.get("description"),
        Some(&string_value("role:existing")),
        "apply must not send a deferred interpolation to the provider"
    );
}

#[test]
fn total_apply_line_formats_duration() {
    assert_eq!(
        format_total_apply_line(Duration::from_secs(5)),
        "Done in 5.0s."
    );
}

#[test]
fn apply_parallelism_default_is_eight() {
    assert_eq!(crate::DEFAULT_PARALLELISM.get(), 8);
}

fn legacy_override_state() -> StateFile {
    let mut state = StateFile::new();
    let mut resource =
        ResourceState::new("test.resource", "legacy", "mock").with_identifier("legacy-id");
    resource.name_overrides.insert(
        "name".to_string(),
        NameOverride {
            temp_value: "legacy-temp".to_string(),
            original_value: None,
        },
    );
    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");
    state
}

fn plan_with_name_override(id: &ResourceId, temp_value: &str, original_value: &str) -> Plan {
    serde_json::from_value(serde_json::json!({
        "effects": [],
        "permanent_name_overrides": [
            {
                "resource_id": id,
                "attribute": "name",
                "temp_value": temp_value,
                "original_value": original_value
            }
        ]
    }))
    .expect("plan with permanent name override should deserialize")
}

#[test]
fn export_only_fast_path_is_disabled_when_deferred_reads_exist() {
    let mut data_source = DataSource::with_provider("mock", "iam.Roles", "roles", None);
    data_source.binding = Some("roles".to_string());
    data_source.attributes.insert(
        "name_regex".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: carina_core::resource::AccessPath::new("target_role", "role_name"),
        }),
    );
    let mut plan = Plan::new();
    plan.add(carina_core::effect::Effect::Read {
        resource: carina_core::resource::ResolvedDataSource::new(data_source.clone()),
    });
    let mut target = Resource::with_provider("mock", "iam.Role", "target_role", None);
    target.binding = Some("target_role".to_string());
    let deferred_reads =
        deferred_data_source_reads_from_data_sources(&[data_source], &[target], &HashMap::new());

    assert!(
        !can_use_export_only_fast_path(&plan, &deferred_reads),
        "deferred reads are executable effects and must not be silently skipped"
    );
}

#[test]
fn saved_plan_classifies_folded_data_source_input_from_preserved_dependency() {
    let mut data_source =
        DataSource::with_provider("mock", "iam.Roles", "registry_publish.roles", None);
    data_source.binding = Some("registry_publish.roles".to_string());
    data_source
        .attributes
        .insert("name_regex".to_string(), string_value("^target-role$"));
    data_source
        .dependency_bindings
        .insert("registry_publish.target_role".to_string());
    let id = data_source.id.clone();
    let mut target =
        Resource::with_provider("mock", "iam.Role", "registry_publish.target_role", None);
    target.binding = Some("registry_publish.target_role".to_string());

    let deferred_reads =
        deferred_data_source_reads_from_data_sources(&[data_source], &[target], &HashMap::new());

    assert!(
        deferred_reads.contains(&id),
        "saved-plan apply must preserve a deferred read after parsing folded its input value"
    );
}

#[tokio::test]
async fn saved_plan_apply_reconstructs_and_dispatches_deferred_data_source_read() {
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mock_state_path = tmp.path().join("mock-provider-state.json");
    std::fs::write(
        &mock_state_path,
        serde_json::json!({
            "test.resource.lookup": {
                "name": "target"
            }
        })
        .to_string(),
    )
    .expect("mock provider state");

    unsafe {
        std::env::set_var("CARINA_MOCK_STATE_FILE", &mock_state_path);
        std::env::set_var("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1");
    }

    let target_id = ResourceId::with_provider_identity("mock", "test.resource", "target", None);
    let mut target = Resource::with_provider("mock", "test.resource", "target", None);
    target.id = target_id.clone();
    target.binding = Some("target".to_string());
    target.set_attr(
        "name",
        Value::Concrete(ConcreteValue::String("target".to_string())),
    );

    let lookup_id = ResourceId::with_provider_identity("mock", "test.resource", "lookup", None);
    let mut lookup = DataSource::with_provider("mock", "test.resource", "lookup", None);
    lookup.id = lookup_id.clone();
    lookup.binding = Some("lookup".to_string());
    lookup.set_attr(
        "filter",
        Value::Deferred(DeferredValue::ResourceRef {
            path: carina_core::resource::AccessPath::new("target", "name"),
        }),
    );

    let consumer_id = ResourceId::with_provider_identity("mock", "test.resource", "consumer", None);
    let mut consumer = Resource::with_provider("mock", "test.resource", "consumer", None);
    consumer.id = consumer_id.clone();
    consumer.binding = Some("consumer".to_string());
    consumer.set_attr(
        "name",
        Value::Concrete(ConcreteValue::String("consumer".to_string())),
    );
    consumer.set_attr(
        "comment",
        Value::Deferred(DeferredValue::ResourceRef {
            path: carina_core::resource::AccessPath::new("lookup", "name"),
        }),
    );

    let mut plan = Plan::new();
    plan.add(carina_core::effect::Effect::Create(resolved(
        target.clone(),
    )));
    plan.add(carina_core::effect::Effect::Read {
        resource: ResolvedDataSource::new(lookup.clone()),
    });
    plan.add(carina_core::effect::Effect::Create(resolved(
        consumer.clone(),
    )));

    let provider_configs = vec![ProviderConfig {
        name: "mock".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        is_default: true,
    }];
    let plan_file = PlanFile {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-07-02T00:00:00Z".to_string(),
        source_path: fixture.config_path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs,
        backend_config: None,
        plan,
        sorted_resources: vec![target.clone(), consumer.clone()],
        unresolved_resources: vec![target, consumer],
        compositions: Vec::new(),
        data_sources: vec![lookup],
        current_states: vec![
            crate::commands::plan::CurrentStateEntry {
                id: target_id,
                state: State::not_found(ResourceId::with_provider_identity(
                    "mock",
                    "test.resource",
                    "target",
                    None,
                )),
            },
            crate::commands::plan::CurrentStateEntry {
                id: consumer_id,
                state: State::not_found(ResourceId::with_provider_identity(
                    "mock",
                    "test.resource",
                    "consumer",
                    None,
                )),
            },
        ],
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };

    let observer_factory = fixture.observer_factory();
    let result = run_apply_from_plan_locked(
        plan_file,
        true,
        fixture.backend(),
        None,
        tmp.path(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(4).unwrap(),
        false,
    )
    .await;

    unsafe {
        std::env::remove_var("CARINA_MOCK_STATE_FILE");
        std::env::remove_var("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA");
    }

    result.expect("saved-plan apply should execute the deferred read before consumer create");
    let state = fixture.read_state().await;
    let consumer = state
        .find_resource("mock", "test.resource", "consumer")
        .expect("consumer should be persisted");
    assert_eq!(
        consumer.attributes.get("comment"),
        Some(&serde_json::json!("target")),
        "consumer must receive the value published by the apply-time read"
    );
}

#[test]
fn apply_refuses_v7_state_without_accept_flag() {
    let state = legacy_override_state();

    let err =
        check_legacy_name_overrides(&state, false).expect_err("legacy state must require opt-in");

    assert!(matches!(err, AppError::Validation(_)));
    let msg = err.to_string();
    assert!(msg.contains("mock.test.resource.legacy"));
    assert!(msg.contains("carina plan"));
    assert!(msg.contains("--accept-legacy-name-overrides"));
}

#[test]
fn apply_proceeds_with_accept_flag_against_legacy_state() {
    let state = legacy_override_state();

    check_legacy_name_overrides(&state, true).expect("accept flag should permit legacy state");
}

#[test]
fn apply_emits_legacy_override_warning_to_stderr() {
    let state = legacy_override_state();
    let resource = Resource::with_provider("mock", "test.resource", "legacy", None).with_attribute(
        "name",
        Value::Concrete(ConcreteValue::String("legacy".to_string())),
    );
    let current_states = HashMap::new();
    let remote_bindings = HashMap::new();
    let wait_aliases = Vec::new();
    let override_aware = OverrideAwareResources::build(
        vec![resource],
        Some(&state),
        PreApplyInputs {
            managed: &[],
            compositions: &[],
            data_sources: &[],
            current_states: &current_states,
            remote_bindings: &remote_bindings,
            wait_aliases: &wait_aliases,
        },
    )
    .expect("override-aware resources should build");

    let mut stderr = Vec::new();
    crate::legacy_name_overrides::write_legacy_override_warnings(
        &override_aware,
        Some(&state),
        &mut stderr,
    )
    .expect("warning write should succeed");
    let stderr = String::from_utf8(stderr).expect("warning should be utf8");

    assert!(stderr.contains("warning: applied legacy name override for mock.test.resource.legacy"));
    assert!(stderr.contains("override attribute 'name' set to 'legacy-temp'"));
    assert!(stderr.contains("original DSL"));
}

#[test]
fn apply_does_not_require_flag_after_v7_to_v8_migration() {
    let state_file = legacy_override_state();
    let mut resource = Resource::with_provider("mock", "test.resource", "legacy", None);
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("legacy".to_string())),
    );
    let id = resource.id.clone();
    let sorted_resources = vec![resource];
    let applied_state = State::existing(
        id.clone(),
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("legacy-temp".to_string())),
        )]),
    )
    .with_identifier("legacy-id");
    let applied_states = HashMap::from([(id.clone(), applied_state)]);
    let plan = plan_with_name_override(&id, "legacy-temp", "legacy");

    let migrated = build_state_after_apply(ApplyStateSave {
        state_file: Some(state_file),
        sorted_resources: &sorted_resources,
        runtime_synthesized_resources: &[],
        current_states: &HashMap::new(),
        applied_states: &applied_states,
        plan: &plan,
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &SchemaRegistry::new(),
    })
    .expect("writeback should migrate legacy override");

    assert!(!migrated.has_legacy_name_overrides());
    check_legacy_name_overrides(&migrated, false)
        .expect("second apply should not require the legacy accept flag");
}

#[tokio::test]
async fn run_apply_cancelled_after_partial_execution_persists_state_and_releases_lock() {
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["first", "second", "third"])
        .cancel_after_successes(1);
    let token = fixture.cancel_token();

    let observer_factory = fixture.observer_factory();
    let err = run_apply_with_observer_factory(
        fixture.config_path(),
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        false,
        fixture.provider_context(),
        token,
        &observer_factory,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, AppError::Interrupted),
        "expected Interrupted, got {err:?}"
    );

    let state = fixture.read_state().await;
    assert!(fixture.backend().state_path().exists());
    assert_eq!(
        state.resources().len(),
        1,
        "state must contain exactly the completed resource"
    );
    assert_eq!(
        state.serial, 1,
        "state serial should advance once from the initial empty state"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "first")
            .is_some()
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "second")
            .is_none()
    );
    assert!(!fixture.lock_path().exists());
}

#[tokio::test]
async fn apply_cancel_token_integration_persists_completed_state_releases_lock_and_returns_interrupted()
 {
    // Regression test for #3498: a cancelled apply must persist completed
    // resources to state, release the lock, and return AppError::Interrupted.
    // The resource names mirror the publish-ALB scenario that surfaced this
    // bug in production.
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["alb", "listener", "target_group"])
        .cancel_after_successes(1);
    let token = fixture.cancel_token();

    let observer_factory = fixture.observer_factory();
    let err = run_apply_with_observer_factory(
        fixture.config_path(),
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        false,
        fixture.provider_context(),
        token,
        &observer_factory,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, AppError::Interrupted));
    let state = fixture.read_state().await;
    assert!(
        state
            .find_resource("mock", "test.resource", "alb")
            .is_some(),
        "alb must be persisted to state (it completed before cancel)"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "listener")
            .is_none(),
        "listener must not be in state (cancelled before completion)"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "target_group")
            .is_none(),
        "target_group must not be in state (cancelled before completion)"
    );
    assert!(!fixture.lock_path().exists(), "lock file must be released");
}

#[tokio::test]
async fn apply_cleanup_priority_persists_completed_state_and_releases_lock() {
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["a_completed", "z_blocked"])
        .with_blocked_create("z_blocked");
    let shutdown = fixture.cancel_token();
    let observer_factory = fixture.observer_factory();

    let apply = run_apply_with_observer_factory(
        fixture.config_path(),
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        false,
        fixture.provider_context(),
        shutdown,
        &observer_factory,
    );
    tokio::pin!(apply);
    tokio::select! {
        () = fixture.wait_for_blocked_create() => {}
        result = &mut apply => panic!("apply returned before the blocking create started: {result:?}"),
    }
    fixture.prioritize_cleanup();
    let err = apply.await.unwrap_err();

    assert!(matches!(err, AppError::Interrupted));
    let state = fixture.read_state().await;
    assert!(
        state
            .find_resource("mock", "test.resource", "a_completed")
            .is_some(),
        "the result folded before cleanup priority must be flushed"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "z_blocked")
            .is_none(),
        "the abandoned create must not be persisted"
    );
    assert!(
        !fixture.lock_path().exists(),
        "cleanup must release the lock"
    );
}

#[tokio::test]
async fn run_apply_locked_with_create_failure_persists_resolved_export_only() {
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    unsafe {
        std::env::remove_var("CARINA_MOCK_STATE_FILE");
        std::env::remove_var("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA");
    }
    let fixture = ApplyCancellationFixture::new()
        .with_resources_and_exports(["a", "b"], &[("ax", "a.name"), ("bx", "b.id")]);
    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        vec![Box::new(FailBCreateFactory)],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let ctx = WiringContext::new(vec![Box::new(FailBCreateFactory)]);
    let observer_factory = fixture.observer_factory();
    let err = run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        true,
        fixture.backend(),
        None,
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .unwrap_err();

    let message = err.to_string();
    assert!(
        message.contains("1 succeeded")
            && message.contains("1 failed")
            && message.contains("1 export not written"),
        "partial apply must report both the resource and export failures, got {err:?}",
    );

    let state = fixture.read_state().await;
    assert!(
        state.find_resource("mock", "test.resource", "a").is_some(),
        "successful resource A must be persisted"
    );
    assert!(
        state.find_resource("mock", "test.resource", "b").is_none(),
        "resource B must not be persisted"
    );
    assert_eq!(state.exports.get("ax"), Some(&serde_json::json!("a")));
    assert!(!state.exports.contains_key("bx"));
    assert_eq!(state.serial, 1);
}

#[tokio::test]
async fn run_apply_locked_defers_value_resolvable_data_source_read_until_referenced_create_exists()
{
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_time_read_crn(fixture.state_path());
    let fixture = fixture.with_raw_config(crn);

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let shared = ApplyTimeReadShared::default();
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let ctx = WiringContext::new(vec![Box::new(ApplyTimeReadFactory {
        shared: shared.clone(),
    })]);
    let observer_factory = fixture.observer_factory();

    run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        true,
        fixture.backend(),
        None,
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(4).unwrap(),
        false,
    )
    .await
    .expect("apply should defer the read until target_role has been created");

    assert_eq!(
        shared.operations.lock().unwrap().as_slice(),
        ["create:target_role", "read:roles", "create:consumer"],
        "apply-time read must run between the upstream create and downstream consumer create"
    );
    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        1,
        "read must execute exactly once at apply time, not once during refresh and again at apply"
    );
    assert_eq!(
        *shared.consumer_description.lock().unwrap(),
        Some(Value::Concrete(ConcreteValue::String(
            "carina-acc-test-3666-target".to_string()
        ))),
        "consumer create must receive the data-source output resolved from the apply-time read"
    );

    let state = fixture.read_state().await;
    assert!(
        state
            .find_resource("mock", "iam.Role", "target_role")
            .is_some(),
        "target role must be persisted"
    );
    assert!(
        state
            .find_resource("mock", "iam.Role", "consumer")
            .is_some(),
        "consumer role must be persisted"
    );
}

#[tokio::test]
async fn run_apply_locked_deposes_old_cbd_instance_when_delete_fails() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), false);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    shared.fail_delete_identifier("vpc-old");
    let ctx = WiringContext::new(vec![Box::new(ApplyCascadeAwsccFactory {
        shared: shared.clone(),
    })]);

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    seed_apply_cascade_state(&fixture, &ctx, &parsed.providers, false);

    let observer_factory = fixture.observer_factory();
    let err = run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        true,
        fixture.backend(),
        None,
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("old VPC delete should fail after replacement create succeeds");

    assert!(
        err.to_string()
            .contains("Apply failed. 1 succeeded, 1 failed."),
        "partial apply must surface the delete failure, got {err:?}"
    );
    assert_eq!(
        shared.delete_calls(),
        vec![("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string())]
    );
    assert_vpc_new_current_and_old_deposed(fixture.state_path());
}

#[tokio::test]
async fn run_apply_locked_deposes_old_cbd_instance_when_delete_is_skipped_by_dependency_failure() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), true);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    shared.fail_delete_identifier("subnet-old");
    let ctx = WiringContext::new(vec![Box::new(ApplyCascadeAwsccFactory {
        shared: shared.clone(),
    })]);

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    seed_apply_cascade_state(&fixture, &ctx, &parsed.providers, true);

    let observer_factory = fixture.observer_factory();
    let err = run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        true,
        fixture.backend(),
        None,
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("old subnet delete failure should skip old VPC delete");

    assert!(
        err.to_string().contains("1 skipped"),
        "partial apply must include a dependency-failure skip, got {err:?}"
    );
    let delete_calls = shared.delete_calls();
    assert_eq!(
        delete_calls.len(),
        1,
        "old VPC delete should be skipped after old subnet delete fails; calls: {delete_calls:?}"
    );
    assert_eq!(delete_calls[0].1, "subnet-old");
    assert_vpc_new_current_and_old_deposed(fixture.state_path());

    let raw = std::fs::read_to_string(fixture.state_path()).expect("state file must be written");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("state must be valid JSON");
    let subnet_identity = concrete_subnet_identity(&ctx, &parsed.providers);
    let subnet = state_json_resource(&state, "awscc", "ec2.Subnet", &subnet_identity);
    assert_eq!(subnet["identifier"], serde_json::json!("subnet-new"));
    let subnet_deposed = subnet
        .get("deposed")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| {
            panic!("expected subnet-old to be recorded as deposed; row: {subnet:#?}")
        });
    assert_eq!(
        subnet_deposed.len(),
        1,
        "expected exactly one deposed subnet"
    );
    assert_eq!(
        subnet_deposed[0]["identifier"],
        serde_json::json!("subnet-old")
    );
}

#[tokio::test]
async fn run_apply_locked_deletes_successful_deposed_generation_and_keeps_current_row() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), false);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    seed_apply_cascade_state_with_deposed(&fixture, &["vpc-old"]);

    run_apply_cascade_fixture(&fixture, &shared)
        .await
        .expect("deposed-only delete should succeed");

    assert_eq!(
        shared.delete_calls(),
        vec![("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string())],
        "apply must execute the deposed generation delete, not the current instance"
    );
    let state = fixture.read_state().await;
    let row = state
        .find_resource("awscc", "ec2.Vpc", "vpc")
        .expect("current row must survive a successful deposed delete");
    assert_eq!(row.identifier.as_deref(), Some("vpc-new"));
    assert!(
        row.deposed.is_empty(),
        "successful deposed delete should remove exactly that generation"
    );
}

#[tokio::test]
async fn run_apply_locked_keeps_failed_deposed_generation_and_replans_it() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), false);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    shared.fail_delete_identifier("vpc-old");
    seed_apply_cascade_state_with_deposed(&fixture, &["vpc-old"]);

    let first = run_apply_cascade_fixture(&fixture, &shared)
        .await
        .expect_err("failed deposed delete should fail apply");
    assert!(
        first.to_string().contains("Apply failed"),
        "unexpected first apply error: {first:?}"
    );
    assert_eq!(deposed_identifiers(fixture.state_path()), vec!["vpc-old"]);

    let second = run_apply_cascade_fixture(&fixture, &shared)
        .await
        .expect_err("failed deposed delete should be planned again");
    assert!(
        second.to_string().contains("Apply failed"),
        "unexpected second apply error: {second:?}"
    );
    assert_eq!(
        shared.delete_calls(),
        vec![
            ("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string()),
            ("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string()),
        ],
        "the failed deposed generation must be retried on the next apply"
    );
    assert_eq!(deposed_identifiers(fixture.state_path()), vec!["vpc-old"]);
}

#[tokio::test]
async fn run_apply_locked_removes_only_successful_deposed_generation() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), false);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    shared.fail_delete_identifier("vpc-older");
    seed_apply_cascade_state_with_deposed(&fixture, &["vpc-old", "vpc-older"]);

    let err = run_apply_cascade_fixture(&fixture, &shared)
        .await
        .expect_err("one failed deposed delete should fail apply");
    assert!(
        err.to_string().contains("Apply failed"),
        "unexpected apply error: {err:?}"
    );

    assert_eq!(
        shared.delete_calls(),
        vec![
            ("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string()),
            ("awscc.ec2.Vpc.vpc".to_string(), "vpc-older".to_string()),
        ],
        "each deposed generation should be independently deletable"
    );
    assert_eq!(
        deposed_identifiers(fixture.state_path()),
        vec!["vpc-older"],
        "only the successful deposed generation should be removed"
    );
}

#[tokio::test]
async fn run_apply_locked_rekeys_anonymous_child_unresolved_source_by_effect_id() {
    let fixture = ApplyCancellationFixture::new();
    let crn = format!(
        r#"backend local {{ path = "{}" }}
provider awscc {{}}

let vpc = awscc.ec2.Vpc {{
  cidr_block = "10.221.0.0/16"
  directives {{
    create_before_destroy = true
  }}
}}

awscc.ec2.Subnet {{
  vpc_id = vpc.vpc_id
  cidr_block = "10.221.1.0/24"
  availability_zone = "ap-northeast-1c"
}}
"#,
        fixture.state_path().display()
    );
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    let ctx = WiringContext::new(vec![Box::new(ApplyCascadeAwsccFactory {
        shared: shared.clone(),
    })]);

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );

    let mut vpc_state = ResourceState::new("ec2.Vpc", "vpc", "awscc")
        .with_identifier("vpc-old")
        .with_attribute("cidr_block", serde_json::json!("10.220.0.0/16"))
        .with_attribute("vpc_id", serde_json::json!("vpc-old"));
    vpc_state.binding = Some("vpc".to_string());

    let mut subnet_state = ResourceState::new(
        "ec2.Subnet",
        concrete_subnet_identity(&ctx, &parsed.providers),
        "awscc",
    )
    .with_identifier("subnet-old")
    .with_attribute("vpc_id", serde_json::json!("vpc-old"))
    .with_attribute("cidr_block", serde_json::json!("10.220.1.0/24"))
    .with_attribute("availability_zone", serde_json::json!("ap-northeast-1c"));
    subnet_state.dependency_bindings.insert("vpc".to_string());

    let mut state_file = carina_state::StateFile::new();
    state_file
        .upsert_resource(vpc_state)
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(subnet_state)
        .expect("test state setup must be valid");
    fixture.write_state(&state_file);

    let observer_factory = fixture.observer_factory();
    run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        true,
        fixture.backend(),
        None,
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect("apply should replace the VPC and anonymous subnet");

    let create_calls = shared.create_calls();
    let subnet_call = create_calls
        .iter()
        .find(|(id, _)| id.contains(".ec2.Subnet."))
        .unwrap_or_else(|| panic!("subnet create missing from calls: {create_calls:?}"));
    assert_eq!(
        subnet_call.1.get("vpc_id"),
        Some(&string_value("vpc-new")),
        "anonymous subnet create must re-resolve vpc.vpc_id from the replacement VPC state; calls: {create_calls:?}"
    );
}

#[tokio::test]
async fn post_apply_plan_refreshes_existing_resource_data_source_and_is_idempotent() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_time_read_crn(fixture.state_path());
    let fixture = fixture.with_raw_config(crn);

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let shared = ApplyTimeReadShared::default();
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let ctx = WiringContext::new(vec![Box::new(ApplyTimeReadFactory {
        shared: shared.clone(),
    })]);
    let observer_factory = fixture.observer_factory();

    run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        true,
        fixture.backend(),
        None,
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(4).unwrap(),
        false,
    )
    .await
    .expect("initial apply should succeed");

    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        1,
        "first run should read at apply time exactly once"
    );

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must reload");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate for plan, got: {validation_errors:?}"
    );

    let state_file = Some(fixture.read_state().await);
    let target_state = state_file
        .as_ref()
        .and_then(|state| state.find_resource("mock", "iam.Role", "target_role"))
        .expect("target role must be persisted before post-apply plan");
    assert_eq!(
        target_state.attributes.get("path"),
        Some(&serde_json::json!("/")),
        "fixture must mirror AWS read-back defaults that are absent from config"
    );
    assert_eq!(
        target_state.attributes.get("max_session_duration"),
        Some(&serde_json::json!(3600)),
        "fixture must mirror AWS read-back defaults that are absent from config"
    );

    let plan_ctx = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        &ctx,
        &parsed,
        &unresolved_parsed.resources,
        &state_file,
        true,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
        base_dir,
    )
    .await
    .expect("post-apply plan should succeed");

    assert!(
        !plan_ctx.plan.has_mutations(),
        "post-apply plan must be idempotent"
    );
    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        2,
        "post-apply plan must refresh the data source instead of deferring it"
    );
}

#[tokio::test]
async fn run_apply_locked_orders_deferred_data_source_read_chain() {
    let fixture = ApplyCancellationFixture::new();
    let crn = format!(
        r#"backend local {{ path = "{}" }}
provider mock {{}}

let target_role = mock.iam.Role {{
  role_name = "carina-acc-test-3666-target"
}}

let roles = read mock.iam.Roles {{
  name_regex = "^${{target_role.role_name}}$"
}}

let roles_again = read mock.iam.Roles {{
  name_regex = "^${{roles.names[0]}}$"
}}

let consumer = mock.iam.Role {{
  role_name = "carina-acc-test-3666-consumer"
  description = roles_again.names[0]
}}
"#,
        fixture.state_path().display()
    );
    let fixture = fixture.with_raw_config(crn);

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let shared = ApplyTimeReadShared::default();
    let validation_errors = crate::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.inference_errors,
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let ctx = WiringContext::new(vec![Box::new(ApplyTimeReadFactory {
        shared: shared.clone(),
    })]);
    let observer_factory = fixture.observer_factory();

    run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        true,
        fixture.backend(),
        None,
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(4).unwrap(),
        false,
    )
    .await
    .expect("apply should order chained deferred reads before the consumer");

    assert_eq!(
        shared.operations.lock().unwrap().as_slice(),
        [
            "create:target_role",
            "read:roles",
            "read:roles_again",
            "create:consumer"
        ],
        "read chain must run target create -> read A -> read B -> consumer create"
    );
    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        2,
        "both deferred reads should execute exactly once at apply time"
    );
    assert_eq!(
        *shared.consumer_description.lock().unwrap(),
        Some(Value::Concrete(ConcreteValue::String(
            "carina-acc-test-3666-target".to_string()
        ))),
        "consumer create must receive the second read's output"
    );
}

fn s3_backend_config_with_encrypt(encrypt: bool) -> carina_core::parser::BackendConfig {
    let mut attributes = HashMap::new();
    attributes.insert(
        "bucket".to_string(),
        Value::Concrete(ConcreteValue::String("state-bucket".to_string())),
    );
    attributes.insert(
        "key".to_string(),
        Value::Concrete(ConcreteValue::String("project/state.json".to_string())),
    );
    attributes.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
    );
    attributes.insert(
        "encrypt".to_string(),
        Value::Concrete(ConcreteValue::Bool(encrypt)),
    );
    carina_core::parser::BackendConfig {
        backend_type: "s3".to_string(),
        attributes,
    }
}

#[test]
fn saved_plan_backend_cross_check_rejects_non_addressing_attribute_change() {
    let planned = s3_backend_config_with_encrypt(false);
    let current = s3_backend_config_with_encrypt(true);

    let err = ensure_saved_plan_backend_matches_current(Some(&planned), Some(&current))
        .expect_err("any backend attribute change should invalidate a saved plan");
    let msg = err.to_string();

    assert!(msg.contains("Saved plan backend does not match the current `backend.crn`"));
    assert!(msg.contains("The plan file recorded one backend"));
    assert!(msg.contains("Re-run `carina plan`"));
}

#[test]
fn build_state_after_apply_finds_write_only_with_provider_prefix() {
    // The schema map is keyed by provider-prefixed names (e.g., "awscc.ec2.Vpc"),
    // but the buggy code used resource.id.resource_type (e.g., "ec2.Vpc") for lookup.
    // This test verifies that write-only attributes are found when the schema key
    // includes the provider prefix.
    let mut schemas = SchemaRegistry::new();
    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::string()))
        .attribute(AttributeSchema::new("ipv4_netmask_length", AttributeType::int()).write_only());
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
    let plan = Plan::new();
    let successfully_deleted = HashSet::new();
    let failed_refreshes = HashSet::new();

    let result = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        runtime_synthesized_resources: &[],
        current_states: &current_states,
        applied_states: &applied_states,
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
fn build_state_after_apply_preserves_block_unique_name_attribute() {
    // When a block_name attribute (e.g., "policies" with block_name "policy")
    // is carried over by the provider because CloudControl doesn't return it,
    // the state after apply should include the attribute under the canonical name.
    // This is the scenario in issue #1499 (iam_role/with_policy).
    use carina_core::schema::StructField;

    let mut schemas = SchemaRegistry::new();
    let schema = ResourceSchema::new("iam.role")
        .attribute(AttributeSchema::new("role_name", AttributeType::string()).create_only())
        .attribute(
            AttributeSchema::new(
                "policies",
                AttributeType::unordered_list(AttributeType::struct_(
                    "Policy".to_string(),
                    vec![
                        StructField::new("policy_name", AttributeType::string()).required(),
                        StructField::new("policy_document", AttributeType::string()).required(),
                    ],
                )),
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
    let plan = Plan::new();
    let successfully_deleted = HashSet::new();
    let failed_refreshes = HashSet::new();

    let state = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        runtime_synthesized_resources: &[],
        current_states: &current_states,
        applied_states: &applied_states,
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
    let id = carina_core::resource::ResourceId::with_provider_identity(
        "awscc",
        "iam.role",
        "test-role",
        None,
    );
    let attrs = saved_attrs.get(&id).unwrap();
    assert!(
        attrs.contains_key("policies"),
        "saved_attrs should contain 'policies': {:?}",
        attrs.keys().collect::<Vec<_>>()
    );
}

#[test]
fn block_unique_name_attribute_no_diff_when_hydrated() {
    // After apply, the state file contains the block_name attribute (canonical name).
    // On re-plan, if hydrate_read_state restores it into current_states,
    // the differ should see no changes.
    //
    // This tests the scenario from issue #1499 where plan-verify fails
    // because the block_name attribute shows as an addition.
    use carina_core::differ::diff;
    use carina_core::schema::StructField;

    let schema = ResourceSchema::new("awscc.iam.role")
        .attribute(AttributeSchema::new("role_name", AttributeType::string()).create_only())
        .attribute(
            AttributeSchema::new(
                "policies",
                AttributeType::unordered_list(AttributeType::struct_(
                    "Policy".to_string(),
                    vec![
                        StructField::new("policy_name", AttributeType::string()).required(),
                        StructField::new("policy_document", AttributeType::string()).required(),
                    ],
                )),
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
fn block_unique_name_attribute_state_roundtrip() {
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
                AttributeType::unordered_list(AttributeType::struct_(
                    "IpamOperatingRegion".to_string(),
                    vec![StructField::new("region_name", AttributeType::string()).required()],
                )),
            )
            .with_block_name("operating_region"),
        )
        .attribute(AttributeSchema::new("description", AttributeType::string()));
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
        runtime_synthesized_resources: &[],
        current_states: &HashMap::new(),
        applied_states: &applied_states,
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
    let id = carina_core::resource::ResourceId::with_provider_identity(
        "awscc",
        "ec2.ipam",
        "test-ipam",
        None,
    );
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

/// Move + Update — same shape as the Replace case but the Update
/// effect changes a non-create-only attribute. The post-Update
/// attribute value must survive Phase 2's Move cleanup.
#[test]
fn move_plus_update_keeps_post_update_attributes() {
    use carina_core::effect::Effect;
    use carina_state::{ResourceState, StateFile};

    let mut schemas = SchemaRegistry::new();
    let schema = ResourceSchema::new("ec2.Tag")
        .attribute(AttributeSchema::new("key", AttributeType::string()).create_only())
        .attribute(AttributeSchema::new("value", AttributeType::string()));
    schemas.insert("awscc", schema);

    let new_id = ResourceId::with_provider_identity("awscc", "ec2.Tag", "tag_new", None);
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
    state_file
        .upsert_resource(old_row)
        .expect("test state setup must be valid");

    let mut plan = Plan::new();
    let from_id = ResourceId::with_provider_identity("awscc", "ec2.Tag", "tag_old", None);
    plan.add(Effect::Update {
        from: Box::new(State::existing(from_id.clone(), HashMap::new())),
        to: resolved(sorted_resources[0].clone()),
        changed_attributes: vec!["value".to_string()],
    });
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(from_id),
        to: carina_core::resource::ResolvedResourceId::new(new_id.clone()),
    });

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(state_file),
        sorted_resources: &sorted_resources,
        runtime_synthesized_resources: &[],
        current_states: &HashMap::new(),
        applied_states: &applied_states,
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
            .attribute(AttributeSchema::new("bucket_name", AttributeType::string()).create_only()),
    );

    let new_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "bucket_new", None);
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
    let from_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "bucket_old", None);
    let old_row = ResourceState::new("s3.Bucket", "bucket_old", "awscc")
        .with_identifier("my-bucket")
        .with_attribute("bucket_name", serde_json::Value::String("my-bucket".into()));
    let mut state_file = StateFile::default();
    state_file
        .upsert_resource(old_row)
        .expect("test state setup must be valid");

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(from_id),
        to: carina_core::resource::ResolvedResourceId::new(new_id.clone()),
    });

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(state_file),
        sorted_resources: &sorted_resources,
        runtime_synthesized_resources: &[],
        current_states: &current_states,
        applied_states: &HashMap::new(),
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
    let from_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "stale_from", None);
    let to_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "stale_to", None);

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(from_id.clone()),
        to: carina_core::resource::ResolvedResourceId::new(to_id.clone()),
    });

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(StateFile::default()),
        sorted_resources: &[],
        runtime_synthesized_resources: &[],
        current_states: &HashMap::new(),
        applied_states: &HashMap::new(),
        plan: &plan,
        successfully_deleted: &HashSet::new(),
        failed_refreshes: &HashSet::new(),
        schemas: &schemas,
    })
    .expect("writeback should succeed");

    assert!(
        saved.resources().is_empty(),
        "no-op move leaves state empty"
    );
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
            .attribute(AttributeSchema::new("bucket_name", AttributeType::string()).create_only()),
    );

    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "stuck", None);
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
    state_file
        .upsert_resource(existing)
        .expect("test state setup must be valid");

    let saved = build_state_after_apply(ApplyStateSave {
        state_file: Some(state_file),
        sorted_resources: &sorted_resources,
        runtime_synthesized_resources: &[],
        current_states: &HashMap::new(),
        applied_states: &HashMap::new(),
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
            .attribute(AttributeSchema::new("bucket_name", AttributeType::string()).create_only()),
    );

    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "collision", None);
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

    let to_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "elsewhere", None);
    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(id.clone()),
        to: carina_core::resource::ResolvedResourceId::new(to_id),
    });

    let result = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        runtime_synthesized_resources: &[],
        current_states: &HashMap::new(),
        applied_states: &applied,
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
            .attribute(AttributeSchema::new("bucket_name", AttributeType::string()).create_only()),
    );

    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "collision", None);
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
    plan.add(Effect::Remove {
        id: carina_core::resource::ResolvedResourceId::new(id.clone()),
    });

    let result = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        runtime_synthesized_resources: &[],
        current_states: &HashMap::new(),
        applied_states: &applied,
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
            .attribute(AttributeSchema::new("bucket_name", AttributeType::string()).create_only()),
    );

    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "self", None);
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
        from: carina_core::resource::ResolvedResourceId::new(id.clone()),
        to: carina_core::resource::ResolvedResourceId::new(id.clone()),
    });

    let result = build_state_after_apply(ApplyStateSave {
        state_file: None,
        sorted_resources: &sorted_resources,
        runtime_synthesized_resources: &[],
        current_states: &HashMap::new(),
        applied_states: &applied,
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
fn resolve_exports_resolves_cross_file_resource_refs() {
    use carina_core::parser::{InferredExportParam as ExportParameter, TypeExpr};
    use carina_core::resource::Value;

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
        state_file_from_json(json)
    };

    // Export param references registry_prod.account_id using the
    // parser-produced ResourceRef shape.
    let export_params = vec![ExportParameter {
        name: "account_id".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        )),
    }];

    // Mirror production callers: the resource is in sorted_resources
    // with a binding; provider-returned attributes flow in via
    // `current_states` derived from `state.resources`.
    let mut registry_prod =
        Resource::with_provider("awscc", "organizations.account", "registry-prod", None);
    registry_prod.binding = Some("registry_prod".to_string());
    let sorted_resources = vec![registry_prod];

    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &std::collections::HashMap::new(),
            &state,
        );
    let exports = resolve_exports(
        &export_params,
        &sorted_resources,
        &[],
        &[],
        &post_apply_states,
        &carina_core::schema::SchemaRegistry::new(),
        &[],
    )
    .unwrap()
    .into_parts()
    .0;

    assert_eq!(
        exports.get("account_id"),
        Some(&serde_json::Value::String("459524413166".to_string())),
        "resolve_exports should resolve ResourceRef values to actual values. Got: {:?}",
        exports
    );
}

#[test]
fn resolve_exports_resolves_module_call_attribute_via_composition() {
    // #2479: writeback used to build bindings from `state.resources`
    // only. A composition resource (synthesised by module-call expansion to
    // expose `attributes { role_arn = role.arn }`) carries no provider
    // identity and never lands in `state.resources`, so an export
    // referencing `<module_call>.<attr>` failed with
    // `unresolved reference <call>.<attr>`.
    use carina_core::parser::{InferredExportParam as ExportParameter, TypeExpr};
    use carina_core::resource::{AccessPath, Composition, DeferredValue, Value};

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
        state_file_from_json(json)
    };

    let mut role_resource =
        Resource::with_provider("awscc", "iam.Role", "github_actions_carina.role", None);
    role_resource.binding = Some("github_actions_carina.role".to_string());

    // Virtual resource as `expand_module_call` produces it: binding is
    // the module-call alias, and each attribute is a ResourceRef into
    // an expanded sub-resource. carina#3181: compositions are their own type.
    let mut virt_attrs: indexmap::IndexMap<String, carina_core::resource::CompositionAttribute> =
        indexmap::IndexMap::new();
    virt_attrs.insert(
        "role_arn".to_string(),
        carina_core::resource::CompositionAttribute::from_value(Value::Deferred(
            DeferredValue::ResourceRef {
                path: AccessPath::new("github_actions_carina.role", "arn"),
            },
        )),
    );
    let composition = Composition {
        id: carina_core::resource::ResourceId::with_identity("_virtual", "github_actions_carina"),
        signature: carina_core::resource::Signature {
            arguments: indexmap::IndexMap::new(),
            attributes: virt_attrs,
        },
        binding: Some("github_actions_carina".to_string()),
        dependency_bindings: std::collections::BTreeSet::new(),
        module_name: "github_module".to_string(),
        instance: "github_actions_carina".to_string(),
        quoted_string_attrs: std::collections::HashSet::new(),
    };
    let sorted_resources = vec![role_resource];
    let pre_resolve_compositions = vec![composition];

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("github_actions_carina", "role_arn"),
        })),
    }];

    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &std::collections::HashMap::new(),
            &state,
        );
    let exports = resolve_exports(
        &export_params,
        &sorted_resources,
        &[],
        &pre_resolve_compositions,
        &post_apply_states,
        &carina_core::schema::SchemaRegistry::new(),
        &[],
    )
    .unwrap()
    .into_parts()
    .0;

    assert_eq!(
        exports.get("role_arn"),
        Some(&serde_json::Value::String(
            "arn:aws:iam::123456789012:role/github-actions-carina".to_string()
        )),
        "module-call attribute export must resolve via composition binding + provider state, got: {:?}",
        exports
    );
}

#[test]
fn resolve_exports_resolves_chained_module_call_attribute_via_two_compositions() {
    // #2479 follow-up: a module-call binding whose attribute itself
    // points at *another* module-call binding's attribute (e.g.
    // `${outer_module.public_role_arn}` where the outer module's
    // `attributes { public_role_arn = inner_module.role_arn }` exposes
    // an inner module-call binding's attribute). Two `Virtual` hops
    // through `ResourceRef` recursion before bottoming out at the real
    // role's `arn` from state. Pins the resolver's transitive walk so a
    // regression that broke after a single hop would surface.
    use carina_core::parser::{InferredExportParam as ExportParameter, TypeExpr};
    use carina_core::resource::{AccessPath, Composition, DeferredValue, Value};

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
        state_file_from_json(json)
    };

    let mut role_resource = Resource::with_provider("awscc", "iam.Role", "outer.inner.role", None);
    role_resource.binding = Some("outer.inner.role".to_string());

    // carina#3181: compositions are a distinct typestate.
    let make_virtual = |id_name: &str, binding: &str, attr: &str, ref_b: &str, ref_a: &str| {
        let mut attributes: indexmap::IndexMap<
            String,
            carina_core::resource::CompositionAttribute,
        > = indexmap::IndexMap::new();
        attributes.insert(
            attr.to_string(),
            carina_core::resource::CompositionAttribute::from_value(Value::Deferred(
                DeferredValue::ResourceRef {
                    path: AccessPath::new(ref_b, ref_a),
                },
            )),
        );
        Composition {
            id: carina_core::resource::ResourceId::with_identity("_virtual", id_name),
            signature: carina_core::resource::Signature {
                arguments: indexmap::IndexMap::new(),
                attributes,
            },
            binding: Some(binding.to_string()),
            dependency_bindings: std::collections::BTreeSet::new(),
            module_name: "mod".to_string(),
            instance: binding.to_string(),
            quoted_string_attrs: std::collections::HashSet::new(),
        }
    };
    let inner_virtual = make_virtual(
        "outer.inner",
        "outer.inner",
        "role_arn",
        "outer.inner.role",
        "arn",
    );
    let outer_virtual = make_virtual(
        "outer",
        "outer",
        "public_role_arn",
        "outer.inner",
        "role_arn",
    );

    let sorted_resources = vec![role_resource];

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("outer", "public_role_arn"),
        })),
    }];

    let pre_resolve_compositions = vec![inner_virtual, outer_virtual];
    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &std::collections::HashMap::new(),
            &state,
        );
    let exports = resolve_exports(
        &export_params,
        &sorted_resources,
        &[],
        &pre_resolve_compositions,
        &post_apply_states,
        &carina_core::schema::SchemaRegistry::new(),
        &[],
    )
    .unwrap()
    .into_parts()
    .0;

    assert_eq!(
        exports.get("role_arn"),
        Some(&serde_json::Value::String(
            "arn:aws:iam::123456789012:role/chained".to_string()
        )),
        "two-hop module-call chain must resolve through both compositions, got: {:?}",
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
    // `Composition`'s `attributes` — into the pre-apply
    // concrete value (here: OLD_ARN). The composition's `role_arn`
    // attribute thus becomes a frozen string snapshot of the
    // pre-apply state. Later in `finalize_apply`, the resource
    // graph is sent into `resolve_exports`, which feeds the
    // composition's stale `role_arn` into `state.exports` — even
    // though the writeback wrote the new ARN into
    // `state.resources[role].arn`. So `state.exports.role_arn` and
    // `state.resources[role].arn` disagree, with exports holding
    // the old ARN.
    //
    // **The fix** (#3177): `apply/mod.rs` snapshots every composition
    // *before* the head-of-pipeline `ResourceRef` collapse — that
    // snapshot still carries the authored `ref role.arn`. It
    // threads that pre-resolve snapshot into `finalize_apply` →
    // `resolve_exports`, which uses it (instead of the mutated
    // `sorted_resources`) to build a fresh post-apply
    // `ResolvedBindings` view via the typestate-split API
    // (`from_managed_with_state` + `resolve_virtual_refs_post_apply`
    // + `layer_compositions_post_apply`). Exports then resolve against the
    // post-apply view.
    //
    // **The test** mirrors that production pipeline: it (1)
    // constructs the same managed + composition graph, (2) takes a
    // pre-resolve snapshot of the composition, (3) runs the head-of-
    // pipeline resolver against a *pre-apply* `current_states`
    // (OLD_ARN), then (4) feeds the pre-resolve snapshot and the
    // resolved working slice into `resolve_exports` along with a
    // *post-apply* `StateFile` (NEW_ARN). The assertion checks the
    // export ends up at NEW_ARN.
    //
    // Without the #3177 fix, `resolve_exports` would consult the
    // mutated composition's stale concrete attribute and return
    // OLD_ARN. With the fix in place, the pre-resolve snapshot
    // kicks in and re-resolves against the post-apply state.
    use carina_core::parser::{InferredExportParam as ExportParameter, TypeExpr};
    use carina_core::resolver::resolve_managed_refs_with_state_and_remote;
    use carina_core::resource::{
        AccessPath, Composition, ConcreteValue, DeferredValue, ResourceId, State as ResourceState,
        Value,
    };
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
        state_file_from_json(json)
    };

    // Step (b): pre-apply `current_states` — what the head-of-
    // pipeline resolver sees. Holds the OLD ARN, so the pre-apply
    // pass freezes composition `role_arn` to OLD_ARN.
    let pre_apply_current_states: HashMap<ResourceId, ResourceState> = {
        let id = ResourceId::with_provider_identity("awscc", "iam.Role", "carina_role", None);
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
    // does not inline `arn` — provider returns it) and a composition
    // module-call binding that references `role.arn`. carina#3181:
    // compositions are a distinct typestate, untouched by the managed
    // head-of-pipeline resolver — they keep their authored
    // `ResourceRef`s, which is exactly the pre-resolve snapshot the
    // #3177 fix needs.
    let mut role_managed = Resource::with_provider("awscc", "iam.Role", "carina_role", None);
    role_managed.binding = Some("role".to_string());

    let mut virt_attrs: indexmap::IndexMap<String, carina_core::resource::CompositionAttribute> =
        indexmap::IndexMap::new();
    virt_attrs.insert(
        "role_arn".to_string(),
        carina_core::resource::CompositionAttribute::from_value(Value::Deferred(
            DeferredValue::ResourceRef {
                path: AccessPath::new("role", "arn"),
            },
        )),
    );
    let composition = Composition {
        id: ResourceId::with_identity("_virtual", "carina_module"),
        signature: carina_core::resource::Signature {
            arguments: indexmap::IndexMap::new(),
            attributes: virt_attrs,
        },
        binding: Some("carina_module".to_string()),
        dependency_bindings: std::collections::BTreeSet::new(),
        module_name: "carina_module".to_string(),
        instance: "carina_module".to_string(),
        quoted_string_attrs: std::collections::HashSet::new(),
    };

    let mut sorted_resources = vec![role_managed];

    // Step (c): pre-resolve snapshot of the composition — carries the
    // authored `ref role.arn`.
    let pre_resolve_compositions: Vec<Composition> = vec![composition];

    // Step (d): head-of-pipeline resolver — runs over the managed
    // slice only. compositions are not part of it, so the pre-resolve
    // snapshot above is preserved verbatim.
    let bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
        carina_core::binding_index::PreApplyInputs {
            managed: &sorted_resources.clone(),
            compositions: &[],
            data_sources: &[],
            current_states: &carina_core::resource::into_plan_input_map(
                pre_apply_current_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &sorted_resources,
            ),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    );
    resolve_managed_refs_with_state_and_remote(&mut sorted_resources, &bindings).unwrap();

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("carina_module", "role_arn"),
        })),
    }];

    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &std::collections::HashMap::new(),
            &post_apply_state_file,
        );
    let exports = resolve_exports(
        &export_params,
        &sorted_resources,
        &[],
        &pre_resolve_compositions,
        &post_apply_states,
        &carina_core::schema::SchemaRegistry::new(),
        &[],
    )
    .unwrap()
    .into_parts()
    .0;

    assert_eq!(
        exports.get("role_arn"),
        Some(&serde_json::Value::String(post_apply_arn.to_string())),
        "exports.role_arn must be the POST-apply ARN ({post_apply_arn}) — \
         the value carried by state.resources, NOT a pre-apply snapshot. \
         Got: {exports:?}",
    );
}

#[test]
fn resolve_exports_resolves_data_source_attribute_after_apply_3266() {
    // carina#3266 root-cause regression test.
    //
    // The bug: `resolve_exports` builds `post_apply_states` from
    // `state.resources` only. Managed resources are persisted there,
    // but data sources are not — they are queried each run and the
    // result lives in `current_states` during execution and is
    // discarded at writeback. So when an export references a
    // data-source attribute (e.g. `exports { x = ds.arns }`), the
    // post-apply binding view layered onto `pre_apply` finds no
    // attributes for the data source (#3265's `layer_data_source_bindings`
    // merge fires only when `current_states[ds.id]` carries the read
    // result), the ResourceRef resolves to nothing, and the resolver
    // silently keeps the prior literal in `state.exports`.
    //
    // The fix: thread `current_states` from `finalize_apply` into
    // `resolve_exports` so the read results stay visible across the
    // exports-resolution boundary. `layer_data_source_bindings` then
    // merges the read attrs and `ds.arns` resolves to the read value.
    //
    // Without the fix, `resolve_exports` returns an empty map (no
    // entry for the export) instead of the NEW arn — exports stays
    // at the pre-apply literal in production.
    use carina_core::parser::{InferredExportParam as ExportParameter, TypeExpr};
    use carina_core::resource::{
        AccessPath, ConcreteValue, DataSource, DeferredValue, ResourceId, State as ResourceState,
        Value,
    };
    use std::collections::HashMap;

    let new_arn = "arn:aws:iam::412038850359:role/aws-reserved/sso.amazonaws.com/ap-northeast-1/AWSReservedSSO_AdministratorAccess_ed2ecac126d82b94";

    // Post-apply state file: no managed resource entries needed; the
    // export's only input is a data source, which is never persisted
    // to `state.resources`.
    let post_apply_state_file = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 2,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": []
        });
        state_file_from_json(json)
    };

    // The authored data source: `let admin_access_roles = read aws.iam.Roles { ... }`.
    let ds_id = ResourceId::with_provider_identity("aws", "iam.Roles", "admin_access_roles", None);
    let mut ds = DataSource::with_provider("aws", "iam.Roles", "admin_access_roles", None);
    ds.binding = Some("admin_access_roles".to_string());
    ds.attributes.insert(
        "path_prefix".to_string(),
        Value::Concrete(ConcreteValue::String(
            "/aws-reserved/sso.amazonaws.com/".to_string(),
        )),
    );
    let data_sources = vec![ds];

    // The provider-read result that `read_data_source_with_retry`
    // writes into `current_states[ds.id]` during apply execution.
    let current_states: HashMap<ResourceId, ResourceState> = {
        let mut attrs: HashMap<String, Value> = HashMap::new();
        attrs.insert(
            "arns".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String(new_arn.to_string()),
            )])),
        );
        let mut m = HashMap::new();
        m.insert(ds_id.clone(), ResourceState::existing(ds_id, attrs));
        m
    };

    // The export: `exports { admin_access_role_arns = admin_access_roles.arns }`.
    let export_params = vec![ExportParameter {
        name: "admin_access_role_arns".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("admin_access_roles", "arns"),
        })),
    }];

    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &current_states,
            &post_apply_state_file,
        );
    let exports = resolve_exports(
        &export_params,
        &[],
        &data_sources,
        &[],
        &post_apply_states,
        &carina_core::schema::SchemaRegistry::new(),
        &[],
    )
    .unwrap()
    .into_parts()
    .0;

    let expected_json = serde_json::json!([new_arn]);
    assert_eq!(
        exports.get("admin_access_role_arns"),
        Some(&expected_json),
        "data-source-derived export must resolve via current_states read result, got: {exports:?}",
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

#[tokio::test]
async fn confirm_apply_returns_confirmed_on_yes() {
    let input = &b"yes\n"[..];
    let cancel = carina_core::shutdown::ShutdownToken::running();
    let outcome = confirm_apply(input, cancel, false).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Confirmed);
}

#[tokio::test]
async fn confirm_apply_returns_cancelled_on_no() {
    let input = &b"no\n"[..];
    let cancel = carina_core::shutdown::ShutdownToken::running();
    let outcome = confirm_apply(input, cancel, false).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Cancelled);
}

#[tokio::test]
async fn confirm_apply_returns_cancelled_on_empty_input() {
    let input = &b"\n"[..];
    let cancel = carina_core::shutdown::ShutdownToken::running();
    let outcome = confirm_apply(input, cancel, false).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Cancelled);
}

#[tokio::test]
async fn confirm_apply_auto_approve_skips_read() {
    // Reader would hang forever; auto_approve must short-circuit without reading.
    let input = tokio::io::BufReader::new(tokio::io::empty());
    let cancel = carina_core::shutdown::ShutdownToken::running();
    let outcome = confirm_apply(input, cancel, true).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Confirmed);
}

#[tokio::test]
async fn confirm_apply_returns_interrupted_when_cancel_fires_after_subscription() {
    let reader = tokio::io::BufReader::new(NeverReady);
    let (trigger, cancel) = carina_core::shutdown::testing::shutdown_channel();
    let waiting = tokio::spawn(confirm_apply(reader, cancel.clone(), false));
    tokio::task::yield_now().await;
    trigger.request_graceful_shutdown();
    let err = waiting.await.unwrap().unwrap_err();
    assert!(matches!(err, AppError::Interrupted));
}

#[tokio::test]
async fn confirm_apply_returns_interrupted_when_cancel_token_fires() {
    let (trigger, token) = carina_core::shutdown::testing::shutdown_channel();
    trigger.request_graceful_shutdown();
    let reader = tokio::io::BufReader::new(NeverReady);
    let err = confirm_apply(reader, token, false).await.unwrap_err();
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
            &carina_core::schema::SchemaRegistry::new(),
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

    /// No refreshed cert state ⇒ the loop is carried as an apply-time
    /// re-expansion target (no mis-expansion), matching the plan path's
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
            &carina_core::schema::SchemaRegistry::new(),
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
        assert_eq!(out.deferred_create_targets.len(), 1);
        assert!(out.residual_deferred_for.is_empty());
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
            &carina_core::schema::SchemaRegistry::new(),
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
            &carina_core::schema::SchemaRegistry::new(),
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

#[cfg(test)]
mod saved_plan_version_tests {
    //! carina#3248: saved plans now persist `compositions` and
    //! `data_sources` (version `4`). Older plans (version `3` and
    //! below) are rejected outright per the repo's
    //! no-backward-compat policy — re-running `carina plan` is the
    //! supported migration path.

    use tempfile::TempDir;

    /// A `version: 3` saved plan must be rejected by
    /// `run_apply_from_plan` with a message that names the expected
    /// version and points the user at re-running `plan`.
    ///
    /// The test writes a minimal v3-shaped JSON to disk and asserts
    /// the rejection. The plan body is deliberately tiny — the
    /// version gate runs first, before any field is consumed, so a
    /// stub `effects: []` / resource arrays are enough.
    #[tokio::test]
    async fn version_3_saved_plan_is_rejected() {
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        // A v3 plan: everything required by the pre-#3248 PlanFile
        // shape, no `compositions` field.
        let v3 = serde_json::json!({
            "version": 3,
            "carina_version": "0.4.0",
            "timestamp": "2026-05-24T00:00:00Z",
            "source_path": "test.crn",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": [] },
            "sorted_resources": [],
            "unresolved_resources": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(&plan_path, serde_json::to_string(&v3).unwrap()).expect("write plan");

        let result = crate::commands::apply::run_apply_from_plan(
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        let err = result.expect_err("v3 saved plan must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("Unsupported plan file version: 3"),
            "error must name the rejected version, got: {msg}",
        );
        let expected = format!(
            "expected {}",
            crate::commands::plan::PlanFile::CURRENT_VERSION
        );
        assert!(
            msg.contains(&expected),
            "error must name the expected version, got: {msg}",
        );
        assert!(
            msg.contains("Re-run 'carina plan'"),
            "error must point the user at the supported migration path, got: {msg}",
        );
    }

    #[tokio::test]
    async fn version_9_saved_plan_is_rejected_after_delete_generation_bump() {
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        let v9 = serde_json::json!({
            "version": 9,
            "carina_version": "0.4.0",
            "timestamp": "2026-07-02T00:00:00Z",
            "source_path": "test.crn",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": [] },
            "sorted_resources": [],
            "unresolved_resources": [],
            "compositions": [],
            "data_sources": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(&plan_path, serde_json::to_string(&v9).unwrap()).expect("write plan");

        let result = crate::commands::apply::run_apply_from_plan(
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        let err = result.expect_err("v9 saved plan must be rejected after v10 bump");
        let msg = err.to_string();
        assert!(
            msg.contains("Unsupported plan file version: 9"),
            "error must name the rejected version, got: {msg}",
        );
        assert!(
            msg.contains("expected 10"),
            "error must name the v10 expected version, got: {msg}",
        );
    }

    fn replacement_plan_json() -> (TempDir, std::path::PathBuf, serde_json::Value) {
        use carina_core::provider::ProviderRouter;
        use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};

        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        let resource = carina_core::resource::Resource::new("ec2.Vpc", "main").with_attribute(
            "cidr_block",
            carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::String(
                "10.1.0.0/16".to_string(),
            )),
        );
        let id = resource.id.clone();
        let current_state = carina_core::resource::State::existing(
            id.clone(),
            std::collections::HashMap::from([(
                "cidr_block".to_string(),
                carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String("10.0.0.0/16".to_string()),
                ),
            )]),
        )
        .with_identifier("vpc-old");
        let current_states = std::collections::HashMap::from([(id.clone(), current_state.clone())]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "",
            ResourceSchema::new("ec2.Vpc").attribute(
                AttributeSchema::new("cidr_block", AttributeType::string()).create_only(),
            ),
        );
        let plan_input_states = carina_core::resource::into_plan_input_map(
            current_states.clone(),
            &SchemaRegistry::new(),
            &[],
        );
        let plan = carina_core::differ::create_plan(
            std::slice::from_ref(&resource),
            &[],
            &ProviderRouter::new(),
            &plan_input_states,
            &std::collections::HashMap::new(),
            &schemas,
            &crate::empty_lifted_saved_attrs(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &[],
        );
        assert_eq!(plan.replace_display_info().count(), 1);
        let plan_file = crate::commands::plan::PlanFile {
            version: crate::commands::plan::PlanFile::CURRENT_VERSION,
            carina_version: "test".to_string(),
            timestamp: "2026-07-02T00:00:00Z".to_string(),
            source_path: "missing.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: Vec::new(),
            backend_config: None,
            plan,
            sorted_resources: vec![resource.clone()],
            unresolved_resources: vec![resource],
            compositions: Vec::new(),
            data_sources: Vec::new(),
            current_states: vec![crate::commands::plan::CurrentStateEntry {
                id,
                state: current_state,
            }],
            upstream_snapshot: std::collections::HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        };
        let json = serde_json::to_value(&plan_file).expect("plan file should serialize");
        (dir, plan_path, json)
    }

    async fn rejected_saved_plan_message(
        plan_path: &std::path::PathBuf,
        json: &serde_json::Value,
        expectation: &str,
    ) -> String {
        std::fs::write(plan_path, serde_json::to_string(json).unwrap()).expect("write plan");

        let result = crate::commands::apply::run_apply_from_plan(
            plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        result.expect_err(expectation).to_string()
    }

    fn replace_first_resource_identity(value: &mut serde_json::Value, identity: &str) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                if map.contains_key("resource_type") && map.contains_key("identity") {
                    map.insert(
                        "identity".to_string(),
                        serde_json::Value::String(identity.to_string()),
                    );
                    return true;
                }
                map.values_mut()
                    .any(|child| replace_first_resource_identity(child, identity))
            }
            serde_json::Value::Array(items) => items
                .iter_mut()
                .any(|child| replace_first_resource_identity(child, identity)),
            _ => false,
        }
    }

    fn set_replace_display_delete_generation_to_deposed(json: &mut serde_json::Value) {
        let delete_idx = json["plan"]["replace_display"][0]["delete_idx"]
            .as_u64()
            .expect("delete_idx should be numeric") as usize;
        let delete_effect = &mut json["plan"]["effects"][delete_idx];
        let deposed = serde_json::to_value(carina_core::effect::EffectGeneration::Deposed(
            carina_core::resource::DeposedKey::new_unique(),
        ))
        .expect("deposed generation should serialize");

        if let serde_json::Value::Object(effect) = delete_effect {
            if let Some(serde_json::Value::Object(delete)) = effect.get_mut("Delete") {
                delete.insert("generation".to_string(), deposed);
                return;
            }
            if effect
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind == "Delete")
            {
                effect.insert("generation".to_string(), deposed);
                return;
            }
        }

        panic!("fixture delete effect has an unexpected serde shape: {delete_effect:#?}");
    }

    #[tokio::test]
    async fn saved_plan_with_out_of_range_replace_display_create_idx_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        json["plan"]["replace_display"][0]["create_idx"] = serde_json::json!(999);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "corrupt replace_display metadata must be rejected",
        )
        .await;
        assert!(
            msg.contains("Invalid saved plan replacement metadata"),
            "error must name replacement metadata, got: {msg}",
        );
        assert!(
            msg.contains("create_idx 999 is out of bounds"),
            "error must name the bad create_idx, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_out_of_range_replace_display_delete_idx_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        json["plan"]["replace_display"][0]["delete_idx"] = serde_json::json!(999);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "corrupt replace_display metadata must be rejected",
        )
        .await;
        assert!(
            msg.contains("Invalid saved plan replacement metadata"),
            "error must name replacement metadata, got: {msg}",
        );
        assert!(
            msg.contains("delete_idx 999 is out of bounds"),
            "error must name the bad delete_idx, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_replace_display_create_idx_kind_mismatch_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        let delete_idx = json["plan"]["replace_display"][0]["delete_idx"]
            .as_u64()
            .expect("delete_idx should be numeric");
        json["plan"]["replace_display"][0]["create_idx"] = serde_json::json!(delete_idx);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "create_idx kind mismatch must be rejected",
        )
        .await;
        assert!(
            msg.contains("expected Create"),
            "error must name the create_idx kind mismatch, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_replace_display_delete_idx_kind_mismatch_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        let create_idx = json["plan"]["replace_display"][0]["create_idx"]
            .as_u64()
            .expect("create_idx should be numeric");
        json["plan"]["replace_display"][0]["delete_idx"] = serde_json::json!(create_idx);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "delete_idx kind mismatch must be rejected",
        )
        .await;
        assert!(
            msg.contains("expected Delete"),
            "error must name the delete_idx kind mismatch, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_replace_display_deposed_delete_half_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        set_replace_display_delete_generation_to_deposed(&mut json);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "deposed replacement delete half must be rejected",
        )
        .await;
        assert!(
            msg.contains("expected Current Delete"),
            "error must require a current-generation delete half, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_cross_wired_replace_display_indices_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        let create_idx = json["plan"]["replace_display"][0]["create_idx"]
            .as_u64()
            .expect("create_idx should be numeric") as usize;
        let effects = json["plan"]["effects"]
            .as_array_mut()
            .expect("effects should be an array");
        let mut other_create = effects[create_idx].clone();
        assert!(
            replace_first_resource_identity(&mut other_create, "other"),
            "fixture create effect should contain a resource identity"
        );
        effects.push(other_create);
        let other_create_idx = effects.len() - 1;
        json["plan"]["replace_display"][0]["create_idx"] = serde_json::json!(other_create_idx);

        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "cross-wired replacement metadata must be rejected",
        )
        .await;
        assert!(
            msg.contains("cross-wires"),
            "error must name cross-wired replacement metadata, got: {msg}",
        );
        assert!(
            msg.contains("expected both halves to address the same resource"),
            "error must describe same-resource invariant, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_duplicate_replace_display_index_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        let first = json["plan"]["replace_display"][0].clone();
        json["plan"]["replace_display"]
            .as_array_mut()
            .expect("replace_display should be an array")
            .push(first);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "duplicate replace_display metadata must be rejected",
        )
        .await;
        assert!(
            msg.contains("Invalid saved plan replacement metadata"),
            "error must name replacement metadata, got: {msg}",
        );
        assert!(
            msg.contains("duplicates or overlaps"),
            "error must name duplicate replacement metadata, got: {msg}",
        );
    }
}
#[test]
fn apply_exit_code_prioritizes_failure_over_partial() {
    assert_eq!(apply_exit_code_for_counts(0, 0), ApplyExitCode::Success);
    assert_eq!(
        apply_exit_code_for_counts(0, 1),
        ApplyExitCode::PartialSuccess
    );
    assert_eq!(apply_exit_code_for_counts(1, 0), ApplyExitCode::Failure);
    assert_eq!(apply_exit_code_for_counts(1, 1), ApplyExitCode::Failure);
}

#[test]
fn skipped_export_does_not_downgrade_partial_success() {
    let result = ExecutionResult {
        success_count: 0,
        failure_count: 0,
        partial_count: 1,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        successfully_deleted: HashSet::new(),
        permanent_name_overrides: HashMap::new(),
        current_states: HashMap::new(),
        bindings: Default::default(),
        failed_refreshes: HashSet::new(),
    };

    let err = finish_apply(
        &result,
        SkippedExports::from_names_for_test(&["role_arn"]),
        Duration::ZERO,
    )
    .unwrap_err();

    assert!(
        matches!(&err, AppError::PartialSuccess(_)),
        "a skipped export must not downgrade partial-success exit semantics: {err:?}",
    );
    let message = err.to_string();
    assert!(message.contains("1 partial"), "error: {message}");
    assert!(message.contains("1 export not written"), "error: {message}",);
}
