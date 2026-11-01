#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use carina_core::resource::{
    ResourceIdentity as CoreResourceIdentity, ResourceIdentityError as CoreResourceIdentityError,
};
use carina_core::schema::{
    AttributeSchema as CoreAttributeSchema, AttributeType as CoreAttributeType, ReadBackBudget,
    ResourceSchema as CoreResourceSchema, ValidatedConfigSchema as CoreValidatedConfigSchema,
    ValidatedResourceSchema as CoreValidatedResourceSchema, legacy_validator,
};
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, DesiredAttribute,
    DesiredResource, FoundRead, NormalizedResource, Pending, PendingDelete, PhasedToken, Poll,
    PollRequest, PollVerdict, Resume, ResumeStep, ResumeVerdict, Sent, UpdateOutcome,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CreateMode {
    #[default]
    Normal,
    Untracked,
    Created,
    Pending,
    Resumed,
    Discarded,
    Undetermined,
    MalformedPending,
    Resend,
}

const UNENCODABLE_CREATE_TOKEN_POLLS: u32 = u32::MAX;

#[derive(Debug, Serialize, Deserialize)]
struct CreateOperation {
    remaining: u32,
    #[serde(serialize_with = "encode_create_poll_count")]
    polls_served: u32,
    #[serde(default, deserialize_with = "decode_create_mode")]
    mode: CreateMode,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum CreateToken {
    Operation(CreateOperation),
    ReadBack,
}

impl PhasedToken for CreateToken {
    type Operation = CreateOperation;

    fn from_operation(operation: CreateOperation) -> Self {
        Self::Operation(operation)
    }

    fn into_operation(self) -> Result<CreateOperation, Self> {
        match self {
            Self::Operation(operation) => Ok(operation),
            Self::ReadBack => Err(self),
        }
    }

    fn operation(&self) -> Option<&CreateOperation> {
        match self {
            Self::Operation(operation) => Some(operation),
            Self::ReadBack => None,
        }
    }
}

fn encode_create_poll_count<S: serde::Serializer>(
    polls_served: &u32,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if *polls_served == UNENCODABLE_CREATE_TOKEN_POLLS {
        return Err(serde::ser::Error::custom(
            "intentional mock create-token encode failure",
        ));
    }
    serializer.serialize_u32(*polls_served)
}

fn decode_create_mode<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<CreateMode, D::Error> {
    match CreateMode::deserialize(deserializer)? {
        CreateMode::MalformedPending => Err(serde::de::Error::custom(
            "intentional mock create-token decode failure",
        )),
        mode => Ok(mode),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateOperation {
    remaining: u32,
    polls_served: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UpdateToken {
    Operation(UpdateOperation),
    ReadBack,
}

impl PhasedToken for UpdateToken {
    type Operation = UpdateOperation;

    fn from_operation(operation: UpdateOperation) -> Self {
        Self::Operation(operation)
    }

    fn into_operation(self) -> Result<UpdateOperation, Self> {
        match self {
            Self::Operation(operation) => Ok(operation),
            Self::ReadBack => Err(self),
        }
    }

    fn operation(&self) -> Option<&UpdateOperation> {
        match self {
            Self::Operation(operation) => Some(operation),
            Self::ReadBack => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct DeleteToken {
    remaining: u32,
}

struct MockProcessProvider {
    reference_table: carina_provider_mock::references::ReferenceTable,
    states: Mutex<HashMap<String, MockState>>,
    read_back_polls: Mutex<HashMap<String, u32>>,
}

enum MockState {
    Present(BTreeMap<String, Value>),
    Deleted,
}

const MOCK_IDENTIFIER_PREFIX: &str = "mock-v1:";

#[derive(Debug, Serialize, Deserialize)]
struct MockIdentifierPayload {
    nonce: String,
    read_back: bool,
    delete_pending_polls: u32,
}

impl Default for MockProcessProvider {
    fn default() -> Self {
        Self {
            reference_table: Default::default(),
            states: Mutex::new(HashMap::new()),
            read_back_polls: Mutex::new(HashMap::new()),
        }
    }
}

impl MockProcessProvider {
    fn optional_string_attribute(name: &str) -> CoreAttributeSchema {
        CoreAttributeSchema::optional(name, CoreAttributeType::string())
            .with_description("Mock create/resume test control")
            .create_only()
            .identity()
            .build()
    }

    fn resource_id_wire_key(id: &ResourceId) -> String {
        match (
            id.provider.is_empty(),
            CoreResourceIdentity::try_from(id.identity.clone()),
        ) {
            (true, Err(CoreResourceIdentityError::Empty)) => id.resource_type.clone(),
            (true, Ok(identity)) => format!("{}.{}", id.resource_type, identity.as_str()),
            (false, Err(CoreResourceIdentityError::Empty)) => {
                format!("{}.{}", id.provider, id.resource_type)
            }
            (false, Ok(identity)) => {
                format!("{}.{}.{}", id.provider, id.resource_type, identity.as_str())
            }
        }
    }

    fn operation_error(operation: &str, id: &ResourceId) -> ProviderError {
        ProviderError {
            kind: ProviderErrorKind::Internal,
            message: format!("intentional mock {operation} error"),
            resource_id: Some(id.clone()),
            cause: None,
            provider_name: Some("mock".to_string()),
            operation: None,
            status: None,
            code: None,
            request_id: None,
        }
    }

    fn pending_polls(attributes: &BTreeMap<String, Value>, operation_key: &str) -> u32 {
        attributes
            .get(operation_key)
            .or_else(|| attributes.get("pending_polls"))
            .and_then(|value| match value {
                Value::Int(value) => u32::try_from(*value).ok(),
                Value::Bool(_)
                | Value::Float(_)
                | Value::String(_)
                | Value::StringList(_)
                | Value::List(_)
                | Value::Map(_) => None,
            })
            .unwrap_or(0)
    }

    fn exact_pending_polls(attributes: &BTreeMap<String, Value>, key: &str) -> u32 {
        attributes
            .get(key)
            .and_then(|value| match value {
                Value::Int(value) => u32::try_from(*value).ok(),
                _ => None,
            })
            .unwrap_or(0)
    }

    fn bool_attribute(attributes: &BTreeMap<String, Value>, key: &str) -> bool {
        matches!(attributes.get(key), Some(Value::Bool(true)))
    }

    fn mint_identifier(attributes: &BTreeMap<String, Value>) -> AppliedIdentifier {
        let payload = MockIdentifierPayload {
            nonce: uuid::Uuid::new_v4().to_string(),
            // The WASM boundary has already decrypted secret inputs. Only
            // typed lifecycle controls may enter a persisted identifier;
            // arbitrary request values have no secrecy provenance here.
            read_back: Self::exact_pending_polls(attributes, "create_read_back_pending_polls") > 0
                || Self::exact_pending_polls(attributes, "update_read_back_pending_polls") > 0
                || attributes.contains_key("computed"),
            delete_pending_polls: Self::pending_polls(attributes, "delete_pending_polls"),
        };
        let payload = serde_json::to_string(&payload)
            .expect("mock lifecycle controls must be JSON encodable");
        AppliedIdentifier::new(format!("{MOCK_IDENTIFIER_PREFIX}{payload}"))
            .expect("the encoded mock identifier is non-empty")
    }

    fn decode_identifier(identifier: &AppliedIdentifier) -> Option<BTreeMap<String, Value>> {
        let payload = identifier.as_str().strip_prefix(MOCK_IDENTIFIER_PREFIX)?;
        serde_json::from_str::<MockIdentifierPayload>(payload)
            .ok()
            .map(|payload| {
                let mut attributes = BTreeMap::new();
                if payload.delete_pending_polls > 0 {
                    attributes.insert(
                        "delete_pending_polls".to_string(),
                        Value::Int(i64::from(payload.delete_pending_polls)),
                    );
                }
                if payload.read_back {
                    attributes.insert("computed".to_string(), Value::String("ready".to_string()));
                    attributes.insert(
                        "read_identifier".to_string(),
                        Value::String(identifier.as_str().to_string()),
                    );
                }
                attributes
            })
    }

    fn remember_state(&self, identifier: &AppliedIdentifier, attributes: &BTreeMap<String, Value>) {
        self.states.lock().unwrap().insert(
            identifier.as_str().to_string(),
            MockState::Present(attributes.clone()),
        );
    }

    fn restore_attributes(
        states: &HashMap<String, MockState>,
        identifier: &AppliedIdentifier,
    ) -> Option<BTreeMap<String, Value>> {
        match states.get(identifier.as_str()) {
            Some(MockState::Present(attributes)) => Some(attributes.clone()),
            // A completed delete takes precedence over the identifier's
            // fallback payload, just as a present in-process snapshot does.
            Some(MockState::Deleted) => None,
            None => Self::decode_identifier(identifier),
        }
    }

    fn read_back_poll_number(&self, identifier: &AppliedIdentifier, operation: &str) -> u32 {
        let key = format!("{operation}:{}", identifier.as_str());
        let mut polls = self.read_back_polls.lock().unwrap();
        let count = polls.entry(key).or_default();
        *count += 1;
        *count
    }

    fn complete_read_back(&self, identifier: &AppliedIdentifier) -> BTreeMap<String, Value> {
        let mut states = self.states.lock().unwrap();
        let mut attributes = Self::restore_attributes(&states, identifier).unwrap_or_default();
        attributes.insert("computed".to_string(), Value::String("ready".to_string()));
        states.insert(
            identifier.as_str().to_string(),
            MockState::Present(attributes.clone()),
        );
        attributes
    }

    fn read_back_attributes(&self, identifier: &AppliedIdentifier) -> BTreeMap<String, Value> {
        Self::restore_attributes(&self.states.lock().unwrap(), identifier).unwrap_or_default()
    }

    fn partial_read_diagnostic() -> PartialReadDiagnostic {
        PartialReadDiagnostic {
            reason: "mock read-back is still pending".to_string(),
            missing_attributes: vec!["computed".to_string()],
        }
    }

    fn create_mode(attributes: &BTreeMap<String, Value>) -> CreateMode {
        let Some(value) = attributes.get("create_mode") else {
            return CreateMode::Normal;
        };
        let value = match value {
            Value::String(value) => value.as_str(),
            Value::Bool(value) => {
                let _ignored = value;
                return CreateMode::Normal;
            }
            Value::Int(value) => {
                let _ignored = value;
                return CreateMode::Normal;
            }
            Value::Float(value) => {
                let _ignored = value;
                return CreateMode::Normal;
            }
            Value::StringList(value) => {
                let _ignored = value;
                return CreateMode::Normal;
            }
            Value::List(value) => {
                let _ignored = value;
                return CreateMode::Normal;
            }
            Value::Map(value) => {
                let _ignored = value;
                return CreateMode::Normal;
            }
        };
        match value {
            "normal" => CreateMode::Normal,
            "untracked" => CreateMode::Untracked,
            "created" => CreateMode::Created,
            "pending" => CreateMode::Pending,
            "resumed" => CreateMode::Resumed,
            "discarded" => CreateMode::Discarded,
            "undetermined" => CreateMode::Undetermined,
            "malformed_pending" => CreateMode::MalformedPending,
            "resend" => CreateMode::Resend,
            _unsupported => CreateMode::Normal,
        }
    }

    fn sent_attributes(attributes: &BTreeMap<String, Value>) -> Sent {
        let Some(value) = attributes.get("create_sent") else {
            return Sent::AllRequestAttributes;
        };
        match value {
            Value::StringList(values) => Sent::only(values.iter().cloned()),
            Value::String(value) => Sent::only(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .map(ToString::to_string),
            ),
            Value::Bool(value) => {
                let _ignored = value;
                Sent::AllRequestAttributes
            }
            Value::Int(value) => {
                let _ignored = value;
                Sent::AllRequestAttributes
            }
            Value::Float(value) => {
                let _ignored = value;
                Sent::AllRequestAttributes
            }
            Value::List(value) => {
                let _ignored = value;
                Sent::AllRequestAttributes
            }
            Value::Map(value) => {
                let _ignored = value;
                Sent::AllRequestAttributes
            }
        }
    }

    fn canonical_identifier(attributes: &BTreeMap<String, Value>) -> Option<AppliedIdentifier> {
        let value = attributes.get("canonical_identifier")?;
        match value {
            Value::String(value) => AppliedIdentifier::new(value.clone()).ok(),
            Value::Bool(value) => {
                let _ignored = value;
                None
            }
            Value::Int(value) => {
                let _ignored = value;
                None
            }
            Value::Float(value) => {
                let _ignored = value;
                None
            }
            Value::StringList(value) => {
                let _ignored = value;
                None
            }
            Value::List(value) => {
                let _ignored = value;
                None
            }
            Value::Map(value) => {
                let _ignored = value;
                None
            }
        }
    }

    fn found_read(
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        attributes: BTreeMap<String, Value>,
    ) -> FoundRead {
        FoundRead::from_applied_state(AppliedState {
            id: id.clone(),
            identifier: identifier.clone(),
            attributes,
        })
    }

    fn create_mode_error(id: &ResourceId, mode: CreateMode) -> ProviderError {
        ProviderError {
            kind: ProviderErrorKind::Internal,
            message: format!("intentional mock {mode:?} create verdict"),
            resource_id: Some(id.clone()),
            cause: None,
            provider_name: Some("mock".to_string()),
            operation: Some("create".to_string()),
            status: None,
            code: None,
            request_id: None,
        }
    }

    fn update_pending_polls(request: &UpdateRequest) -> u32 {
        request
            .patch
            .ops
            .iter()
            .find(|op| op.key() == "update_pending_polls")
            .or_else(|| {
                request
                    .patch
                    .ops
                    .iter()
                    .find(|op| op.key() == "pending_polls")
            })
            .and_then(PatchOp::sent_value)
            .and_then(|value| match value {
                Value::Int(value) => u32::try_from(*value).ok(),
                Value::Bool(_)
                | Value::Float(_)
                | Value::String(_)
                | Value::StringList(_)
                | Value::List(_)
                | Value::Map(_) => None,
            })
            .unwrap_or(0)
    }

    fn update_exact_pending_polls(request: &UpdateRequest, key: &str) -> u32 {
        request
            .patch
            .ops
            .iter()
            .find(|op| op.key() == key)
            .and_then(PatchOp::sent_value)
            .and_then(|value| match value {
                Value::Int(value) => u32::try_from(*value).ok(),
                _ => None,
            })
            .unwrap_or(0)
    }

    fn update_bool_attribute(request: &UpdateRequest, key: &str) -> bool {
        request
            .patch
            .ops
            .iter()
            .find(|op| op.key() == key)
            .and_then(PatchOp::sent_value)
            .is_some_and(|value| matches!(value, Value::Bool(true)))
    }

    fn created_attributes(
        mut request: CreateRequest,
        polls_served: Option<u32>,
    ) -> BTreeMap<String, Value> {
        if let Some(polls_served) = polls_served {
            request.resource.attributes.insert(
                "__mock_create_polls_served__".to_string(),
                Value::Int(i64::from(polls_served)),
            );
        }
        let id = &request.resource.id;
        if id.resource_type == "route53.HostedZone"
            && let Some((attribute, value)) = carina_provider_mock::generated_attribute(
                &id.resource_type,
                Some(&id.identity),
                None,
            )
        {
            request
                .resource
                .attributes
                .entry(attribute.to_string())
                .or_insert_with(|| Value::String(value));
        }
        request.resource.attributes
    }

    fn create_done(
        &self,
        request: CreateRequest,
        polls_served: Option<u32>,
    ) -> (AppliedIdentifier, BTreeMap<String, Value>) {
        let attributes = Self::created_attributes(request, polls_served);
        let identifier = Self::mint_identifier(&attributes);
        self.remember_state(&identifier, &attributes);
        (identifier, attributes)
    }

    fn create_done_with_identifier(
        &self,
        identifier: &AppliedIdentifier,
        request: CreateRequest,
        polls_served: Option<u32>,
    ) -> BTreeMap<String, Value> {
        let attributes = Self::created_attributes(request, polls_served);
        self.remember_state(identifier, &attributes);
        attributes
    }

    fn update_done(
        &self,
        _id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        polls_served: Option<u32>,
    ) -> carina_provider_protocol::UpdateOutcome {
        // Apply the patch on top of `from` while preserving typed persisted
        // values that were not mentioned by the patch. Also echo the patch op
        // kinds into a sentinel attribute so integration tests can assert the
        // patch round-tripped through the WIT boundary.
        let mut state = request.state_after_patch();
        let mut applied_op_kinds: Vec<Value> = Vec::with_capacity(request.patch.ops.len());
        for op in &request.patch.ops {
            let kind = match op {
                PatchOp::Add { .. } => "add",
                PatchOp::Replace { .. } => "replace",
                PatchOp::Reassert { .. } => "reassert",
                PatchOp::Remove { .. } => "remove",
            };
            applied_op_kinds.push(Value::String(format!("{kind}:{}", op.key())));
        }
        let patch_sentinel = Value::List(applied_op_kinds);
        state.attributes.insert(
            "__mock_patch_ops__".to_string(),
            patch_sentinel.clone().into(),
        );
        if let Some(polls_served) = polls_served {
            state.attributes.insert(
                "__mock_update_polls_served__".to_string(),
                Value::Int(i64::from(polls_served)).into(),
            );
        }
        let mut states = self.states.lock().unwrap();
        let mut cloud_attributes =
            Self::restore_attributes(&states, identifier).unwrap_or_default();
        // The fake cloud retains its existing ordinary values and receives
        // only the checked patch. Never rebuild it from persisted `from` state:
        // that would either send redacted digests to the cloud or discard an
        // entire nested attribute containing an unchanged stored secret.
        for op in &request.patch.ops {
            match op {
                PatchOp::Add { key, value }
                | PatchOp::Replace { key, value }
                | PatchOp::Reassert { key, value } => {
                    cloud_attributes.insert(key.clone(), value.clone());
                }
                PatchOp::Remove { key } => {
                    cloud_attributes.remove(key);
                }
            }
        }
        cloud_attributes.insert("__mock_patch_ops__".to_string(), patch_sentinel);
        if let Some(polls_served) = polls_served {
            cloud_attributes.insert(
                "__mock_update_polls_served__".to_string(),
                Value::Int(i64::from(polls_served)),
            );
        }

        // Fresh instances recover synthetic read-back/delete behavior from
        // this version of the identifier rather than create-time controls.
        let updated_identifier = Self::mint_identifier(&cloud_attributes);
        states.insert(
            identifier.as_str().to_string(),
            MockState::Present(cloud_attributes.clone()),
        );
        states.insert(
            updated_identifier.as_str().to_string(),
            MockState::Present(cloud_attributes),
        );
        let state = AppliedPersistedState {
            id: state.id,
            identifier: updated_identifier,
            attributes: state.attributes,
        };

        carina_provider_protocol::UpdateOutcome::Success { state }
    }

    fn update_read_back_done(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
    ) -> carina_provider_protocol::UpdateOutcome {
        let outcome = self.update_done(id, identifier, request, None);
        let carina_provider_protocol::UpdateOutcome::Success { mut state } = outcome else {
            unreachable!("mock update_done always succeeds")
        };
        state.attributes.insert(
            "computed".to_string(),
            Value::String("ready".to_string()).into(),
        );
        let _attributes = self.complete_read_back(&state.identifier);
        carina_provider_protocol::UpdateOutcome::Success { state }
    }

    fn update_read_back_partial(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
    ) -> carina_provider_protocol::UpdateOutcome {
        let outcome = self.update_done(id, identifier, request, None);
        let carina_provider_protocol::UpdateOutcome::Success { state } = outcome else {
            unreachable!("mock update_done always succeeds")
        };
        carina_provider_protocol::UpdateOutcome::PartialSuccess {
            state,
            diagnostic: Self::partial_read_diagnostic(),
        }
    }

    fn delete_done(&self, identifier: &AppliedIdentifier) {
        self.states
            .lock()
            .unwrap()
            .insert(identifier.as_str().to_string(), MockState::Deleted);
    }
}

impl CarinaProvider for MockProcessProvider {
    type CreateToken = CreateToken;
    type UpdateToken = UpdateToken;
    type DeleteToken = DeleteToken;

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "mock".into(),
            display_name: "Mock Provider (Process)".into(),
            capabilities: vec![],
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn schemas(&self) -> Vec<CoreValidatedResourceSchema> {
        vec![
            carina_provider_mock::references::resource_schema(),
            CoreResourceSchema::new("test.read_back")
                .attribute(
                    CoreAttributeSchema::optional(
                        "create_read_back_pending_polls",
                        CoreAttributeType::int(),
                    )
                    .build(),
                )
                .attribute(
                    CoreAttributeSchema::optional("present", CoreAttributeType::string()).build(),
                )
                .attribute(
                    CoreAttributeSchema::optional("from_read_back", CoreAttributeType::string())
                        .build(),
                )
                .attribute(
                    CoreAttributeSchema::provider_populated(
                        "computed",
                        CoreAttributeType::string(),
                    )
                    .build(),
                )
                .attribute(
                    CoreAttributeSchema::provider_populated(
                        "read_identifier",
                        CoreAttributeType::string(),
                    )
                    .build(),
                )
                .with_description("Mock resource for host-owned read-back polling")
                .with_operation_config(OperationConfig {
                    create_timeout_secs: Some(5),
                    update_timeout_secs: Some(5),
                    delete_timeout_secs: Some(5),
                    create_max_retries: None,
                    delete_max_retries: None,
                    read_back: ReadBackBudget::from_secs(2),
                })
                .validate()
                .expect("mock read-back schema must be valid"),
            CoreResourceSchema::new("test.short_budget")
                .attribute(Self::optional_string_attribute("create_mode"))
                .with_description("Mock resource with short operation budgets")
                .with_operation_config(OperationConfig {
                    create_timeout_secs: Some(2),
                    update_timeout_secs: Some(2),
                    delete_timeout_secs: Some(2),
                    create_max_retries: None,
                    delete_max_retries: None,
                    read_back: ReadBackBudget::from_secs(1),
                })
                .validate()
                .expect("mock process schema must be valid"),
            CoreResourceSchema::new("test.sdk_validator")
                .attribute(
                    CoreAttributeSchema::optional(
                        "guarded",
                        CoreAttributeType::refined_string_with_validator(
                            carina_core::schema::TypeIdentity::new(
                                Some("mock"),
                                ["test", "sdk_validator"],
                                "Guarded",
                            ),
                            None,
                            None,
                            legacy_validator(|_value| {
                                Err("intentional SDK schema validator rejection".to_string())
                            }),
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .expect("SDK validator schema must be valid"),
            CoreResourceSchema::new("test.recovery_probe")
                .attribute(
                    CoreAttributeSchema::optional(
                        "value",
                        CoreAttributeType::refined_string_with_validator(
                            carina_core::schema::TypeIdentity::new(
                                Some("mock"),
                                Vec::<String>::new(),
                                "RecoveryProbe",
                            ),
                            None,
                            None,
                            legacy_validator(|_value| Ok(())),
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .expect("recovery probe schema must be valid"),
            CoreResourceSchema::new("test.timing_probe")
                .attribute(
                    CoreAttributeSchema::optional(
                        "value",
                        CoreAttributeType::refined_string_with_validator(
                            carina_core::schema::TypeIdentity::new(
                                Some("mock"),
                                Vec::<String>::new(),
                                "TimingProbe",
                            ),
                            None,
                            None,
                            legacy_validator(|value| {
                                if matches!(
                                    value,
                                    carina_core::resource::Value::Concrete(
                                        carina_core::resource::ConcreteValue::String(value)
                                    ) if value == "__mock_validate_custom_type_hang__"
                                ) {
                                    std::thread::sleep(std::time::Duration::from_secs(10));
                                }
                                Ok(())
                            }),
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .expect("timing probe schema must be valid"),
            CoreResourceSchema::new("test.normalize_required_together")
                .attribute(
                    CoreAttributeSchema::optional("primary", CoreAttributeType::string()).build(),
                )
                .attribute(
                    CoreAttributeSchema::optional("companion", CoreAttributeType::string()).build(),
                )
                .with_description("Mock desired-normalization three-way attribute probe")
                .validate()
                .expect("normalization probe schema must be valid"),
        ]
        .into_iter()
        .chain(
            carina_provider_mock::anonymous_identity_schemas()
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("mock identity schema must be valid")
                }),
        )
        .collect()
    }

    fn reference_forms(&self) -> carina_core::schema::ReferenceForms {
        BTreeMap::new()
    }

    fn provider_config_attribute_types(&self) -> CoreValidatedConfigSchema {
        CoreValidatedConfigSchema::validate(BTreeMap::from([(
            "reference_table".into(),
            CoreAttributeType::string(),
        )]))
        .expect("mock config schema")
    }

    fn validate_config(&self, attrs: &BTreeMap<String, Value>) -> Result<(), String> {
        if attrs.contains_key("__mock_validate_config_trap__") {
            panic!("intentional mock validate_config trap");
        }
        if let Some(Value::Int(delay_millis)) = attrs.get("validate_delay_millis")
            && let Ok(delay_millis) = u64::try_from(*delay_millis)
        {
            std::thread::sleep(std::time::Duration::from_millis(delay_millis));
        }
        Ok(())
    }

    fn initialize(&mut self, attrs: &BTreeMap<String, Value>) -> Result<ReferenceScope, String> {
        if let Some(Value::String(table)) = attrs.get("reference_table") {
            self.reference_table =
                serde_json::from_str(table).map_err(|error| error.to_string())?;
        }
        if attrs.contains_key("__mock_initialize_error__") {
            return Err("intentional mock initialization failure".to_string());
        }
        match attrs.get("__mock_reference_scope_region__") {
            Some(Value::String(region)) => {
                ReferenceScope::available("mock", region.clone(), "test-account")
                    .map_err(|error| error.to_string())
            }
            Some(_) | None => Ok(ReferenceScope::Unavailable),
        }
    }

    fn resolve_references(
        &self,
        requests: Vec<ReferenceRequest>,
    ) -> Result<Vec<Result<ReferenceOutcome, ReferenceError>>, ProviderError> {
        Ok(carina_provider_mock::references::resolve(
            &self.reference_table,
            requests,
        ))
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> Result<ReadOutcome, ProviderError> {
        if identifier.as_str() == "__mock_read_error__" {
            return Err(Self::operation_error("read", id));
        }
        if identifier.as_str() == "__mock_non_finite_read__" {
            return Ok(ReadOutcome::Found {
                state: AppliedState {
                    id: id.clone(),
                    identifier: identifier.clone(),
                    attributes: BTreeMap::from([(
                        "values".to_string(),
                        Value::List(vec![Value::Float(f64::NAN)]),
                    )]),
                },
            });
        }

        let mut states = self.states.lock().unwrap();
        let key = identifier.as_str().to_string();
        let Some(mut attributes) = Self::restore_attributes(&states, identifier) else {
            return Ok(ReadOutcome::NotFound { id: id.clone() });
        };
        let read_back_was_requested =
            Self::exact_pending_polls(&attributes, "create_read_back_pending_polls") > 0
                || Self::exact_pending_polls(&attributes, "update_read_back_pending_polls") > 0;
        if read_back_was_requested {
            attributes
                .entry("computed".to_string())
                .or_insert_with(|| Value::String("ready".to_string()));
            attributes.insert(
                "read_identifier".to_string(),
                Value::String(identifier.as_str().to_string()),
            );
        }
        states.insert(key, MockState::Present(attributes.clone()));
        Ok(ReadOutcome::Found {
            state: AppliedState {
                id: id.clone(),
                identifier: identifier.clone(),
                attributes,
            },
        })
    }

    /// Exercise the `read_data_source` path end-to-end through the WASM
    /// bridge: echo the user-supplied inputs back into state plus a
    /// sentinel `__mock_read_data_source__` flag so integration tests can
    /// verify the call was routed through the WASM boundary.
    fn read_data_source(&self, resource: &Resource) -> Result<State, ProviderError> {
        let mut attributes = resource.attributes.clone();
        attributes.insert("__mock_read_data_source__".to_string(), Value::Bool(true));
        Ok(State {
            id: resource.id.clone(),
            identifier: Some("mock-id".into()),
            attributes,
            exists: true,
        })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> Result<CreateStep<Self::CreateToken>, CreateVerdict<Self::CreateToken>> {
        if id.identity == "__mock_create_trap__" {
            panic!("intentional mock create trap");
        }
        if id.identity == "__mock_create_error__" {
            return Err(attempt.untracked(ProviderError {
                kind: ProviderErrorKind::Internal,
                message: "intentional mock create error".to_string(),
                resource_id: Some(id.clone()),
                cause: None,
                provider_name: None,
                operation: None,
                status: None,
                code: None,
                request_id: None,
            }));
        }

        let unencodable_created = matches!(
            request.resource.attributes.get("create_mode"),
            Some(Value::String(mode)) if mode == "unencodable_created"
        );
        let unencodable_pending = matches!(
            request.resource.attributes.get("create_mode"),
            Some(Value::String(mode)) if mode == "unencodable_pending"
        );
        let unencodable_pending_issued = matches!(
            request.resource.attributes.get("create_mode"),
            Some(Value::String(mode)) if mode == "unencodable_pending_issued"
        );
        let unencodable_poll = matches!(
            request.resource.attributes.get("create_mode"),
            Some(Value::String(mode)) if mode == "unencodable_poll"
        );
        let unencodable_poll_issued = matches!(
            request.resource.attributes.get("create_mode"),
            Some(Value::String(mode)) if mode == "unencodable_poll_issued"
        );
        let mode = Self::create_mode(&request.resource.attributes);
        let sent = Self::sent_attributes(&request.resource.attributes);
        let canonical_identifier = Self::canonical_identifier(&request.resource.attributes);
        if unencodable_created {
            let (identifier, mut attributes) = self.create_done(request, None);
            attributes.insert(
                "__mock_unencodable__".to_string(),
                Value::List(vec![Value::Float(f64::NAN)]),
            );
            return Ok(attempt.issued(identifier, sent).succeeded(attributes));
        }
        if unencodable_pending || unencodable_pending_issued {
            let token = CreateOperation {
                remaining: 100,
                polls_served: UNENCODABLE_CREATE_TOKEN_POLLS,
                mode: CreateMode::Pending,
            };
            if unencodable_pending_issued {
                let identifier = Self::mint_identifier(&request.resource.attributes);
                self.remember_state(&identifier, &request.resource.attributes);
                return Ok(attempt
                    .issued(identifier, sent)
                    .pending(CreateToken::Operation(token), 1));
            }
            return Ok(attempt.pending(token, sent, 1));
        }
        if unencodable_poll || unencodable_poll_issued {
            let token = CreateOperation {
                remaining: 100,
                polls_served: 0,
                mode: CreateMode::Pending,
            };
            if unencodable_poll_issued {
                let identifier = Self::mint_identifier(&request.resource.attributes);
                self.remember_state(&identifier, &request.resource.attributes);
                return Ok(attempt
                    .issued(identifier, sent)
                    .pending(CreateToken::Operation(token), 1));
            }
            return Ok(attempt.pending(token, sent, 1));
        }
        if matches!(
            request.resource.attributes.get("call_spin_forever"),
            Some(Value::Bool(true))
        ) {
            let mut value = 0_u64;
            loop {
                value = std::hint::black_box(value.wrapping_add(1));
            }
        }
        if matches!(
            request
                .resource
                .attributes
                .get("call_spin_with_host_calls_forever"),
            Some(Value::Bool(true))
        ) {
            let mut value = 0_u64;
            loop {
                for _ in 0..16_384 {
                    value = std::hint::black_box(value.wrapping_add(1));
                }
                std::hint::black_box(std::time::Instant::now());
            }
        }
        if let Some(Value::Int(delay_millis)) = request.resource.attributes.get("call_delay_millis")
            && let Ok(delay_millis) = u64::try_from(*delay_millis)
        {
            std::thread::sleep(std::time::Duration::from_millis(delay_millis));
        }
        let pending_polls =
            Self::pending_polls(&request.resource.attributes, "create_pending_polls");
        let read_back_pending_polls = Self::exact_pending_polls(
            &request.resource.attributes,
            "create_read_back_pending_polls",
        );
        match mode {
            CreateMode::Untracked => Err(attempt.untracked(Self::create_mode_error(id, mode))),
            CreateMode::Created => {
                let (identifier, attributes) = self.create_done(request, None);
                let observation = Observation::Read(ReadOutcome::Found {
                    state: AppliedState {
                        id: id.clone(),
                        identifier: identifier.clone(),
                        attributes: attributes.clone(),
                    },
                });
                let issued = attempt.issued(identifier, sent);
                Err(match canonical_identifier {
                    Some(identifier) => issued
                        .canonicalize(identifier)
                        .failed(observation, Self::create_mode_error(id, mode)),
                    None => issued.failed(observation, Self::create_mode_error(id, mode)),
                })
            }
            CreateMode::Normal if pending_polls == 0 && read_back_pending_polls > 0 => {
                let (identifier, _attributes) = self.create_done(request, None);
                Ok(attempt
                    .issued(identifier, sent)
                    .pending(CreateToken::ReadBack, 1))
            }
            CreateMode::Normal if pending_polls == 0 => {
                let (identifier, attributes) = self.create_done(request, None);
                let issued = attempt.issued(identifier, sent);
                Ok(match canonical_identifier {
                    Some(identifier) => issued.canonicalize(identifier).succeeded(attributes),
                    None => issued.succeeded(attributes),
                })
            }
            CreateMode::Normal => Ok(attempt.pending(
                CreateOperation {
                    remaining: pending_polls - 1,
                    polls_served: 0,
                    mode,
                },
                sent,
                1,
            )),
            CreateMode::Resumed | CreateMode::Discarded => {
                let identifier = Self::mint_identifier(&request.resource.attributes);
                self.remember_state(&identifier, &request.resource.attributes);
                Ok(attempt.issued(identifier, sent).pending(
                    CreateToken::Operation(CreateOperation {
                        remaining: 100,
                        polls_served: 0,
                        mode,
                    }),
                    1,
                ))
            }
            CreateMode::Pending
            | CreateMode::Undetermined
            | CreateMode::MalformedPending
            | CreateMode::Resend => Ok(attempt.pending(
                CreateOperation {
                    remaining: 100,
                    polls_served: 0,
                    mode,
                },
                sent,
                1,
            )),
        }
    }

    fn poll_create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        poll: Poll<Self::CreateToken>,
        poll_request: PollRequest,
    ) -> Result<CreateStep<Self::CreateToken>, PollVerdict<Self::CreateToken>> {
        let unencodable_poll = matches!(
            request.resource.attributes.get("create_mode"),
            Some(Value::String(mode))
                if mode == "unencodable_poll" || mode == "unencodable_poll_issued"
        );
        if unencodable_poll {
            let token = CreateOperation {
                remaining: 100,
                polls_served: UNENCODABLE_CREATE_TOKEN_POLLS,
                mode: CreateMode::Pending,
            };
            return Ok(match poll {
                Poll::Unissued(attempt, _previous) => attempt.pending(token, 1),
                Poll::Issued(issued, _previous) => issued.pending(CreateToken::Operation(token), 1),
            });
        }
        let canonical_identifier = Self::canonical_identifier(&request.resource.attributes);
        match poll {
            Poll::Unissued(attempt, token) => {
                let CreateOperation {
                    remaining,
                    polls_served,
                    mode,
                } = token;
                match mode {
                    CreateMode::Normal => {
                        if remaining > 0 {
                            return Ok(attempt.pending(
                                CreateOperation {
                                    remaining: remaining - 1,
                                    polls_served: polls_served + 1,
                                    mode,
                                },
                                1,
                            ));
                        }
                        let read_back_pending_polls = Self::exact_pending_polls(
                            &request.resource.attributes,
                            "create_read_back_pending_polls",
                        );
                        let (identifier, attributes) =
                            self.create_done(request, Some(polls_served + 1));
                        let issued = attempt.issued(identifier);
                        if read_back_pending_polls > 0 {
                            return Ok(issued.pending(CreateToken::ReadBack, 1));
                        }
                        Ok(match canonical_identifier {
                            Some(identifier) => {
                                issued.canonicalize(identifier).succeeded(attributes)
                            }
                            None => issued.succeeded(attributes),
                        })
                    }
                    mode @ (CreateMode::Pending
                    | CreateMode::Resumed
                    | CreateMode::Discarded
                    | CreateMode::Undetermined
                    | CreateMode::Resend) => Ok(attempt.pending(
                        CreateOperation {
                            remaining,
                            polls_served: polls_served + 1,
                            mode,
                        },
                        1,
                    )),
                    CreateMode::MalformedPending => Err(attempt.preserve(
                        CreateOperation {
                            remaining,
                            polls_served,
                            mode,
                        },
                        1,
                        Self::create_mode_error(id, CreateMode::MalformedPending),
                    )),
                    CreateMode::Untracked => {
                        Err(attempt.untracked(Self::create_mode_error(id, CreateMode::Untracked)))
                    }
                    CreateMode::Created => {
                        let identifier = Self::mint_identifier(&request.resource.attributes);
                        self.remember_state(&identifier, &request.resource.attributes);
                        let observation =
                            Observation::Read(ReadOutcome::NotFound { id: id.clone() });
                        let issued = attempt.issued(identifier);
                        Err(match canonical_identifier {
                            Some(identifier) => issued
                                .canonicalize(identifier)
                                .failed(
                                    observation,
                                    Self::create_mode_error(id, CreateMode::Created),
                                )
                                .into(),
                            None => issued
                                .failed(
                                    observation,
                                    Self::create_mode_error(id, CreateMode::Created),
                                )
                                .into(),
                        })
                    }
                }
            }
            Poll::Issued(
                issued,
                CreateToken::Operation(CreateOperation {
                    remaining,
                    polls_served,
                    mode,
                }),
            ) => match mode {
                CreateMode::Normal => {
                    if remaining > 0 {
                        return Ok(issued.pending(
                            CreateToken::Operation(CreateOperation {
                                remaining: remaining - 1,
                                polls_served: polls_served + 1,
                                mode,
                            }),
                            1,
                        ));
                    }
                    let read_back_pending_polls = Self::exact_pending_polls(
                        &request.resource.attributes,
                        "create_read_back_pending_polls",
                    );
                    let identifier = issued.identifier().clone();
                    let attributes = self.create_done_with_identifier(
                        &identifier,
                        request,
                        Some(polls_served + 1),
                    );
                    if read_back_pending_polls > 0 {
                        return Ok(issued.pending(CreateToken::ReadBack, 1));
                    }
                    Ok(match canonical_identifier {
                        Some(identifier) => issued.canonicalize(identifier).succeeded(attributes),
                        None => issued.succeeded(attributes),
                    })
                }
                mode @ (CreateMode::Pending
                | CreateMode::Resumed
                | CreateMode::Discarded
                | CreateMode::Undetermined
                | CreateMode::Resend) => Ok(issued.pending(
                    CreateToken::Operation(CreateOperation {
                        remaining,
                        polls_served: polls_served + 1,
                        mode,
                    }),
                    1,
                )),
                CreateMode::MalformedPending | CreateMode::Untracked => {
                    Err(issued.discarded(Self::create_mode_error(id, mode)).into())
                }
                CreateMode::Created => {
                    let observation = Observation::Read(ReadOutcome::NotFound { id: id.clone() });
                    Err(match canonical_identifier {
                        Some(identifier) => issued
                            .canonicalize(identifier)
                            .failed(
                                observation,
                                Self::create_mode_error(id, CreateMode::Created),
                            )
                            .into(),
                        None => issued
                            .failed(
                                observation,
                                Self::create_mode_error(id, CreateMode::Created),
                            )
                            .into(),
                    })
                }
            },
            Poll::Issued(issued, CreateToken::ReadBack) => {
                if id.identity == "__mock_read_back_preserve_error__" {
                    return Err(issued
                        .preserve(
                            CreateToken::ReadBack,
                            1,
                            Self::operation_error("read-back temporarily unavailable", id),
                        )
                        .into());
                }
                if poll_request.finality == carina_plugin_sdk::PollFinality::FinalAttempt {
                    if Self::bool_attribute(
                        &request.resource.attributes,
                        "create_read_back_operation_after_final",
                    ) {
                        return Ok(issued.pending(
                            CreateToken::Operation(CreateOperation {
                                remaining: 0,
                                polls_served: 0,
                                mode: CreateMode::Normal,
                            }),
                            1,
                        ));
                    }
                    if Self::bool_attribute(
                        &request.resource.attributes,
                        "create_read_back_pending_after_final",
                    ) {
                        return Ok(issued.pending(CreateToken::ReadBack, 1));
                    }
                    let attributes = self.read_back_attributes(issued.identifier());
                    return Ok(issued.partial(attributes, Self::partial_read_diagnostic()));
                }
                let target = Self::exact_pending_polls(
                    &request.resource.attributes,
                    "create_read_back_pending_polls",
                );
                if self.read_back_poll_number(issued.identifier(), "create") >= target {
                    let attributes = self.complete_read_back(issued.identifier());
                    Ok(issued.succeeded(attributes))
                } else {
                    Ok(issued.pending(CreateToken::ReadBack, 1))
                }
            }
        }
    }

    fn resume_create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        resume: Resume<Self::CreateToken>,
        poll_request: carina_plugin_sdk::PollRequest,
    ) -> Result<ResumeStep<Self::CreateToken>, ResumeVerdict<Self::CreateToken>> {
        let unencodable_repending = matches!(
            request.resource.attributes.get("create_mode"),
            Some(Value::String(mode)) if mode == "unencodable_repending"
        );
        if unencodable_repending {
            let token = CreateOperation {
                remaining: 100,
                polls_served: UNENCODABLE_CREATE_TOKEN_POLLS,
                mode: CreateMode::Pending,
            };
            return Ok(match resume {
                Resume::Unissued(attempt, _previous) => attempt.pending(token, 1),
                Resume::Issued(resumed, _previous) => {
                    resumed.pending(CreateToken::Operation(token), 1)
                }
            });
        }
        let canonical_identifier = Self::canonical_identifier(&request.resource.attributes);
        match resume {
            Resume::Unissued(attempt, token) => {
                let CreateOperation {
                    remaining,
                    polls_served,
                    mode,
                } = token;
                match mode {
                    CreateMode::Normal => {
                        if remaining > 0 {
                            return Ok(attempt.pending(
                                CreateOperation {
                                    remaining: remaining - 1,
                                    polls_served: polls_served + 1,
                                    mode,
                                },
                                1,
                            ));
                        }
                        let (identifier, attributes) =
                            self.create_done(request, Some(polls_served + 1));
                        let resumed = attempt.issued(identifier);
                        let found = Self::found_read(id, resumed.identifier(), attributes);
                        Ok(match canonical_identifier {
                            Some(identifier) => resumed.canonicalize(identifier).completed(found),
                            None => resumed.completed(found),
                        })
                    }
                    CreateMode::Pending => Ok(attempt.pending(
                        CreateOperation {
                            remaining,
                            polls_served: polls_served + 1,
                            mode,
                        },
                        1,
                    )),
                    CreateMode::Resumed => {
                        let (identifier, attributes) =
                            self.create_done(request, Some(polls_served + 1));
                        let resumed = attempt.issued(identifier);
                        let found = Self::found_read(id, resumed.identifier(), attributes);
                        Ok(match canonical_identifier {
                            Some(identifier) => resumed.canonicalize(identifier).completed(found),
                            None => resumed.completed(found),
                        })
                    }
                    CreateMode::Discarded | CreateMode::Untracked => {
                        Err(attempt.untracked(Self::create_mode_error(id, mode)))
                    }
                    CreateMode::Undetermined => Err(attempt.preserve(
                        CreateOperation {
                            remaining,
                            polls_served,
                            mode,
                        },
                        1,
                        Self::create_mode_error(id, CreateMode::Undetermined),
                    )),
                    CreateMode::MalformedPending => Err(attempt.preserve(
                        CreateOperation {
                            remaining,
                            polls_served,
                            mode,
                        },
                        1,
                        Self::create_mode_error(id, CreateMode::MalformedPending),
                    )),
                    CreateMode::Resend => {
                        let (identifier, attributes) =
                            self.create_done(request, Some(polls_served + 1));
                        let issued = attempt
                            .resend()
                            .issued(identifier, Sent::AllRequestAttributes);
                        Ok(match canonical_identifier {
                            Some(identifier) => {
                                issued.canonicalize(identifier).succeeded(attributes).into()
                            }
                            None => issued.succeeded(attributes).into(),
                        })
                    }
                    CreateMode::Created => {
                        let identifier = Self::mint_identifier(&request.resource.attributes);
                        self.remember_state(&identifier, &request.resource.attributes);
                        let observation =
                            Observation::Read(ReadOutcome::NotFound { id: id.clone() });
                        let resumed = attempt.issued(identifier);
                        Err(match canonical_identifier {
                            Some(identifier) => resumed.canonicalize(identifier).failed(
                                observation,
                                Self::create_mode_error(id, CreateMode::Created),
                            ),
                            None => resumed.failed(
                                observation,
                                Self::create_mode_error(id, CreateMode::Created),
                            ),
                        })
                    }
                }
            }
            Resume::Issued(
                resumed,
                CreateToken::Operation(CreateOperation {
                    remaining,
                    polls_served,
                    mode,
                }),
            ) => match mode {
                CreateMode::Normal => {
                    if remaining > 0 {
                        return Ok(resumed.pending(
                            CreateToken::Operation(CreateOperation {
                                remaining: remaining - 1,
                                polls_served: polls_served + 1,
                                mode,
                            }),
                            1,
                        ));
                    }
                    let identifier = resumed.identifier().clone();
                    let read_back_pending_polls = Self::exact_pending_polls(
                        &request.resource.attributes,
                        "create_read_back_pending_polls",
                    );
                    let attributes = self.create_done_with_identifier(
                        &identifier,
                        request,
                        Some(polls_served + 1),
                    );
                    if read_back_pending_polls > 0 {
                        return Ok(resumed.pending(CreateToken::ReadBack, 1));
                    }
                    let found = Self::found_read(id, resumed.identifier(), attributes);
                    Ok(match canonical_identifier {
                        Some(identifier) => resumed.canonicalize(identifier).completed(found),
                        None => resumed.completed(found),
                    })
                }
                CreateMode::Pending => Ok(resumed.pending(
                    CreateToken::Operation(CreateOperation {
                        remaining,
                        polls_served: polls_served + 1,
                        mode,
                    }),
                    1,
                )),
                CreateMode::Resumed | CreateMode::Resend => {
                    let identifier = resumed.identifier().clone();
                    let attributes = self.create_done_with_identifier(
                        &identifier,
                        request,
                        Some(polls_served + 1),
                    );
                    let found = Self::found_read(id, resumed.identifier(), attributes);
                    Ok(match canonical_identifier {
                        Some(identifier) => resumed.canonicalize(identifier).completed(found),
                        None => resumed.completed(found),
                    })
                }
                CreateMode::Discarded | CreateMode::Untracked | CreateMode::MalformedPending => {
                    Err(resumed.discarded(Self::create_mode_error(id, mode)))
                }
                CreateMode::Undetermined | CreateMode::Created => {
                    let observation = Observation::Read(ReadOutcome::NotFound { id: id.clone() });
                    Err(match canonical_identifier {
                        Some(identifier) => resumed
                            .canonicalize(identifier)
                            .failed(observation, Self::create_mode_error(id, mode)),
                        None => resumed.failed(observation, Self::create_mode_error(id, mode)),
                    })
                }
            },
            Resume::Issued(resumed, CreateToken::ReadBack) => {
                if id.identity == "__mock_read_back_preserve_error__" {
                    return Err(resumed.preserve(
                        CreateToken::ReadBack,
                        1,
                        Self::operation_error("read-back temporarily unavailable", id),
                    ));
                }
                if poll_request.finality == carina_plugin_sdk::PollFinality::FinalAttempt {
                    if Self::bool_attribute(
                        &request.resource.attributes,
                        "create_read_back_pending_after_final",
                    ) {
                        return Ok(resumed.pending(CreateToken::ReadBack, 1));
                    }
                    let attributes = self.read_back_attributes(resumed.identifier());
                    return Ok(resumed.partial(attributes, Self::partial_read_diagnostic()));
                }
                let target = Self::exact_pending_polls(
                    &request.resource.attributes,
                    "create_read_back_pending_polls",
                );
                if self.read_back_poll_number(resumed.identifier(), "create") < target {
                    return Ok(resumed.pending(CreateToken::ReadBack, 1));
                }
                match self.read(id, resumed.identifier(), ReadRequest) {
                    Ok(ReadOutcome::Found { state }) => {
                        Ok(resumed.completed(FoundRead::from_applied_state(state)))
                    }
                    Ok(ReadOutcome::NotFound { .. }) => {
                        Ok(resumed.pending(CreateToken::ReadBack, 1))
                    }
                    Err(error) => Err(resumed.preserve(CreateToken::ReadBack, 1, error)),
                }
            }
        }
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
    ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
        let pending_polls = Self::update_pending_polls(&request);
        if pending_polls > 0 {
            return Ok(UpdateOutcome::Pending(Pending::new(
                UpdateToken::Operation(UpdateOperation {
                    remaining: pending_polls - 1,
                    polls_served: 0,
                }),
                1,
            )));
        }
        if Self::update_exact_pending_polls(&request, "update_read_back_pending_polls") > 0 {
            let _outcome = self.update_done(id, identifier, request, None);
            return Ok(UpdateOutcome::Pending(Pending::new(
                UpdateToken::ReadBack,
                1,
            )));
        }
        Ok(UpdateOutcome::Done(
            self.update_done(id, identifier, request, None),
        ))
    }

    fn poll_update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        pending: Pending<Self::UpdateToken>,
        poll_request: PollRequest,
    ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
        match pending.token {
            UpdateToken::Operation(UpdateOperation {
                remaining,
                polls_served,
            }) if remaining > 0 => Ok(UpdateOutcome::Pending(Pending::new(
                UpdateToken::Operation(UpdateOperation {
                    remaining: remaining - 1,
                    polls_served: polls_served + 1,
                }),
                1,
            ))),
            UpdateToken::Operation(UpdateOperation {
                remaining: _,
                polls_served,
            }) => {
                if Self::update_exact_pending_polls(&request, "update_read_back_pending_polls") > 0
                {
                    let _outcome =
                        self.update_done(id, identifier, request, Some(polls_served + 1));
                    return Ok(UpdateOutcome::Pending(Pending::new(
                        UpdateToken::ReadBack,
                        1,
                    )));
                }
                Ok(UpdateOutcome::Done(self.update_done(
                    id,
                    identifier,
                    request,
                    Some(polls_served + 1),
                )))
            }
            UpdateToken::ReadBack
                if poll_request.finality == carina_plugin_sdk::PollFinality::FinalAttempt =>
            {
                if Self::update_bool_attribute(&request, "update_read_back_operation_after_final") {
                    return Ok(UpdateOutcome::Pending(Pending::new(
                        UpdateToken::Operation(UpdateOperation {
                            remaining: 0,
                            polls_served: 0,
                        }),
                        1,
                    )));
                }
                if Self::update_bool_attribute(&request, "update_read_back_pending_after_final") {
                    return Ok(UpdateOutcome::Pending(Pending::new(
                        UpdateToken::ReadBack,
                        1,
                    )));
                }
                Ok(UpdateOutcome::Done(
                    self.update_read_back_partial(id, identifier, request),
                ))
            }
            UpdateToken::ReadBack => {
                let target =
                    Self::update_exact_pending_polls(&request, "update_read_back_pending_polls");
                if self.read_back_poll_number(identifier, "update") >= target {
                    Ok(UpdateOutcome::Done(
                        self.update_read_back_done(id, identifier, request),
                    ))
                } else {
                    Ok(UpdateOutcome::Pending(Pending::new(
                        UpdateToken::ReadBack,
                        1,
                    )))
                }
            }
        }
    }

    fn delete(
        &self,
        _id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: DeleteRequest,
    ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
        let pending_polls = Self::restore_attributes(&self.states.lock().unwrap(), identifier)
            .map(|attributes| Self::pending_polls(&attributes, "delete_pending_polls"))
            .unwrap_or(0);
        if pending_polls > 0 {
            return Ok(DeleteOutcome::Pending(PendingDelete::new(
                DeleteToken {
                    remaining: pending_polls - 1,
                },
                1,
            )));
        }
        self.delete_done(identifier);
        Ok(DeleteOutcome::Deleted)
    }

    fn poll_delete(
        &self,
        _id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: DeleteRequest,
        pending: PendingDelete<Self::DeleteToken>,
    ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
        if pending.token.remaining > 0 {
            return Ok(DeleteOutcome::Pending(PendingDelete::new(
                DeleteToken {
                    remaining: pending.token.remaining - 1,
                },
                1,
            )));
        }
        self.delete_done(identifier);
        Ok(DeleteOutcome::Deleted)
    }

    fn required_permissions(
        &self,
        id: &ResourceId,
        _op: carina_plugin_sdk::PlanOp,
    ) -> Result<Vec<String>, ProviderError> {
        if id.identity == "__mock_required_permissions_trap__" {
            panic!("intentional mock required_permissions trap");
        }
        Ok(Vec::new())
    }

    fn satisfier_hint(
        &self,
        target_id: &ResourceId,
        _attr_path: &[String],
    ) -> Result<Vec<carina_plugin_sdk::BindingPattern>, ProviderError> {
        if target_id.identity == "__mock_invalid_satisfier_hint__" {
            return Ok(vec![carina_plugin_sdk::BindingPattern::AttributeMatch {
                resource_type: "test.resource".to_string(),
                attr: Vec::new(),
                from: vec!["source".to_string()],
            }]);
        }
        Ok(Vec::new())
    }

    fn normalize_desired(
        &self,
        resources: Vec<DesiredResource>,
    ) -> Result<Vec<NormalizedResource>, ProviderError> {
        if let Some(resource) = resources
            .iter()
            .find(|resource| resource.id().identity == "__mock_normalize_desired_error__")
        {
            return Err(Self::operation_error("normalize_desired", resource.id()));
        }
        if let Some(resource) = resources.iter().find(|resource| {
            resource.id().resource_type == "test.normalize_required_together"
                && matches!(resource.attribute("primary"), DesiredAttribute::Known(_))
                && matches!(resource.attribute("companion"), DesiredAttribute::Absent)
        }) {
            return Err(ProviderError {
                kind: ProviderErrorKind::InvalidInput,
                message:
                    "test.normalize_required_together companion is required when primary is present"
                        .to_string(),
                resource_id: Some(resource.id().clone()),
                cause: None,
                provider_name: Some("mock".to_string()),
                operation: None,
                status: None,
                code: None,
                request_id: None,
            });
        }
        Ok(resources.into_iter().map(DesiredResource::finish).collect())
    }

    fn normalize_state(
        &self,
        states: HashMap<String, PersistedState>,
    ) -> Result<HashMap<String, PersistedState>, ProviderError> {
        let rekey_single_result = states
            .values()
            .any(|state| state.attributes.contains_key("__mock_rekey_state_result__"));
        let drop_single_result = states
            .values()
            .any(|state| state.attributes.contains_key("__mock_drop_state_result__"));
        let special_id = states
            .values()
            .map(|state| state.id.clone())
            .find(|id| id.identity.starts_with("__mock_normalize_state_"));
        if special_id
            .as_ref()
            .is_some_and(|id| id.identity == "__mock_normalize_state_trap__")
        {
            panic!("intentional mock normalize_state trap");
        }
        if let Some(id) = special_id
            .as_ref()
            .filter(|id| id.identity == "__mock_normalize_state_error__")
        {
            return Err(Self::operation_error("normalize_state", id));
        }

        let mut normalized: HashMap<_, _> = states
            .into_values()
            .map(|mut state| {
                if let Some(marker) = state.attributes.get("__mock_normalize_state__").cloned() {
                    state
                        .attributes
                        .insert("__mock_normalized_state__".to_string(), marker);
                }
                let key = Self::resource_id_wire_key(&state.id);
                (key, state)
            })
            .collect();

        if let Some(id) = special_id {
            if id.identity == "__mock_normalize_state_missing_key__" {
                normalized.remove(&Self::resource_id_wire_key(&id));
            } else if id.identity == "__mock_normalize_state_extra_key__" {
                let state = normalized
                    .values()
                    .next()
                    .expect("mock extra-key hook requires one state")
                    .clone();
                normalized.insert("__mock_unexpected_state_key__".to_string(), state);
            }
        }

        if drop_single_result {
            normalized.clear();
        } else if rekey_single_result && normalized.len() == 1 {
            let state = normalized
                .into_values()
                .next()
                .expect("single-result re-key hook requires one state");
            normalized = HashMap::from([("__mock_guest_rekeyed_state__".to_string(), state)]);
        }

        Ok(normalized)
    }

    fn hydrate_read_state(
        &self,
        states: &mut HashMap<String, PersistedState>,
        saved_attrs: &HashMap<String, BTreeMap<String, PersistedValue>>,
    ) -> Result<(), ProviderError> {
        let rekey_single_result = states
            .values()
            .any(|state| state.attributes.contains_key("__mock_rekey_state_result__"));
        let drop_single_result = states
            .values()
            .any(|state| state.attributes.contains_key("__mock_drop_state_result__"));
        let special_id = states
            .values()
            .map(|state| state.id.clone())
            .find(|id| id.identity.starts_with("__mock_hydrate_state_"));
        if let Some(id) = special_id
            .as_ref()
            .filter(|id| id.identity == "__mock_hydrate_state_error__")
        {
            return Err(Self::operation_error("hydrate_read_state", id));
        }
        *states = std::mem::take(states)
            .into_iter()
            .map(|(input_key, mut state)| {
                if let Some(marker) = saved_attrs
                    .get(&input_key)
                    .and_then(|attrs| attrs.get("__mock_hydrate_read_state__"))
                    .cloned()
                {
                    state
                        .attributes
                        .insert("__mock_hydrated_read_state__".to_string(), marker);
                }
                if let Some(marker) = saved_attrs
                    .get(&input_key)
                    .and_then(|attrs| attrs.get("__mock_write_only_secret__"))
                    .cloned()
                {
                    state
                        .attributes
                        .insert("__mock_write_only_secret__".to_string(), marker);
                }
                (Self::resource_id_wire_key(&state.id), state)
            })
            .collect();

        if let Some(id) = special_id {
            if id.identity == "__mock_hydrate_state_missing_key__" {
                states.remove(&Self::resource_id_wire_key(&id));
            } else if id.identity == "__mock_hydrate_state_extra_key__" {
                let state = states
                    .values()
                    .next()
                    .expect("mock extra-key hook requires one state")
                    .clone();
                states.insert("__mock_unexpected_hydrate_key__".to_string(), state);
            }
        }

        if drop_single_result {
            states.clear();
        } else if rekey_single_result && states.len() == 1 {
            let state = std::mem::take(states)
                .into_values()
                .next()
                .expect("single-result re-key hook requires one state");
            states.insert("__mock_guest_rekeyed_state__".to_string(), state);
        }
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!(
        "carina-provider-mock no longer has a native provider transport; build the wasm32-wasip2 component"
    );
}

// For WASM: export_provider! macro bridges CarinaProvider to the WIT interface.
// An empty main() is still required for the binary target.
#[cfg(target_arch = "wasm32")]
carina_plugin_sdk::export_provider!(MockProcessProvider);

#[cfg(target_arch = "wasm32")]
fn main() {}
