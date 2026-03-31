//! ProcessProviderNormalizer forwards normalizer calls to the provider process via JSON-RPC.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use carina_core::provider::{ProviderNormalizer, SavedAttrs};
use carina_core::resource::{Expr, Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;
use carina_provider_protocol::methods;

use crate::convert;
use crate::process::ProviderProcess;

pub struct ProcessProviderNormalizer {
    process: Arc<Mutex<ProviderProcess>>,
}

impl ProcessProviderNormalizer {
    pub fn new(process: Arc<Mutex<ProviderProcess>>) -> Self {
        Self { process }
    }
}

impl ProviderNormalizer for ProcessProviderNormalizer {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        let proto_resources: Vec<_> = resources
            .iter()
            .map(convert::core_to_proto_resource)
            .collect();
        let params = methods::NormalizeDesiredParams {
            resources: proto_resources,
        };

        let mut process = match self.process.lock() {
            Ok(p) => p,
            Err(e) => {
                log::error!("Process lock poisoned in normalize_desired: {e}");
                return;
            }
        };

        match process.call::<_, methods::NormalizeDesiredResult>("normalize_desired", &params) {
            Ok(result) => {
                for (core_res, proto_res) in resources.iter_mut().zip(result.resources.iter()) {
                    let resolved = convert::proto_to_core_value_map(&proto_res.attributes);
                    for (key, value) in resolved {
                        core_res.attributes.insert(key, Expr(value));
                    }
                }
            }
            Err(e) => log::error!("normalize_desired RPC failed: {e}"),
        }
    }

    fn normalize_state(&self, current_states: &mut HashMap<ResourceId, State>) {
        let proto_states: HashMap<String, _> = current_states
            .iter()
            .map(|(id, state)| {
                let key = format!("{}.{}.{}", id.provider, id.resource_type, id.name);
                (key, convert::core_to_proto_state(state))
            })
            .collect();

        let params = methods::NormalizeStateParams {
            states: proto_states,
        };

        let mut process = match self.process.lock() {
            Ok(p) => p,
            Err(e) => {
                log::error!("Process lock poisoned in normalize_state: {e}");
                return;
            }
        };

        match process.call::<_, methods::NormalizeStateResult>("normalize_state", &params) {
            Ok(result) => {
                for state in current_states.values_mut() {
                    let key = format!(
                        "{}.{}.{}",
                        state.id.provider, state.id.resource_type, state.id.name
                    );
                    if let Some(proto_state) = result.states.get(&key) {
                        state.attributes =
                            convert::proto_to_core_value_map(&proto_state.attributes);
                    }
                }
            }
            Err(e) => log::error!("normalize_state RPC failed: {e}"),
        }
    }

    fn hydrate_read_state(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &SavedAttrs,
    ) {
        let proto_states: HashMap<String, _> = current_states
            .iter()
            .map(|(id, state)| {
                let key = format!("{}.{}.{}", id.provider, id.resource_type, id.name);
                (key, convert::core_to_proto_state(state))
            })
            .collect();

        let proto_saved: HashMap<String, HashMap<String, _>> = saved_attrs
            .iter()
            .map(|(id, attrs)| {
                let key = format!("{}.{}.{}", id.provider, id.resource_type, id.name);
                (key, convert::core_to_proto_value_map(attrs))
            })
            .collect();

        let params = methods::HydrateReadStateParams {
            states: proto_states,
            saved_attrs: proto_saved,
        };

        let mut process = match self.process.lock() {
            Ok(p) => p,
            Err(e) => {
                log::error!("Process lock poisoned in hydrate_read_state: {e}");
                return;
            }
        };

        match process.call::<_, methods::HydrateReadStateResult>("hydrate_read_state", &params) {
            Ok(result) => {
                for state in current_states.values_mut() {
                    let key = format!(
                        "{}.{}.{}",
                        state.id.provider, state.id.resource_type, state.id.name
                    );
                    if let Some(proto_state) = result.states.get(&key) {
                        state.attributes =
                            convert::proto_to_core_value_map(&proto_state.attributes);
                    }
                }
            }
            Err(e) => log::error!("hydrate_read_state RPC failed: {e}"),
        }
    }

    fn merge_default_tags(
        &self,
        resources: &mut [Resource],
        default_tags: &HashMap<String, Value>,
        schemas: &HashMap<String, ResourceSchema>,
    ) {
        let proto_resources: Vec<_> = resources
            .iter()
            .map(convert::core_to_proto_resource)
            .collect();
        let proto_tags = convert::core_to_proto_value_map(default_tags);
        let proto_schemas: Vec<_> = schemas
            .values()
            .map(convert::core_to_proto_schema)
            .collect();

        let params = methods::MergeDefaultTagsParams {
            resources: proto_resources,
            default_tags: proto_tags,
            schemas: proto_schemas,
        };

        let mut process = match self.process.lock() {
            Ok(p) => p,
            Err(e) => {
                log::error!("Process lock poisoned in merge_default_tags: {e}");
                return;
            }
        };

        match process.call::<_, methods::MergeDefaultTagsResult>("merge_default_tags", &params) {
            Ok(result) => {
                for (core_res, proto_res) in resources.iter_mut().zip(result.resources.iter()) {
                    let resolved = convert::proto_to_core_value_map(&proto_res.attributes);
                    for (key, value) in resolved {
                        core_res.attributes.insert(key, Expr(value));
                    }
                }
            }
            Err(e) => log::error!("merge_default_tags RPC failed: {e}"),
        }
    }
}
