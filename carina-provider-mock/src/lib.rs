use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use carina_core::provider::{
    BoxFuture, CreateRequest, DeleteRequest, PatchOpKind, Provider, ProviderError, ProviderResult,
    ReadRequest, UpdateRequest,
};
use carina_core::resource::{DataSource, ResourceId, State, Value};
use carina_core::value::{json_to_dsl_value, value_to_json};

pub struct MockProvider {
    state_file: PathBuf,
}

impl MockProvider {
    pub fn new() -> Self {
        Self {
            state_file: PathBuf::from(".carina/state.json"),
        }
    }

    fn load_states(&self) -> HashMap<String, HashMap<String, serde_json::Value>> {
        if let Ok(content) = fs::read_to_string(&self.state_file) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        }
    }

    fn save_states(
        &self,
        states: &HashMap<String, HashMap<String, serde_json::Value>>,
    ) -> Result<(), std::io::Error> {
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = carina_core::utils::pretty_with_newline(states)?;
        fs::write(&self.state_file, content)
    }

    fn resource_key(id: &ResourceId) -> String {
        format!("{}.{}", id.resource_type, id.name)
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MockProvider {
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
        Box::pin(async move {
            let states = self.load_states();
            let key = Self::resource_key(&id);

            if let Some(attrs) = states.get(&key) {
                let attributes: HashMap<String, Value> = attrs
                    .iter()
                    .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                    .collect();
                Ok(State::existing(id, attributes).with_identifier("mock-id"))
            } else {
                Ok(State::not_found(id))
            }
        })
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        self.read(&resource.id, None, ReadRequest)
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let resource = request.resource;
        Box::pin(async move {
            let mut states = self.load_states();
            let key = Self::resource_key(&id);

            let attrs: HashMap<String, serde_json::Value> = resource
                .attributes
                .iter()
                .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect::<Result<_, _>>()
                .map_err(|e| ProviderError::internal(format!("Failed to convert value: {}", e)))?;

            states.insert(key, attrs);
            self.save_states(&states)
                .map_err(|e| ProviderError::internal("Failed to save state").with_cause(e))?;

            Ok(State::existing(id, resource.resolved_attributes()).with_identifier("mock-id"))
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move {
            // Apply the patch on top of `from` to construct the post-update
            // attribute map. The mock writes only what the user changed —
            // matching the Level 3 contract that providers MUST NOT touch
            // unspecified fields.
            let mut attributes = request.from.attributes.clone();
            for op in request.patch.ops {
                match op.kind {
                    PatchOpKind::Add | PatchOpKind::Replace => {
                        if let Some(value) = op.value {
                            attributes.insert(op.key, value);
                        }
                    }
                    PatchOpKind::Remove => {
                        attributes.remove(&op.key);
                    }
                }
            }

            let mut states = self.load_states();
            let key = Self::resource_key(&id);
            let attrs: HashMap<String, serde_json::Value> = attributes
                .iter()
                .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect::<Result<_, _>>()
                .map_err(|e| ProviderError::internal(format!("Failed to convert value: {}", e)))?;

            states.insert(key, attrs);
            self.save_states(&states)
                .map_err(|e| ProviderError::internal("Failed to save state").with_cause(e))?;

            Ok(State::existing(id, attributes))
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move {
            let mut states = self.load_states();
            let key = Self::resource_key(&id);

            states.remove(&key);
            self.save_states(&states)
                .map_err(|e| ProviderError::internal("Failed to save state").with_cause(e))?;

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the byte-level shape so MockProvider's state file matches
    // the trailing-newline convention used by carina.state.json
    // (#2721) and carina-backend.lock (#2583).
    #[test]
    fn state_file_ends_with_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = MockProvider {
            state_file: state_file.clone(),
        };
        let mut states = HashMap::new();
        let mut entry = HashMap::new();
        entry.insert("k".to_string(), serde_json::json!("v"));
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
}
