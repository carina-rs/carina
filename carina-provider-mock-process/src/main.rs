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
        }
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![]
    }

    fn validate_config(&self, _attrs: &HashMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn read(&self, id: &ResourceId, _identifier: Option<&str>) -> Result<State, ProviderError> {
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

    fn create(&self, resource: &Resource) -> Result<State, ProviderError> {
        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(&resource.id);
        states.insert(key, resource.attributes.clone());

        Ok(State {
            id: resource.id.clone(),
            identifier: Some("mock-id".into()),
            attributes: resource.attributes.clone(),
            exists: true,
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _from: &State,
        to: &Resource,
    ) -> Result<State, ProviderError> {
        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(id);
        states.insert(key, to.attributes.clone());

        Ok(State {
            id: id.clone(),
            identifier: Some("mock-id".into()),
            attributes: to.attributes.clone(),
            exists: true,
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _lifecycle: &LifecycleConfig,
    ) -> Result<(), ProviderError> {
        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(id);
        states.remove(&key);
        Ok(())
    }
}

fn main() {
    carina_plugin_sdk::run(MockProcessProvider::default());
}
