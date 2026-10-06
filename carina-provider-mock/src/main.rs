use carina_core::resource::{
    ResourceIdentity as CoreResourceIdentity, ResourceIdentityError as CoreResourceIdentityError,
};
use carina_plugin_sdk::CarinaProvider;
use carina_plugin_sdk::types::*;
use std::collections::HashMap;
use std::sync::Mutex;

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
}

impl CarinaProvider for MockProcessProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "mock".into(),
            display_name: "Mock Provider (Process)".into(),
            capabilities: vec![],
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![]
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attrs: &HashMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> Result<State, ProviderError> {
        if id.identity == "__mock_non_finite_read__" {
            return Ok(State {
                id: id.clone(),
                identifier: Some("mock-id".into()),
                attributes: HashMap::from([(
                    "values".to_string(),
                    Value::List(vec![Value::Float(f64::NAN)]),
                )]),
                exists: true,
            });
        }

        let states = self.states.lock().unwrap();
        let key = Self::resource_key(id);

        if let Some(attrs) = states.get(&key) {
            Ok(State {
                id: id.clone(),
                identifier: Some("mock-id".into()),
                attributes: attrs.clone(),
                exists: true,
            })
        } else {
            Ok(State {
                id: id.clone(),
                identifier: None,
                attributes: HashMap::new(),
                exists: false,
            })
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
    ) -> Result<CreateOutcome, ProviderError> {
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

        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(id);
        let resource = request.resource;
        states.insert(key, resource.attributes.clone());

        let state = State {
            id: id.clone(),
            identifier: Some("mock-id".into()),
            attributes: resource.attributes,
            exists: true,
        };
        Ok(CreateOutcome::Success { state })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        request: UpdateRequest,
    ) -> Result<UpdateOutcome, ProviderError> {
        // Apply the patch on top of `from` to construct the post-update
        // attribute map. Also echo the patch op kinds into a sentinel
        // attribute so integration tests can assert the patch
        // round-tripped through the WIT boundary.
        let mut attributes = request.from.attributes.clone();
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
            match op.kind {
                PatchOpKind::Add | PatchOpKind::Replace => {
                    if let Some(value) = op.value.clone() {
                        attributes.insert(op.key.clone(), value);
                    }
                }
                PatchOpKind::Remove => {
                    attributes.remove(&op.key);
                }
            }
        }
        attributes.insert(
            "__mock_patch_ops__".to_string(),
            Value::List(applied_op_kinds),
        );

        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(id);
        states.insert(key, attributes.clone());

        let state = State {
            id: id.clone(),
            identifier: Some("mock-id".into()),
            attributes,
            exists: true,
        };
        Ok(UpdateOutcome::Success { state })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> Result<(), ProviderError> {
        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(id);
        states.remove(&key);
        Ok(())
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

    fn normalize_state(
        &self,
        states: HashMap<String, State>,
    ) -> Result<HashMap<String, State>, ProviderError> {
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
        if special_id
            .as_ref()
            .is_some_and(|id| id.identity == "__mock_normalize_state_error__")
        {
            return Err(ProviderError {
                kind: ProviderErrorKind::Internal,
                message: "intentional mock normalize_state error".to_string(),
                resource_id: special_id,
                cause: None,
                provider_name: None,
                operation: None,
                status: None,
                code: None,
                request_id: None,
            });
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
        states: &mut HashMap<String, State>,
        saved_attrs: &HashMap<String, HashMap<String, Value>>,
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

    /// Echo the host-provided `default_tags` back into each resource's
    /// attributes under a sentinel `__mock_merged_default_tags__` key so
    /// integration tests can verify the WIT bridge dispatched the call.
    /// Real providers would call `merge_default_tags_for_provider` here;
    /// the mock provider's job is just to prove the call landed.
    fn merge_default_tags(
        &self,
        resources: &mut Vec<Resource>,
        default_tags: &HashMap<String, Value>,
        _schemas: &Vec<ResourceSchema>,
    ) -> Result<(), ProviderError> {
        let snapshot: Vec<Value> = default_tags
            .iter()
            .map(|(k, v)| {
                Value::Map(HashMap::from([
                    ("k".to_string(), Value::String(k.clone())),
                    ("v".to_string(), v.clone()),
                ]))
            })
            .collect();
        for r in resources.iter_mut() {
            r.attributes.insert(
                "__mock_merged_default_tags__".to_string(),
                Value::List(snapshot.clone()),
            );
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
