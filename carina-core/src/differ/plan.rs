//! Plan generation from diffs and cascading update logic.

use std::collections::{HashMap, HashSet};

use crate::deps::get_resource_dependencies;
use crate::effect::{CascadingUpdate, Effect, TemporaryName};
use crate::identifier::generate_random_suffix;
use crate::plan::Plan;
use crate::resource::{Expr, LifecycleConfig, Resource, ResourceId, ResourceKind, State, Value};
use crate::schema::ResourceSchema;

use super::{Diff, diff};

/// A pending merge operation: cascade-triggered create-only attributes to add to an existing effect.
struct CascadeMerge {
    resource_id: ResourceId,
    create_only_attrs: Vec<String>,
    lifecycle: LifecycleConfig,
    ref_hints: Vec<(String, String)>,
}

/// Check which changed attributes are create-only according to the schema
fn find_changed_create_only(
    provider: &str,
    resource_type: &str,
    changed_attributes: &[String],
    schemas: &HashMap<String, ResourceSchema>,
) -> Vec<String> {
    let Some(schema) = find_schema(provider, resource_type, schemas) else {
        return Vec::new();
    };

    let create_only_attrs = schema.create_only_attributes();
    changed_attributes
        .iter()
        .filter(|attr| create_only_attrs.contains(&attr.as_str()))
        .cloned()
        .collect()
}

/// Filter out non-removable attribute removals from the changed list.
/// A "removal" is an attribute that appears in `changed_attributes` but not in `to`.
/// Only attributes marked as `removable` in the schema should be kept as removals.
/// Non-removal changes (attribute present in `to`) are always kept.
fn filter_non_removable_removals(
    provider: &str,
    resource_type: &str,
    to: &Resource,
    changed_attributes: Vec<String>,
    schemas: &HashMap<String, ResourceSchema>,
) -> Vec<String> {
    let Some(schema) = find_schema(provider, resource_type, schemas) else {
        // No schema available — keep all changes (conservative)
        return changed_attributes;
    };

    let removable_attrs = schema.removable_attributes();

    changed_attributes
        .into_iter()
        .filter(|attr| {
            // Keep if the attribute is still in desired (it's a change, not a removal)
            if to.attributes.contains_key(attr) {
                return true;
            }
            // It's a removal — only keep if the attribute is removable
            removable_attrs.contains(&attr.as_str())
        })
        .collect()
}

/// Look up the schema for a resource, trying both direct and provider-prefixed keys.
fn find_schema<'a>(
    provider: &str,
    resource_type: &str,
    schemas: &'a HashMap<String, ResourceSchema>,
) -> Option<&'a ResourceSchema> {
    schemas.get(resource_type).or_else(|| {
        if !provider.is_empty() {
            schemas.get(&format!("{}.{}", provider, resource_type))
        } else {
            None
        }
    })
}

/// Generate a temporary name for create-before-destroy replacement.
///
/// When a resource has a `name_attribute` with a unique constraint and uses
/// `create_before_destroy`, we need a temporary name for the new resource to
/// avoid conflicts with the old resource that still exists.
///
/// Returns `None` if no temporary name is needed (no name_attribute,
/// the resource already uses name_prefix for that attribute, or
/// the name_attribute value changed between `from` and `to`).
fn generate_temporary_name(
    resource: &Resource,
    from: &State,
    schema: &ResourceSchema,
) -> Option<TemporaryName> {
    let name_attr = schema.name_attribute.as_ref()?;

    // Skip if the resource uses name_prefix for this attribute
    if resource.prefixes.contains_key(name_attr) {
        return None;
    }

    // Get the current value of the name attribute
    let original_value = match resource.get_attr(name_attr) {
        Some(Value::String(s)) => s.clone(),
        _ => return None,
    };

    // Skip if the name_attribute value changed (new name is already different from old)
    if let Some(Value::String(from_name)) = from.attributes.get(name_attr)
        && *from_name != original_value
    {
        return None;
    }

    // Check if the name attribute is create-only (cannot be renamed after creation)
    let can_rename = schema
        .attributes
        .get(name_attr)
        .map(|attr| !attr.create_only)
        .unwrap_or(false);

    let temporary_value = format!("{}-{}", original_value, generate_random_suffix());

    Some(TemporaryName {
        attribute: name_attr.clone(),
        original_value,
        temporary_value,
        can_rename,
    })
}

/// Compute Diff for multiple resources and generate a Plan
///
/// The `lifecycles` map provides lifecycle configuration for orphaned resources
/// (resources in state but not in desired). For desired resources, the lifecycle
/// is read directly from the Resource struct.
///
/// The `saved_attrs` map provides the last-known attribute values from the state file.
/// This is used to merge unmanaged nested fields into desired values before comparison,
/// preventing false diffs when AWS returns extra fields not specified in the .crn file.
///
/// The `prev_desired_keys` map provides the attribute keys that the user explicitly
/// specified in their .crn file during the last apply. This is used to detect
/// attribute removal: if a key was previously in the user's desired state but is
/// now absent, it means the user intentionally removed it.
pub fn create_plan(
    desired: &[Resource],
    current_states: &HashMap<ResourceId, State>,
    lifecycles: &HashMap<ResourceId, LifecycleConfig>,
    schemas: &HashMap<String, ResourceSchema>,
    saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    prev_desired_keys: &HashMap<ResourceId, Vec<String>>,
    orphan_dependencies: &HashMap<ResourceId, Vec<String>>,
) -> Plan {
    let mut plan = Plan::new();

    let desired_ids: std::collections::HashSet<&ResourceId> =
        desired.iter().map(|r| &r.id).collect();

    for resource in desired {
        // Skip virtual resources (module attribute containers)
        if resource.is_virtual() {
            continue;
        }

        // Data sources (read-only resources) only generate Read effects
        if resource.is_data_source() {
            plan.add(Effect::Read {
                resource: resource.clone(),
            });
            continue;
        }

        let current = current_states
            .get(&resource.id)
            .cloned()
            .unwrap_or_else(|| State::not_found(resource.id.clone()));

        let saved = saved_attrs.get(&resource.id);
        let prev_keys = prev_desired_keys.get(&resource.id);
        let schema = find_schema(&resource.id.provider, &resource.id.resource_type, schemas);
        let d = diff(
            resource,
            &current,
            saved,
            prev_keys.map(|v| v.as_slice()),
            schema,
        );

        match d {
            Diff::Create(r) => plan.add(Effect::Create(r)),
            Diff::Update {
                id,
                from,
                to,
                changed_attributes,
            } => {
                // Filter out non-removable attribute removals.
                // A "removal" is an attribute in changed_attributes that is not in `to`.
                // Only attributes marked as `removable` in the schema should trigger removal.
                let changed_attributes = filter_non_removable_removals(
                    &resource.id.provider,
                    &resource.id.resource_type,
                    &to,
                    changed_attributes,
                    schemas,
                );

                if changed_attributes.is_empty() {
                    // All changes were spurious non-removable removals
                    continue;
                }

                // Check if any changed attributes are create-only
                let changed_create_only = find_changed_create_only(
                    &resource.id.provider,
                    &resource.id.resource_type,
                    &changed_attributes,
                    schemas,
                );

                // Check if the resource type forces replacement (no update support)
                let schema_force_replace =
                    find_schema(&resource.id.provider, &resource.id.resource_type, schemas)
                        .is_some_and(|s| s.force_replace);

                if changed_create_only.is_empty() && !schema_force_replace {
                    plan.add(Effect::Update {
                        id,
                        from,
                        to,
                        changed_attributes,
                    });
                } else {
                    let lifecycle = resource.lifecycle.clone();
                    let temporary_name = if lifecycle.create_before_destroy {
                        find_schema(&resource.id.provider, &resource.id.resource_type, schemas)
                            .and_then(|schema| generate_temporary_name(&to, &from, schema))
                    } else {
                        None
                    };

                    // If a temporary name is generated, modify the `to` resource
                    let to = if let Some(ref temp) = temporary_name {
                        let mut modified = to;
                        modified.set_attr(
                            temp.attribute.clone(),
                            Value::String(temp.temporary_value.clone()),
                        );
                        modified
                    } else {
                        to
                    };

                    plan.add(Effect::Replace {
                        id,
                        from,
                        to,
                        lifecycle,
                        changed_create_only,
                        cascading_updates: vec![],
                        temporary_name,
                        cascade_ref_hints: vec![],
                    });
                }
            }
            Diff::NoChange(_) => {}
            Diff::Delete(id) => {
                let identifier = current_states
                    .get(&id)
                    .and_then(|s| s.identifier.clone())
                    .unwrap_or_default();
                let lifecycle = resource.lifecycle.clone();
                let binding = resource.binding.clone();
                let dependencies = get_resource_dependencies(resource);
                plan.add(Effect::Delete {
                    id,
                    identifier,
                    lifecycle,
                    binding,
                    dependencies,
                });
            }
        }
    }

    // Detect orphaned resources: exist in current_states but not in desired
    for (id, state) in current_states {
        if state.exists && !desired_ids.contains(id) {
            let identifier = state.identifier.clone().unwrap_or_default();
            let lifecycle = lifecycles.get(id).cloned().unwrap_or_default();
            let binding = state.attributes.get("_binding").and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            });
            // Use stored dependency bindings from state file if available,
            // otherwise fall back to extracting from state attributes
            let dependencies = if let Some(dep_bindings) = orphan_dependencies.get(id) {
                dep_bindings.iter().cloned().collect()
            } else {
                let temp_resource = Resource {
                    id: id.clone(),
                    attributes: Expr::wrap_map(state.attributes.clone()),
                    kind: ResourceKind::Real,
                    lifecycle: lifecycle.clone(),
                    prefixes: HashMap::new(),
                    binding: None,
                    dependency_bindings: Vec::new(),
                    module_source: None,
                };
                get_resource_dependencies(&temp_resource)
            };
            plan.add(Effect::Delete {
                id: id.clone(),
                identifier,
                lifecycle,
                binding,
                dependencies,
            });
        }
    }

    plan
}

/// Populate cascading updates for Replace effects with create_before_destroy.
///
/// When a resource is replaced with create_before_destroy, dependent resources
/// that reference the replaced resource's computed attributes must be updated
/// between the create (new) and delete (old) steps. This function:
///
/// 1. Finds all Replace effects with create_before_destroy = true
/// 2. Identifies dependent resources that reference the replaced resource's binding
/// 3. If the referencing attribute is create-only on the dependent (per `schemas`),
///    promotes the dependent to its own Replace effect in the plan
/// 4. Otherwise, adds a CascadingUpdate entry to the parent Replace effect
///
/// `unresolved_resources` should be the resources BEFORE ref resolution (still containing
/// ResourceRef values). `current_states` provides the `from` state for each dependent.
/// `schemas` provides attribute metadata to detect create-only attributes.
pub fn cascade_dependent_updates(
    plan: &mut Plan,
    unresolved_resources: &[Resource],
    current_states: &HashMap<ResourceId, State>,
    schemas: &HashMap<String, ResourceSchema>,
) {
    // Build binding/key -> unresolved resource mapping.
    // Uses the same key logic as the dependent lookup below so anonymous resources
    // (without _binding) are also found.
    let mut binding_to_unresolved: HashMap<String, &Resource> = HashMap::new();
    for resource in unresolved_resources {
        let key = resource
            .binding
            .clone()
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name));
        binding_to_unresolved.insert(key, resource);
    }

    // Auto-detect create_before_destroy: if a Replace effect has dependents
    // among the unresolved resources, promote it to create_before_destroy.
    // This must happen before collecting replaced_bindings so that the
    // promoted effects are picked up by the existing cascade logic.
    {
        // Collect all Replace bindings (regardless of CBD flag) with their resource IDs
        let all_replace_bindings: HashMap<String, ResourceId> = plan
            .effects()
            .iter()
            .filter_map(|e| {
                if let Effect::Replace { id, lifecycle, .. } = e {
                    // Only consider Replace effects that are NOT already CBD
                    if !lifecycle.create_before_destroy {
                        e.binding_name().map(|b| (b, id.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        if !all_replace_bindings.is_empty() {
            for resource in unresolved_resources {
                let deps = get_resource_dependencies(resource);
                for dep in &deps {
                    if let Some(resource_id) = all_replace_bindings.get(dep) {
                        plan.promote_to_create_before_destroy(resource_id);
                    }
                }
            }
        }
    }

    // Build reverse dependency map: replaced_binding -> [dependent_bindings]
    let mut dependents_of_replaced: HashMap<String, Vec<String>> = HashMap::new();

    // Collect binding names of resources being replaced (now includes auto-promoted ones)
    let replaced_bindings: HashSet<String> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let Effect::Replace { .. } = e {
                return e.binding_name();
            }
            None
        })
        .collect();

    if replaced_bindings.is_empty() {
        return;
    }

    // Collect resource IDs that already have effects in the plan
    let planned_ids: HashSet<ResourceId> = plan
        .effects()
        .iter()
        .map(|e| e.resource_id().clone())
        .collect();

    // For each unresolved resource, check if it depends on a replaced binding.
    // Resources already in the plan are handled separately below.
    for resource in unresolved_resources {
        if planned_ids.contains(&resource.id) {
            continue;
        }

        let deps = get_resource_dependencies(resource);
        for dep in &deps {
            if replaced_bindings.contains(dep) {
                let binding = resource.binding.clone().unwrap_or_else(|| {
                    format!("{}:{}", resource.id.resource_type, resource.id.name)
                });
                dependents_of_replaced
                    .entry(dep.clone())
                    .or_default()
                    .push(binding);
            }
        }
    }

    // For resources already in the plan, check if cascade-triggered create-only
    // attributes need to be merged into their existing effects.
    let mut merge_operations: Vec<CascadeMerge> = Vec::new();

    for resource in unresolved_resources {
        if !planned_ids.contains(&resource.id) {
            continue;
        }

        let deps = get_resource_dependencies(resource);
        for dep in &deps {
            if !replaced_bindings.contains(dep) {
                continue;
            }

            // Find which attributes on this resource hold a ResourceRef
            // pointing to the replaced binding, and extract ref hints
            let ref_attrs: Vec<String> = resource
                .attributes
                .iter()
                .filter(|(_, v)| {
                    matches!(v.as_value(), Value::ResourceRef { path } if path.binding() == dep)
                })
                .map(|(k, _)| k.clone())
                .collect();

            let ref_hints: Vec<(String, String)> = resource
                .attributes
                .iter()
                .filter_map(|(k, v)| match v.as_value() {
                    Value::ResourceRef { path } if path.binding() == dep => Some((
                        k.clone(),
                        format!("{}.{}", path.binding(), path.attribute()),
                    )),
                    _ => None,
                })
                .collect();

            // Check if any of those attributes are create-only
            let create_only_refs = find_changed_create_only(
                &resource.id.provider,
                &resource.id.resource_type,
                &ref_attrs,
                schemas,
            );

            if !create_only_refs.is_empty() {
                // Only keep hints for attributes that are actually create-only
                let filtered_hints: Vec<(String, String)> = ref_hints
                    .into_iter()
                    .filter(|(attr, _)| create_only_refs.contains(attr))
                    .collect();
                merge_operations.push(CascadeMerge {
                    resource_id: resource.id.clone(),
                    create_only_attrs: create_only_refs,
                    lifecycle: resource.lifecycle.clone(),
                    ref_hints: filtered_hints,
                });
            }
        }
    }

    // Apply merge operations to existing effects
    for merge in merge_operations {
        plan.merge_cascade_create_only(
            &merge.resource_id,
            merge.create_only_attrs,
            merge.lifecycle,
            merge.ref_hints,
        );
    }

    // Build cascading updates for each Replace effect.
    // Dependents whose affected attributes are create-only get promoted to
    // their own Replace effect instead of being added as a CascadingUpdate.
    let mut updates_by_replaced_binding: HashMap<String, Vec<CascadingUpdate>> = HashMap::new();
    let mut promoted_replaces: Vec<Effect> = Vec::new();

    for (replaced_binding, dependent_bindings) in &dependents_of_replaced {
        for dep_binding in dependent_bindings {
            if let Some(unresolved) = binding_to_unresolved.get(dep_binding) {
                let from = current_states
                    .get(&unresolved.id)
                    .cloned()
                    .unwrap_or_else(|| State::not_found(unresolved.id.clone()));

                if !from.exists {
                    continue;
                }

                // Find which attributes on this dependent hold a ResourceRef
                // pointing to the replaced binding
                let ref_attrs: Vec<String> = unresolved
                    .attributes
                    .iter()
                    .filter(|(_, v)| {
                        matches!(v.as_value(), Value::ResourceRef { path } if path.binding() == replaced_binding)
                    })
                    .map(|(k, _)| k.clone())
                    .collect();

                // Check if any of those attributes are create-only
                let create_only_refs = find_changed_create_only(
                    &unresolved.id.provider,
                    &unresolved.id.resource_type,
                    &ref_attrs,
                    schemas,
                );

                if create_only_refs.is_empty() {
                    // Normal cascading update
                    updates_by_replaced_binding
                        .entry(replaced_binding.clone())
                        .or_default()
                        .push(CascadingUpdate {
                            id: unresolved.id.clone(),
                            from: Box::new(from),
                            to: (*unresolved).clone(),
                        });
                } else {
                    // Extract ref hints for attributes being promoted
                    let ref_hints: Vec<(String, String)> = unresolved
                        .attributes
                        .iter()
                        .filter_map(|(k, v)| match v.as_value() {
                            Value::ResourceRef { path }
                                if path.binding() == replaced_binding
                                    && create_only_refs.contains(k) =>
                            {
                                Some((
                                    k.clone(),
                                    format!("{}.{}", path.binding(), path.attribute()),
                                ))
                            }
                            _ => None,
                        })
                        .collect();

                    // Promote to a separate Replace effect
                    promoted_replaces.push(Effect::Replace {
                        id: unresolved.id.clone(),
                        from: Box::new(from),
                        to: (*unresolved).clone(),
                        lifecycle: unresolved.lifecycle.clone(),
                        changed_create_only: create_only_refs,
                        cascading_updates: vec![],
                        temporary_name: None,
                        cascade_ref_hints: ref_hints,
                    });
                }
            }
        }
    }

    // Apply cascading updates to the plan's Replace effects
    plan.set_cascading_updates(&replaced_bindings, &updates_by_replaced_binding);

    // Add promoted Replace effects to the plan
    for effect in promoted_replaces {
        plan.add(effect);
    }
}
