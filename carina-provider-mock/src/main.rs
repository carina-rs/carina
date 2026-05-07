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
        format!("{}.{}", id.resource_type, id.name)
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
        _identifier: &str,
        _request: ReadRequest,
    ) -> Result<State, ProviderError> {
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

    fn create(&self, id: &ResourceId, request: CreateRequest) -> Result<State, ProviderError> {
        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(id);
        let resource = request.resource;
        states.insert(key, resource.attributes.clone());

        Ok(State {
            id: id.clone(),
            identifier: Some("mock-id".into()),
            attributes: resource.attributes,
            exists: true,
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        request: UpdateRequest,
    ) -> Result<State, ProviderError> {
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

        Ok(State {
            id: id.clone(),
            identifier: Some("mock-id".into()),
            attributes,
            exists: true,
        })
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
