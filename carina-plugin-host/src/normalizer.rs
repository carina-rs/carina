//! ProcessProviderNormalizer forwards normalizer calls to the provider process via JSON-RPC.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

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

    fn lock_process(&self) -> Result<MutexGuard<'_, ProviderProcess>, ()> {
        self.process.lock().map_err(|e| {
            log::error!("Process lock poisoned: {e}");
        })
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

        let Ok(mut process) = self.lock_process() else {
            return;
        };

        match process.call::<_, methods::NormalizeDesiredResult>("normalize_desired", &params) {
            Ok(result) => {
                for (core_res, proto_res) in resources.iter_mut().zip(result.resources.iter()) {
                    let resolved = convert::proto_to_core_value_map(&proto_res.attributes);
                    for (key, value) in resolved {
                        // Only update attributes that were literal values.
                        // Non-literal expressions (ResourceRef, Interpolation, etc.)
                        // get corrupted during proto conversion and must be preserved.
                        if let Some(Expr(v)) = core_res.attributes.get(&key)
                            && matches!(
                                v,
                                Value::ResourceRef { .. }
                                    | Value::Interpolation(_)
                                    | Value::FunctionCall { .. }
                                    | Value::Closure { .. }
                                    | Value::Secret(_)
                            )
                        {
                            continue;
                        }
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
            .map(|(id, state)| (id.to_string(), convert::core_to_proto_state(state)))
            .collect();

        let params = methods::NormalizeStateParams {
            states: proto_states,
        };

        let Ok(mut process) = self.lock_process() else {
            return;
        };

        match process.call::<_, methods::NormalizeStateResult>("normalize_state", &params) {
            Ok(result) => {
                for state in current_states.values_mut() {
                    let key = state.id.to_string();
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
            .map(|(id, state)| (id.to_string(), convert::core_to_proto_state(state)))
            .collect();

        let proto_saved: HashMap<String, HashMap<String, _>> = saved_attrs
            .iter()
            .map(|(id, attrs)| (id.to_string(), convert::core_to_proto_value_map(attrs)))
            .collect();

        let params = methods::HydrateReadStateParams {
            states: proto_states,
            saved_attrs: proto_saved,
        };

        let Ok(mut process) = self.lock_process() else {
            return;
        };

        match process.call::<_, methods::HydrateReadStateResult>("hydrate_read_state", &params) {
            Ok(result) => {
                for state in current_states.values_mut() {
                    let key = state.id.to_string();
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

        let Ok(mut process) = self.lock_process() else {
            return;
        };

        match process.call::<_, methods::MergeDefaultTagsResult>("merge_default_tags", &params) {
            Ok(result) => {
                for (core_res, proto_res) in resources.iter_mut().zip(result.resources.iter()) {
                    let resolved = convert::proto_to_core_value_map(&proto_res.attributes);
                    for (key, value) in resolved {
                        // Preserve non-literal expressions (same as normalize_desired)
                        if let Some(Expr(v)) = core_res.attributes.get(&key)
                            && matches!(
                                v,
                                Value::ResourceRef { .. }
                                    | Value::Interpolation(_)
                                    | Value::FunctionCall { .. }
                                    | Value::Closure { .. }
                                    | Value::Secret(_)
                            )
                        {
                            continue;
                        }
                        core_res.attributes.insert(key, Expr(value));
                    }
                }
            }
            Err(e) => log::error!("merge_default_tags RPC failed: {e}"),
        }
    }
}
