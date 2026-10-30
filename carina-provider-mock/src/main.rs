#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use carina_core::resource::{
    ResourceIdentity as CoreResourceIdentity, ResourceIdentityError as CoreResourceIdentityError,
};
use carina_core::schema::{
    AttributeSchema as CoreAttributeSchema, AttributeType as CoreAttributeType,
    ResourceSchema as CoreResourceSchema, ValidatedConfigSchema as CoreValidatedConfigSchema,
    ValidatedResourceSchema as CoreValidatedResourceSchema, legacy_validator,
};
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, DesiredAttribute,
    DesiredResource, FoundRead, NormalizedResource, Pending, Poll, PollVerdict, Resume, ResumeStep,
    ResumeVerdict, Sent, UpdateOutcome,
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

#[derive(Debug)]
struct CreateToken {
    remaining: u32,
    polls_served: u32,
    mode: CreateMode,
}

#[derive(Serialize, Deserialize)]
struct EncodedCreateToken {
    remaining: u32,
    polls_served: u32,
    #[serde(default)]
    mode: CreateMode,
}

impl Serialize for CreateToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.polls_served == UNENCODABLE_CREATE_TOKEN_POLLS {
            return Err(serde::ser::Error::custom(
                "intentional mock create-token encode failure",
            ));
        }
        EncodedCreateToken {
            remaining: self.remaining,
            polls_served: self.polls_served,
            mode: self.mode,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CreateToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = EncodedCreateToken::deserialize(deserializer)?;
        if encoded.mode == CreateMode::MalformedPending {
            return Err(serde::de::Error::custom(
                "intentional mock create-token decode failure",
            ));
        }
        Ok(Self {
            remaining: encoded.remaining,
            polls_served: encoded.polls_served,
            mode: encoded.mode,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateToken {
    remaining: u32,
    polls_served: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct DeleteToken {
    remaining: u32,
}

struct MockProcessProvider {
    states: Mutex<HashMap<String, BTreeMap<String, Value>>>,
}

impl Default for MockProcessProvider {
    fn default() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
        }
    }
}

impl MockProcessProvider {
    fn optional_string_attribute(name: &str) -> CoreAttributeSchema {
        CoreAttributeSchema::optional(name, CoreAttributeType::string())
            .with_description("Mock create/resume test control")
            .identity()
            .build()
    }

    fn resource_key(id: &ResourceId) -> String {
        format!("{}.{}", id.resource_type, id.identity)
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

    fn found_read(id: &ResourceId, attributes: BTreeMap<String, Value>) -> FoundRead {
        FoundRead::from_applied_state(AppliedState {
            id: id.clone(),
            identifier: AppliedIdentifier::new("mock-read-id")
                .expect("the mock read identifier is non-empty"),
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

    fn create_done(
        &self,
        id: &ResourceId,
        mut request: CreateRequest,
        polls_served: Option<u32>,
    ) -> BTreeMap<String, Value> {
        if let Some(polls_served) = polls_served {
            request.resource.attributes.insert(
                "__mock_create_polls_served__".to_string(),
                Value::Int(i64::from(polls_served)),
            );
        }
        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(id);
        let resource = request.resource;
        states.insert(key, resource.attributes.clone());

        resource.attributes
    }

    fn update_done(
        &self,
        id: &ResourceId,
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
        let state = AppliedPersistedState {
            id: state.id,
            identifier: identifier.clone(),
            attributes: state.attributes,
        };

        let mut states = self.states.lock().unwrap();
        let cloud_attributes = states.entry(Self::resource_key(id)).or_default();
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

        carina_provider_protocol::UpdateOutcome::Success { state }
    }

    fn delete_done(&self, id: &ResourceId) {
        self.states.lock().unwrap().remove(&Self::resource_key(id));
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
            CoreResourceSchema::new("test.short_budget")
                .attribute(Self::optional_string_attribute("create_mode"))
                .with_description("Mock resource with short operation budgets")
                .with_operation_config(OperationConfig {
                    create_timeout_secs: Some(2),
                    update_timeout_secs: Some(2),
                    delete_timeout_secs: Some(2),
                    create_max_retries: None,
                    delete_max_retries: None,
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
    }

    fn reference_forms(&self) -> carina_core::schema::ReferenceForms {
        BTreeMap::new()
    }

    fn provider_config_attribute_types(&self) -> CoreValidatedConfigSchema {
        CoreValidatedConfigSchema::default()
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

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> Result<ReadOutcome, ProviderError> {
        if id.identity == "__mock_read_error__" {
            return Err(Self::operation_error("read", id));
        }
        if id.identity == "__mock_non_finite_read__" {
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

        let states = self.states.lock().unwrap();
        let key = Self::resource_key(id);

        if let Some(attrs) = states.get(&key) {
            Ok(ReadOutcome::Found {
                state: AppliedState {
                    id: id.clone(),
                    identifier: identifier.clone(),
                    attributes: attrs.clone(),
                },
            })
        } else {
            Ok(ReadOutcome::NotFound { id: id.clone() })
        }
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
            let mut attributes = self.create_done(id, request, None);
            attributes.insert(
                "__mock_unencodable__".to_string(),
                Value::List(vec![Value::Float(f64::NAN)]),
            );
            let identifier =
                AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty");
            return Ok(attempt.issued(identifier, sent).succeeded(attributes));
        }
        if unencodable_pending || unencodable_pending_issued {
            let token = CreateToken {
                remaining: 100,
                polls_served: UNENCODABLE_CREATE_TOKEN_POLLS,
                mode: CreateMode::Pending,
            };
            if unencodable_pending_issued {
                let identifier =
                    AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty");
                return Ok(attempt.issued(identifier, sent).pending(token, 1));
            }
            return Ok(attempt.pending(token, sent, 1));
        }
        if unencodable_poll || unencodable_poll_issued {
            let token = CreateToken {
                remaining: 100,
                polls_served: 0,
                mode: CreateMode::Pending,
            };
            if unencodable_poll_issued {
                let identifier =
                    AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty");
                return Ok(attempt.issued(identifier, sent).pending(token, 1));
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
        match mode {
            CreateMode::Untracked => Err(attempt.untracked(Self::create_mode_error(id, mode))),
            CreateMode::Created => {
                let attributes = self.create_done(id, request, None);
                let identifier =
                    AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty");
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
            CreateMode::Normal if pending_polls == 0 => {
                let attributes = self.create_done(id, request, None);
                let identifier =
                    AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty");
                let issued = attempt.issued(identifier, sent);
                Ok(match canonical_identifier {
                    Some(identifier) => issued.canonicalize(identifier).succeeded(attributes),
                    None => issued.succeeded(attributes),
                })
            }
            CreateMode::Normal => Ok(attempt.pending(
                CreateToken {
                    remaining: pending_polls - 1,
                    polls_served: 0,
                    mode,
                },
                sent,
                1,
            )),
            CreateMode::Resumed | CreateMode::Discarded => {
                let identifier =
                    AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty");
                Ok(attempt.issued(identifier, sent).pending(
                    CreateToken {
                        remaining: 100,
                        polls_served: 0,
                        mode,
                    },
                    1,
                ))
            }
            CreateMode::Pending
            | CreateMode::Undetermined
            | CreateMode::MalformedPending
            | CreateMode::Resend => Ok(attempt.pending(
                CreateToken {
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
    ) -> Result<CreateStep<Self::CreateToken>, PollVerdict<Self::CreateToken>> {
        let unencodable_poll = matches!(
            request.resource.attributes.get("create_mode"),
            Some(Value::String(mode))
                if mode == "unencodable_poll" || mode == "unencodable_poll_issued"
        );
        if unencodable_poll {
            let token = CreateToken {
                remaining: 100,
                polls_served: UNENCODABLE_CREATE_TOKEN_POLLS,
                mode: CreateMode::Pending,
            };
            return Ok(match poll {
                Poll::Unissued(attempt, _previous) => attempt.pending(token, 1),
                Poll::Issued(issued, _previous) => issued.pending(token, 1),
            });
        }
        let canonical_identifier = Self::canonical_identifier(&request.resource.attributes);
        match poll {
            Poll::Unissued(attempt, token) => match token.mode {
                CreateMode::Normal => {
                    if token.remaining > 0 {
                        return Ok(attempt.pending(
                            CreateToken {
                                remaining: token.remaining - 1,
                                polls_served: token.polls_served + 1,
                                mode: token.mode,
                            },
                            1,
                        ));
                    }
                    let attributes = self.create_done(id, request, Some(token.polls_served + 1));
                    let identifier = AppliedIdentifier::new("mock-id")
                        .expect("the mock identifier is non-empty");
                    let issued = attempt.issued(identifier);
                    Ok(match canonical_identifier {
                        Some(identifier) => issued.canonicalize(identifier).succeeded(attributes),
                        None => issued.succeeded(attributes),
                    })
                }
                mode @ (CreateMode::Pending
                | CreateMode::Resumed
                | CreateMode::Discarded
                | CreateMode::Undetermined
                | CreateMode::Resend) => Ok(attempt.pending(
                    CreateToken {
                        remaining: token.remaining,
                        polls_served: token.polls_served + 1,
                        mode,
                    },
                    1,
                )),
                CreateMode::MalformedPending => Err(attempt.preserve(
                    token,
                    1,
                    Self::create_mode_error(id, CreateMode::MalformedPending),
                )),
                CreateMode::Untracked => {
                    Err(attempt.untracked(Self::create_mode_error(id, CreateMode::Untracked)))
                }
                CreateMode::Created => {
                    let identifier = AppliedIdentifier::new("mock-id")
                        .expect("the mock identifier is non-empty");
                    let observation = Observation::Read(ReadOutcome::NotFound { id: id.clone() });
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
            },
            Poll::Issued(issued, token) => match token.mode {
                CreateMode::Normal => {
                    if token.remaining > 0 {
                        return Ok(issued.pending(
                            CreateToken {
                                remaining: token.remaining - 1,
                                polls_served: token.polls_served + 1,
                                mode: token.mode,
                            },
                            1,
                        ));
                    }
                    let attributes = self.create_done(id, request, Some(token.polls_served + 1));
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
                    CreateToken {
                        remaining: token.remaining,
                        polls_served: token.polls_served + 1,
                        mode,
                    },
                    1,
                )),
                CreateMode::MalformedPending | CreateMode::Untracked => Err(issued
                    .discarded(Self::create_mode_error(id, token.mode))
                    .into()),
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
        }
    }

    fn resume_create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        resume: Resume<Self::CreateToken>,
    ) -> Result<ResumeStep<Self::CreateToken>, ResumeVerdict<Self::CreateToken>> {
        let unencodable_repending = matches!(
            request.resource.attributes.get("create_mode"),
            Some(Value::String(mode)) if mode == "unencodable_repending"
        );
        if unencodable_repending {
            let token = CreateToken {
                remaining: 100,
                polls_served: UNENCODABLE_CREATE_TOKEN_POLLS,
                mode: CreateMode::Pending,
            };
            return Ok(match resume {
                Resume::Unissued(attempt, _previous) => attempt.pending(token, 1),
                Resume::Issued(resumed, _previous) => resumed.pending(token, 1),
            });
        }
        let canonical_identifier = Self::canonical_identifier(&request.resource.attributes);
        match resume {
            Resume::Unissued(attempt, token) => match token.mode {
                CreateMode::Normal => {
                    if token.remaining > 0 {
                        return Ok(attempt.pending(
                            CreateToken {
                                remaining: token.remaining - 1,
                                polls_served: token.polls_served + 1,
                                mode: token.mode,
                            },
                            1,
                        ));
                    }
                    let attributes = self.create_done(id, request, Some(token.polls_served + 1));
                    let identifier = AppliedIdentifier::new("mock-id")
                        .expect("the mock identifier is non-empty");
                    let resumed = attempt.issued(identifier);
                    Ok(match canonical_identifier {
                        Some(identifier) => resumed
                            .canonicalize(identifier)
                            .completed(Self::found_read(id, attributes)),
                        None => resumed.completed(Self::found_read(id, attributes)),
                    })
                }
                CreateMode::Pending => Ok(attempt.pending(
                    CreateToken {
                        remaining: token.remaining,
                        polls_served: token.polls_served + 1,
                        mode: token.mode,
                    },
                    1,
                )),
                CreateMode::Resumed => {
                    let attributes = self.create_done(id, request, Some(token.polls_served + 1));
                    let identifier = AppliedIdentifier::new("mock-id")
                        .expect("the mock identifier is non-empty");
                    let resumed = attempt.issued(identifier);
                    Ok(match canonical_identifier {
                        Some(identifier) => resumed
                            .canonicalize(identifier)
                            .completed(Self::found_read(id, attributes)),
                        None => resumed.completed(Self::found_read(id, attributes)),
                    })
                }
                CreateMode::Discarded | CreateMode::Untracked => {
                    Err(attempt.untracked(Self::create_mode_error(id, token.mode)))
                }
                CreateMode::Undetermined => Err(attempt.preserve(
                    token,
                    1,
                    Self::create_mode_error(id, CreateMode::Undetermined),
                )),
                CreateMode::MalformedPending => Err(attempt.preserve(
                    token,
                    1,
                    Self::create_mode_error(id, CreateMode::MalformedPending),
                )),
                CreateMode::Resend => {
                    let attributes = self.create_done(id, request, Some(token.polls_served + 1));
                    let identifier = AppliedIdentifier::new("mock-id")
                        .expect("the mock identifier is non-empty");
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
                    let identifier = AppliedIdentifier::new("mock-id")
                        .expect("the mock identifier is non-empty");
                    let observation = Observation::Read(ReadOutcome::NotFound { id: id.clone() });
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
            },
            Resume::Issued(resumed, token) => match token.mode {
                CreateMode::Normal => {
                    if token.remaining > 0 {
                        return Ok(resumed.pending(
                            CreateToken {
                                remaining: token.remaining - 1,
                                polls_served: token.polls_served + 1,
                                mode: token.mode,
                            },
                            1,
                        ));
                    }
                    let attributes = self.create_done(id, request, Some(token.polls_served + 1));
                    Ok(match canonical_identifier {
                        Some(identifier) => resumed
                            .canonicalize(identifier)
                            .completed(Self::found_read(id, attributes)),
                        None => resumed.completed(Self::found_read(id, attributes)),
                    })
                }
                CreateMode::Pending => Ok(resumed.pending(
                    CreateToken {
                        remaining: token.remaining,
                        polls_served: token.polls_served + 1,
                        mode: token.mode,
                    },
                    1,
                )),
                CreateMode::Resumed | CreateMode::Resend => {
                    let attributes = self.create_done(id, request, Some(token.polls_served + 1));
                    Ok(match canonical_identifier {
                        Some(identifier) => resumed
                            .canonicalize(identifier)
                            .completed(Self::found_read(id, attributes)),
                        None => resumed.completed(Self::found_read(id, attributes)),
                    })
                }
                CreateMode::Discarded | CreateMode::Untracked | CreateMode::MalformedPending => {
                    Err(resumed.discarded(Self::create_mode_error(id, token.mode)))
                }
                CreateMode::Undetermined | CreateMode::Created => {
                    let observation = Observation::Read(ReadOutcome::NotFound { id: id.clone() });
                    Err(match canonical_identifier {
                        Some(identifier) => resumed
                            .canonicalize(identifier)
                            .failed(observation, Self::create_mode_error(id, token.mode)),
                        None => {
                            resumed.failed(observation, Self::create_mode_error(id, token.mode))
                        }
                    })
                }
            },
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
            return Ok(UpdateOutcome::Pending(Pending {
                token: UpdateToken {
                    remaining: pending_polls - 1,
                    polls_served: 0,
                },
                poll_after_secs: 1,
            }));
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
    ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
        if pending.token.remaining > 0 {
            return Ok(UpdateOutcome::Pending(Pending {
                token: UpdateToken {
                    remaining: pending.token.remaining - 1,
                    polls_served: pending.token.polls_served + 1,
                },
                poll_after_secs: 1,
            }));
        }
        Ok(UpdateOutcome::Done(self.update_done(
            id,
            identifier,
            request,
            Some(pending.token.polls_served + 1),
        )))
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: DeleteRequest,
    ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
        let pending_polls = self
            .states
            .lock()
            .unwrap()
            .get(&Self::resource_key(id))
            .map(|attributes| Self::pending_polls(attributes, "delete_pending_polls"))
            .unwrap_or(0);
        if pending_polls > 0 {
            return Ok(DeleteOutcome::Pending(Pending {
                token: DeleteToken {
                    remaining: pending_polls - 1,
                },
                poll_after_secs: 1,
            }));
        }
        self.delete_done(id);
        Ok(DeleteOutcome::Deleted)
    }

    fn poll_delete(
        &self,
        id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: DeleteRequest,
        pending: Pending<Self::DeleteToken>,
    ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
        if pending.token.remaining > 0 {
            return Ok(DeleteOutcome::Pending(Pending {
                token: DeleteToken {
                    remaining: pending.token.remaining - 1,
                },
                poll_after_secs: 1,
            }));
        }
        self.delete_done(id);
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
