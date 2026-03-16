use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use carina_core::provider::{BoxFuture, Provider, ProviderError, ProviderResult, ResourceType};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
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
        let content = serde_json::to_string_pretty(states)?;
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
    fn name(&self) -> &'static str {
        "mock"
    }

    fn resource_types(&self) -> Vec<Box<dyn ResourceType>> {
        vec![]
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move {
            let states = self.load_states();
            let key = Self::resource_key(&id);

            if let Some(attrs) = states.get(&key) {
                let attributes: HashMap<String, Value> = attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), json_to_dsl_value(v)))
                    .collect();
                Ok(State::existing(id, attributes).with_identifier("mock-id"))
            } else {
                Ok(State::not_found(id))
            }
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        Box::pin(async move {
            let mut states = self.load_states();
            let key = Self::resource_key(&resource.id);

            let attrs: HashMap<String, serde_json::Value> = resource
                .attributes
                .iter()
                .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect::<Result<_, _>>()
                .map_err(|e| ProviderError::new(format!("Failed to convert value: {}", e)))?;

            states.insert(key, attrs);
            self.save_states(&states)
                .map_err(|e| ProviderError::new(format!("Failed to save state: {}", e)))?;

            Ok(
                State::existing(resource.id.clone(), resource.attributes.clone())
                    .with_identifier("mock-id"),
            )
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let to = to.clone();
        Box::pin(async move {
            let mut states = self.load_states();
            let key = Self::resource_key(&id);

            let attrs: HashMap<String, serde_json::Value> = to
                .attributes
                .iter()
                .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect::<Result<_, _>>()
                .map_err(|e| ProviderError::new(format!("Failed to convert value: {}", e)))?;

            states.insert(key, attrs);
            self.save_states(&states)
                .map_err(|e| ProviderError::new(format!("Failed to save state: {}", e)))?;

            Ok(State::existing(id, to.attributes.clone()))
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move {
            let mut states = self.load_states();
            let key = Self::resource_key(&id);

            states.remove(&key);
            self.save_states(&states)
                .map_err(|e| ProviderError::new(format!("Failed to save state: {}", e)))?;

            Ok(())
        })
    }
}
