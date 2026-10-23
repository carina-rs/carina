use carina_core::resource::{
    ResourceIdentity as CoreResourceIdentity, ResourceIdentityError as CoreResourceIdentityError,
};
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{CarinaProvider, CreateOutcome, DeleteOutcome, Pending, UpdateOutcome};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Serialize, Deserialize)]
struct CreateToken {
    remaining: u32,
    polls_served: u32,
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
    states: Mutex<HashMap<String, HashMap<String, Value>>>,
}

impl Default for MockProcessProvider {
    fn default() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
        }
    }
}

impl MockProcessProvider {
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

    fn normalizer_error(operation: &str, id: &ResourceId) -> ProviderError {
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

    fn pending_polls(attributes: &HashMap<String, Value>, operation_key: &str) -> u32 {
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

    fn update_pending_polls(request: &UpdateRequest) -> u32 {
        request
            .patch
            .ops
            .iter()
            .find(|op| op.key == "update_pending_polls")
            .or_else(|| {
                request
                    .patch
                    .ops
                    .iter()
                    .find(|op| op.key == "pending_polls")
            })
            .and_then(|op| op.value.as_ref())
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
    ) -> carina_provider_protocol::CreateOutcome {
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

        let state = AppliedState {
            id: id.clone(),
            identifier: AppliedIdentifier::new("mock-id")
                .expect("the mock identifier is non-empty"),
            attributes: resource.attributes,
        };
        carina_provider_protocol::CreateOutcome::Success { state }
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
            applied_op_kinds.push(Value::String(format!(
                "{}:{}",
                match op.kind {
                    PatchOpKind::Add => "add",
                    PatchOpKind::Replace => "replace",
                    PatchOpKind::Remove => "remove",
                },
                op.key,
            )));
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
        // only the user's patch. Never rebuild it from persisted `from` state:
        // that would either send redacted digests to the cloud or discard an
        // entire nested attribute containing an unchanged stored secret.
        for op in &request.patch.ops {
            match op.kind {
                PatchOpKind::Add | PatchOpKind::Replace => {
                    if let Some(value) = &op.value {
                        cloud_attributes.insert(op.key.clone(), value.clone());
                    } else {
                        cloud_attributes.remove(&op.key);
                    }
                }
                PatchOpKind::Remove => {
                    cloud_attributes.remove(&op.key);
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

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![ResourceSchema {
            resource_type: "test.short_budget".to_string(),
            attributes: HashMap::new(),
            description: Some("Mock resource with short operation budgets".to_string()),
            kind: SchemaKind::Managed,
            unique_name: UniqueNameSpec::default(),
            operation_config: Some(OperationConfig {
                create_timeout_secs: Some(2),
                update_timeout_secs: Some(2),
                delete_timeout_secs: Some(2),
                create_max_retries: None,
                delete_max_retries: None,
            }),
            validators: Vec::new(),
            exclusive_required: Vec::new(),
            defs: std::collections::BTreeMap::new(),
        }]
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attrs: &HashMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn initialize(&mut self, attrs: &HashMap<String, Value>) -> Result<(), String> {
        if attrs.contains_key("__mock_initialize_error__") {
            return Err("intentional mock initialization failure".to_string());
        }
        Ok(())
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> Result<ReadOutcome, ProviderError> {
        if id.identity == "__mock_non_finite_read__" {
            return Ok(ReadOutcome::Found {
                state: AppliedState {
                    id: id.clone(),
                    identifier: identifier.clone(),
                    attributes: HashMap::from([(
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
    ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError> {
        if id.identity == "__mock_create_trap__" {
            panic!("intentional mock create trap");
        }
        if id.identity == "__mock_create_error__" {
            return Err(ProviderError {
                kind: ProviderErrorKind::Internal,
                message: "intentional mock create error".to_string(),
                resource_id: Some(id.clone()),
                cause: None,
                provider_name: None,
                operation: None,
                status: None,
                code: None,
                request_id: None,
            });
        }

        if let Some(Value::Int(delay_millis)) = request.resource.attributes.get("call_delay_millis")
            && let Ok(delay_millis) = u64::try_from(*delay_millis)
        {
            std::thread::sleep(std::time::Duration::from_millis(delay_millis));
        }
        let pending_polls =
            Self::pending_polls(&request.resource.attributes, "create_pending_polls");
        if pending_polls > 0 {
            return Ok(CreateOutcome::Pending(Pending {
                token: CreateToken {
                    remaining: pending_polls - 1,
                    polls_served: 0,
                },
                poll_after_secs: 1,
            }));
        }

        Ok(CreateOutcome::Done(self.create_done(id, request, None)))
    }

    fn poll_create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        pending: Pending<Self::CreateToken>,
    ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError> {
        if pending.token.remaining > 0 {
            return Ok(CreateOutcome::Pending(Pending {
                token: CreateToken {
                    remaining: pending.token.remaining - 1,
                    polls_served: pending.token.polls_served + 1,
                },
                poll_after_secs: 1,
            }));
        }
        Ok(CreateOutcome::Done(self.create_done(
            id,
            request,
            Some(pending.token.polls_served + 1),
        )))
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

    fn normalize_desired(&self, resources: Vec<Resource>) -> Result<Vec<Resource>, ProviderError> {
        if let Some(resource) = resources
            .iter()
            .find(|resource| resource.id.identity == "__mock_normalize_desired_error__")
        {
            return Err(Self::normalizer_error("normalize_desired", &resource.id));
        }
        Ok(resources)
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
            return Err(Self::normalizer_error("normalize_state", id));
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
        saved_attrs: &HashMap<String, HashMap<String, PersistedValue>>,
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
            return Err(Self::normalizer_error("hydrate_read_state", id));
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
    carina_plugin_sdk::run(MockProcessProvider::default());
}

// For WASM: export_provider! macro bridges CarinaProvider to the WIT interface.
// An empty main() is still required for the binary target.
#[cfg(target_arch = "wasm32")]
carina_plugin_sdk::export_provider!(MockProcessProvider);

#[cfg(target_arch = "wasm32")]
fn main() {}
