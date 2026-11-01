pub mod references;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::PathBuf;
use std::sync::Arc;

use carina_core::effect::PlanOp;
use carina_core::parser::ProviderConfig;
use carina_core::provider::{
    AppliedIdentifier, AppliedState, BoxFuture, CreateAttempt, CreateFailure, CreateOutcome,
    CreateRequest, DeleteRequest, Observation, PartialReadDiagnostic, PatchOp, PendingToken,
    Provider, ProviderError, ProviderNormalizer, ProviderResult, ReadOutcome, ReadRequest,
    Resume as CoreResume, ResumeCreateRequest, ResumeFailure, ResumeOutcome, SavedAttrs, Sent,
    StateTransformMap, UpdateOutcome, UpdateRequest,
};
#[cfg(test)]
use carina_core::provider::{
    CreateFailureKind, CreateOutcomeKind, InFlightCreate, InFlightPersistence, ResumeFailureKind,
    ResumeOutcomeKind,
};
use carina_core::resource::{
    BindingAddress, ConcreteValue, PlaceholderKind, Resource, ResourceId, State, Value,
    legacy_secret_migration,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use carina_core::value::{PersistedValue, migrate_legacy_persisted_json};
use serde::{Deserialize, Serialize};

mod behavior;
mod scope;
pub use behavior::*;
pub use scope::MockScope;

pub const BUILTIN_PROVIDER_NAME: &str = "mock";
const WRITE_ONLY_PASSWORD_ATTRIBUTE: &str = "password";
const NESTED_WRITE_ONLY_ATTRIBUTE: &str = "oidc_config";
const NESTED_WRITE_ONLY_FIELD: &str = "client_secret";
const SERVER_ADDED_LIST_RESOURCE_TYPE: &str = "test.server_added_list";
const KEYED_PARTIAL_UPDATE_RESOURCE_TYPE: &str = "test.keyed_partial_update_list";
const SERVER_ADDED_LIST_ATTRIBUTE: &str = "target_group_attributes";
const SERVER_ADDED_LIST_DEFAULTS: &[(&str, &str)] = &[
    ("stickiness.type", "lb_cookie"),
    ("load_balancing.algorithm.type", "round_robin"),
];

const MOCK_STATE_VERSION: u32 = 2;

type MockStates = HashMap<String, HashMap<String, PersistedValue>>;
type RawMockStates = HashMap<String, HashMap<String, serde_json::Value>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedMockStateFile {
    version: u32,
    states: RawMockStates,
}

#[derive(Serialize)]
struct MockStateFileRef<'a> {
    version: u32,
    states: &'a MockStates,
}

#[derive(Debug)]
struct MockStateValueDecodeError {
    resource_key: String,
    attribute_key: String,
    source: serde_json::Error,
}

impl std::fmt::Display for MockStateValueDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "mock state resource {:?} attribute {:?} is not valid persisted JSON",
            self.resource_key, self.attribute_key
        )
    }
}

impl std::error::Error for MockStateValueDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Return whether Carina selects its built-in mock provider.
///
/// This is the single selection rule shared by provider routing and schema
/// registration. The built-in implementation is selected either for the
/// legacy no-provider fallback, or for an explicit source-less default
/// `provider mock {}` whose provider factory was not loaded. A sourced mock
/// always belongs to the plugin path, including when that plugin failed to
/// load.
pub fn uses_builtin_provider<'a, P: PlaceholderKind + 'a, B: BindingAddress + 'a>(
    provider_configs: impl IntoIterator<Item = &'a ProviderConfig<P, B>>,
    provider_is_loaded: impl FnOnce(&str) -> bool,
) -> bool {
    let mut provider_configs = provider_configs.into_iter().peekable();
    if provider_configs.peek().is_none() {
        return true;
    }

    provider_configs.any(|config| {
        config.is_default() && config.name == BUILTIN_PROVIDER_NAME && config.source.is_none()
    }) && !provider_is_loaded(BUILTIN_PROVIDER_NAME)
}

/// Schemas exposed by Carina's built-in `provider mock {}` implementation.
pub fn builtin_schemas() -> Vec<ValidatedResourceSchema> {
    let schemas = vec![
        ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "rules",
                    AttributeType::list(AttributeType::struct_(
                        "Rule",
                        vec![
                            StructField::optional("action", AttributeType::string()).build(),
                            StructField::optional(
                                "conditions",
                                AttributeType::list(AttributeType::struct_(
                                    "Condition",
                                    vec![
                                        StructField::optional("value", AttributeType::string())
                                            .build(),
                                    ],
                                )),
                            )
                            .with_block_name("condition")
                            .build(),
                        ],
                    )),
                )
                .with_block_name("rule")
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "lifecycle_configuration",
                    AttributeType::struct_(
                        "LifecycleConfiguration",
                        vec![
                            StructField::optional(
                                "transitions",
                                AttributeType::list(AttributeType::struct_(
                                    "Transition",
                                    vec![
                                        StructField::optional(
                                            "storage_class",
                                            AttributeType::string(),
                                        )
                                        .build(),
                                    ],
                                )),
                            )
                            .with_block_name("transition")
                            .build(),
                            StructField::optional(
                                "transition",
                                AttributeType::struct_(
                                    "Transition",
                                    vec![
                                        StructField::optional(
                                            "storage_class",
                                            AttributeType::string(),
                                        )
                                        .build(),
                                    ],
                                ),
                            )
                            .build(),
                        ],
                    ),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::provider_populated("identifier", AttributeType::string()).build(),
            )
            .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    NESTED_WRITE_ONLY_ATTRIBUTE,
                    AttributeType::struct_(
                        "OidcConfig",
                        vec![
                            StructField::required("issuer", AttributeType::string()).build(),
                            StructField::required(NESTED_WRITE_ONLY_FIELD, AttributeType::string())
                                .write_only()
                                .build(),
                        ],
                    ),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(WRITE_ONLY_PASSWORD_ATTRIBUTE, AttributeType::string())
                    .write_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("replacement_key", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "choice",
                    AttributeType::union(vec![
                        AttributeType::struct_(
                            "StringChoice",
                            vec![StructField::required("value", AttributeType::string()).build()],
                        ),
                        AttributeType::struct_(
                            "IntegerChoice",
                            vec![StructField::required("number", AttributeType::int()).build()],
                        ),
                    ]),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("web_acl_arn", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("parent", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("parent_name", AttributeType::string()).build())
            .with_unique_name_attribute("name"),
        ResourceSchema::new("test.default_prevent_destroy")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("name")
            .default_prevent_destroy(),
        ResourceSchema::new(SERVER_ADDED_LIST_RESOURCE_TYPE)
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    SERVER_ADDED_LIST_ATTRIBUTE,
                    AttributeType::list(AttributeType::struct_(
                        "ServerAddedAttribute",
                        vec![
                            StructField::required("key", AttributeType::string()).build(),
                            StructField::required("value", AttributeType::string()).build(),
                        ],
                    )),
                )
                .with_block_name("target_group_attribute")
                .build(),
            )
            .with_unique_name_attribute("name"),
        ResourceSchema::new(KEYED_PARTIAL_UPDATE_RESOURCE_TYPE)
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    SERVER_ADDED_LIST_ATTRIBUTE,
                    AttributeType::list(AttributeType::struct_(
                        "KeyedPartialUpdateAttribute",
                        vec![
                            StructField::required("key", AttributeType::string()).build(),
                            StructField::required("value", AttributeType::string()).build(),
                        ],
                    )),
                )
                .with_block_name("target_group_attribute")
                .keyed_partial_update("key")
                .build(),
            )
            .with_unique_name_attribute("name"),
        ResourceSchema::new("test.deferred_source")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "replacement_options",
                    AttributeType::map(AttributeType::string()),
                )
                .create_only()
                .build(),
            )
            .attribute(
                AttributeSchema::provider_populated(
                    "generated_rules",
                    AttributeType::list(AttributeType::struct_(
                        "GeneratedRule",
                        vec![StructField::optional("action", AttributeType::string()).build()],
                    )),
                )
                .build(),
            )
            .with_unique_name_attribute("name"),
    ]
    .into_iter()
    .chain(anonymous_identity_schemas().into_iter().map(|schema| {
        if schema.resource_type() == "budgets.Budget" {
            schema.attribute(
                AttributeSchema::optional("cost_filters", AttributeType::string()).build(),
            )
        } else {
            schema
        }
    }))
    .chain([
        ResourceSchema::new("compute.Instance")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("cluster_name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::provider_populated("arn", AttributeType::string()).build()),
        ResourceSchema::new("example.thing")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()),
        ResourceSchema::new("example.consumer")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("plain", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("ref_only", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("prefix", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("nested", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("list", AttributeType::list(AttributeType::string()))
                    .build(),
            ),
        ResourceSchema::new("iam.Role")
            .attribute(
                AttributeSchema::optional("role_name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("path", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("max_session_duration", AttributeType::int())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build())
            .with_unique_name_attribute("role_name"),
        ResourceSchema::new("logs.LogGroup")
            .attribute(
                AttributeSchema::optional("log_group_name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .with_unique_name_attribute("log_group_name"),
        ResourceSchema::new("iam.Roles").attribute(
            AttributeSchema::optional("names", AttributeType::list(AttributeType::string()))
                .build(),
        ),
        ResourceSchema::new("iam.Roles")
            .attribute(AttributeSchema::optional("name_regex", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("names", AttributeType::list(AttributeType::string()))
                    .build(),
            )
            .as_data_source(),
        ResourceSchema::new("test.Thing")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .with_unique_name_attribute("name"),
    ]);

    schemas
        .map(|schema| {
            schema
                .validate()
                .expect("built-in mock resource schema must be valid")
        })
        .chain(std::iter::once(references::resource_schema()))
        .collect()
}

/// Portable unique-list fixture shared by native diagnostics and the WASM mock.
pub fn unique_list_schema() -> ResourceSchema {
    use carina_core::schema::{ListElements, TypeIdentity};

    let claim_keys = AttributeType::refined_list(
        AttributeType::enum_(
            TypeIdentity::new(Some("mock"), ["actions", "OidcSubject"], "ClaimKey"),
            Some(vec!["repo".into(), "context".into()]),
            Vec::new(),
            None,
            None,
        ),
        true,
        Some((Some(1), None)),
        ListElements::Unique,
    );
    ResourceSchema::new("actions.OidcSubject")
        .attribute(AttributeSchema::required("claim_keys", claim_keys.clone()).build())
        .attribute(
            AttributeSchema::optional(
                "claim_keys_or_number",
                AttributeType::union(vec![claim_keys, AttributeType::int()]),
            )
            .build(),
        )
}

/// Identity schemas shared by the built-in and process mock providers.
pub fn anonymous_identity_schemas() -> Vec<ResourceSchema> {
    vec![
        ResourceSchema::new("test.nested_identity")
            .attribute(
                AttributeSchema::required(
                    "identity",
                    AttributeType::struct_(
                        "NestedIdentity",
                        vec![
                            StructField::required("key", AttributeType::string())
                                .create_only()
                                .identity()
                                .build(),
                        ],
                    ),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build()),
        ResourceSchema::new("test.unique_identity")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .attribute(
                AttributeSchema::required("replacement_key", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build())
            .with_unique_name_attribute("name"),
        ResourceSchema::new("budgets.Budget")
            .attribute(
                AttributeSchema::required(
                    "budget",
                    AttributeType::struct_(
                        "BudgetData",
                        vec![
                            StructField::optional("budget_name", AttributeType::string())
                                .create_only()
                                .identity()
                                .build(),
                            StructField::required("budget_type", AttributeType::string()).build(),
                            StructField::required("time_unit", AttributeType::string()).build(),
                        ],
                    ),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "notifications_with_subscribers",
                    AttributeType::list(AttributeType::struct_(
                        "NotificationWithSubscribers",
                        vec![
                            StructField::required("notification_type", AttributeType::string())
                                .build(),
                            StructField::required("threshold", AttributeType::float()).build(),
                        ],
                    )),
                )
                .create_only()
                .build(),
            ),
        ResourceSchema::new("route53.HostedZone")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::provider_populated("id", AttributeType::string()).build()),
        ResourceSchema::new("route53.RecordSet")
            .attribute(
                AttributeSchema::required("hosted_zone_id", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .attribute(
                AttributeSchema::required("type", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "resource_records",
                    AttributeType::list(AttributeType::string()),
                )
                .create_only()
                .build(),
            ),
    ]
}

/// Values generated by the mock provider after a resource is created.
pub fn generated_attribute(
    resource_type: &str,
    identity: Option<&str>,
    name: Option<&str>,
) -> Option<(&'static str, String)> {
    match resource_type {
        "test.resource" => name
            .or(identity)
            .map(|identity| ("identifier", format!("{identity}-id"))),
        "route53.HostedZone" => identity.map(|identity| ("id", format!("{identity}-id"))),
        _ => None,
    }
}

/// Aggregate schema witness exposed by Carina's built-in mock provider.
pub fn builtin_schema_witness() -> ValidatedProviderSchemaWitness {
    ValidatedProviderSchemaWitness::new(
        builtin_schemas(),
        ValidatedConfigSchema::default(),
        Default::default(),
    )
    .expect("built-in mock schemas form a valid provider-wide witness")
}

pub struct MockProvider {
    behavior: MockBehavior,
    scope: MockScope,
    telemetry: Arc<scope::UpdateTelemetry>,
}

/// One invocation owns the storage scope and update telemetry shared by all aliases.
#[derive(Clone)]
pub struct ConfiguredBuiltinMock {
    behavior: MockBehavior,
    scope: MockScope,
    telemetry: Arc<scope::UpdateTelemetry>,
}

impl ConfiguredBuiltinMock {
    pub fn new(behavior: MockBehavior, scope: MockScope) -> Self {
        let telemetry = Arc::new(scope::UpdateTelemetry::new(&behavior.observations));
        Self {
            behavior,
            scope,
            telemetry,
        }
    }
    pub fn create_provider(&self) -> MockProvider {
        MockProvider {
            behavior: self.behavior.clone(),
            scope: self.scope.clone(),
            telemetry: self.telemetry.clone(),
        }
    }
    pub fn create_normalizer(&self) -> MockNormalizer {
        MockNormalizer {
            observations: self.behavior.observations.clone(),
        }
    }
}

/// Normalization has no store and cannot initialize provider telemetry or CRUD.
pub struct MockNormalizer {
    observations: MockObservations,
}

struct ReadOutcomeGuard {
    path: Option<PathBuf>,
    completed: bool,
}

impl ReadOutcomeGuard {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
        if let Some(path) = &self.path {
            let _ = fs::write(path, "completed\n");
        }
    }
}

impl Drop for ReadOutcomeGuard {
    fn drop(&mut self) {
        if !self.completed
            && let Some(path) = &self.path
        {
            let _ = fs::write(path, "abandoned\n");
        }
    }
}

impl scope::ScopeLock<'_> {
    fn load_states_for_operation(&self) -> ProviderResult<MockStates> {
        self.load()
            .map_err(|error| ProviderError::internal_caused_by("Failed to load mock state", error))
    }
}

impl MockProvider {
    fn resolve_reference_requests(
        &self,
        requests: Vec<carina_plugin_sdk::types::ReferenceRequest>,
    ) -> ProviderResult<
        Vec<
            Result<
                carina_plugin_sdk::types::ReferenceOutcome,
                carina_plugin_sdk::types::ReferenceError,
            >,
        >,
    > {
        let table = match &self.behavior.reference_table {
            Some(table) => serde_json::from_str(table).map_err(|error| {
                ProviderError::invalid_input_caused_by("Invalid mock reference table", error)
            })?,
            None => references::ReferenceTable::new(),
        };
        Ok(references::resolve(&table, requests))
    }

    fn canonicalize_mock_reference_write(
        &self,
        id: &ResourceId,
        attributes: &mut HashMap<String, Value>,
    ) -> ProviderResult<()> {
        if id.resource_type != "test.reference" || self.behavior.reference_echo_alias {
            return Ok(());
        }
        for name in ["key", "fixed_key"] {
            let Some(Value::Concrete(ConcreteValue::String(raw))) = attributes.get(name) else {
                continue;
            };
            let request = carina_plugin_sdk::types::ReferenceRequest {
                ty: carina_plugin_sdk::types::TypeIdentity {
                    provider: "mock".into(),
                    segments: vec!["keys".into(), "Key".into()],
                    kind: "Reference".into(),
                },
                value: raw.expose_plaintext().into(),
            };
            let mut results = self.resolve_reference_requests(vec![request])?;
            if let Some(Ok(carina_plugin_sdk::types::ReferenceOutcome::Resolved(value))) =
                results.pop()
            {
                attributes.insert(
                    name.into(),
                    Value::Concrete(ConcreteValue::String(value.into())),
                );
            }
        }
        Ok(())
    }

    fn decode_state_file(content: &str) -> io::Result<MockStates> {
        let mut encoded: serde_json::Value = serde_json::from_str(content)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
        let is_versioned = encoded
            .as_object()
            .is_some_and(|object| object.contains_key("version") || object.contains_key("states"));
        if !is_versioned {
            let mut states: RawMockStates = serde_json::from_value(encoded)
                .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
            for attributes in states.values_mut() {
                for value in attributes.values_mut() {
                    migrate_legacy_persisted_json(value);
                }
            }
            encoded = serde_json::json!({
                "version": 1,
                "states": states,
            });
        }

        let file: EncodedMockStateFile = serde_json::from_value(encoded.clone())
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
        let mut states = match file.version {
            MOCK_STATE_VERSION => file.states,
            1 => {
                legacy_secret_migration::migrate_mock_state_document_v1(&mut encoded)
                    .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
                serde_json::from_value::<EncodedMockStateFile>(encoded)
                    .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?
                    .states
            }
            version => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "unsupported mock state version {version}; expected {MOCK_STATE_VERSION}"
                    ),
                ));
            }
        };

        let mut decoded = HashMap::with_capacity(states.len());
        for (resource_key, attributes) in &mut states {
            let mut decoded_attributes = HashMap::with_capacity(attributes.len());
            for (attribute_key, value) in attributes {
                let value = serde_json::from_value(value.take()).map_err(|source| {
                    io::Error::new(
                        ErrorKind::InvalidData,
                        MockStateValueDecodeError {
                            resource_key: resource_key.clone(),
                            attribute_key: attribute_key.clone(),
                            source,
                        },
                    )
                })?;
                decoded_attributes.insert(attribute_key.clone(), value);
            }
            decoded.insert(resource_key.clone(), decoded_attributes);
        }
        Ok(decoded)
    }

    fn lock_state_file(&self) -> ProviderResult<scope::ScopeLock<'_>> {
        self.scope
            .lock()
            .map_err(|error| ProviderError::internal_caused_by("Failed to lock mock state", error))
    }

    fn resource_key(id: &ResourceId) -> Option<String> {
        id.identity_str()
            .map(|identity| format!("{}.{}", id.resource_type, identity))
    }

    fn resolved_resource_key(id: &ResourceId) -> ProviderResult<String> {
        Self::resource_key(id).ok_or_else(|| {
            ProviderError::invalid_input(format!(
                "mock provider operation requires a resolved resource identity for {id}"
            ))
        })
    }

    fn partial_create_config_for(&self, id: &ResourceId) -> Option<&PartialResult> {
        let config = self.behavior.partial_create.as_ref()?;
        Self::partial_config_matches(config, id).then_some(config)
    }

    fn partial_update_config_for(&self, id: &ResourceId) -> Option<&PartialResult> {
        let config = self.behavior.partial_update.as_ref()?;
        Self::partial_config_matches(config, id).then_some(config)
    }

    fn partial_config_matches(config: &PartialResult, id: &ResourceId) -> bool {
        config.selector.matches(id)
    }

    fn append_delete_log(path: PathBuf, key: &str) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(format!("{key}\n").as_bytes())
    }

    fn append_op_log(path: PathBuf, op: &str, key: &str) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(format!("{op} {key}\n").as_bytes())
    }

    fn append_op_log_if_configured(&self, op: &str, id: &ResourceId) -> ProviderResult<()> {
        if let Some(path) = self.behavior.observations.operation_log.clone() {
            let key = Self::resolved_resource_key(id)?;
            Self::append_op_log(path, op, &key).map_err(|e| {
                ProviderError::internal_caused_by("Failed to append operation log", e)
            })?;
        }
        Ok(())
    }

    fn append_update_request_if_configured(&self, request: &UpdateRequest) -> ProviderResult<()> {
        let Some(path) = self.behavior.observations.update_request_log.clone() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                ProviderError::internal_caused_by("Failed to create update request log", error)
            })?;
        }
        let ops = request
            .patch()
            .ops
            .iter()
            .map(|op| {
                let (operation, key, value) = match op {
                    PatchOp::Add { key, value } => ("add", key, Some(value)),
                    PatchOp::Replace { key, value } => ("replace", key, Some(value)),
                    PatchOp::Reassert { key, value } => ("reassert", key, Some(value)),
                    PatchOp::Remove { key } => ("remove", key, None),
                };
                let value = value
                    .map(|value| PersistedValue::encode_provider_value(value, None))
                    .transpose()
                    .map_err(|error| {
                        ProviderError::internal(format!(
                            "Failed to encode mock update request: {error}"
                        ))
                    })?;
                Ok(serde_json::json!({
                    "op": operation,
                    "key": key,
                    "value": value,
                }))
            })
            .collect::<ProviderResult<Vec<_>>>()?;
        let line = serde_json::to_string(&serde_json::json!({ "ops": ops })).map_err(|error| {
            ProviderError::internal_caused_by("Failed to serialize update request log", error)
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| {
                ProviderError::internal_caused_by("Failed to open update request log", error)
            })?;
        file.write_all(format!("{line}\n").as_bytes())
            .map_err(|error| {
                ProviderError::internal_caused_by("Failed to append update request log", error)
            })
    }

    fn create_fail_error_for(&self, id: &ResourceId) -> Option<ProviderError> {
        let target = self.behavior.create_fail_for.as_ref()?;
        let key = Self::resource_key(id)?;
        (target == &key).then(|| {
            ProviderError::internal(format!(
                "CARINA_MOCK_CREATE_FAIL_FOR requested create failure for {key}"
            ))
        })
    }

    fn update_fail_error_for(&self, id: &ResourceId) -> Option<ProviderError> {
        let target = self.behavior.update_fail_for.as_ref()?;
        let key = Self::resource_key(id)?;
        (target.matches(id)).then(|| {
            ProviderError::internal(format!(
                "CARINA_MOCK_UPDATE_FAIL_FOR requested update failure for {key}"
            ))
        })
    }

    fn delete_fail_error_for(&self, id: &ResourceId, identifier: &str) -> Option<ProviderError> {
        let target = self.behavior.delete_fail_for.as_ref()?;
        let key = Self::resource_key(id)?;
        if !target.matches(id) {
            return None;
        }
        if let Some(target_identifier) = &self.behavior.delete_fail_identifier
            && target_identifier != identifier
        {
            return None;
        }
        Some(ProviderError::internal(format!(
            "CARINA_MOCK_DELETE_FAIL_FOR requested delete failure for {key} ({identifier})"
        )))
    }

    fn delete_timeout_error_for(&self, id: &ResourceId) -> Option<ProviderError> {
        let target = self.behavior.delete_timeout_for.as_ref()?;
        let key = Self::resource_key(id)?;
        (target.matches(id)).then(|| {
            ProviderError::timeout(format!(
                "Operation timed out: CARINA_MOCK_DELETE_TIMEOUT_FOR matched {key}"
            ))
        })
    }

    fn populate_generated_attribute_json(
        id: &ResourceId,
        attrs: &mut HashMap<String, PersistedValue>,
    ) {
        let name =
            attrs
                .get("name")
                .and_then(PersistedValue::decode)
                .and_then(|value| match value {
                    Value::Concrete(ConcreteValue::String(value)) => Some(value.into_plaintext()),
                    _ => None,
                });
        if let Some((attribute, value)) =
            generated_attribute(&id.resource_type, id.identity_str(), name.as_deref())
        {
            attrs.entry(attribute.to_string()).or_insert_with(|| {
                PersistedValue::encode_provider_value(
                    &Value::Concrete(ConcreteValue::String(value.clone().into())),
                    None,
                )
                .expect("mock generated attribute is persistable")
            });
        }
    }

    fn populate_generated_attribute_value(id: &ResourceId, attrs: &mut HashMap<String, Value>) {
        let name = attrs
            .get("name")
            .and_then(|value| match value {
                Value::Concrete(ConcreteValue::String(name)) => Some(name.expose_plaintext()),
                _ => None,
            })
            .map(str::to_string);
        if let Some((attribute, value)) =
            generated_attribute(&id.resource_type, id.identity_str(), name.as_deref())
        {
            attrs
                .entry(attribute.to_string())
                .or_insert_with(|| Value::Concrete(ConcreteValue::String(value.clone().into())));
        }
    }

    fn strip_unobservable_nested_write_only(attributes: &mut HashMap<String, Value>) {
        let Some(Value::Concrete(ConcreteValue::Map(config))) =
            attributes.get_mut(NESTED_WRITE_ONLY_ATTRIBUTE)
        else {
            return;
        };
        config.shift_remove(NESTED_WRITE_ONLY_FIELD);
    }

    fn add_server_list_defaults(id: &ResourceId, attributes: &mut HashMap<String, Value>) {
        if id.resource_type != SERVER_ADDED_LIST_RESOURCE_TYPE
            && id.resource_type != KEYED_PARTIAL_UPDATE_RESOURCE_TYPE
        {
            return;
        }

        let list = attributes
            .entry(SERVER_ADDED_LIST_ATTRIBUTE.to_string())
            .or_insert_with(|| Value::Concrete(ConcreteValue::List(Vec::new())));
        let Value::Concrete(ConcreteValue::List(items)) = list else {
            return;
        };

        for (default_key, default_value) in SERVER_ADDED_LIST_DEFAULTS {
            let already_present = items.iter().any(|item| {
                let Value::Concrete(ConcreteValue::Map(fields)) = item else {
                    return false;
                };
                matches!(
                    fields.get("key"),
                    Some(Value::Concrete(ConcreteValue::String(key)))
                        if key.expose_plaintext() == *default_key
                )
            });
            if already_present {
                continue;
            }

            items.push(Value::Concrete(ConcreteValue::Map(
                [
                    (
                        "key".to_string(),
                        Value::Concrete(ConcreteValue::String((*default_key).into())),
                    ),
                    (
                        "value".to_string(),
                        Value::Concrete(ConcreteValue::String((*default_value).into())),
                    ),
                ]
                .into_iter()
                .collect(),
            )));
        }
    }

    fn add_server_list_update_out_of_band_element(
        &self,
        id: &ResourceId,
        attributes: &mut HashMap<String, Value>,
    ) {
        if id.resource_type != SERVER_ADDED_LIST_RESOURCE_TYPE {
            return;
        }
        let Some(ServerListMutation { key, value }) = &self.behavior.server_list_mutation else {
            return;
        };
        let Some(Value::Concrete(ConcreteValue::List(items))) =
            attributes.get_mut(SERVER_ADDED_LIST_ATTRIBUTE)
        else {
            return;
        };
        items.push(Value::Concrete(ConcreteValue::Map(
            [
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String(key.clone().into())),
                ),
                (
                    "value".to_string(),
                    Value::Concrete(ConcreteValue::String(value.clone().into())),
                ),
            ]
            .into_iter()
            .collect(),
        )));
    }

    fn merge_keyed_partial_update_list(
        id: &ResourceId,
        attributes: &mut HashMap<String, Value>,
        key: &str,
        value: &Value,
    ) -> bool {
        if id.resource_type != KEYED_PARTIAL_UPDATE_RESOURCE_TYPE
            || key != SERVER_ADDED_LIST_ATTRIBUTE
        {
            return false;
        }
        let Value::Concrete(ConcreteValue::List(sent)) = value else {
            return false;
        };
        let current = attributes
            .entry(key.to_string())
            .or_insert_with(|| Value::Concrete(ConcreteValue::List(Vec::new())));
        let Value::Concrete(ConcreteValue::List(current)) = current else {
            return false;
        };
        for sent_element in sent {
            let sent_key = match sent_element {
                Value::Concrete(ConcreteValue::Map(fields)) => match fields.get("key") {
                    Some(Value::Concrete(ConcreteValue::String(key))) => {
                        Some(key.expose_plaintext())
                    }
                    _ => None,
                },
                _ => None,
            };
            let Some(sent_key) = sent_key else {
                continue;
            };
            if let Some(existing) = current.iter_mut().find(|element| {
                matches!(
                    element,
                    Value::Concrete(ConcreteValue::Map(fields))
                        if matches!(
                            fields.get("key"),
                            Some(Value::Concrete(ConcreteValue::String(key)))
                                if key.expose_plaintext() == sent_key
                        )
                )
            }) {
                *existing = sent_element.clone();
            } else {
                current.push(sent_element.clone());
            }
        }
        true
    }

    fn signal_ready(path: &Option<PathBuf>) -> ProviderResult<()> {
        let Some(path) = path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                ProviderError::internal_caused_by("Failed to create mock readiness directory", err)
            })?;
        }
        fs::write(path, "ready\n").map_err(|err| {
            ProviderError::internal_caused_by("Failed to write mock readiness file", err)
        })
    }

    async fn create_resource<S, D>(
        &self,
        id: ResourceId,
        resource: Resource<
            S,
            carina_core::resource::NoPlaceholder,
            carina_core::resource::AbsoluteBindingAddress,
            D,
        >,
        attempt: CreateAttempt,
    ) -> Result<CreateOutcome, CreateFailure> {
        let mode = &self.behavior.create_mode;
        if let Some(err) = self.create_fail_error_for(&id) {
            return Err(attempt.untracked(err));
        }
        let sent = self.behavior.create_sent.sent();
        let fail_after_create = match mode {
            CreateMode::Success => false,
            CreateMode::Created => true,
            CreateMode::Untracked => {
                return Err(attempt.untracked(ProviderError::api_error(
                    "builtin mock returned an untracked create verdict",
                )));
            }
            CreateMode::Pending | CreateMode::MalformedPending => {
                let token = if *mode == CreateMode::MalformedPending {
                    "not-a-decodable-mock-token"
                } else {
                    "builtin-mock-pending"
                };
                let pending = attempt.pending(PendingToken::new(token), sent, 1);
                return Err(pending.preserve(ProviderError::timeout(
                    "builtin mock create remains in progress",
                )));
            }
        };

        if let Some(delay) = self
            .behavior
            .create_delay
            .as_ref()
            .and_then(|delay| delay.for_resource(&id))
        {
            if let Err(error) = Self::signal_ready(&self.behavior.observations.create_ready) {
                return Err(attempt.untracked(error));
            }
            if let Some(release) = &self.behavior.create_release {
                // Readiness/release files order tests without a finite sleep.
                while !release.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            } else {
                tokio::time::sleep(delay).await;
            }
        }

        let state_guard = match self.lock_state_file() {
            Ok(guard) => guard,
            Err(error) => return Err(attempt.untracked(error)),
        };
        let mut states = match state_guard.load_states_for_operation() {
            Ok(states) => states,
            Err(error) => return Err(attempt.untracked(error)),
        };
        let key = match Self::resolved_resource_key(&id) {
            Ok(key) => key,
            Err(error) => return Err(attempt.untracked(error)),
        };

        let mut persisted_values = resource.resolved_attributes();
        if let Err(error) = self.canonicalize_mock_reference_write(&id, &mut persisted_values) {
            return Err(attempt.untracked(error));
        }
        Self::add_server_list_defaults(&id, &mut persisted_values);
        let attrs = persisted_values
            .iter()
            .map(|(k, v)| PersistedValue::encode_provider_value(v, None).map(|jv| (k.clone(), jv)))
            .collect::<Result<_, _>>()
            .map_err(|e| ProviderError::internal(format!("Failed to convert value: {}", e)));
        let mut attrs = match attrs {
            Ok(attrs) => attrs,
            Err(error) => return Err(attempt.untracked(error)),
        };
        Self::populate_generated_attribute_json(&id, &mut attrs);

        let partial_create = self.partial_create_config_for(&id);
        if let Some(config) = partial_create {
            for attr in &config.missing_attributes {
                attrs.remove(attr);
            }
        }

        states.insert(key, attrs);
        if let Err(error) = state_guard
            .save(&states)
            .map_err(|e| ProviderError::internal_caused_by("Failed to save state", e))
        {
            return Err(attempt.untracked(error));
        }

        let mut state_attrs = persisted_values;
        Self::populate_generated_attribute_value(&id, &mut state_attrs);
        Self::strip_unobservable_nested_write_only(&mut state_attrs);
        if let Some(config) = partial_create {
            for attr in &config.missing_attributes {
                state_attrs.remove(attr);
            }
        }
        let identifier =
            AppliedIdentifier::new("mock-id").expect("mock provider identifier is non-empty");
        let observed_state =
            AppliedState::from_identifier(id.clone(), identifier.clone(), state_attrs.clone());
        let issued = attempt.issued(identifier, sent);
        if let Err(error) = self.append_op_log_if_configured("create", &id) {
            return Err(issued.failed(Observation::Read(ReadOutcome::found(observed_state)), error));
        }
        if fail_after_create {
            return Err(issued.failed(
                Observation::Read(ReadOutcome::found(observed_state)),
                ProviderError::api_error("builtin mock failed after creating the resource"),
            ));
        }
        if let Some(config) = partial_create {
            let outcome = match PartialReadDiagnostic::new(
                "mock partial create".to_string(),
                config.missing_attributes.clone(),
            ) {
                Some(diagnostic) => issued.partial(state_attrs, diagnostic),
                None => issued.succeeded(state_attrs),
            };
            return Ok(outcome);
        }

        Ok(issued.succeeded(state_attrs))
    }

    fn persist_resumed_resource<S, D>(
        &self,
        id: &ResourceId,
        resource: &Resource<
            S,
            carina_core::resource::NoPlaceholder,
            carina_core::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> ProviderResult<HashMap<String, Value>> {
        let state_guard = self.lock_state_file()?;
        let mut states = state_guard.load_states_for_operation()?;
        let key = Self::resolved_resource_key(id)?;
        let mut persisted_values = resource.resolved_attributes();
        Self::add_server_list_defaults(id, &mut persisted_values);
        let mut persisted = persisted_values
            .iter()
            .map(|(key, value)| {
                PersistedValue::encode_provider_value(value, None).map(|value| (key.clone(), value))
            })
            .collect::<Result<HashMap<_, _>, _>>()
            .map_err(|error| {
                ProviderError::internal_caused_by(
                    "Failed to encode builtin mock resumed-create state",
                    error,
                )
            })?;
        Self::populate_generated_attribute_json(id, &mut persisted);
        states.insert(key, persisted);
        state_guard.save(&states).map_err(|error| {
            ProviderError::internal_caused_by(
                "Failed to save builtin mock resumed-create state",
                error,
            )
        })?;

        let mut attributes = persisted_values;
        Self::populate_generated_attribute_value(id, &mut attributes);
        Self::strip_unobservable_nested_write_only(&mut attributes);
        Ok(attributes)
    }
}

fn append_update_patch_log(
    path: &Option<PathBuf>,
    id: &ResourceId,
    ops: &[PatchOp],
) -> ProviderResult<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            ProviderError::internal_caused_by(
                "Failed to create mock update patch log directory",
                error,
            )
        })?;
    }
    let operations = ops
        .iter()
        .map(|op| match op {
            PatchOp::Add { key, .. } => serde_json::json!({ "op": "add", "key": key }),
            PatchOp::Replace { key, .. } => {
                serde_json::json!({ "op": "replace", "key": key })
            }
            PatchOp::Reassert { key, .. } => {
                serde_json::json!({ "op": "reassert", "key": key })
            }
            PatchOp::Remove { key } => serde_json::json!({ "op": "remove", "key": key }),
        })
        .collect::<Vec<_>>();
    let line = serde_json::to_string(&serde_json::json!({
        "resource": id.to_string(),
        "operations": operations,
    }))
    .map_err(|error| {
        ProviderError::internal_caused_by("Failed to encode mock update patch log", error)
    })?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| {
            ProviderError::internal_caused_by("Failed to open mock update patch log", error)
        })?;
    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|error| {
            ProviderError::internal_caused_by("Failed to append mock update patch log", error)
        })
}

impl MockProvider {
    async fn read_attributes(
        &self,
        id: &ResourceId,
    ) -> ProviderResult<Option<HashMap<String, Value>>> {
        self.append_op_log_if_configured("read", id)?;
        let mut read_outcome = None;
        if let Some(delay) = self
            .behavior
            .read_delay
            .as_ref()
            .and_then(|delay| delay.for_resource(id))
        {
            read_outcome = Some(ReadOutcomeGuard::new(
                self.behavior.observations.read_outcome.clone(),
            ));
            Self::signal_ready(&self.behavior.observations.read_ready)?;
            tokio::time::sleep(delay).await;
        }

        let state_guard = self.lock_state_file()?;
        let states = state_guard.load_states_for_operation()?;
        let attributes = Self::resource_key(id).and_then(|key| {
            states.get(&key).map(|attrs| {
                let mut attributes: HashMap<_, _> = attrs
                    .iter()
                    .filter(|(key, _value)| key.as_str() != WRITE_ONLY_PASSWORD_ATTRIBUTE)
                    .filter_map(|(key, value)| value.decode().map(|value| (key.clone(), value)))
                    .collect();
                Self::strip_unobservable_nested_write_only(&mut attributes);
                attributes
            })
        });
        if let Some(read_outcome) = &mut read_outcome {
            read_outcome.complete();
        }
        Ok(attributes)
    }
}

impl MockNormalizer {
    fn append_normalizer_log_if_configured(&self, op: &str, id: &ResourceId) -> ProviderResult<()> {
        if let Some(path) = self.observations.operation_log.clone() {
            let key = MockProvider::resource_key(id).unwrap_or_else(|| id.to_string());
            MockProvider::append_op_log(path, op, &key).map_err(|error| {
                ProviderError::internal_caused_by("Failed to append operation log", error)
            })?;
        }
        Ok(())
    }
}

impl ProviderNormalizer for MockNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [carina_core::resource::ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            for resource in resources
                .iter()
                .filter(|resource| resource.id.provider == BUILTIN_PROVIDER_NAME)
            {
                self.append_normalizer_log_if_configured("normalize_desired", &resource.id)?;
            }
            Ok(())
        })
    }

    fn normalize_state<'a>(
        &'a self,
        current_states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            for (id, _state) in current_states
                .iter()
                .filter(|(id, _state)| id.provider == BUILTIN_PROVIDER_NAME)
            {
                self.append_normalizer_log_if_configured("normalize_state", id)?;
            }
            Ok(())
        })
    }

    fn hydrate_read_state<'a>(
        &'a self,
        current_states: &'a mut StateTransformMap,
        _saved_attrs: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            for (id, _state) in current_states
                .iter()
                .filter(|(id, _state)| id.provider == BUILTIN_PROVIDER_NAME)
            {
                self.append_normalizer_log_if_configured("hydrate_read_state", id)?;
            }
            Ok(())
        })
    }
}

impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn resolve_references<'a>(
        &'a self,
        requests: &'a [carina_core::references::ReferenceRequest],
    ) -> BoxFuture<
        'a,
        ProviderResult<
            Vec<
                Result<
                    carina_core::references::ReferenceOutcome,
                    carina_core::references::ReferenceError,
                >,
            >,
        >,
    > {
        Box::pin(async move {
            use carina_core::references::{ReferenceError, ReferenceOutcome};
            use carina_plugin_sdk::types::{
                ReferenceError as WireError, TypeIdentity as WireIdentity,
            };
            let requests = requests
                .iter()
                .map(|request| carina_plugin_sdk::types::ReferenceRequest {
                    ty: WireIdentity {
                        provider: request.ty.provider.clone().unwrap_or_default(),
                        segments: request.ty.segments.clone(),
                        kind: request.ty.kind.clone(),
                    },
                    value: request.value.clone(),
                })
                .collect();
            Ok(self
                .resolve_reference_requests(requests)?
                .into_iter()
                .map(|result| match result {
                    Ok(outcome) => ReferenceOutcome::from_guest(outcome),
                    Err(WireError::AccessDenied(error)) => Err(ReferenceError::AccessDenied(
                        ProviderError::from_wire(*error).into(),
                    )),
                    Err(WireError::Transient(error)) => Err(ReferenceError::Transient(
                        ProviderError::from_wire(*error).into(),
                    )),
                    Err(WireError::InvalidReference(error)) => Err(
                        ReferenceError::InvalidReference(ProviderError::from_wire(*error).into()),
                    ),
                })
                .collect())
        })
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let id = id.clone();
        let identifier = identifier.clone();
        Box::pin(async move {
            let outcome = if let Some(attributes) = self.read_attributes(&id).await? {
                ReadOutcome::found(AppliedState::from_identifier(id, identifier, attributes))
            } else {
                ReadOutcome::not_found(id)
            };
            Ok(outcome)
        })
    }

    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = resource.id.clone();
        Box::pin(async move {
            Ok(match self.read_attributes(&id).await? {
                Some(attributes) => State::existing(id, attributes),
                None => State::not_found(id),
            })
        })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let id = id.clone();
        let resource = request.resource().as_resource().clone();
        Box::pin(async move { self.create_resource(id, resource, attempt).await })
    }

    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        Box::pin(async move {
            let id = request.id().clone();
            let mode = &self.behavior.resume_mode;
            match mode {
                ResumeMode::Discarded => {
                    let (resume, create_request) = request.into_parts();
                    drop(create_request);
                    return Err(resume.discarded(ProviderError::not_found(
                        "builtin mock reports that the recorded create ended",
                    )));
                }
                ResumeMode::Untracked => {
                    let (resume, create_request) = request.into_parts();
                    drop(create_request);
                    return Err(resume.untracked(ProviderError::api_error(
                        "builtin mock lost the resume response",
                    )));
                }
                ResumeMode::Undetermined | ResumeMode::MalformedPending => {
                    let (resume, create_request) = request.into_parts();
                    drop(create_request);
                    return Err(resume.preserve(ProviderError::internal(
                        "builtin mock cannot determine the recorded create outcome",
                    )));
                }
                ResumeMode::Pending => {
                    let (resume, create_request) = request.into_parts();
                    drop(create_request);
                    let next_token = PendingToken::new("builtin-mock-repending");
                    let resume = match resume {
                        CoreResume::Unissued(attempt) => attempt.repend(next_token),
                        CoreResume::Issued(resumed) => resumed.repend(next_token),
                    };
                    return Err(resume.preserve(ProviderError::timeout(
                        "builtin mock create remains in progress",
                    )));
                }
                ResumeMode::Resumed | ResumeMode::Resend | ResumeMode::CreatedReadFailed => {}
                ResumeMode::Unsupported(unsupported) => {
                    let (resume, create_request) = request.into_parts();
                    drop(create_request);
                    return Err(resume.preserve(ProviderError::invalid_input(format!(
                        "unsupported CARINA_MOCK_RESUME_MODE {unsupported:?}"
                    ))));
                }
            }

            let (resume, create_request) = request.into_parts();
            let resource = create_request.resource().as_resource().clone();
            let fallback_identifier =
                AppliedIdentifier::new("mock-id").expect("mock provider identifier is non-empty");

            if *mode == ResumeMode::Resend {
                let attempt = match resume {
                    CoreResume::Unissued(attempt) => attempt.resend(),
                    CoreResume::Issued(resumed) => {
                        return Err(resumed.preserve(ProviderError::invalid_input(
                            "builtin mock cannot resend a create with a known identifier",
                        )));
                    }
                };
                let attributes = match self.persist_resumed_resource(&id, &resource) {
                    Ok(attributes) => attributes,
                    Err(error) => return Err(attempt.preserve(error)),
                };
                return Ok(attempt
                    .issued(fallback_identifier, Sent::AllRequestAttributes)
                    .succeeded(attributes));
            }

            let resumed = match resume {
                CoreResume::Unissued(attempt) => attempt.issued(fallback_identifier),
                CoreResume::Issued(resumed) => resumed,
            };
            let identifier = resumed.identifier().clone();
            let attributes = match self.persist_resumed_resource(&id, &resource) {
                Ok(attributes) => attributes,
                Err(error) => return Err(resumed.preserve(error)),
            };
            if *mode == ResumeMode::CreatedReadFailed {
                return Err(resumed.failed(
                    Observation::ReadFailed(ProviderError::api_error(
                        "builtin mock post-apply read failed",
                    )),
                    ProviderError::api_error(
                        "builtin mock resume finished after creating the resource",
                    ),
                ));
            }
            let observed_state = AppliedState::from_identifier(id, identifier, attributes);
            Ok(resumed.completed(observed_state))
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.as_str().to_string();
        Box::pin(async move {
            let key = Self::resolved_resource_key(&id)?;
            let _active = self
                .telemetry
                .enter_update(key.clone(), &self.behavior.observations);
            if self.behavior.observations.update_trace.is_some() {
                // This test hook is coupled to the executor's continuous-refill loop:
                // it dispatches every currently ready future that fits before polling
                // any of them. Yield once so that dispatch batch records all starts
                // before any finish. Trace tests reconstruct causal waves from that
                // ordering; an await before this write or during dispatch must revisit
                // those assertions. The executor itself is not wave-based.
                tokio::task::yield_now().await;
            }
            if let Some(delay) = self.behavior.update_delay {
                tokio::time::sleep(delay).await;
            }
            if let Some(error) = self.update_fail_error_for(&id) {
                return Err(error);
            }
            self.append_update_request_if_configured(&request)?;
            append_update_patch_log(
                &self.behavior.observations.update_patch_log,
                &id,
                &request.patch().ops,
            )?;

            let state_guard = self.lock_state_file()?;
            let mut states = state_guard.load_states_for_operation()?;
            // Apply only the requested operations to the provider's current
            // value. In particular, a read may omit write-only leaves that
            // still exist in the backend and must survive an unrelated patch.
            let mut attributes = states.get(&key).map_or_else(
                || request.from().attributes.clone(),
                |attributes| {
                    attributes
                        .iter()
                        .filter_map(|(key, value)| value.decode().map(|value| (key.clone(), value)))
                        .collect()
                },
            );
            for op in &request.patch().ops {
                match op {
                    PatchOp::Add { key, value }
                    | PatchOp::Replace { key, value }
                    | PatchOp::Reassert { key, value } => {
                        if !Self::merge_keyed_partial_update_list(&id, &mut attributes, key, value)
                        {
                            attributes.insert(key.clone(), value.clone());
                        }
                    }
                    PatchOp::Remove { key } => {
                        attributes.remove(key);
                    }
                }
            }
            // This test hook models a service-side addition after saved-plan
            // drift verification but before the update response is read back.
            self.add_server_list_update_out_of_band_element(&id, &mut attributes);
            self.canonicalize_mock_reference_write(&id, &mut attributes)?;
            Self::add_server_list_defaults(&id, &mut attributes);

            let partial_update = self.partial_update_config_for(&id);
            let mut attrs: HashMap<String, PersistedValue> = attributes
                .iter()
                .map(|(k, v)| {
                    PersistedValue::encode_provider_value(v, None).map(|jv| (k.clone(), jv))
                })
                .collect::<Result<_, _>>()
                .map_err(|e| ProviderError::internal(format!("Failed to convert value: {}", e)))?;
            if let Some(config) = partial_update {
                for attr in &config.missing_attributes {
                    attrs.remove(attr);
                }
            }

            states.insert(key, attrs);
            state_guard
                .save(&states)
                .map_err(|e| ProviderError::internal_caused_by("Failed to save state", e))?;

            Self::strip_unobservable_nested_write_only(&mut attributes);

            if let Some(config) = partial_update {
                for attr in &config.missing_attributes {
                    attributes.remove(attr);
                }
            }
            self.append_op_log_if_configured("update", &id)?;
            let applied_identifier = self
                .behavior
                .update_result_identifier
                .clone()
                .unwrap_or(identifier);
            let state =
                AppliedState::new(id, applied_identifier, attributes).map_err(|source| {
                    ProviderError::internal_caused_by(
                        "mock update result identifier must not be empty",
                        source,
                    )
                })?;
            if let Some(config) = partial_update {
                return Ok(UpdateOutcome::partial_success(
                    state,
                    "mock partial update".to_string(),
                    config.missing_attributes.clone(),
                ));
            }

            Ok(UpdateOutcome::success(state))
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let identifier = identifier.as_str().to_string();
        Box::pin(async move {
            let key = Self::resolved_resource_key(&id)?;
            if let Some(err) = self.delete_fail_error_for(&id, &identifier) {
                return Err(err);
            }
            if let Some(err) = self.delete_timeout_error_for(&id) {
                return Err(err);
            }

            if let Some(delay) = self
                .behavior
                .delete_delay
                .as_ref()
                .and_then(|delay| delay.for_resource(&id))
            {
                Self::signal_ready(&self.behavior.observations.delete_ready)?;
                if let Some(release) = &self.behavior.delete_release {
                    // Readiness/release files order tests without a finite sleep.
                    while !release.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                } else {
                    tokio::time::sleep(delay).await;
                }
            }

            if let Some(path) = self.behavior.observations.delete_log.clone() {
                Self::append_delete_log(path, &key).map_err(|e| {
                    ProviderError::internal_caused_by("Failed to append delete log", e)
                })?;
            }

            let state_guard = self.lock_state_file()?;
            let mut states = state_guard.load_states_for_operation()?;

            states.remove(&key);
            state_guard
                .save(&states)
                .map_err(|e| ProviderError::internal_caused_by("Failed to save state", e))?;

            self.append_op_log_if_configured("delete", &id)?;
            Ok(())
        })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: PlanOp) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::NoPlaceholder;
    use std::time::Duration;

    fn delete_request_without_schema() -> DeleteRequest {
        DeleteRequest::resolve(
            &carina_core::resource::Directives::default(),
            carina_core::provider::ProviderDirectiveSchema::Unavailable,
        )
    }

    #[test]
    fn builtin_schemas_are_all_valid() {
        let schemas = builtin_schemas();
        assert!(!schemas.is_empty());
        assert!(
            schemas
                .iter()
                .any(|schema| schema.as_schema().resource_type() == "test.deferred_source")
        );
        let witness = builtin_schema_witness();
        assert_eq!(witness.resources().len(), schemas.len());
        assert!(witness.reference_forms().is_empty());
    }

    #[test]
    fn builtin_mock_identity_schemas_include_nested_budget_and_full_record_set_key() {
        use carina_core::schema::IdentityPathSegment;

        let schemas = builtin_schemas();
        let identity_paths = |resource_type: &str| {
            schemas
                .iter()
                .find(|schema| schema.as_schema().resource_type() == resource_type)
                .unwrap_or_else(|| panic!("missing built-in mock schema {resource_type}"))
                .identity_paths()
                .iter()
                .map(|path| path.segments().to_vec())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            identity_paths("test.nested_identity"),
            vec![vec![
                IdentityPathSegment::Attribute("identity".to_string()),
                IdentityPathSegment::StructField("key".to_string()),
            ]]
        );
        assert_eq!(
            identity_paths("budgets.Budget"),
            vec![vec![
                IdentityPathSegment::Attribute("budget".to_string()),
                IdentityPathSegment::StructField("budget_name".to_string()),
            ]]
        );
        assert_eq!(
            identity_paths("route53.RecordSet"),
            vec![
                vec![IdentityPathSegment::Attribute("hosted_zone_id".to_string())],
                vec![IdentityPathSegment::Attribute("name".to_string())],
                vec![IdentityPathSegment::Attribute("type".to_string())],
            ]
        );
    }

    fn string_value(value: impl Into<String>) -> Value {
        let value: String = value.into();
        Value::Concrete(ConcreteValue::String(value.clone().into()))
    }

    async fn dispatch_mock_create(
        provider: &MockProvider,
        id: ResourceId,
        resource: Resource,
    ) -> Result<CreateOutcome, CreateFailure> {
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = carina_core::schema::SchemaRegistry::new();
        let preparation = carina_core::executor::ProviderPreparationContext::new(
            &bindings,
            &module_gate,
            &[],
            &carina_core::provider::NoopNormalizer,
            &[],
            &schemas,
        );
        let request = carina_core::executor::prepare_create_request(resource, &preparation)
            .await
            .expect("mock test resource should pass create preparation");
        let (create, _cancellation) =
            carina_core::executor::dispatch_create(provider, &id, request);
        create.await
    }

    fn persist_pending_for_resume(pending: &InFlightCreate) -> InFlightCreate {
        let InFlightPersistence::Fresh(fresh) = pending.persistence() else {
            panic!("builtin create fixture starts as a fresh pending record")
        };
        let id = pending.id();
        let identity = id
            .identity()
            .cloned()
            .expect("resume fixture identity is resolved");
        let mut row = serde_json::to_value(carina_state::ResourceState::new(
            &id.resource_type,
            identity,
            &id.provider,
        ))
        .expect("fixture row serializes");
        row["current"] = serde_json::json!({
            "kind": "create_in_flight",
            "envelope": {
                "token": fresh.token(),
                "identifier": fresh.identifier(),
                "write_only": {},
                "secrets": {},
            },
            "started_at": pending.started_at(),
        });
        let mut document =
            serde_json::to_value(carina_state::StateFile::new()).expect("fixture state serializes");
        document["resources"] = serde_json::json!([row]);
        let loaded = carina_state::state::check_and_migrate(&document.to_string())
            .expect("v15 fixture passes the public state loader")
            .into_state();
        match loaded.build_state_for_resource(id) {
            carina_core::resource::Current::CreateInFlight(pending) => pending,
            carina_core::resource::Current::Absent(state) => {
                panic!("resume fixture loaded as absent: {state:?}")
            }
            carina_core::resource::Current::Present(state) => {
                panic!("resume fixture loaded as present: {state:?}")
            }
            carina_core::resource::Current::MissingIdentifier(state) => {
                panic!("resume fixture loaded without an identifier: {state:?}")
            }
            carina_core::resource::Current::CreateOutcomeUnknown(unknown) => {
                panic!("resume fixture loaded as outcome unknown: {unknown:?}")
            }
        }
    }

    #[test]
    fn threaded_mock_instances_keep_behavior_and_logs_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let a_log = tmp.path().join("a.log");
        let b_log = tmp.path().join("b.log");
        let a = ConfiguredBuiltinMock::new(
            MockBehavior {
                create_mode: CreateMode::Created,
                observations: MockObservations {
                    operation_log: Some(a_log.clone()),
                    ..MockObservations::default()
                },
                ..MockBehavior::default()
            },
            MockScope::file(tmp.path().join("a.json")).unwrap(),
        )
        .create_provider();
        let b = ConfiguredBuiltinMock::new(
            MockBehavior {
                observations: MockObservations {
                    operation_log: Some(b_log.clone()),
                    ..MockObservations::default()
                },
                ..MockBehavior::default()
            },
            MockScope::file(tmp.path().join("b.json")).unwrap(),
        )
        .create_provider();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let worker = |provider: MockProvider, barrier: std::sync::Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .unwrap();
                barrier.wait();
                let id = ResourceId::with_provider_identity("mock", "test.resource", "same", None);
                let resource = Resource::with_provider("mock", "test.resource", "same", None)
                    .with_attribute("name", string_value("same"));
                runtime
                    .block_on(dispatch_mock_create(&provider, id, resource))
                    .map(CreateOutcome::into_kind)
                    .map_err(|error| Box::new(CreateFailure::into_kind(error)))
            })
        };
        let a = worker(a, barrier.clone());
        let b = worker(b, barrier);
        let a = a.join().unwrap();
        let b = b.join().unwrap();
        assert!(
            matches!(
                a,
                Err(ref error)
                    if matches!(
                        **error,
                        CreateFailureKind::Created {
                            created: _,
                            error: _
                        }
                    )
            ),
            "instance A must retain its created verdict, got {a:?}"
        );
        assert!(
            matches!(b, Ok(CreateOutcomeKind::Success { state: _ })),
            "instance B must retain its success verdict, got {b:?}"
        );
        assert_eq!(
            fs::read_to_string(a_log).unwrap(),
            "create test.resource.same\n"
        );
        assert_eq!(
            fs::read_to_string(b_log).unwrap(),
            "create test.resource.same\n"
        );
        assert!(tmp.path().join("a.json").exists());
        assert!(tmp.path().join("b.json").exists());
    }

    #[test]
    fn file_scope_preserves_concurrent_transactions() {
        let tmp = tempfile::tempdir().unwrap();
        let shared_path = tmp.path().join("shared.json");
        let shared_scope = MockScope::file(shared_path.clone()).unwrap();
        let shared_mock = ConfiguredBuiltinMock::new(MockBehavior::default(), shared_scope);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|threads| {
            for worker in 0..2 {
                let configured = shared_mock.clone();
                let barrier = barrier.clone();
                threads.spawn(move || {
                    let provider = configured.create_provider();
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .unwrap();
                    barrier.wait();
                    for item in 0..8 {
                        let identity = carina_core::resource::ResourceIdentity::try_from(format!(
                            "worker-{worker}-{item}"
                        ))
                        .unwrap();
                        let id = ResourceId::with_provider_identity(
                            "mock",
                            "test.resource",
                            identity.clone(),
                            None,
                        );
                        let resource =
                            Resource::with_provider("mock", "test.resource", identity, None)
                                .with_attribute("name", string_value("transaction"));
                        runtime
                            .block_on(dispatch_mock_create(&provider, id, resource))
                            .unwrap();
                    }
                });
            }
        });
        let states =
            MockProvider::decode_state_file(&fs::read_to_string(shared_path).unwrap()).unwrap();
        assert_eq!(
            states.len(),
            16,
            "file transactions must not lose another provider's writes"
        );

        assert!(!tmp.path().join("shared.json.lock").exists());
    }

    #[test]
    fn normalizer_construction_does_not_initialize_storage_or_telemetry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store.json");
        let maximum = tmp.path().join("max");
        let trace = tmp.path().join("trace");
        let configured = ConfiguredBuiltinMock::new(
            MockBehavior {
                observations: MockObservations {
                    max_active: Some(maximum.clone()),
                    update_trace: Some(trace.clone()),
                    ..MockObservations::default()
                },
                ..MockBehavior::default()
            },
            MockScope::file(store.clone()).unwrap(),
        );
        fs::write(&maximum, "already observed").unwrap();
        fs::write(&trace, "existing trace").unwrap();
        let _normalizer = configured.create_normalizer();
        assert!(!store.exists());
        assert_eq!(fs::read_to_string(maximum).unwrap(), "already observed");
        assert_eq!(fs::read_to_string(trace).unwrap(), "existing trace");
        assert!(MockScope::file(std::path::PathBuf::from("relative.json")).is_err());
    }

    #[test]
    fn builtin_mock_exercises_every_create_and_resume_verdict_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::file(tmp.path().join("mock-state.json")).unwrap(),
        )
        .create_provider();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        for (mode, expected) in [
            ("untracked", "untracked"),
            ("created", "created"),
            ("pending", "in_flight"),
            ("malformed_pending", "in_flight"),
        ] {
            let provider = ConfiguredBuiltinMock::new(
                decode_controls(&[("CARINA_MOCK_CREATE_MODE".to_owned(), mode.into())].into())
                    .behavior,
                provider.scope.clone(),
            )
            .create_provider();
            let identity = format!("create-{mode}");
            let id = ResourceId::with_provider_identity(
                "mock",
                "test.resource",
                carina_core::resource::ResourceIdentity::try_from(identity.clone())
                    .expect("fixture identity is non-empty"),
                None,
            );
            let resource = Resource::with_provider(
                "mock",
                "test.resource",
                carina_core::resource::ResourceIdentity::try_from(identity)
                    .expect("fixture identity is non-empty"),
                None,
            )
            .with_attribute("name", string_value(mode));
            let result = runtime
                .block_on(dispatch_mock_create(&provider, id, resource))
                .map(CreateOutcome::into_kind)
                .map_err(CreateFailure::into_kind);
            match (expected, result) {
                ("untracked", Err(CreateFailureKind::Untracked(error))) => drop(error),
                ("created", Err(CreateFailureKind::Created { created, error })) => {
                    assert_eq!(created.identifier().as_str(), "mock-id");
                    drop(error);
                }
                ("in_flight", Err(CreateFailureKind::InFlight { pending, error })) => {
                    assert_eq!(
                        pending.token().expose_for_provider_call(),
                        if mode == "malformed_pending" {
                            "not-a-decodable-mock-token"
                        } else {
                            "builtin-mock-pending"
                        }
                    );
                    drop(error);
                }
                (expected, Ok(CreateOutcomeKind::Success { state })) => {
                    panic!("expected {expected}, got success {state:?}")
                }
                (expected, Ok(CreateOutcomeKind::PartialSuccess { state, diagnostic })) => {
                    panic!("expected {expected}, got partial {state:?}: {diagnostic:?}")
                }
                (expected, Err(CreateFailureKind::Untracked(error))) => {
                    panic!("expected {expected}, got untracked {error}")
                }
                (expected, Err(CreateFailureKind::Created { created, error })) => {
                    panic!("expected {expected}, got created {created:?}: {error}")
                }
                (expected, Err(CreateFailureKind::InFlight { pending, error })) => {
                    panic!("expected {expected}, got in flight {pending:?}: {error}")
                }
                (expected, Err(CreateFailureKind::OutcomeUnknown { unknown, error })) => {
                    panic!(
                        "expected {expected}, got outcome unknown for {}: {error}",
                        unknown.id()
                    )
                }
            }
        }

        let provider = ConfiguredBuiltinMock::new(
            MockBehavior {
                create_mode: CreateMode::Pending,
                ..MockBehavior::default()
            },
            provider.scope.clone(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "resume-modes", None);
        let resource = Resource::with_provider("mock", "test.resource", "resume-modes", None)
            .with_attribute("name", string_value("resume-modes"));
        let pending = match runtime
            .block_on(dispatch_mock_create(
                &provider,
                id.clone(),
                resource.clone(),
            ))
            .map(CreateOutcome::into_kind)
            .map_err(CreateFailure::into_kind)
        {
            Err(CreateFailureKind::InFlight { pending, error }) => {
                drop(error);
                persist_pending_for_resume(&pending)
            }
            Ok(CreateOutcomeKind::Success { state }) => panic!("expected pending, got {state:?}"),
            Ok(CreateOutcomeKind::PartialSuccess { state, diagnostic }) => {
                panic!("expected pending, got {state:?}: {diagnostic:?}")
            }
            Err(CreateFailureKind::Untracked(error)) => panic!("expected pending, got {error}"),
            Err(CreateFailureKind::Created { created, error }) => {
                panic!("expected pending, got {created:?}: {error}")
            }
            Err(CreateFailureKind::OutcomeUnknown { unknown, error }) => {
                panic!(
                    "expected pending, got outcome unknown for {}: {error}",
                    unknown.id()
                )
            }
        };

        for (mode, expected) in [
            ("resumed", "resumed"),
            ("discarded", "ended"),
            ("undetermined", "in_flight"),
            ("pending", "in_flight"),
            ("malformed_pending", "in_flight"),
            ("resend", "resent"),
        ] {
            let provider = ConfiguredBuiltinMock::new(
                decode_controls(&[("CARINA_MOCK_RESUME_MODE".to_owned(), mode.into())].into())
                    .behavior,
                provider.scope.clone(),
            )
            .create_provider();
            let request = runtime.block_on(async {
                let bindings = carina_core::binding_index::ResolvedBindings::default();
                let module_gate =
                    carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
                let schemas = carina_core::schema::SchemaRegistry::new();
                let preparation = carina_core::executor::ProviderPreparationContext::new(
                    &bindings,
                    &module_gate,
                    &[],
                    &carina_core::provider::NoopNormalizer,
                    &[],
                    &schemas,
                );
                let create_request =
                    carina_core::executor::prepare_create_request(resource.clone(), &preparation)
                        .await
                        .expect("resume fixture passes create preparation");
                ResumeCreateRequest::for_test(pending.clone(), create_request)
                    .expect("resume fixture route matches")
            });
            let result = runtime
                .block_on(provider.resume_create(request))
                .map(ResumeOutcome::into_kind)
                .map_err(ResumeFailure::into_kind);
            match (expected, result) {
                ("resumed", Ok(ResumeOutcomeKind::Resumed(created))) => {
                    assert_eq!(created.identifier().as_str(), "mock-id");
                }
                ("resent", Ok(ResumeOutcomeKind::Resent(outcome))) => match outcome.into_kind() {
                    CreateOutcomeKind::Success { state } => {
                        assert_eq!(state.identifier().as_str(), "mock-id");
                    }
                    CreateOutcomeKind::PartialSuccess { state, diagnostic } => {
                        panic!("unexpected partial resend {state:?}: {diagnostic:?}")
                    }
                },
                ("ended", Err(ResumeFailureKind::Ended(error))) => drop(error),
                ("in_flight", Err(ResumeFailureKind::InFlight { pending, error })) => {
                    assert_eq!(pending.id(), &id);
                    drop(error);
                }
                (expected, Ok(ResumeOutcomeKind::Resumed(created))) => {
                    panic!("expected {expected}, got resumed {created:?}")
                }
                (expected, Ok(ResumeOutcomeKind::PartialSuccess(outcome))) => {
                    panic!("expected {expected}, got partial {outcome:?}")
                }
                (expected, Ok(ResumeOutcomeKind::Resent(outcome))) => {
                    panic!("expected {expected}, got resent {outcome:?}")
                }
                (expected, Err(ResumeFailureKind::Ended(error))) => {
                    panic!("expected {expected}, got ended {error}")
                }
                (expected, Err(ResumeFailureKind::Created { created, error })) => {
                    panic!("expected {expected}, got created {created:?}: {error}")
                }
                (expected, Err(ResumeFailureKind::InFlight { pending, error })) => {
                    panic!("expected {expected}, got in flight {pending:?}: {error}")
                }
            }
        }
    }

    fn valid_legacy_marker_shaped_string(plaintext: &str) -> String {
        let secret = Value::Deferred(carina_core::resource::DeferredValue::Secret(Box::new(
            string_value(plaintext),
        )));
        let encoded = PersistedValue::encode_provider_value(&secret, None)
            .expect("test secret must be persistable");
        let encoded = serde_json::to_value(encoded).expect("serialize persisted test secret");
        format!(
            "_secret:{}",
            encoded
                .as_object()
                .and_then(|tag| tag.get(carina_core::value::PERSISTED_SECRET_KEY))
                .and_then(serde_json::Value::as_str)
                .expect("structural test secret carries a digest")
        )
    }

    fn mock_provider_config() -> carina_core::parser::ExpandedProviderConfig {
        carina_core::parser::parse_file(
            "provider mock {}\n",
            std::path::Path::new("mock_provider_config.crn"),
            &carina_core::parser::ProviderContext::default(),
        )
        .unwrap()
        .into_expanded(&carina_core::schema::SchemaRegistry::new())
        .expect("mock provider fixture must not retain parser placeholders")
        .providers
        .into_iter()
        .next()
        .unwrap()
    }

    #[test]
    fn builtin_provider_selection_is_shared_by_implicit_and_explicit_mock_paths() {
        assert!(uses_builtin_provider::<
            NoPlaceholder,
            carina_core::resource::AbsoluteBindingAddress,
        >(&[], |_| {
            panic!("the implicit fallback does not depend on loaded factories")
        }));

        let source_less = mock_provider_config();
        assert!(uses_builtin_provider([&source_less], |_| false));
        assert!(!uses_builtin_provider([&source_less], |_| true));

        let mut sourced = source_less.clone();
        sourced.source = Some("file:///missing/mock.wasm".parse().unwrap());
        assert!(!uses_builtin_provider([&sourced], |_| false));

        let mut named = source_less.clone();
        named.binding = Some("secondary".to_string());
        assert!(
            !uses_builtin_provider([&sourced, &named], |_| false),
            "a named instance's absent source must not override its sourced kind default",
        );
    }

    // Pin the byte-level shape so MockProvider's state file matches
    // the trailing-newline convention used by carina.state.json
    // (#2721) and carina-backend.lock (#2583).
    #[test]
    fn state_file_ends_with_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::file(state_file.clone()).unwrap(),
        )
        .create_provider();
        let mut states = HashMap::new();
        let mut entry = HashMap::new();
        entry.insert(
            "k".to_string(),
            serde_json::from_value(serde_json::json!("v")).unwrap(),
        );
        states.insert("aws.s3.Bucket.b".to_string(), entry);
        provider.lock_state_file().unwrap().save(&states).unwrap();
        let bytes = fs::read(&state_file).unwrap();
        assert_eq!(
            bytes.last().copied(),
            Some(b'\n'),
            "MockProvider state file must end with a trailing newline; got {:?}",
            bytes.last().map(|b| *b as char),
        );
    }

    #[test]
    fn unversioned_state_migrates_dollar_keys_and_legacy_enums() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        fs::write(
            &state_file,
            carina_core::utils::pretty_with_newline(&serde_json::json!({
                "test.resource.legacy": {
                    "payload": {"$x": "kept"},
                    "mode": {
                        "Enum": {
                            "identity": {
                                "provider": "mock",
                                "segments": ["test", "resource"],
                                "kind": "Mode"
                            },
                            "api_value": "active"
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::file(state_file).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "legacy", None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let state = runtime
            .block_on(provider.read(
                &id,
                &AppliedIdentifier::new("mock-id").unwrap(),
                ReadRequest,
            ))
            .expect("legacy mock state should load after migration")
            .into_state();

        let Some(Value::Concrete(ConcreteValue::Map(payload))) = state.attributes.get("payload")
        else {
            panic!("legacy payload should remain a user map")
        };
        assert_eq!(payload.get("$x"), Some(&string_value("kept")));
        let Some(Value::Concrete(ConcreteValue::CanonicalEnum(mode))) =
            state.attributes.get("mode")
        else {
            panic!("legacy enum should migrate to a canonical enum")
        };
        assert_eq!(mode.api_value(), "active");
    }

    #[test]
    fn v1_state_migrates_self_contained_markers_to_structural_tags() {
        let marker = valid_legacy_marker_shaped_string("mock-v1-secret");
        let digest = marker
            .strip_prefix("_secret:")
            .expect("test marker has the old boundary prefix");
        let content = serde_json::json!({
            "version": 1,
            "states": {
                "test.resource.legacy": {
                    "top": marker,
                    "nested": {
                        "items": [marker],
                        "explicit": "_secret:argon2-legacy:73616c74:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    },
                    "raw": "_secret:argon2:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                }
            }
        });

        let states = MockProvider::decode_state_file(&content.to_string())
            .expect("mock v1 state should migrate");
        let attrs = &states["test.resource.legacy"];
        let top = serde_json::to_value(&attrs["top"]).expect("serialize migrated top value");
        let nested =
            serde_json::to_value(&attrs["nested"]).expect("serialize migrated nested value");
        assert_eq!(top, serde_json::json!({"$secret": digest}));
        assert_eq!(nested["items"][0], serde_json::json!({"$secret": digest}));
        assert!(
            nested["explicit"]["$secret"]
                .as_str()
                .is_some_and(|value| value.starts_with("argon2-legacy:"))
        );
        assert_eq!(
            attrs["raw"].decode(),
            Some(string_value(
                "_secret:argon2:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )),
            "mock v1 never assigned row-aware semantics to raw legacy strings",
        );
    }

    #[test]
    fn fresh_v2_state_round_trip_keeps_marker_shaped_plaintext_plain() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::file(state_file.clone()).unwrap(),
        )
        .create_provider();
        let marker = valid_legacy_marker_shaped_string("mock-v2-collision");
        let plain = string_value(marker.clone());
        let id = ResourceId::with_provider_identity("mock", "test.resource", "collision", None);
        let resource = Resource::with_provider("mock", "test.resource", "collision", None)
            .with_attribute("name", string_value("collision"))
            .with_attribute("comment", plain.clone())
            .with_attribute(
                "tags",
                Value::Concrete(ConcreteValue::Map(
                    [("nested".to_string(), plain.clone())]
                        .into_iter()
                        .collect(),
                )),
            );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime
            .block_on(dispatch_mock_create(&provider, id.clone(), resource))
            .expect("create should save mock v2 state");
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
        assert_eq!(raw["version"], 2);
        assert_eq!(raw["states"]["test.resource.collision"]["comment"], marker);
        assert_eq!(
            raw["states"]["test.resource.collision"]["tags"]["nested"],
            marker
        );

        let state = runtime
            .block_on(provider.read(
                &id,
                &AppliedIdentifier::new("mock-id").unwrap(),
                ReadRequest,
            ))
            .expect("fresh mock v2 state should reload")
            .into_state();
        assert_eq!(state.attributes.get("comment"), Some(&plain));
        assert!(matches!(
            state.attributes.get("tags"),
            Some(Value::Concrete(ConcreteValue::Map(values)))
                if values.get("nested") == Some(&plain)
        ));
    }

    #[test]
    fn read_omits_write_only_password() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::file(state_file).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "hidden", None);
        let resource = Resource::with_provider("mock", "test.resource", "hidden", None)
            .with_attribute("name", string_value("hidden"))
            .with_attribute("password", string_value("not-returned"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime
            .block_on(dispatch_mock_create(&provider, id.clone(), resource))
            .expect("create should persist the write-only request value");
        let state = runtime
            .block_on(provider.read(
                &id,
                &AppliedIdentifier::new("hidden-id").unwrap(),
                ReadRequest,
            ))
            .expect("read should succeed")
            .into_state();

        assert!(
            !state.attributes.contains_key(WRITE_ONLY_PASSWORD_ATTRIBUTE),
            "provider reads must model a non-echoing write-only attribute"
        );
    }

    #[test]
    fn corrupt_or_invalid_state_file_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::file(state_file.clone()).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "broken", None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        for (content, terminal_detail) in [
            ("{".to_string(), "EOF while parsing an object"),
            (
                serde_json::json!({
                    "version": 1,
                    "states": {
                        "test.resource.broken": {
                            "payload": {"$unknown": true}
                        }
                    }
                })
                .to_string(),
                "mock state resource \"test.resource.broken\" attribute \"payload\" is not valid persisted JSON",
            ),
        ] {
            fs::write(&state_file, content).unwrap();
            let error = runtime
                .block_on(provider.read(
                    &id,
                    &AppliedIdentifier::new("mock-id").unwrap(),
                    ReadRequest,
                ))
                .expect_err("corrupt mock state must not look like an empty store");
            let rendered = error.to_string();
            assert!(
                rendered.contains("Failed to load mock state")
                    && rendered.contains(terminal_detail),
                "unexpected error: {error}"
            );
            assert_eq!(rendered.matches(terminal_detail).count(), 1, "{rendered}");
        }
    }

    #[test]
    fn versioned_state_file_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::file(state_file.clone()).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "versioned", None);
        let payload = Value::Concrete(ConcreteValue::Map(
            [("$x".to_string(), string_value("kept"))]
                .into_iter()
                .collect(),
        ));
        let resource = Resource::with_provider("mock", "test.resource", "versioned", None)
            .with_attribute("payload", payload.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime
            .block_on(dispatch_mock_create(&provider, id.clone(), resource))
            .expect("create should save versioned mock state");
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
        assert_eq!(raw.get("version"), Some(&serde_json::json!(2)));
        assert!(raw.get("states").is_some());

        let state = runtime
            .block_on(provider.read(
                &id,
                &AppliedIdentifier::new("mock-id").unwrap(),
                ReadRequest,
            ))
            .expect("new mock state should reload")
            .into_state();
        assert_eq!(state.attributes.get("payload"), Some(&payload));
    }

    #[test]
    fn canonical_enum_survives_create_then_read() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::file(tmp.path().join("mock-state.json")).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "enum", None);
        let canonical: carina_core::value::PersistedValue =
            serde_json::from_value(serde_json::json!({
                "$enum": {
                    "identity": {
                        "provider": "mock",
                        "segments": ["test", "resource"],
                        "kind": "Mode"
                    },
                    "api_value": "active"
                }
            }))
            .expect("canonical enum fixture is valid");
        let canonical = canonical
            .decode()
            .expect("canonical enum fixture is non-null");
        let resource = Resource::with_provider("mock", "test.resource", "enum", None)
            .with_attribute("mode", canonical.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        let read = runtime.block_on(async {
            dispatch_mock_create(&provider, id.clone(), resource)
                .await
                .expect("create succeeds");
            provider
                .read(
                    &id,
                    &AppliedIdentifier::new("mock-id").unwrap(),
                    ReadRequest,
                )
                .await
                .expect("read succeeds")
                .into_state()
        });

        assert_eq!(read.attributes.get("mode"), Some(&canonical));
    }

    #[test]
    fn required_permissions_returns_empty_vec() {
        let provider = ConfiguredBuiltinMock::new(MockBehavior::default(), MockScope::memory())
            .create_provider();
        let id = ResourceId::with_provider_identity("mock", "foo", "example", None);
        assert_eq!(
            provider
                .required_permissions(&id, carina_core::effect::PlanOp::Create)
                .expect("mock permissions should succeed"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn append_delete_log_records_resource_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("delete.log");
        let first = ResourceId::with_provider_identity("mock", "test.resource", "first", None);
        let second = ResourceId::with_provider_identity("mock", "test.resource", "second", None);

        MockProvider::append_delete_log(
            log_path.clone(),
            &MockProvider::resolved_resource_key(&first).unwrap(),
        )
        .unwrap();
        MockProvider::append_delete_log(
            log_path.clone(),
            &MockProvider::resolved_resource_key(&second).unwrap(),
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(log_path).unwrap(),
            "test.resource.first\ntest.resource.second\n"
        );
    }

    #[test]
    fn op_log_records_successful_create_update_delete_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let log_path = tmp.path().join("op.log");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior {
                observations: MockObservations {
                    operation_log: Some(log_path.clone()),
                    ..MockObservations::default()
                },
                ..MockBehavior::default()
            },
            MockScope::file(state_file).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "web-acl-new", None);
        let resource = Resource::with_provider("mock", "test.resource", "web-acl-new", None)
            .with_attribute("name", string_value("web-acl-new"))
            .with_attribute("comment", string_value("v1"));
        let update_resource = resource
            .clone()
            .with_attribute("comment", string_value("v2"));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            dispatch_mock_create(&provider, id.clone(), resource)
                .await
                .expect("create should succeed");

            let mut from_attrs = HashMap::new();
            from_attrs.insert("name".to_string(), string_value("web-acl-new"));
            from_attrs.insert("comment".to_string(), string_value("v1"));
            let from = State::existing(id.clone(), from_attrs).with_identifier("mock-id");
            let bindings = carina_core::binding_index::ResolvedBindings::default();
            let module_gate =
                carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
            let mut schemas = carina_core::schema::SchemaRegistry::new();
            schemas.insert(
                "mock",
                carina_core::schema::ResourceSchema::new("test.resource")
                    .validate()
                    .expect("test schema must be valid"),
            );
            let preparation = carina_core::executor::ProviderPreparationContext::new(
                &bindings,
                &module_gate,
                &[],
                &carina_core::provider::NoopNormalizer,
                &[],
                &schemas,
            );
            let request = carina_core::executor::prepare_update_request(
                update_resource,
                from,
                &["comment".to_string()],
                &preparation,
            )
            .await
            .expect("update request should pass checked preparation");
            let identifier =
                AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty");
            provider
                .update(&id, &identifier, request)
                .await
                .expect("update should succeed");

            provider
                .delete(&id, &identifier, delete_request_without_schema())
                .await
                .expect("delete should succeed");
        });

        assert_eq!(
            fs::read_to_string(log_path).unwrap(),
            "create test.resource.web-acl-new\n\
             update test.resource.web-acl-new\n\
             delete test.resource.web-acl-new\n"
        );
    }

    #[test]
    fn op_log_records_provider_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let log_path = tmp.path().join("op.log");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior {
                observations: MockObservations {
                    operation_log: Some(log_path.clone()),
                    ..MockObservations::default()
                },
                ..MockBehavior::default()
            },
            MockScope::file(state_file).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "read-me", None);
        let resource = Resource::with_provider("mock", "test.resource", "read-me", None)
            .with_attribute("name", string_value("read-me"));
        let identifier = AppliedIdentifier::new("mock-id").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            dispatch_mock_create(&provider, id.clone(), resource)
                .await
                .expect("create should succeed");
            fs::write(&log_path, "").expect("clear create log");
            provider
                .read(&id, &identifier, ReadRequest)
                .await
                .expect("read should succeed");
        });

        assert_eq!(
            fs::read_to_string(log_path).unwrap(),
            "read test.resource.read-me\n"
        );
    }

    #[test]
    fn op_log_records_normalizer_calls() {
        use carina_core::provider::ProviderNormalizer;

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("op.log");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior {
                observations: MockObservations {
                    operation_log: Some(log_path.clone()),
                    ..MockObservations::default()
                },
                ..MockBehavior::default()
            },
            MockScope::file(tmp.path().join("mock-state.json")).unwrap(),
        )
        .create_provider();
        let mut resources =
            vec![Resource::with_provider("mock", "test.resource", "normalize-me", None).into()];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime
            .block_on(
                MockNormalizer {
                    observations: provider.behavior.observations.clone(),
                }
                .normalize_desired(&mut resources),
            )
            .expect("normalizer should succeed");

        assert_eq!(
            fs::read_to_string(log_path).unwrap(),
            "normalize_desired test.resource.normalize-me\n"
        );
    }

    #[test]
    fn create_fail_for_returns_error_without_op_log_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let log_path = tmp.path().join("op.log");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior {
                observations: MockObservations {
                    operation_log: Some(log_path.clone()),
                    ..MockObservations::default()
                },
                create_fail_for: Some("test.resource.blocked".to_owned()),
                ..MockBehavior::default()
            },
            MockScope::file(state_file).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "blocked", None);
        let resource = Resource::with_provider("mock", "test.resource", "blocked", None)
            .with_attribute("name", string_value("blocked"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        let err = runtime
            .block_on(dispatch_mock_create(&provider, id, resource))
            .expect_err("create should fail when CARINA_MOCK_CREATE_FAIL_FOR matches");
        assert_eq!(err.to_string(), "create failed before it could be tracked");
        let message = std::error::Error::source(&err)
            .expect("classified create failure retains the provider error")
            .to_string();
        assert!(
            message.contains("CARINA_MOCK_CREATE_FAIL_FOR")
                && message.contains("test.resource.blocked"),
            "create failure source should mention env var and resource key, got: {message}"
        );

        let log = fs::read_to_string(log_path).unwrap_or_default();
        assert!(
            !log.lines()
                .any(|line| line == "create test.resource.blocked"),
            "failed create must not emit CARINA_MOCK_OP_LOG entry, got: {log:?}"
        );
    }

    #[test]
    fn create_release_handshake_keeps_create_pending_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let ready = tmp.path().join("create-ready");
        let release = tmp.path().join("create-release");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior {
                create_delay: Some(SelectedDelay {
                    address: "test.resource.slow".to_owned(),
                    duration: Duration::from_millis(25),
                }),
                create_release: Some(release.clone()),
                observations: MockObservations {
                    create_ready: Some(ready.clone()),
                    ..MockObservations::default()
                },
                ..MockBehavior::default()
            },
            MockScope::file(tmp.path().join("mock-state.json")).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "slow", None);
        let resource = Resource::with_provider("mock", "test.resource", "slow", None)
            .with_attribute("name", string_value("slow"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();

        let (initially_pending, ready_seen, still_pending, released, outcome) =
            runtime.block_on(async {
                let create = dispatch_mock_create(&provider, id, resource);
                tokio::pin!(create);
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                let first = create.as_mut().poll(&mut context);
                let initially_pending = first.is_pending();
                let ready_seen = ready.exists();
                tokio::time::advance(Duration::from_secs(60)).await;
                let after_wait = if initially_pending {
                    create.as_mut().poll(&mut context)
                } else {
                    first
                };
                let still_pending = after_wait.is_pending();
                let released = fs::write(&release, b"");
                let outcome = match after_wait {
                    std::task::Poll::Ready(result) => Some(result),
                    std::task::Poll::Pending if released.is_ok() => Some(create.await),
                    std::task::Poll::Pending => None,
                };
                (
                    initially_pending,
                    ready_seen,
                    still_pending,
                    released,
                    outcome,
                )
            });

        assert!(initially_pending);
        assert!(ready_seen, "the mock must report when its create is parked");
        assert!(still_pending, "elapsed time must not release the create");
        released.unwrap();
        outcome.unwrap().unwrap();
    }

    #[test]
    fn create_delay_delays_matching_resource_create() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior {
                create_delay: Some(SelectedDelay {
                    address: "test.resource.slow".to_owned(),
                    duration: Duration::from_millis(25),
                }),
                ..MockBehavior::default()
            },
            MockScope::file(tmp.path().join("mock-state.json")).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "slow", None);
        let resource = Resource::with_provider("mock", "test.resource", "slow", None)
            .with_attribute("name", string_value("slow"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();

        let (outcome, elapsed) = runtime.block_on(async {
            let started = tokio::time::Instant::now();
            let outcome = dispatch_mock_create(&provider, id, resource).await;
            (outcome, started.elapsed())
        });

        outcome.unwrap();
        assert_eq!(elapsed, Duration::from_millis(25));
    }

    #[test]
    fn delete_release_handshake_keeps_delete_pending_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let ready = tmp.path().join("delete-ready");
        let release = tmp.path().join("delete-release");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior {
                delete_delay: Some(SelectedDelay {
                    address: "test.resource.slow".to_owned(),
                    duration: Duration::from_millis(25),
                }),
                delete_release: Some(release.clone()),
                observations: MockObservations {
                    delete_ready: Some(ready.clone()),
                    ..MockObservations::default()
                },
                ..MockBehavior::default()
            },
            MockScope::file(tmp.path().join("mock-state.json")).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "slow", None);
        let identifier = AppliedIdentifier::new("mock-id").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();

        let (initially_pending, ready_seen, still_pending, released, outcome) =
            runtime.block_on(async {
                let delete = provider.delete(&id, &identifier, delete_request_without_schema());
                tokio::pin!(delete);
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                let first = delete.as_mut().poll(&mut context);
                let initially_pending = first.is_pending();
                let ready_seen = ready.exists();
                tokio::time::advance(Duration::from_secs(60)).await;
                let after_wait = if initially_pending {
                    delete.as_mut().poll(&mut context)
                } else {
                    first
                };
                let still_pending = after_wait.is_pending();
                let released = fs::write(&release, b"");
                let outcome = match after_wait {
                    std::task::Poll::Ready(result) => Some(result),
                    std::task::Poll::Pending if released.is_ok() => Some(delete.await),
                    std::task::Poll::Pending => None,
                };
                (
                    initially_pending,
                    ready_seen,
                    still_pending,
                    released,
                    outcome,
                )
            });

        assert!(initially_pending);
        assert!(ready_seen, "the mock must report when its delete is parked");
        assert!(still_pending, "elapsed time must not release the delete");
        released.unwrap();
        outcome.unwrap().unwrap();
    }

    #[test]
    fn delete_delay_delays_matching_resource_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = ConfiguredBuiltinMock::new(
            MockBehavior {
                delete_delay: Some(SelectedDelay {
                    address: "test.resource.slow".to_owned(),
                    duration: Duration::from_millis(25),
                }),
                ..MockBehavior::default()
            },
            MockScope::file(state_file).unwrap(),
        )
        .create_provider();
        let id = ResourceId::with_provider_identity("mock", "test.resource", "slow", None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();

        let identifier =
            AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty");
        let (outcome, elapsed) = runtime.block_on(async {
            let started = tokio::time::Instant::now();
            let outcome = provider
                .delete(&id, &identifier, delete_request_without_schema())
                .await;
            (outcome, tokio::time::Instant::now() - started)
        });

        outcome.unwrap();
        assert_eq!(elapsed, Duration::from_millis(25));
    }
}
