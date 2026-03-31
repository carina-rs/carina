use std::collections::HashMap;

use carina_plugin_host::convert;
use carina_plugin_sdk::CarinaProvider;
use carina_provider_protocol::types as proto;

use carina_core::provider::{
    Provider, ProviderError as CoreProviderError, ProviderNormalizer, SavedAttrs,
};
use carina_core::resource::{ResourceId as CoreResourceId, State as CoreState, Value as CoreValue};

use carina_provider_awscc::AwsccNormalizer;
use carina_provider_awscc::provider::AwsccProvider;
use carina_provider_awscc::schemas;

struct AwsccProcessProvider {
    runtime: tokio::runtime::Runtime,
    provider: Option<AwsccProvider>,
    normalizer: AwsccNormalizer,
}

impl AwsccProcessProvider {
    fn new() -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        Self {
            runtime,
            provider: None,
            normalizer: AwsccNormalizer,
        }
    }

    fn convert_error(e: CoreProviderError) -> proto::ProviderError {
        proto::ProviderError {
            message: e.to_string(),
            resource_id: e
                .resource_id
                .as_ref()
                .map(convert::core_to_proto_resource_id),
            is_timeout: e.is_timeout,
        }
    }

    fn provider(&self) -> &AwsccProvider {
        self.provider
            .as_ref()
            .expect("Provider not initialized; call initialize() first")
    }
}

impl CarinaProvider for AwsccProcessProvider {
    fn info(&self) -> proto::ProviderInfo {
        proto::ProviderInfo {
            name: "awscc".into(),
            display_name: "AWS Cloud Control provider".into(),
        }
    }

    fn schemas(&self) -> Vec<proto::ResourceSchema> {
        schemas::all_schemas()
            .iter()
            .map(convert::core_to_proto_schema)
            .collect()
    }

    fn validate_config(&self, attrs: &HashMap<String, proto::Value>) -> Result<(), String> {
        let core_attrs = convert::proto_to_core_value_map(attrs);
        let region_type = schemas::awscc_types::awscc_region();
        if let Some(region_value) = core_attrs.get("region") {
            region_type
                .validate(region_value)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn initialize(&mut self, attrs: &HashMap<String, proto::Value>) -> Result<(), String> {
        let core_attrs = convert::proto_to_core_value_map(attrs);
        let region = if let Some(CoreValue::String(region)) = core_attrs.get("region") {
            carina_core::utils::convert_region_value(region)
        } else {
            "ap-northeast-1".to_string()
        };
        let provider = self.runtime.block_on(AwsccProvider::new(&region));
        self.provider = Some(provider);
        Ok(())
    }

    fn read(
        &self,
        id: &proto::ResourceId,
        identifier: Option<&str>,
    ) -> Result<proto::State, proto::ProviderError> {
        let core_id = convert::proto_to_core_resource_id(id);
        let result = self
            .runtime
            .block_on(self.provider().read(&core_id, identifier));
        match result {
            Ok(state) => Ok(convert::core_to_proto_state(&state)),
            Err(e) => Err(Self::convert_error(e)),
        }
    }

    fn create(&self, resource: &proto::Resource) -> Result<proto::State, proto::ProviderError> {
        let core_resource = convert::proto_to_core_resource(resource);
        let result = self
            .runtime
            .block_on(self.provider().create(&core_resource));
        match result {
            Ok(state) => Ok(convert::core_to_proto_state(&state)),
            Err(e) => Err(Self::convert_error(e)),
        }
    }

    fn update(
        &self,
        id: &proto::ResourceId,
        identifier: &str,
        from: &proto::State,
        to: &proto::Resource,
    ) -> Result<proto::State, proto::ProviderError> {
        let core_id = convert::proto_to_core_resource_id(id);
        let core_from = convert::proto_to_core_state(from);
        let core_to = convert::proto_to_core_resource(to);
        let result = self.runtime.block_on(
            self.provider()
                .update(&core_id, identifier, &core_from, &core_to),
        );
        match result {
            Ok(state) => Ok(convert::core_to_proto_state(&state)),
            Err(e) => Err(Self::convert_error(e)),
        }
    }

    fn delete(
        &self,
        id: &proto::ResourceId,
        identifier: &str,
        lifecycle: &proto::LifecycleConfig,
    ) -> Result<(), proto::ProviderError> {
        let core_id = convert::proto_to_core_resource_id(id);
        let core_lifecycle = carina_core::resource::LifecycleConfig {
            force_delete: lifecycle.force_delete,
            create_before_destroy: lifecycle.create_before_destroy,
        };
        let result = self.runtime.block_on(self.provider().delete(
            &core_id,
            identifier,
            &core_lifecycle,
        ));
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(Self::convert_error(e)),
        }
    }

    fn normalize_desired(&self, resources: Vec<proto::Resource>) -> Vec<proto::Resource> {
        let mut core_resources: Vec<_> = resources
            .iter()
            .map(convert::proto_to_core_resource)
            .collect();
        self.normalizer.normalize_desired(&mut core_resources);
        core_resources
            .iter()
            .map(convert::core_to_proto_resource)
            .collect()
    }

    fn normalize_state(
        &self,
        states: HashMap<String, proto::State>,
    ) -> HashMap<String, proto::State> {
        let mut core_states: HashMap<CoreResourceId, CoreState> = states
            .values()
            .map(|s| {
                let core_state = convert::proto_to_core_state(s);
                (core_state.id.clone(), core_state)
            })
            .collect();
        self.normalizer.normalize_state(&mut core_states);
        core_states
            .iter()
            .map(|(id, state)| (id.to_string(), convert::core_to_proto_state(state)))
            .collect()
    }

    fn hydrate_read_state(
        &self,
        states: &mut HashMap<String, proto::State>,
        saved_attrs: &HashMap<String, HashMap<String, proto::Value>>,
    ) {
        // Build a key-to-CoreResourceId lookup from states (which contain structured IDs)
        let key_to_id: HashMap<&str, CoreResourceId> = states
            .iter()
            .map(|(k, s)| (k.as_str(), convert::proto_to_core_resource_id(&s.id)))
            .collect();

        let mut core_states: HashMap<CoreResourceId, CoreState> = states
            .values()
            .map(|s| {
                let core_state = convert::proto_to_core_state(s);
                (core_state.id.clone(), core_state)
            })
            .collect();
        let core_saved: SavedAttrs = saved_attrs
            .iter()
            .filter_map(|(k, v)| {
                let id = key_to_id.get(k.as_str())?.clone();
                let attrs = convert::proto_to_core_value_map(v);
                Some((id, attrs))
            })
            .collect();
        self.normalizer
            .hydrate_read_state(&mut core_states, &core_saved);
        *states = core_states
            .iter()
            .map(|(id, state)| (id.to_string(), convert::core_to_proto_state(state)))
            .collect();
    }

    fn merge_default_tags(
        &self,
        resources: &mut Vec<proto::Resource>,
        default_tags: &HashMap<String, proto::Value>,
        proto_schemas: &Vec<proto::ResourceSchema>,
    ) {
        let mut core_resources: Vec<_> = resources
            .iter()
            .map(convert::proto_to_core_resource)
            .collect();
        let core_tags = convert::proto_to_core_value_map(default_tags);
        let core_schemas: HashMap<String, _> = proto_schemas
            .iter()
            .map(|s| (s.resource_type.clone(), convert::proto_to_core_schema(s)))
            .collect();
        self.normalizer
            .merge_default_tags(&mut core_resources, &core_tags, &core_schemas);
        *resources = core_resources
            .iter()
            .map(convert::core_to_proto_resource)
            .collect();
    }
}

fn main() {
    carina_plugin_sdk::run(AwsccProcessProvider::new());
}
