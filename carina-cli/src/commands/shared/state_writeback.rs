//! State write-back helpers shared between `apply` and `destroy`.
//!
//! These helpers translate the in-memory execution result into a persisted
//! `StateFile`: applying name overrides, building the post-apply state,
//! resolving exports, and (for destroy) removing destroyed resources.

use std::collections::{HashMap, HashSet};

use carina_core::effect::Effect;
use carina_core::executor::ExecutionResult;
use carina_core::plan::Plan;
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;
use carina_state::{LockInfo, ResourceState, StateBackend, StateFile};

use crate::error::AppError;

/// Apply permanent name overrides from state to desired resources.
///
/// When a create_before_destroy replacement produces a non-renameable temporary name
/// (can_rename=false), the state stores the permanent name. This function applies
/// those overrides so the plan doesn't detect a false diff.
pub(crate) fn apply_name_overrides(resources: &mut [Resource], state_file: &Option<StateFile>) {
    let state_file = match state_file {
        Some(sf) => sf,
        None => return,
    };

    let overrides = state_file.build_name_overrides();
    if overrides.is_empty() {
        return;
    }

    for resource in resources.iter_mut() {
        if let Some(name_overrides) = overrides.get(&resource.id) {
            for (attr, value) in name_overrides {
                resource
                    .attributes
                    .insert(attr.clone(), Value::String(value.clone()));
            }
        }
    }
}

/// Queue a state refresh for a resource after a failed operation.
///
/// This is kept for use by tests in `tests.rs`. The core executor has its own
/// internal version.
#[cfg(test)]
pub(crate) fn queue_state_refresh(
    pending_refreshes: &mut HashMap<ResourceId, String>,
    id: &ResourceId,
    identifier: Option<&str>,
) {
    if let Some(identifier) = identifier.filter(|identifier| !identifier.is_empty()) {
        pending_refreshes.insert(id.clone(), identifier.to_string());
    }
}

/// Input parameters for `finalize_apply`.
///
/// Groups the execution result, resource data, and backend configuration
/// needed to save state after an apply operation.
pub(crate) struct FinalizeApplyInput<'a> {
    pub result: &'a ExecutionResult,
    pub state_file: Option<StateFile>,
    pub sorted_resources: &'a [Resource],
    pub current_states: &'a HashMap<ResourceId, State>,
    pub plan: &'a Plan,
    pub backend: &'a dyn StateBackend,
    pub lock: Option<&'a LockInfo>,
    pub schemas: &'a HashMap<String, ResourceSchema>,
    pub export_params: &'a [carina_core::parser::ExportParameter],
}

/// Resolve export expressions using bindings built from applied state.
pub(crate) fn resolve_exports(
    export_params: &[carina_core::parser::ExportParameter],
    state: &StateFile,
) -> HashMap<String, serde_json::Value> {
    use carina_core::binding_index::{BindingValueSource, ResolvedBindings};
    use carina_core::resource::Value;

    let mut bindings = ResolvedBindings::default();
    for rs in &state.resources {
        if let Some(ref binding) = rs.binding {
            let attrs: HashMap<String, Value> = rs
                .attributes
                .iter()
                .filter_map(|(k, v)| {
                    carina_core::value::json_to_dsl_value(v).map(|val| (k.clone(), val))
                })
                .collect();
            bindings.set(binding, attrs, BindingValueSource::Local);
        }
    }

    let mut exports = HashMap::new();
    for param in export_params {
        if let Some(ref value) = param.value {
            // Resolve both ResourceRef and cross-file dot-notation strings
            // (e.g., "registry_prod.account_id" parsed from a different .crn file).
            let resolved = crate::commands::plan::resolve_export_value(value, &bindings);
            if let Some(json) = dsl_value_to_json(&resolved) {
                exports.insert(param.name.clone(), json);
            }
        }
    }
    exports
}

/// Convert a DSL Value to a serde_json::Value for state persistence.
pub(crate) fn dsl_value_to_json(value: &carina_core::resource::Value) -> Option<serde_json::Value> {
    use carina_core::resource::Value;
    match value {
        Value::String(s) => Some(serde_json::Value::String(s.clone())),
        Value::Bool(b) => Some(serde_json::Value::Bool(*b)),
        Value::Int(i) => Some(serde_json::Value::Number((*i).into())),
        Value::Float(f) => serde_json::Number::from_f64(*f).map(serde_json::Value::Number),
        Value::List(items) => {
            let json_items: Vec<serde_json::Value> =
                items.iter().filter_map(dsl_value_to_json).collect();
            Some(serde_json::Value::Array(json_items))
        }
        Value::Map(map) => {
            let json_map: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .filter_map(|(k, v)| dsl_value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect();
            Some(serde_json::Value::Object(json_map))
        }
        _ => None, // ResourceRef, Null, etc. — skip
    }
}

pub(crate) struct ApplyStateSave<'a> {
    pub state_file: Option<StateFile>,
    pub sorted_resources: &'a [Resource],
    pub current_states: &'a HashMap<ResourceId, State>,
    pub applied_states: &'a HashMap<ResourceId, State>,
    pub permanent_name_overrides: &'a HashMap<ResourceId, HashMap<String, String>>,
    pub plan: &'a Plan,
    pub successfully_deleted: &'a HashSet<ResourceId>,
    pub failed_refreshes: &'a HashSet<ResourceId>,
    pub schemas: &'a HashMap<String, ResourceSchema>,
}

pub(crate) fn build_state_after_apply(save: ApplyStateSave<'_>) -> Result<StateFile, AppError> {
    let ApplyStateSave {
        state_file,
        sorted_resources,
        current_states,
        applied_states,
        permanent_name_overrides,
        plan,
        successfully_deleted,
        failed_refreshes,
        schemas,
    } = save;
    let mut state = state_file.unwrap_or_default();

    for resource in sorted_resources {
        let existing = state.find_resource(
            &resource.id.provider,
            &resource.id.resource_type,
            resource.id.name_str(),
        );
        // Collect write-only attribute names from the schema for this resource type.
        // Schema keys include the provider prefix (e.g., "awscc.ec2.Vpc"), so we must
        // construct the key the same way as schema_key_for_resource().
        let schema_key = if resource.id.provider.is_empty() {
            resource.id.resource_type.clone()
        } else {
            format!("{}.{}", resource.id.provider, resource.id.resource_type)
        };
        let write_only_keys: Vec<String> = schemas
            .get(&schema_key)
            .map(|schema| {
                schema
                    .attributes
                    .iter()
                    .filter(|(_, attr)| attr.write_only)
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();

        if let Some(applied_state) = applied_states.get(&resource.id) {
            let mut resource_state =
                ResourceState::from_provider_state(resource, applied_state, existing)?;
            if let Some(overrides) = permanent_name_overrides.get(&resource.id) {
                resource_state.name_overrides = overrides.clone();
            }
            if !write_only_keys.is_empty() {
                resource_state.merge_write_only_attributes(resource, &write_only_keys);
            }
            state.upsert_resource(resource_state);
        } else if failed_refreshes.contains(&resource.id) {
            continue;
        } else if let Some(current_state) = current_states.get(&resource.id) {
            if current_state.exists {
                let mut resource_state =
                    ResourceState::from_provider_state(resource, current_state, existing)?;
                if !write_only_keys.is_empty() {
                    resource_state.merge_write_only_attributes(resource, &write_only_keys);
                }
                state.upsert_resource(resource_state);
            } else {
                state.remove_resource(
                    &resource.id.provider,
                    &resource.id.resource_type,
                    resource.id.name_str(),
                );
            }
        }
    }

    for effect in plan.effects() {
        match effect {
            Effect::Delete { id, .. } if successfully_deleted.contains(id) => {
                state.remove_resource(&id.provider, &id.resource_type, id.name_str());
            }
            Effect::Import { .. } => {
                // Already handled in the sorted_resources loop above via applied_states.
                // Re-upserting here would overwrite metadata (lifecycle, prefixes,
                // desired_keys, binding, dependency_bindings) with bare defaults.
            }
            Effect::Remove { id } => {
                state.remove_resource(&id.provider, &id.resource_type, id.name_str());
            }
            Effect::Move { from, to } => {
                // Move: update the resource's identity in state
                if let Some(existing) = state
                    .find_resource(&from.provider, &from.resource_type, from.name_str())
                    .cloned()
                {
                    state.remove_resource(&from.provider, &from.resource_type, from.name_str());
                    let mut moved_resource = existing;
                    moved_resource.provider = to.provider.clone();
                    moved_resource.resource_type = to.resource_type.clone();
                    moved_resource.name = to.name_str().to_string();
                    state.upsert_resource(moved_resource);
                }
            }
            _ => {}
        }
    }

    Ok(state)
}

/// Apply destroy results to the state file: remove destroyed resources and
/// clear any exports (since exports reference attributes of destroyed resources).
pub(crate) fn apply_destroy_to_state(
    state: &mut carina_state::StateFile,
    destroyed_ids: &[ResourceId],
) {
    for id in destroyed_ids {
        state.remove_resource(&id.provider, &id.resource_type, id.name_str());
    }
    state.exports.clear();
}

/// Build a minimal `Resource` for an orphaned resource from the state file.
///
/// This creates a Resource with attributes reconstructed from state data,
/// including `_binding` and `_dependency_bindings` so that dependency ordering
/// and tree display work correctly.
pub(crate) fn build_orphan_resource(sf: &carina_state::StateFile, id: &ResourceId) -> Resource {
    let rs = sf
        .find_resource(&id.provider, &id.resource_type, id.name_str())
        .expect("orphan must exist in state file");
    let attributes: HashMap<String, Value> = rs
        .attributes
        .iter()
        .filter_map(|(k, v)| carina_core::value::json_to_dsl_value(v).map(|val| (k.clone(), val)))
        .collect();
    Resource {
        id: id.clone(),
        attributes: attributes.into_iter().collect(),
        kind: carina_core::resource::ResourceKind::Managed,
        lifecycle: rs.lifecycle.clone(),
        prefixes: rs.prefixes.clone(),
        binding: rs.binding.clone(),
        dependency_bindings: rs.dependency_bindings.clone(),
        module_source: None,
        quoted_string_attrs: std::collections::HashSet::new(),
    }
}
