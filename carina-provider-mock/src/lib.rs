use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

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
    ConcreteValue, PlaceholderKind, Resource, ResourceId, State, Value, legacy_secret_migration,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use carina_core::value::{PersistedValue, migrate_legacy_persisted_json};
use serde::{Deserialize, Serialize};

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
pub fn uses_builtin_provider<'a, P: PlaceholderKind + 'a>(
    provider_configs: impl IntoIterator<Item = &'a ProviderConfig<P>>,
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
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build()),
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
    ];

    schemas
        .into_iter()
        .map(|schema| {
            schema
                .validate()
                .expect("built-in mock resource schema must be valid")
        })
        .collect()
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
    state_file: PathBuf,
    partial_create: Option<PartialConfig>,
    partial_update: Option<PartialConfig>,
}

#[derive(Clone, Debug)]
struct PartialConfig {
    resource_id_pattern: String,
    missing_attributes: Vec<String>,
}

static ACTIVE_UPDATES: AtomicUsize = AtomicUsize::new(0);
static MAX_ACTIVE_UPDATES: AtomicUsize = AtomicUsize::new(0);
static MOCK_STATE_LOCK: Mutex<()> = Mutex::new(());
static MOCK_STATE_WRITE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct ActiveUpdateGuard {
    resource: String,
}

struct ReadOutcomeGuard {
    path: Option<PathBuf>,
    completed: bool,
}

impl ReadOutcomeGuard {
    fn from_env() -> Self {
        Self {
            path: env::var_os("CARINA_MOCK_READ_OUTCOME_PATH").map(PathBuf::from),
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

impl ActiveUpdateGuard {
    fn enter(resource: String) -> Self {
        let active = ACTIVE_UPDATES.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_ACTIVE_UPDATES.fetch_max(active, Ordering::SeqCst);
        write_max_active();
        append_update_trace("start", &resource, Some(active));
        Self { resource }
    }
}

impl Drop for ActiveUpdateGuard {
    fn drop(&mut self) {
        append_update_trace("finish", &self.resource, None);
        ACTIVE_UPDATES.fetch_sub(1, Ordering::SeqCst);
    }
}

impl MockProvider {
    pub fn new() -> Self {
        let state_file = env::var_os("CARINA_MOCK_STATE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".carina/state.json"));
        if env::var_os("CARINA_MOCK_MAX_ACTIVE_PATH").is_some() {
            ACTIVE_UPDATES.store(0, Ordering::SeqCst);
            MAX_ACTIVE_UPDATES.store(0, Ordering::SeqCst);
            write_max_active();
        }
        if env::var_os("CARINA_MOCK_UPDATE_TRACE_PATH").is_some() {
            reset_update_trace();
        }
        Self {
            state_file,
            partial_create: partial_create_config_from_env(),
            partial_update: partial_update_config_from_env(),
        }
    }

    pub fn with_partial_create_for(
        mut self,
        resource_id_pattern: impl Into<String>,
        missing_attributes: Vec<String>,
    ) -> Self {
        self.partial_create = Some(PartialConfig {
            resource_id_pattern: resource_id_pattern.into(),
            missing_attributes,
        });
        self
    }

    pub fn with_partial_update_for(
        mut self,
        resource_id_pattern: impl Into<String>,
        missing_attributes: Vec<String>,
    ) -> Self {
        self.partial_update = Some(PartialConfig {
            resource_id_pattern: resource_id_pattern.into(),
            missing_attributes,
        });
        self
    }

    fn load_states(&self) -> io::Result<MockStates> {
        let content = match fs::read_to_string(&self.state_file) {
            Ok(content) => content,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(error) => return Err(error),
        };
        Self::decode_state_file(&content)
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

    fn load_states_for_operation(&self) -> ProviderResult<MockStates> {
        self.load_states()
            .map_err(|error| ProviderError::internal_caused_by("Failed to load mock state", error))
    }

    fn lock_state_file() -> ProviderResult<MutexGuard<'static, ()>> {
        MOCK_STATE_LOCK
            .lock()
            .map_err(|_| ProviderError::internal("Mock state lock is poisoned"))
    }

    fn save_states(&self, states: &MockStates) -> Result<(), std::io::Error> {
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = carina_core::utils::pretty_with_newline(&MockStateFileRef {
            version: MOCK_STATE_VERSION,
            states,
        })?;
        let sequence = MOCK_STATE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = self
            .state_file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("mock-state.json");
        let temporary = self.state_file.with_file_name(format!(
            ".{file_name}.tmp-{}-{sequence}",
            std::process::id()
        ));
        fs::write(&temporary, content)?;
        if let Err(error) = fs::rename(&temporary, &self.state_file) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(())
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

    fn partial_create_config_for(&self, id: &ResourceId) -> Option<&PartialConfig> {
        let config = self.partial_create.as_ref()?;
        Self::partial_config_matches(config, id).then_some(config)
    }

    fn partial_update_config_for(&self, id: &ResourceId) -> Option<&PartialConfig> {
        let config = self.partial_update.as_ref()?;
        Self::partial_config_matches(config, id).then_some(config)
    }

    fn partial_config_matches(config: &PartialConfig, id: &ResourceId) -> bool {
        let Some(identity) = id.identity_str() else {
            return false;
        };
        let full = format!("{}.{}.{}", id.provider, id.resource_type, identity);
        let Some(short) = Self::resource_key(id) else {
            return false;
        };
        config.resource_id_pattern == "*"
            || config.resource_id_pattern == full
            || config.resource_id_pattern == short
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

    fn append_op_log_if_configured(op: &str, id: &ResourceId) -> ProviderResult<()> {
        if let Some(path) = env::var_os("CARINA_MOCK_OP_LOG").map(PathBuf::from) {
            let key = Self::resolved_resource_key(id)?;
            Self::append_op_log(path, op, &key).map_err(|e| {
                ProviderError::internal_caused_by("Failed to append operation log", e)
            })?;
        }
        Ok(())
    }

    fn append_update_request_if_configured(request: &UpdateRequest) -> ProviderResult<()> {
        let Some(path) = env::var_os("CARINA_MOCK_UPDATE_REQUEST_LOG").map(PathBuf::from) else {
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

    fn append_normalizer_log_if_configured(op: &str, id: &ResourceId) -> ProviderResult<()> {
        if let Some(path) = env::var_os("CARINA_MOCK_OP_LOG").map(PathBuf::from) {
            let key = Self::resource_key(id).unwrap_or_else(|| id.to_string());
            Self::append_op_log(path, op, &key).map_err(|error| {
                ProviderError::internal_caused_by("Failed to append operation log", error)
            })?;
        }
        Ok(())
    }

    fn create_fail_error_for(id: &ResourceId) -> Option<ProviderError> {
        let target = env::var("CARINA_MOCK_CREATE_FAIL_FOR").ok()?;
        let key = Self::resource_key(id)?;
        (target == key).then(|| {
            ProviderError::internal(format!(
                "CARINA_MOCK_CREATE_FAIL_FOR requested create failure for {key}"
            ))
        })
    }

    fn update_fail_error_for(id: &ResourceId) -> Option<ProviderError> {
        let target = env::var("CARINA_MOCK_UPDATE_FAIL_FOR").ok()?;
        let identity = id.identity_str()?;
        let full = format!("{}.{}.{}", id.provider, id.resource_type, identity);
        let key = Self::resource_key(id)?;
        (target == "*" || target == full || target == key).then(|| {
            ProviderError::internal(format!(
                "CARINA_MOCK_UPDATE_FAIL_FOR requested update failure for {key}"
            ))
        })
    }

    fn delete_fail_error_for(id: &ResourceId, identifier: &str) -> Option<ProviderError> {
        let target = env::var("CARINA_MOCK_DELETE_FAIL_FOR").ok()?;
        let identity = id.identity_str()?;
        let full = format!("{}.{}.{}", id.provider, id.resource_type, identity);
        let key = Self::resource_key(id)?;
        if target != "*" && target != full && target != key {
            return None;
        }
        if let Ok(target_identifier) = env::var("CARINA_MOCK_DELETE_FAIL_IDENTIFIER")
            && target_identifier != identifier
        {
            return None;
        }
        Some(ProviderError::internal(format!(
            "CARINA_MOCK_DELETE_FAIL_FOR requested delete failure for {key} ({identifier})"
        )))
    }

    fn delete_timeout_error_for(id: &ResourceId) -> Option<ProviderError> {
        let target = env::var("CARINA_MOCK_DELETE_TIMEOUT_FOR").ok()?;
        let identity = id.identity_str()?;
        let full = format!("{}.{}.{}", id.provider, id.resource_type, identity);
        let key = Self::resource_key(id)?;
        (target == "*" || target == full || target == key).then(|| {
            ProviderError::timeout(format!(
                "Operation timed out: CARINA_MOCK_DELETE_TIMEOUT_FOR matched {key}"
            ))
        })
    }

    fn test_resource_identifier(id: &ResourceId, name: Option<&str>) -> Option<String> {
        if id.resource_type != "test.resource" {
            return None;
        }
        name.or_else(|| id.identity_str())
            .map(|identity| format!("{identity}-id"))
    }

    fn populate_test_resource_identifier_json(
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
        if let Some(identifier) = Self::test_resource_identifier(id, name.as_deref()) {
            attrs.entry("identifier".to_string()).or_insert_with(|| {
                PersistedValue::encode_provider_value(
                    &Value::Concrete(ConcreteValue::String(identifier.into())),
                    None,
                )
                .expect("mock identifier is persistable")
            });
        }
    }

    fn populate_test_resource_identifier_value(
        id: &ResourceId,
        attrs: &mut HashMap<String, Value>,
    ) {
        let name = attrs
            .get("name")
            .and_then(|value| match value {
                Value::Concrete(ConcreteValue::String(name)) => Some(name.expose_plaintext()),
                _ => None,
            })
            .map(str::to_string);
        if let Some(identifier) = Self::test_resource_identifier(id, name.as_deref()) {
            attrs
                .entry("identifier".to_string())
                .or_insert_with(|| Value::Concrete(ConcreteValue::String(identifier.into())));
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
        id: &ResourceId,
        attributes: &mut HashMap<String, Value>,
    ) {
        if id.resource_type != SERVER_ADDED_LIST_RESOURCE_TYPE {
            return;
        }
        let Ok(key) = env::var("CARINA_MOCK_UPDATE_SERVER_LIST_OUT_OF_BAND_KEY") else {
            return;
        };
        let value = env::var("CARINA_MOCK_UPDATE_SERVER_LIST_OUT_OF_BAND_VALUE")
            .unwrap_or_else(|_| "enabled".to_string());
        let Some(Value::Concrete(ConcreteValue::List(items))) =
            attributes.get_mut(SERVER_ADDED_LIST_ATTRIBUTE)
        else {
            return;
        };
        items.push(Value::Concrete(ConcreteValue::Map(
            [
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String(key.into())),
                ),
                (
                    "value".to_string(),
                    Value::Concrete(ConcreteValue::String(value.into())),
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

    fn delete_delay_for(id: &ResourceId) -> Option<Duration> {
        let target = env::var("CARINA_MOCK_DELETE_DELAY_MS_FOR").ok()?;
        if Some(target) != Self::resource_key(id) {
            return None;
        }
        env::var("CARINA_MOCK_DELETE_DELAY_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|millis| *millis > 0)
            .map(Duration::from_millis)
    }

    fn read_delay_for(id: &ResourceId) -> Option<Duration> {
        let target = env::var("CARINA_MOCK_READ_DELAY_MS_FOR").ok()?;
        if Some(target) != Self::resource_key(id) {
            return None;
        }
        env::var("CARINA_MOCK_READ_DELAY_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|millis| *millis > 0)
            .map(Duration::from_millis)
    }

    fn create_delay_for(id: &ResourceId) -> Option<Duration> {
        let target = env::var("CARINA_MOCK_CREATE_DELAY_MS_FOR").ok()?;
        if Some(target) != Self::resource_key(id) {
            return None;
        }
        env::var("CARINA_MOCK_CREATE_DELAY_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|millis| *millis > 0)
            .map(Duration::from_millis)
    }

    fn signal_ready(path_env: &str) -> ProviderResult<()> {
        let Some(path) = env::var_os(path_env).map(PathBuf::from) else {
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

    async fn create_resource<S>(
        &self,
        id: ResourceId,
        resource: Resource<S>,
        attempt: CreateAttempt,
    ) -> Result<CreateOutcome, CreateFailure> {
        let mode = env::var("CARINA_MOCK_CREATE_MODE").unwrap_or_else(|_| "success".to_string());
        if let Some(err) = Self::create_fail_error_for(&id) {
            return Err(attempt.untracked(err));
        }
        if mode == "untracked" {
            return Err(attempt.untracked(ProviderError::api_error(
                "builtin mock returned an untracked create verdict",
            )));
        }

        let sent = match env::var("CARINA_MOCK_CREATE_SENT") {
            Ok(raw) if raw != "all" => Sent::Only(
                raw.split(',')
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .map(ToString::to_string)
                    .collect::<BTreeSet<_>>(),
            ),
            Ok(raw) => {
                drop(raw);
                Sent::AllRequestAttributes
            }
            Err(error) => {
                drop(error);
                Sent::AllRequestAttributes
            }
        };
        if mode == "pending" || mode == "malformed_pending" {
            let token = if mode == "malformed_pending" {
                "not-a-decodable-mock-token"
            } else {
                "builtin-mock-pending"
            };
            let pending = attempt.pending(PendingToken::new(token), sent, 1);
            return Err(pending.preserve(ProviderError::timeout(
                "builtin mock create remains in progress",
            )));
        }

        if let Some(delay) = Self::create_delay_for(&id) {
            if let Err(error) = Self::signal_ready("CARINA_MOCK_CREATE_READY_PATH") {
                return Err(attempt.untracked(error));
            }
            tokio::time::sleep(delay).await;
        }

        let _state_guard = match Self::lock_state_file() {
            Ok(guard) => guard,
            Err(error) => return Err(attempt.untracked(error)),
        };
        let mut states = match self.load_states_for_operation() {
            Ok(states) => states,
            Err(error) => return Err(attempt.untracked(error)),
        };
        let key = match Self::resolved_resource_key(&id) {
            Ok(key) => key,
            Err(error) => return Err(attempt.untracked(error)),
        };

        let mut persisted_values = resource.resolved_attributes();
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
        Self::populate_test_resource_identifier_json(&id, &mut attrs);

        let partial_create = self.partial_create_config_for(&id);
        if let Some(config) = partial_create {
            for attr in &config.missing_attributes {
                attrs.remove(attr);
            }
        }

        states.insert(key, attrs);
        if let Err(error) = self
            .save_states(&states)
            .map_err(|e| ProviderError::internal_caused_by("Failed to save state", e))
        {
            return Err(attempt.untracked(error));
        }

        let mut state_attrs = persisted_values;
        Self::populate_test_resource_identifier_value(&id, &mut state_attrs);
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
        if let Err(error) = Self::append_op_log_if_configured("create", &id) {
            return Err(issued.failed(Observation::Read(ReadOutcome::found(observed_state)), error));
        }
        if mode == "created" {
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

    fn persist_resumed_resource<S>(
        &self,
        id: &ResourceId,
        resource: &Resource<S>,
    ) -> ProviderResult<HashMap<String, Value>> {
        let _state_guard = Self::lock_state_file()?;
        let mut states = self.load_states_for_operation()?;
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
        Self::populate_test_resource_identifier_json(id, &mut persisted);
        states.insert(key, persisted);
        self.save_states(&states).map_err(|error| {
            ProviderError::internal_caused_by(
                "Failed to save builtin mock resumed-create state",
                error,
            )
        })?;

        let mut attributes = persisted_values;
        Self::populate_test_resource_identifier_value(id, &mut attributes);
        Self::strip_unobservable_nested_write_only(&mut attributes);
        Ok(attributes)
    }
}

fn partial_create_config_from_env() -> Option<PartialConfig> {
    let resource_id_pattern = env::var("CARINA_MOCK_PARTIAL_CREATE_FOR").ok()?;
    let missing_attributes = env::var("CARINA_MOCK_PARTIAL_CREATE_MISSING")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec!["computed".to_string()]);
    Some(PartialConfig {
        resource_id_pattern,
        missing_attributes,
    })
}

fn partial_update_config_from_env() -> Option<PartialConfig> {
    let resource_id_pattern = env::var("CARINA_MOCK_PARTIAL_UPDATE_FOR").ok()?;
    let missing_attributes = env::var("CARINA_MOCK_PARTIAL_UPDATE_MISSING")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec!["computed".to_string()]);
    Some(PartialConfig {
        resource_id_pattern,
        missing_attributes,
    })
}

fn update_delay() -> Option<Duration> {
    env::var("CARINA_MOCK_UPDATE_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
}

fn write_max_active() {
    let Some(path) = env::var_os("CARINA_MOCK_MAX_ACTIVE_PATH").map(PathBuf::from) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, MAX_ACTIVE_UPDATES.load(Ordering::SeqCst).to_string());
}

fn update_trace_path() -> Option<PathBuf> {
    env::var_os("CARINA_MOCK_UPDATE_TRACE_PATH").map(PathBuf::from)
}

fn reset_update_trace() {
    let Some(path) = update_trace_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, "");
}

fn append_update_trace(event: &str, resource: &str, active: Option<usize>) {
    let Some(path) = update_trace_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let line = if let Some(active) = active {
        format!("{event} {resource} {active}\n")
    } else {
        format!("{event} {resource}\n")
    };
    let _ = file.write_all(line.as_bytes());
}

fn append_update_patch_log(id: &ResourceId, ops: &[PatchOp]) -> ProviderResult<()> {
    let Some(path) = env::var_os("CARINA_MOCK_UPDATE_PATCH_LOG").map(PathBuf::from) else {
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
        Self::append_op_log_if_configured("read", id)?;
        let mut read_outcome = None;
        if let Some(delay) = Self::read_delay_for(id) {
            read_outcome = Some(ReadOutcomeGuard::from_env());
            Self::signal_ready("CARINA_MOCK_READ_READY_PATH")?;
            tokio::time::sleep(delay).await;
        }

        let _state_guard = Self::lock_state_file()?;
        let states = self.load_states_for_operation()?;
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

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderNormalizer for MockProvider {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [carina_core::resource::ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            for resource in resources
                .iter()
                .filter(|resource| resource.id.provider == BUILTIN_PROVIDER_NAME)
            {
                Self::append_normalizer_log_if_configured("normalize_desired", &resource.id)?;
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
                Self::append_normalizer_log_if_configured("normalize_state", id)?;
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
                Self::append_normalizer_log_if_configured("hydrate_read_state", id)?;
            }
            Ok(())
        })
    }
}

impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
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
            let mode =
                env::var("CARINA_MOCK_RESUME_MODE").unwrap_or_else(|_| "resumed".to_string());
            match mode.as_str() {
                "discarded" => {
                    let (resume, create_request) = request.into_parts();
                    drop(create_request);
                    return Err(resume.discarded(ProviderError::not_found(
                        "builtin mock reports that the recorded create ended",
                    )));
                }
                "untracked" => {
                    let (resume, create_request) = request.into_parts();
                    drop(create_request);
                    return Err(resume.untracked(ProviderError::api_error(
                        "builtin mock lost the resume response",
                    )));
                }
                "undetermined" | "malformed_pending" => {
                    let (resume, create_request) = request.into_parts();
                    drop(create_request);
                    return Err(resume.preserve(ProviderError::internal(
                        "builtin mock cannot determine the recorded create outcome",
                    )));
                }
                "pending" => {
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
                "resumed" | "resend" | "created_read_failed" => {}
                unsupported => {
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

            if mode == "resend" {
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
            if mode == "created_read_failed" {
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
            let _active = ActiveUpdateGuard::enter(key.clone());
            if update_trace_path().is_some() {
                // This test hook is coupled to the executor's continuous-refill loop:
                // it dispatches every currently ready future that fits before polling
                // any of them. Yield once so that dispatch batch records all starts
                // before any finish. Trace tests reconstruct causal waves from that
                // ordering; an await before this write or during dispatch must revisit
                // those assertions. The executor itself is not wave-based.
                tokio::task::yield_now().await;
            }
            if let Some(delay) = update_delay() {
                tokio::time::sleep(delay).await;
            }
            if let Some(error) = Self::update_fail_error_for(&id) {
                return Err(error);
            }
            Self::append_update_request_if_configured(&request)?;
            append_update_patch_log(&id, &request.patch().ops)?;

            let _state_guard = Self::lock_state_file()?;
            let mut states = self.load_states_for_operation()?;
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
            Self::add_server_list_update_out_of_band_element(&id, &mut attributes);
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
            self.save_states(&states)
                .map_err(|e| ProviderError::internal_caused_by("Failed to save state", e))?;

            Self::strip_unobservable_nested_write_only(&mut attributes);

            if let Some(config) = partial_update {
                for attr in &config.missing_attributes {
                    attributes.remove(attr);
                }
            }
            Self::append_op_log_if_configured("update", &id)?;
            let applied_identifier =
                env::var("CARINA_MOCK_UPDATE_RESULT_IDENTIFIER").unwrap_or(identifier);
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

            Ok(UpdateOutcome::Success { state })
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
            if let Some(err) = Self::delete_fail_error_for(&id, &identifier) {
                return Err(err);
            }
            if let Some(err) = Self::delete_timeout_error_for(&id) {
                return Err(err);
            }

            if let Some(delay) = Self::delete_delay_for(&id) {
                Self::signal_ready("CARINA_MOCK_DELETE_READY_PATH")?;
                tokio::time::sleep(delay).await;
            }

            if let Some(path) = env::var_os("CARINA_MOCK_DELETE_LOG").map(PathBuf::from) {
                Self::append_delete_log(path, &key).map_err(|e| {
                    ProviderError::internal_caused_by("Failed to append delete log", e)
                })?;
            }

            let _state_guard = Self::lock_state_file()?;
            let mut states = self.load_states_for_operation()?;

            states.remove(&key);
            self.save_states(&states)
                .map_err(|e| ProviderError::internal_caused_by("Failed to save state", e))?;

            Self::append_op_log_if_configured("delete", &id)?;
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
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;
    use std::time::Instant;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    struct EnvGuard {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = env::var_os(name);
            // SAFETY: EnvGuard is used only while ENV_LOCK is held in these tests.
            unsafe {
                env::set_var(name, value);
            }
            Self { name, previous }
        }

        fn unset(name: &'static str) -> Self {
            let previous = env::var_os(name);
            // SAFETY: EnvGuard is used only while ENV_LOCK is held in these tests.
            unsafe {
                env::remove_var(name);
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: EnvGuard values are declared after the ENV_LOCK guard, so
            // they are dropped before the lock is released.
            unsafe {
                if let Some(previous) = &self.previous {
                    env::set_var(self.name, previous);
                } else {
                    env::remove_var(self.name);
                }
            }
        }
    }

    fn string_value(value: impl Into<String>) -> Value {
        let value: String = value.into();
        Value::Concrete(ConcreteValue::String(value.into()))
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
    fn builtin_mock_exercises_every_create_and_resume_verdict_mode() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _legacy_create_failure = EnvGuard::unset("CARINA_MOCK_CREATE_FAIL_FOR");
        let _operation_log = EnvGuard::unset("CARINA_MOCK_OP_LOG");
        let tmp = tempfile::tempdir().unwrap();
        let provider = MockProvider {
            state_file: tmp.path().join("mock-state.json"),
            partial_create: None,
            partial_update: None,
        };
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
            let mode_guard = EnvGuard::set("CARINA_MOCK_CREATE_MODE", mode);
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
            drop(mode_guard);
        }

        let create_mode = EnvGuard::set("CARINA_MOCK_CREATE_MODE", "pending");
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
        drop(create_mode);

        for (mode, expected) in [
            ("resumed", "resumed"),
            ("discarded", "ended"),
            ("undetermined", "in_flight"),
            ("pending", "in_flight"),
            ("malformed_pending", "in_flight"),
            ("resend", "resent"),
        ] {
            let mode_guard = EnvGuard::set("CARINA_MOCK_RESUME_MODE", mode);
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
            drop(mode_guard);
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

    fn mock_provider_config() -> ProviderConfig {
        carina_core::parser::parse_file(
            "provider mock {}\n",
            std::path::Path::new("mock_provider_config.crn"),
            &carina_core::parser::ProviderContext::default(),
        )
        .unwrap()
        .into_expanded()
        .expect("mock provider fixture must not retain parser placeholders")
        .providers
        .into_iter()
        .next()
        .unwrap()
    }

    #[test]
    fn builtin_provider_selection_is_shared_by_implicit_and_explicit_mock_paths() {
        assert!(uses_builtin_provider::<NoPlaceholder>(&[], |_| {
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
        let provider = MockProvider {
            state_file: state_file.clone(),
            partial_create: None,
            partial_update: None,
        };
        let mut states = HashMap::new();
        let mut entry = HashMap::new();
        entry.insert(
            "k".to_string(),
            serde_json::from_value(serde_json::json!("v")).unwrap(),
        );
        states.insert("aws.s3.Bucket.b".to_string(), entry);
        provider.save_states(&states).unwrap();
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
        let provider = MockProvider {
            state_file,
            partial_create: None,
            partial_update: None,
        };
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
        let _guard = ENV_LOCK.lock().unwrap();
        let _op_log = EnvGuard::unset("CARINA_MOCK_OP_LOG");
        let _create_fail = EnvGuard::unset("CARINA_MOCK_CREATE_FAIL_FOR");
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = MockProvider {
            state_file: state_file.clone(),
            partial_create: None,
            partial_update: None,
        };
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
        let provider = MockProvider {
            state_file,
            partial_create: None,
            partial_update: None,
        };
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
        let provider = MockProvider {
            state_file: state_file.clone(),
            partial_create: None,
            partial_update: None,
        };
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
        let _guard = ENV_LOCK.lock().unwrap();
        let _op_log = EnvGuard::unset("CARINA_MOCK_OP_LOG");
        let _create_fail = EnvGuard::unset("CARINA_MOCK_CREATE_FAIL_FOR");
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = MockProvider {
            state_file: state_file.clone(),
            partial_create: None,
            partial_update: None,
        };
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
        let _guard = ENV_LOCK.lock().unwrap();
        let _op_log = EnvGuard::unset("CARINA_MOCK_OP_LOG");
        let _create_fail = EnvGuard::unset("CARINA_MOCK_CREATE_FAIL_FOR");
        let tmp = tempfile::tempdir().unwrap();
        let provider = MockProvider {
            state_file: tmp.path().join("mock-state.json"),
            partial_create: None,
            partial_update: None,
        };
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
        let provider = MockProvider::default();
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
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let log_path = tmp.path().join("op.log");
        let _op_log = EnvGuard::set("CARINA_MOCK_OP_LOG", log_path.as_os_str());
        let _create_fail = EnvGuard::unset("CARINA_MOCK_CREATE_FAIL_FOR");
        let provider = MockProvider {
            state_file,
            partial_create: None,
            partial_update: None,
        };
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
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let log_path = tmp.path().join("op.log");
        let _op_log = EnvGuard::set("CARINA_MOCK_OP_LOG", log_path.as_os_str());
        let _create_fail = EnvGuard::unset("CARINA_MOCK_CREATE_FAIL_FOR");
        let provider = MockProvider {
            state_file,
            partial_create: None,
            partial_update: None,
        };
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

        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("op.log");
        let _op_log = EnvGuard::set("CARINA_MOCK_OP_LOG", log_path.as_os_str());
        let provider = MockProvider {
            state_file: tmp.path().join("mock-state.json"),
            partial_create: None,
            partial_update: None,
        };
        let mut resources =
            vec![Resource::with_provider("mock", "test.resource", "normalize-me", None).into()];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime
            .block_on(provider.normalize_desired(&mut resources))
            .expect("normalizer should succeed");

        assert_eq!(
            fs::read_to_string(log_path).unwrap(),
            "normalize_desired test.resource.normalize-me\n"
        );
    }

    #[test]
    fn create_fail_for_returns_error_without_op_log_entry() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let log_path = tmp.path().join("op.log");
        let _op_log = EnvGuard::set("CARINA_MOCK_OP_LOG", log_path.as_os_str());
        let _create_fail = EnvGuard::set("CARINA_MOCK_CREATE_FAIL_FOR", "test.resource.blocked");
        let provider = MockProvider {
            state_file,
            partial_create: None,
            partial_update: None,
        };
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
    fn delete_delay_env_var_delays_matching_resource_delete() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = MockProvider {
            state_file,
            partial_create: None,
            partial_update: None,
        };
        let id = ResourceId::with_provider_identity("mock", "test.resource", "slow", None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        // SAFETY: This test serializes access to the process environment with
        // ENV_LOCK and clears the variables before releasing the lock.
        unsafe {
            env::set_var("CARINA_MOCK_DELETE_DELAY_MS_FOR", "test.resource.slow");
            env::set_var("CARINA_MOCK_DELETE_DELAY_MS", "25");
        }

        let started = Instant::now();
        let identifier =
            AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty");
        runtime
            .block_on(provider.delete(&id, &identifier, delete_request_without_schema()))
            .unwrap();
        let elapsed = started.elapsed();

        // SAFETY: See the set_var safety note above; ENV_LOCK is still held.
        unsafe {
            env::remove_var("CARINA_MOCK_DELETE_DELAY_MS_FOR");
            env::remove_var("CARINA_MOCK_DELETE_DELAY_MS");
        }

        assert!(
            elapsed >= Duration::from_millis(25),
            "matching delete should be delayed by at least 25ms, elapsed {elapsed:?}"
        );
    }
}
