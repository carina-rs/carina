//! Differ - Compare desired state with current state to generate a Plan
//!
//! Compares the "desired state" declared in DSL with the "current state" fetched
//! from the Provider, and generates a list of required Effects (Plan).

use std::collections::{HashMap, HashSet};

use crate::deps::get_resource_dependencies;
use crate::effect::{CascadingUpdate, Effect, TemporaryName};
use crate::identifier::generate_random_suffix;
use crate::plan::Plan;
use crate::resource::{LifecycleConfig, Resource, ResourceId, State, Value, merge_with_saved};
use crate::schema::{AttributeType, ResourceSchema};

/// Result of a diff operation
#[derive(Debug, Clone, PartialEq)]
pub enum Diff {
    /// Resource does not exist -> needs creation
    Create(Resource),
    /// Resource exists with differences -> needs update
    Update {
        id: ResourceId,
        from: Box<State>,
        to: Resource,
        changed_attributes: Vec<String>,
    },
    /// Resource exists with no differences -> no action needed
    NoChange(ResourceId),
    /// Resource exists but not in desired state -> needs deletion
    Delete(ResourceId),
}

impl Diff {
    /// Returns whether this Diff involves a change
    pub fn is_change(&self) -> bool {
        !matches!(self, Diff::NoChange(_))
    }
}

/// Compare desired state with current state to compute a Diff.
/// If `saved` is provided, unmanaged nested fields from the saved state are merged
/// into desired before comparison, preventing false diffs when AWS returns extra fields.
/// If `prev_desired_keys` is provided, attributes that were previously in the user's
/// desired state but are now absent are detected as removals.
/// If `schema` is provided, type-aware comparison is used (e.g., Int/Float coercion,
/// case-insensitive enum matching).
pub fn diff(
    desired: &Resource,
    current: &State,
    saved: Option<&HashMap<String, Value>>,
    prev_desired_keys: Option<&[String]>,
    schema: Option<&ResourceSchema>,
) -> Diff {
    if !current.exists {
        return Diff::Create(desired.clone());
    }

    let changed = find_changed_attributes(
        &desired.attributes,
        &current.attributes,
        saved,
        prev_desired_keys,
        schema,
    );

    if changed.is_empty() {
        Diff::NoChange(desired.id.clone())
    } else {
        Diff::Update {
            id: desired.id.clone(),
            from: Box::new(current.clone()),
            to: desired.clone(),
            changed_attributes: changed,
        }
    }
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

/// Type-aware semantic comparison of two Values.
///
/// When an `AttributeType` is provided, the comparison uses type information
/// to detect semantically equivalent values that differ textually:
/// - Int/Float coercion: `Int(1)` equals `Float(1.0)` for numeric types
/// - StringEnum: case-insensitive + hyphen/underscore flexible matching
/// - List/Map: recurse with inner element type
/// - Struct: recurse with per-field type information
///
/// Without type information, falls back to `Value::semantically_equal()`.
fn type_aware_equal(a: &Value, b: &Value, attr_type: Option<&AttributeType>) -> bool {
    match attr_type {
        None => a.semantically_equal(b),
        Some(at) => match (a, b, at) {
            // Int/Float coercion for numeric types
            (Value::Int(i), Value::Float(f), AttributeType::Float | AttributeType::Int) => {
                (*i as f64) == *f && (*i as f64) as i64 == *i
            }
            (Value::Float(f), Value::Int(i), AttributeType::Float | AttributeType::Int) => {
                *f == (*i as f64) && (*i as f64) as i64 == *i
            }

            // StringEnum: case-insensitive + hyphen/underscore flexible matching
            (Value::String(sa), Value::String(sb), AttributeType::StringEnum { to_dsl, .. }) => {
                if sa == sb {
                    return true;
                }
                // Normalize both values through to_dsl if available, then compare
                let na = to_dsl.map_or_else(|| sa.clone(), |f| f(sa));
                let nb = to_dsl.map_or_else(|| sb.clone(), |f| f(sb));
                if na == nb {
                    return true;
                }
                // Case-insensitive + hyphen/underscore flexible
                na.eq_ignore_ascii_case(&nb)
                    || na
                        .replace('_', "-")
                        .eq_ignore_ascii_case(&nb.replace('_', "-"))
            }

            // Lists: multiset comparison with inner type awareness
            (Value::List(la), Value::List(lb), AttributeType::List(inner)) => {
                type_aware_lists_equal(la, lb, Some(inner))
            }

            // Maps: recursive comparison with inner value type
            (Value::Map(ma), Value::Map(mb), AttributeType::Map(inner)) => {
                type_aware_maps_equal(ma, mb, |_key| Some(inner.as_ref()))
            }

            // Struct: per-field type-aware comparison
            (Value::Map(ma), Value::Map(mb), AttributeType::Struct { fields, .. }) => {
                let field_types: HashMap<&str, &AttributeType> = fields
                    .iter()
                    .map(|f| (f.name.as_str(), &f.field_type))
                    .collect();
                type_aware_maps_equal(ma, mb, |key| field_types.get(key).copied())
            }

            // Union: try each member type; if any says equal, they're equal
            (_, _, AttributeType::Union(types)) => {
                // Also check Int/Float coercion for unions containing numeric types
                match (a, b) {
                    (Value::Int(i), Value::Float(f)) | (Value::Float(f), Value::Int(i))
                        if types
                            .iter()
                            .any(|t| matches!(t, AttributeType::Float | AttributeType::Int)) =>
                    {
                        (*i as f64) == *f && (*i as f64) as i64 == *i
                    }
                    _ => types.iter().any(|t| type_aware_equal(a, b, Some(t))),
                }
            }

            // Custom types with base type: delegate to base
            (_, _, AttributeType::Custom { base, .. }) => type_aware_equal(a, b, Some(base)),

            // All other cases: fall back to semantic equality
            _ => a.semantically_equal(b),
        },
    }
}

/// Multiset comparison for lists with type-aware element comparison.
fn type_aware_lists_equal(a: &[Value], b: &[Value], inner: Option<&AttributeType>) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut matched = vec![false; b.len()];
    for item_a in a {
        let mut found = false;
        for (j, item_b) in b.iter().enumerate() {
            if !matched[j] && type_aware_equal(item_a, item_b, inner) {
                matched[j] = true;
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// Map comparison with per-key type lookup.
fn type_aware_maps_equal<'a, F>(
    a: &HashMap<String, Value>,
    b: &HashMap<String, Value>,
    get_type: F,
) -> bool
where
    F: Fn(&str) -> Option<&'a AttributeType>,
{
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(k, va)| {
        b.get(k)
            .map(|vb| type_aware_equal(va, vb, get_type(k)))
            .unwrap_or(false)
    })
}

/// Find changed attributes between desired and current state.
/// If `saved` is provided, each desired value is merged with the saved value
/// before comparison, filling in unmanaged nested fields.
/// If `prev_desired_keys` is provided, attributes that were previously in the user's
/// desired state but are now absent from desired (while still present in current)
/// are detected as removals.
/// If `schema` is provided, type-aware comparison is used for each attribute.
fn find_changed_attributes(
    desired: &HashMap<String, Value>,
    current: &HashMap<String, Value>,
    saved: Option<&HashMap<String, Value>>,
    prev_desired_keys: Option<&[String]>,
    schema: Option<&ResourceSchema>,
) -> Vec<String> {
    let mut changed = Vec::new();

    for (key, desired_value) in desired {
        // Skip internal attributes (starting with _)
        if key.starts_with('_') {
            continue;
        }

        let attr_type = schema
            .and_then(|s| s.attributes.get(key))
            .map(|a| &a.attr_type);

        let is_equal = match saved.and_then(|s| s.get(key)) {
            Some(saved_value) => {
                let effective_desired = merge_with_saved(desired_value, saved_value);
                current
                    .get(key)
                    .map(|cv| type_aware_equal(cv, &effective_desired, attr_type))
                    .unwrap_or(false)
            }
            None => current
                .get(key)
                .map(|cv| type_aware_equal(cv, desired_value, attr_type))
                .unwrap_or(false),
        };

        if !is_equal {
            changed.push(key.clone());
        }
    }

    // Detect attributes removed from desired but still present in current.
    // Only flag attributes that were previously in the user's desired state
    // (from the state file's desired_keys). This prevents false removals for
    // computed/provider-returned attributes the user never specified.
    if let Some(prev_keys) = prev_desired_keys {
        for key in prev_keys {
            if key.starts_with('_') {
                continue;
            }
            if desired.contains_key(key) {
                continue;
            }
            if current.contains_key(key) {
                changed.push(key.clone());
            }
        }
    }

    changed
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
    let original_value = match resource.attributes.get(name_attr) {
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
) -> Plan {
    let mut plan = Plan::new();

    let desired_ids: std::collections::HashSet<&ResourceId> =
        desired.iter().map(|r| &r.id).collect();

    for resource in desired {
        // Data sources (read-only resources) only generate Read effects
        if resource.read_only {
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
                        modified.attributes.insert(
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
                plan.add(Effect::Delete {
                    id,
                    identifier,
                    lifecycle,
                });
            }
        }
    }

    // Detect orphaned resources: exist in current_states but not in desired
    for (id, state) in current_states {
        if state.exists && !desired_ids.contains(id) {
            let identifier = state.identifier.clone().unwrap_or_default();
            let lifecycle = lifecycles.get(id).cloned().unwrap_or_default();
            plan.add(Effect::Delete {
                id: id.clone(),
                identifier,
                lifecycle,
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
/// 3. Adds CascadingUpdate entries to the Replace effect with the unresolved
///    resource (containing ResourceRef values) so apply can re-resolve using the
///    new resource's state
///
/// `unresolved_resources` should be the resources BEFORE ref resolution (still containing
/// ResourceRef values). `current_states` provides the `from` state for each dependent.
pub fn cascade_dependent_updates(
    plan: &mut Plan,
    unresolved_resources: &[Resource],
    current_states: &HashMap<ResourceId, State>,
) {
    // Build binding/key -> unresolved resource mapping.
    // Uses the same key logic as the dependent lookup below so anonymous resources
    // (without _binding) are also found.
    let mut binding_to_unresolved: HashMap<String, &Resource> = HashMap::new();
    for resource in unresolved_resources {
        let key = resource
            .attributes
            .get("_binding")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name));
        binding_to_unresolved.insert(key, resource);
    }

    // Build reverse dependency map: replaced_binding -> [dependent_bindings]
    let mut dependents_of_replaced: HashMap<String, Vec<String>> = HashMap::new();

    // Collect binding names of resources being replaced with create_before_destroy
    let replaced_bindings: HashSet<String> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let Effect::Replace { lifecycle, .. } = e
                && lifecycle.create_before_destroy
            {
                return e.binding_name();
            }
            None
        })
        .collect();

    if replaced_bindings.is_empty() {
        return;
    }

    // Collect resource IDs that already have effects in the plan
    let planned_ids: HashSet<&ResourceId> =
        plan.effects().iter().map(|e| e.resource_id()).collect();

    // For each unresolved resource, check if it depends on a replaced binding
    for resource in unresolved_resources {
        // Skip resources that already have effects in the plan
        if planned_ids.contains(&resource.id) {
            continue;
        }

        let deps = get_resource_dependencies(resource);
        for dep in &deps {
            if replaced_bindings.contains(dep) {
                let binding = resource
                    .attributes
                    .get("_binding")
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        format!("{}:{}", resource.id.resource_type, resource.id.name)
                    });
                dependents_of_replaced
                    .entry(dep.clone())
                    .or_default()
                    .push(binding);
            }
        }
    }

    // Build cascading updates for each Replace effect
    // We need to collect updates first, then mutate the plan
    let mut updates_by_replaced_binding: HashMap<String, Vec<CascadingUpdate>> = HashMap::new();

    for (replaced_binding, dependent_bindings) in &dependents_of_replaced {
        for dep_binding in dependent_bindings {
            if let Some(unresolved) = binding_to_unresolved.get(dep_binding) {
                let from = current_states
                    .get(&unresolved.id)
                    .cloned()
                    .unwrap_or_else(|| State::not_found(unresolved.id.clone()));

                if from.exists {
                    updates_by_replaced_binding
                        .entry(replaced_binding.clone())
                        .or_default()
                        .push(CascadingUpdate {
                            id: unresolved.id.clone(),
                            from: Box::new(from),
                            to: (*unresolved).clone(),
                        });
                }
            }
        }
    }

    // Apply cascading updates to the plan's Replace effects
    plan.set_cascading_updates(&replaced_bindings, &updates_by_replaced_binding);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_create_when_not_exists() {
        let desired = Resource::new("bucket", "test");
        let current = State::not_found(ResourceId::new("bucket", "test"));

        let result = diff(&desired, &current, None, None, None);
        assert!(matches!(result, Diff::Create(_)));
    }

    #[test]
    fn diff_no_change_when_same() {
        let desired = Resource::new("bucket", "test")
            .with_attribute("region", Value::String("ap-northeast-1".to_string()));

        let mut attrs = HashMap::new();
        attrs.insert(
            "region".to_string(),
            Value::String("ap-northeast-1".to_string()),
        );
        let current = State::existing(ResourceId::new("bucket", "test"), attrs);

        let result = diff(&desired, &current, None, None, None);
        assert!(matches!(result, Diff::NoChange(_)));
    }

    #[test]
    fn diff_update_when_different() {
        let desired = Resource::new("bucket", "test")
            .with_attribute("region", Value::String("us-east-1".to_string()));

        let mut attrs = HashMap::new();
        attrs.insert(
            "region".to_string(),
            Value::String("ap-northeast-1".to_string()),
        );
        let current = State::existing(ResourceId::new("bucket", "test"), attrs);

        let result = diff(&desired, &current, None, None, None);
        match result {
            Diff::Update {
                changed_attributes, ..
            } => {
                assert!(changed_attributes.contains(&"region".to_string()));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn create_plan_from_resources() {
        let resources = vec![
            Resource::new("bucket", "new-bucket"),
            Resource::new("bucket", "existing-bucket")
                .with_attribute("versioning", Value::Bool(true)),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert("versioning".to_string(), Value::Bool(false));
        current_states.insert(
            ResourceId::new("bucket", "existing-bucket"),
            State::existing(ResourceId::new("bucket", "existing-bucket"), attrs),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 2);
        assert!(matches!(plan.effects()[0], Effect::Create(_)));
        assert!(matches!(plan.effects()[1], Effect::Update { .. }));
    }

    #[test]
    fn create_plan_with_read_only_resource() {
        let resources = vec![
            Resource::new("bucket", "existing-bucket")
                .with_attribute("name", Value::String("existing-bucket".to_string()))
                .with_read_only(true),
            Resource::new("bucket", "new-bucket"),
        ];

        let current_states = HashMap::new();
        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );

        // Should have 2 effects: Read for data source, Create for new bucket
        assert_eq!(plan.effects().len(), 2);
        assert!(matches!(plan.effects()[0], Effect::Read { .. }));
        assert!(matches!(plan.effects()[1], Effect::Create(_)));
    }

    #[test]
    fn diff_update_when_list_of_maps_changed() {
        let mut ingress1 = HashMap::new();
        ingress1.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        ingress1.insert("from_port".to_string(), Value::Int(80));
        ingress1.insert("to_port".to_string(), Value::Int(80));

        let mut ingress2 = HashMap::new();
        ingress2.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        ingress2.insert("from_port".to_string(), Value::Int(443));
        ingress2.insert("to_port".to_string(), Value::Int(443));

        let desired = Resource::new("ec2_security_group", "test-sg").with_attribute(
            "security_group_ingress",
            Value::List(vec![Value::Map(ingress1.clone()), Value::Map(ingress2)]),
        );

        let mut current_attrs = HashMap::new();
        current_attrs.insert(
            "security_group_ingress".to_string(),
            Value::List(vec![Value::Map(ingress1)]),
        );
        let current = State::existing(
            ResourceId::new("ec2_security_group", "test-sg"),
            current_attrs,
        );

        let result = diff(&desired, &current, None, None, None);
        match result {
            Diff::Update {
                changed_attributes, ..
            } => {
                assert!(
                    changed_attributes.contains(&"security_group_ingress".to_string()),
                    "Should detect security_group_ingress as changed"
                );
            }
            _ => panic!("Expected Update when list-of-maps changed"),
        }
    }

    #[test]
    fn create_plan_detects_orphaned_resources_for_deletion() {
        // A resource exists in current_states but NOT in desired list
        // create_plan() should generate a Delete effect for it
        let desired = vec![Resource::new("bucket", "keep-this")];

        let mut current_states = HashMap::new();
        // "keep-this" exists and matches
        current_states.insert(
            ResourceId::new("bucket", "keep-this"),
            State::existing(ResourceId::new("bucket", "keep-this"), HashMap::new()),
        );
        // "orphaned-bucket" exists in state but not in desired
        let mut orphan_attrs = HashMap::new();
        orphan_attrs.insert(
            "name".to_string(),
            Value::String("orphaned-bucket".to_string()),
        );
        current_states.insert(
            ResourceId::new("bucket", "orphaned-bucket"),
            State::existing(ResourceId::new("bucket", "orphaned-bucket"), orphan_attrs),
        );

        let plan = create_plan(
            &desired,
            &current_states,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );

        // Should have 1 effect: Delete for orphaned-bucket
        // (keep-this has NoChange, so no effect)
        let delete_effects: Vec<_> = plan
            .effects()
            .iter()
            .filter(|e| matches!(e, Effect::Delete { .. }))
            .collect();
        assert_eq!(
            delete_effects.len(),
            1,
            "Expected 1 Delete effect for orphaned resource, got {}. Effects: {:?}",
            delete_effects.len(),
            plan.effects()
        );
    }

    #[test]
    fn read_only_resource_always_generates_read_effect() {
        // Even if the resource "exists", read-only resources should only generate Read effect
        let resources = vec![
            Resource::new("bucket", "existing-bucket")
                .with_attribute("name", Value::String("existing-bucket".to_string()))
                .with_read_only(true),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::String("existing-bucket".to_string()),
        );
        current_states.insert(
            ResourceId::new("bucket", "existing-bucket"),
            State::existing(ResourceId::new("bucket", "existing-bucket"), attrs),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );

        // Should still have Read effect, not NoChange
        assert_eq!(plan.effects().len(), 1);
        assert!(matches!(plan.effects()[0], Effect::Read { .. }));
    }

    /// Regression test for issue #146: when neither desired nor current state has
    /// a "name" attribute (the normal case for AWSCC resources after PR #151),
    /// the differ should report NoChange, not a false update.
    #[test]
    fn no_false_update_without_name_attribute() {
        // Simulate AWSCC resource: desired has cidr_block but no "name"
        let desired = Resource::new("ec2.vpc", "vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));

        // Current state from provider read also has cidr_block but no "name"
        let mut attrs = HashMap::new();
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let current = State::existing(ResourceId::new("ec2.vpc", "vpc"), attrs);

        let result = diff(&desired, &current, None, None, None);
        assert!(
            matches!(result, Diff::NoChange(_)),
            "Expected NoChange when neither side has 'name', got {:?}",
            result
        );
    }

    #[test]
    fn replace_when_create_only_attr_changed() {
        use crate::schema::{AttributeSchema, AttributeType};

        let resources = vec![
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string())),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        current_states.insert(
            ResourceId::new("ec2.vpc", "my-vpc"),
            State::existing(ResourceId::new("ec2.vpc", "my-vpc"), attrs),
        );

        // Build schema with cidr_block marked as create-only
        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.vpc".to_string(),
            crate::schema::ResourceSchema::new("ec2.vpc")
                .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only()),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Replace {
                changed_create_only,
                ..
            } => {
                assert_eq!(changed_create_only, &vec!["cidr_block".to_string()]);
            }
            other => panic!("Expected Replace, got {:?}", other),
        }
    }

    #[test]
    fn normal_update_when_non_create_only_attr_changed() {
        use crate::schema::{AttributeSchema, AttributeType};

        let resources = vec![
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("enable_dns_support", Value::Bool(true)),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert("enable_dns_support".to_string(), Value::Bool(false));
        current_states.insert(
            ResourceId::new("ec2.vpc", "my-vpc"),
            State::existing(ResourceId::new("ec2.vpc", "my-vpc"), attrs),
        );

        // cidr_block is create-only, but enable_dns_support is not
        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.vpc".to_string(),
            crate::schema::ResourceSchema::new("ec2.vpc")
                .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only())
                .attribute(AttributeSchema::new(
                    "enable_dns_support",
                    AttributeType::Bool,
                )),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        assert!(
            matches!(plan.effects()[0], Effect::Update { .. }),
            "Expected Update, got {:?}",
            plan.effects()[0]
        );
    }

    #[test]
    fn replace_when_schema_force_replace() {
        use crate::schema::AttributeType;

        // Resource has changed attributes but NO create-only attributes
        let resources = vec![
            Resource::new("ec2.internet_gateway", "my-igw").with_attribute(
                "tags",
                Value::Map(
                    vec![("Name".to_string(), Value::String("new-name".to_string()))]
                        .into_iter()
                        .collect(),
                ),
            ),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "tags".to_string(),
            Value::Map(
                vec![("Name".to_string(), Value::String("old-name".to_string()))]
                    .into_iter()
                    .collect(),
            ),
        );
        current_states.insert(
            ResourceId::new("ec2.internet_gateway", "my-igw"),
            State::existing(ResourceId::new("ec2.internet_gateway", "my-igw"), attrs),
        );

        // Schema has force_replace=true (no create-only attributes)
        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.internet_gateway".to_string(),
            crate::schema::ResourceSchema::new("ec2.internet_gateway")
                .attribute(crate::schema::AttributeSchema::new(
                    "tags",
                    AttributeType::String,
                ))
                .force_replace(),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        assert!(
            matches!(plan.effects()[0], Effect::Replace { .. }),
            "Expected Replace for force_replace schema, got {:?}",
            plan.effects()[0]
        );
    }

    #[test]
    fn replace_when_mix_of_create_only_and_normal_attrs_changed() {
        use crate::schema::{AttributeSchema, AttributeType};

        let resources = vec![
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()))
                .with_attribute("enable_dns_support", Value::Bool(true)),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        attrs.insert("enable_dns_support".to_string(), Value::Bool(false));
        current_states.insert(
            ResourceId::new("ec2.vpc", "my-vpc"),
            State::existing(ResourceId::new("ec2.vpc", "my-vpc"), attrs),
        );

        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.vpc".to_string(),
            crate::schema::ResourceSchema::new("ec2.vpc")
                .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only())
                .attribute(AttributeSchema::new(
                    "enable_dns_support",
                    AttributeType::Bool,
                )),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Replace {
                changed_create_only,
                ..
            } => {
                assert_eq!(changed_create_only, &vec!["cidr_block".to_string()]);
            }
            other => panic!("Expected Replace, got {:?}", other),
        }
    }

    #[test]
    fn replace_carries_create_before_destroy_lifecycle() {
        use crate::schema::{AttributeSchema, AttributeType};

        let mut resource = Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));
        resource.lifecycle.create_before_destroy = true;

        let resources = vec![resource];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        current_states.insert(
            ResourceId::new("ec2.vpc", "my-vpc"),
            State::existing(ResourceId::new("ec2.vpc", "my-vpc"), attrs),
        );

        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.vpc".to_string(),
            crate::schema::ResourceSchema::new("ec2.vpc")
                .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only()),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Replace {
                lifecycle,
                changed_create_only,
                ..
            } => {
                assert!(lifecycle.create_before_destroy);
                assert_eq!(changed_create_only, &vec!["cidr_block".to_string()]);
            }
            other => panic!("Expected Replace, got {:?}", other),
        }
    }

    #[test]
    fn diff_no_change_when_list_of_maps_reordered() {
        let mut rule1 = HashMap::new();
        rule1.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        rule1.insert("from_port".to_string(), Value::Int(80));
        rule1.insert("to_port".to_string(), Value::Int(80));

        let mut rule2 = HashMap::new();
        rule2.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        rule2.insert("from_port".to_string(), Value::Int(443));
        rule2.insert("to_port".to_string(), Value::Int(443));

        // Desired: [rule1, rule2]
        let desired = Resource::new("ec2_security_group", "test-sg").with_attribute(
            "security_group_egress",
            Value::List(vec![Value::Map(rule1.clone()), Value::Map(rule2.clone())]),
        );

        // Current (from AWS): [rule2, rule1] — same content, different order
        let mut current_attrs = HashMap::new();
        current_attrs.insert(
            "security_group_egress".to_string(),
            Value::List(vec![Value::Map(rule2), Value::Map(rule1)]),
        );
        let current = State::existing(
            ResourceId::new("ec2_security_group", "test-sg"),
            current_attrs,
        );

        let result = diff(&desired, &current, None, None, None);
        assert!(
            matches!(result, Diff::NoChange(_)),
            "Expected NoChange when list-of-maps has same content in different order, got {:?}",
            result
        );
    }

    #[test]
    fn replace_with_provider_prefixed_schema_key() {
        use crate::schema::{AttributeSchema, AttributeType};

        // In production, schemas are keyed by "awscc.ec2.vpc" but resource_type is "ec2.vpc"
        // The resource must have provider set so the generic lookup works
        let resources = vec![
            Resource::with_provider("awscc", "ec2.vpc", "my-vpc")
                .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string())),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        current_states.insert(
            ResourceId::with_provider("awscc", "ec2.vpc", "my-vpc"),
            State::existing(
                ResourceId::with_provider("awscc", "ec2.vpc", "my-vpc"),
                attrs,
            ),
        );

        // Schema keyed with provider prefix (as in production)
        let mut schemas = HashMap::new();
        schemas.insert(
            "awscc.ec2.vpc".to_string(),
            crate::schema::ResourceSchema::new("awscc.ec2.vpc")
                .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only()),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        assert!(
            matches!(plan.effects()[0], Effect::Replace { .. }),
            "Expected Replace with awscc-prefixed schema key, got {:?}",
            plan.effects()[0]
        );
    }

    /// Regression test for issue #172: desired has 2 fields in a struct,
    /// current (AWS) returns 3, saved state has 3. Should be NoChange.
    #[test]
    fn diff_no_change_when_struct_has_extra_fields_with_saved() {
        let desired = Resource::new("ec2.subnet", "test-subnet").with_attribute(
            "private_dns_name_options_on_launch",
            Value::Map(HashMap::from([
                (
                    "hostname_type".to_string(),
                    Value::String("ip-name".to_string()),
                ),
                (
                    "enable_resource_name_dns_a_record".to_string(),
                    Value::Bool(true),
                ),
            ])),
        );

        let current_attrs = HashMap::from([(
            "private_dns_name_options_on_launch".to_string(),
            Value::Map(HashMap::from([
                (
                    "hostname_type".to_string(),
                    Value::String("ip-name".to_string()),
                ),
                (
                    "enable_resource_name_dns_a_record".to_string(),
                    Value::Bool(true),
                ),
                (
                    "enable_resource_name_dns_aaaa_record".to_string(),
                    Value::Bool(false),
                ),
            ])),
        )]);
        let current = State::existing(ResourceId::new("ec2.subnet", "test-subnet"), current_attrs);

        let saved = HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Bool(true),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Bool(false),
            ),
        ]);
        let saved_map = HashMap::from([(
            "private_dns_name_options_on_launch".to_string(),
            Value::Map(saved),
        )]);

        let result = diff(&desired, &current, Some(&saved_map), None, None);
        assert!(
            matches!(result, Diff::NoChange(_)),
            "Expected NoChange when saved fills extra struct fields, got {:?}",
            result
        );
    }

    /// When an unmanaged field drifts externally, diff should still detect the change.
    #[test]
    fn diff_detects_drift_on_unmanaged_field() {
        let desired = Resource::new("ec2.subnet", "test-subnet").with_attribute(
            "private_dns_name_options_on_launch",
            Value::Map(HashMap::from([
                (
                    "hostname_type".to_string(),
                    Value::String("ip-name".to_string()),
                ),
                (
                    "enable_resource_name_dns_a_record".to_string(),
                    Value::Bool(true),
                ),
            ])),
        );

        // AWS returns aaaa_record: true (drifted from saved false)
        let current_attrs = HashMap::from([(
            "private_dns_name_options_on_launch".to_string(),
            Value::Map(HashMap::from([
                (
                    "hostname_type".to_string(),
                    Value::String("ip-name".to_string()),
                ),
                (
                    "enable_resource_name_dns_a_record".to_string(),
                    Value::Bool(true),
                ),
                (
                    "enable_resource_name_dns_aaaa_record".to_string(),
                    Value::Bool(true),
                ),
            ])),
        )]);
        let current = State::existing(ResourceId::new("ec2.subnet", "test-subnet"), current_attrs);

        let saved = HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            (
                "enable_resource_name_dns_a_record".to_string(),
                Value::Bool(true),
            ),
            (
                "enable_resource_name_dns_aaaa_record".to_string(),
                Value::Bool(false),
            ),
        ]);
        let saved_map = HashMap::from([(
            "private_dns_name_options_on_launch".to_string(),
            Value::Map(saved),
        )]);

        let result = diff(&desired, &current, Some(&saved_map), None, None);
        assert!(
            matches!(result, Diff::Update { .. }),
            "Expected Update when unmanaged field drifted, got {:?}",
            result
        );
    }

    /// Regression test for issue #350: desired is Map (from `= {}` syntax),
    /// but current and saved are List([Map]) (from provider read path).
    /// After merge + semantic comparison, this should be NoChange.
    #[test]
    fn diff_no_change_when_bare_struct_with_extra_fields() {
        let desired = Resource::new("ec2.subnet", "test-subnet").with_attribute(
            "private_dns_name_options_on_launch",
            Value::Map(HashMap::from([
                (
                    "hostname_type".to_string(),
                    Value::String("ip-name".to_string()),
                ),
                (
                    "enable_resource_name_dns_a_record".to_string(),
                    Value::Bool(true),
                ),
            ])),
        );

        // Provider read returns Map with extra fields not in desired
        let current_attrs = HashMap::from([(
            "private_dns_name_options_on_launch".to_string(),
            Value::Map(HashMap::from([
                (
                    "hostname_type".to_string(),
                    Value::String("ip-name".to_string()),
                ),
                (
                    "enable_resource_name_dns_a_record".to_string(),
                    Value::Bool(true),
                ),
                (
                    "enable_resource_name_dns_aaaa_record".to_string(),
                    Value::Bool(false),
                ),
            ])),
        )]);
        let current = State::existing(ResourceId::new("ec2.subnet", "test-subnet"), current_attrs);

        // Saved state has the same Map with extra fields
        let saved_map = HashMap::from([(
            "private_dns_name_options_on_launch".to_string(),
            Value::Map(HashMap::from([
                (
                    "hostname_type".to_string(),
                    Value::String("ip-name".to_string()),
                ),
                (
                    "enable_resource_name_dns_a_record".to_string(),
                    Value::Bool(true),
                ),
                (
                    "enable_resource_name_dns_aaaa_record".to_string(),
                    Value::Bool(false),
                ),
            ])),
        )]);

        let result = diff(&desired, &current, Some(&saved_map), None, None);
        assert!(
            matches!(result, Diff::NoChange(_)),
            "Expected NoChange for bare struct with extra fields from saved, got {:?}",
            result
        );
    }

    /// When saved state is None, behavior should be unchanged from before.
    #[test]
    fn diff_works_without_saved_state() {
        // Desired has 2 fields, current has 3 (extra field). Without saved state,
        // this should still be NoChange because find_changed_attributes only checks
        // desired keys against current (not the other direction).
        let desired = Resource::new("ec2.subnet", "test-subnet").with_attribute(
            "opts",
            Value::Map(HashMap::from([
                ("a".to_string(), Value::Int(1)),
                ("b".to_string(), Value::Int(2)),
            ])),
        );

        let current_attrs = HashMap::from([(
            "opts".to_string(),
            Value::Map(HashMap::from([
                ("a".to_string(), Value::Int(1)),
                ("b".to_string(), Value::Int(2)),
                ("c".to_string(), Value::Int(3)),
            ])),
        )]);
        let current = State::existing(ResourceId::new("ec2.subnet", "test-subnet"), current_attrs);

        // Without saved state, the map comparison uses semantically_equal which
        // checks both key count AND values. Since desired map has 2 keys and current
        // has 3, this will show as Update (which is the existing behavior).
        let result = diff(&desired, &current, None, None, None);
        assert!(
            matches!(result, Diff::Update { .. }),
            "Expected Update without saved state when maps have different sizes, got {:?}",
            result
        );
    }

    #[test]
    fn cascade_dependent_updates_adds_update_for_dependent() {
        // VPC is being replaced with create_before_destroy
        // Subnet depends on VPC via ResourceRef
        // cascade_dependent_updates should add a CascadingUpdate to the Replace

        let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
        let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

        // Unresolved resources (before ref resolution)
        let vpc = Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

        let subnet = Resource::new("ec2.subnet", "my-subnet")
            .with_attribute("_binding", Value::String("subnet".to_string()))
            .with_attribute(
                "vpc_id",
                Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                },
            )
            .with_attribute("cidr_block", Value::String("10.1.1.0/24".to_string()));

        let unresolved_resources = vec![vpc.clone(), subnet.clone()];

        // Current states
        let mut current_states = HashMap::new();
        let mut vpc_attrs = HashMap::new();
        vpc_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        vpc_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
        current_states.insert(
            vpc_id.clone(),
            State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
        );

        let mut subnet_attrs = HashMap::new();
        subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
        subnet_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.1.1.0/24".to_string()),
        );
        current_states.insert(
            subnet_id.clone(),
            State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
        );

        // Build a plan with Replace for VPC (create_before_destroy)
        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: vpc_id.clone(),
            from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
            to: vpc
                .clone()
                .with_attribute("_binding", Value::String("vpc".to_string())),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
        });

        // Apply cascade
        cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states);

        // Verify the Replace effect now has a cascading update for the subnet
        let effects = plan.effects();
        assert_eq!(effects.len(), 1);
        if let Effect::Replace {
            cascading_updates, ..
        } = &effects[0]
        {
            assert_eq!(cascading_updates.len(), 1);
            assert_eq!(cascading_updates[0].id, subnet_id);
            // The `to` should have unresolved ResourceRef
            assert!(matches!(
                cascading_updates[0].to.attributes.get("vpc_id"),
                Some(Value::ResourceRef { .. })
            ));
            // The `from` should have the current state
            assert_eq!(
                cascading_updates[0].from.attributes.get("vpc_id"),
                Some(&Value::String("vpc-old".to_string()))
            );
        } else {
            panic!("Expected Replace effect");
        }
    }

    #[test]
    fn cascade_skips_resources_already_in_plan() {
        // If the dependent resource already has its own effect (e.g., Update),
        // cascade should not add a duplicate

        let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
        let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

        let vpc = Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

        let subnet = Resource::new("ec2.subnet", "my-subnet")
            .with_attribute("_binding", Value::String("subnet".to_string()))
            .with_attribute(
                "vpc_id",
                Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                },
            )
            .with_attribute("cidr_block", Value::String("10.1.2.0/24".to_string()));

        let unresolved_resources = vec![vpc.clone(), subnet.clone()];

        let mut current_states = HashMap::new();
        let mut vpc_attrs = HashMap::new();
        vpc_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        current_states.insert(
            vpc_id.clone(),
            State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
        );
        let mut subnet_attrs = HashMap::new();
        subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
        subnet_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.1.1.0/24".to_string()),
        );
        current_states.insert(
            subnet_id.clone(),
            State::existing(subnet_id.clone(), subnet_attrs.clone()).with_identifier("subnet-123"),
        );

        // Plan with both Replace for VPC and Update for subnet
        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: vpc_id.clone(),
            from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
            to: vpc.clone(),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
        });
        plan.add(Effect::Update {
            id: subnet_id.clone(),
            from: Box::new(current_states.get(&subnet_id).unwrap().clone()),
            to: subnet.clone(),
            changed_attributes: vec!["cidr_block".to_string()],
        });

        cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states);

        // The Replace should have NO cascading updates since subnet already has an Update
        if let Effect::Replace {
            cascading_updates, ..
        } = &plan.effects()[0]
        {
            assert!(
                cascading_updates.is_empty(),
                "Expected no cascading updates when dependent already has an effect"
            );
        } else {
            panic!("Expected Replace effect");
        }
    }

    #[test]
    fn cascade_no_op_without_create_before_destroy() {
        // Replace without create_before_destroy should not trigger cascading

        let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");

        let vpc = Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

        let subnet = Resource::new("ec2.subnet", "my-subnet")
            .with_attribute("_binding", Value::String("subnet".to_string()))
            .with_attribute(
                "vpc_id",
                Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                },
            );

        let unresolved_resources = vec![vpc.clone(), subnet.clone()];

        let mut current_states = HashMap::new();
        let mut vpc_attrs = HashMap::new();
        vpc_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        current_states.insert(
            vpc_id.clone(),
            State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
        );

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: vpc_id.clone(),
            from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
            to: vpc.clone(),
            lifecycle: LifecycleConfig::default(), // create_before_destroy = false
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
        });

        cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states);

        if let Effect::Replace {
            cascading_updates, ..
        } = &plan.effects()[0]
        {
            assert!(cascading_updates.is_empty());
        }
    }

    #[test]
    fn cascade_transitive_dependencies() {
        // VPC → Subnet → Instance (transitive chain)
        // Only Subnet directly depends on VPC, so only Subnet gets cascading update

        let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
        let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");
        let instance_id = ResourceId::new("ec2.instance", "my-instance");

        let vpc = Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

        let subnet = Resource::new("ec2.subnet", "my-subnet")
            .with_attribute("_binding", Value::String("subnet".to_string()))
            .with_attribute(
                "vpc_id",
                Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                },
            );

        let instance = Resource::new("ec2.instance", "my-instance")
            .with_attribute("_binding", Value::String("instance".to_string()))
            .with_attribute(
                "subnet_id",
                Value::ResourceRef {
                    binding_name: "subnet".to_string(),
                    attribute_name: "subnet_id".to_string(),
                },
            );

        let unresolved_resources = vec![vpc.clone(), subnet.clone(), instance.clone()];

        let mut current_states = HashMap::new();
        let mut vpc_attrs = HashMap::new();
        vpc_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        vpc_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
        current_states.insert(
            vpc_id.clone(),
            State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
        );
        let mut subnet_attrs = HashMap::new();
        subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
        subnet_attrs.insert(
            "subnet_id".to_string(),
            Value::String("subnet-123".to_string()),
        );
        current_states.insert(
            subnet_id.clone(),
            State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
        );
        let mut instance_attrs = HashMap::new();
        instance_attrs.insert(
            "subnet_id".to_string(),
            Value::String("subnet-123".to_string()),
        );
        current_states.insert(
            instance_id.clone(),
            State::existing(instance_id.clone(), instance_attrs).with_identifier("i-123"),
        );

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: vpc_id.clone(),
            from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
            to: vpc.clone(),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
        });

        cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states);

        // Only subnet directly depends on VPC, so only subnet gets cascading update
        // Instance depends on subnet, not VPC directly
        if let Effect::Replace {
            cascading_updates, ..
        } = &plan.effects()[0]
        {
            assert_eq!(cascading_updates.len(), 1);
            assert_eq!(cascading_updates[0].id, subnet_id);
        } else {
            panic!("Expected Replace effect");
        }
    }

    #[test]
    fn cascade_anonymous_resource_dependent() {
        // Anonymous resource (no _binding) that depends on a replaced resource
        // should still get a cascading update

        let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
        let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

        let vpc = Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

        // Anonymous subnet (no _binding) with a ResourceRef to the VPC
        let subnet = Resource::new("ec2.subnet", "my-subnet").with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            },
        );

        let unresolved_resources = vec![vpc.clone(), subnet.clone()];

        let mut current_states = HashMap::new();
        let mut vpc_attrs = HashMap::new();
        vpc_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        vpc_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
        current_states.insert(
            vpc_id.clone(),
            State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
        );

        let mut subnet_attrs = HashMap::new();
        subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
        current_states.insert(
            subnet_id.clone(),
            State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
        );

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: vpc_id.clone(),
            from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
            to: vpc.clone(),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
        });

        cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states);

        if let Effect::Replace {
            cascading_updates, ..
        } = &plan.effects()[0]
        {
            assert_eq!(
                cascading_updates.len(),
                1,
                "Anonymous resource should get cascading update"
            );
            assert_eq!(cascading_updates[0].id, subnet_id);
        } else {
            panic!("Expected Replace effect");
        }
    }

    #[test]
    fn create_before_destroy_generates_temporary_name_for_name_attribute() {
        use crate::schema::{AttributeSchema, AttributeType};

        let mut resource = Resource::new("s3.bucket", "my-bucket")
            .with_attribute("bucket_name", Value::String("my-bucket".to_string()))
            .with_attribute("object_lock_enabled", Value::Bool(true));
        resource.lifecycle.create_before_destroy = true;

        let resources = vec![resource];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "bucket_name".to_string(),
            Value::String("my-bucket".to_string()),
        );
        attrs.insert("object_lock_enabled".to_string(), Value::Bool(false));
        current_states.insert(
            ResourceId::new("s3.bucket", "my-bucket"),
            State::existing(ResourceId::new("s3.bucket", "my-bucket"), attrs),
        );

        let mut schemas = HashMap::new();
        schemas.insert(
            "s3.bucket".to_string(),
            ResourceSchema::new("s3.bucket")
                .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only())
                .attribute(
                    AttributeSchema::new("object_lock_enabled", AttributeType::Bool).create_only(),
                )
                .with_name_attribute("bucket_name"),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Replace {
                temporary_name, to, ..
            } => {
                let temp = temporary_name.as_ref().expect(
                    "Should have temporary_name for create_before_destroy with name_attribute",
                );
                assert_eq!(temp.attribute, "bucket_name");
                assert_eq!(temp.original_value, "my-bucket");
                assert!(
                    temp.temporary_value.starts_with("my-bucket-"),
                    "Temporary value '{}' should start with 'my-bucket-'",
                    temp.temporary_value
                );
                assert_eq!(temp.temporary_value.len(), "my-bucket-".len() + 8);
                // bucket_name is create-only, so can_rename should be false
                assert!(!temp.can_rename);
                // The `to` resource should have the temporary name
                assert_eq!(
                    to.attributes.get("bucket_name"),
                    Some(&Value::String(temp.temporary_value.clone()))
                );
            }
            other => panic!("Expected Replace, got {:?}", other),
        }
    }

    #[test]
    fn create_before_destroy_generates_temporary_name_with_can_rename() {
        use crate::schema::{AttributeSchema, AttributeType};

        let mut resource = Resource::new("logs.log_group", "my-log-group")
            .with_attribute(
                "log_group_name".to_string(),
                Value::String("my-log-group".to_string()),
            )
            .with_attribute("kms_key_id", Value::String("new-key".to_string()));
        resource.lifecycle.create_before_destroy = true;

        let resources = vec![resource];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "log_group_name".to_string(),
            Value::String("my-log-group".to_string()),
        );
        attrs.insert(
            "kms_key_id".to_string(),
            Value::String("old-key".to_string()),
        );
        current_states.insert(
            ResourceId::new("logs.log_group", "my-log-group"),
            State::existing(ResourceId::new("logs.log_group", "my-log-group"), attrs),
        );

        let mut schemas = HashMap::new();
        schemas.insert(
            "logs.log_group".to_string(),
            ResourceSchema::new("logs.log_group")
                .attribute(
                    // log_group_name is NOT create-only in this test (can be renamed)
                    AttributeSchema::new("log_group_name", AttributeType::String),
                )
                .attribute(AttributeSchema::new("kms_key_id", AttributeType::String).create_only())
                .with_name_attribute("log_group_name"),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Replace { temporary_name, .. } => {
                let temp = temporary_name.as_ref().expect("Should have temporary_name");
                assert_eq!(temp.attribute, "log_group_name");
                assert_eq!(temp.original_value, "my-log-group");
                // log_group_name is not create-only, so can_rename should be true
                assert!(temp.can_rename);
            }
            other => panic!("Expected Replace, got {:?}", other),
        }
    }

    #[test]
    fn no_temporary_name_without_create_before_destroy() {
        use crate::schema::{AttributeSchema, AttributeType};

        // Default lifecycle (create_before_destroy = false)
        let resources = vec![
            Resource::new("s3.bucket", "my-bucket")
                .with_attribute("bucket_name", Value::String("my-bucket".to_string()))
                .with_attribute("object_lock_enabled", Value::Bool(true)),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "bucket_name".to_string(),
            Value::String("my-bucket".to_string()),
        );
        attrs.insert("object_lock_enabled".to_string(), Value::Bool(false));
        current_states.insert(
            ResourceId::new("s3.bucket", "my-bucket"),
            State::existing(ResourceId::new("s3.bucket", "my-bucket"), attrs),
        );

        let mut schemas = HashMap::new();
        schemas.insert(
            "s3.bucket".to_string(),
            ResourceSchema::new("s3.bucket")
                .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only())
                .attribute(
                    AttributeSchema::new("object_lock_enabled", AttributeType::Bool).create_only(),
                )
                .with_name_attribute("bucket_name"),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Replace { temporary_name, .. } => {
                assert!(
                    temporary_name.is_none(),
                    "Should not have temporary_name without create_before_destroy"
                );
            }
            other => panic!("Expected Replace, got {:?}", other),
        }
    }

    #[test]
    fn no_temporary_name_when_name_prefix_is_used() {
        use crate::schema::{AttributeSchema, AttributeType};

        let mut resource = Resource::new("s3.bucket", "my-bucket")
            .with_attribute("bucket_name", Value::String("my-app-abc12345".to_string()))
            .with_attribute("object_lock_enabled", Value::Bool(true));
        resource.lifecycle.create_before_destroy = true;
        // Simulate that name_prefix was used
        resource
            .prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());

        let resources = vec![resource];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "bucket_name".to_string(),
            Value::String("my-app-abc12345".to_string()),
        );
        attrs.insert("object_lock_enabled".to_string(), Value::Bool(false));
        current_states.insert(
            ResourceId::new("s3.bucket", "my-bucket"),
            State::existing(ResourceId::new("s3.bucket", "my-bucket"), attrs),
        );

        let mut schemas = HashMap::new();
        schemas.insert(
            "s3.bucket".to_string(),
            ResourceSchema::new("s3.bucket")
                .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only())
                .attribute(
                    AttributeSchema::new("object_lock_enabled", AttributeType::Bool).create_only(),
                )
                .with_name_attribute("bucket_name"),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Replace { temporary_name, .. } => {
                assert!(
                    temporary_name.is_none(),
                    "Should not generate temporary_name when name_prefix is used"
                );
            }
            other => panic!("Expected Replace, got {:?}", other),
        }
    }

    #[test]
    fn no_temporary_name_without_name_attribute_in_schema() {
        use crate::schema::{AttributeSchema, AttributeType};

        let mut resource = Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));
        resource.lifecycle.create_before_destroy = true;

        let resources = vec![resource];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        current_states.insert(
            ResourceId::new("ec2.vpc", "my-vpc"),
            State::existing(ResourceId::new("ec2.vpc", "my-vpc"), attrs),
        );

        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.vpc".to_string(),
            ResourceSchema::new("ec2.vpc")
                .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only()),
            // No name_attribute set
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Replace { temporary_name, .. } => {
                assert!(
                    temporary_name.is_none(),
                    "Should not generate temporary_name without name_attribute in schema"
                );
            }
            other => panic!("Expected Replace, got {:?}", other),
        }
    }

    #[test]
    fn no_temporary_name_when_name_attribute_changes() {
        use crate::schema::{AttributeSchema, AttributeType};

        // name_attribute itself changed: old-bucket → new-bucket
        // No temporary name needed since names are already different
        let mut resource = Resource::new("s3.bucket", "my-bucket")
            .with_attribute("bucket_name", Value::String("new-bucket".to_string()))
            .with_attribute("object_lock_enabled", Value::Bool(true));
        resource.lifecycle.create_before_destroy = true;

        let resources = vec![resource];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "bucket_name".to_string(),
            Value::String("old-bucket".to_string()),
        );
        attrs.insert("object_lock_enabled".to_string(), Value::Bool(true));
        current_states.insert(
            ResourceId::new("s3.bucket", "my-bucket"),
            State::existing(ResourceId::new("s3.bucket", "my-bucket"), attrs),
        );

        let mut schemas = HashMap::new();
        schemas.insert(
            "s3.bucket".to_string(),
            ResourceSchema::new("s3.bucket")
                .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only())
                .attribute(
                    AttributeSchema::new("object_lock_enabled", AttributeType::Bool).create_only(),
                )
                .with_name_attribute("bucket_name"),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Replace { temporary_name, .. } => {
                assert!(
                    temporary_name.is_none(),
                    "Should not generate temporary_name when name_attribute value changes"
                );
            }
            other => panic!("Expected Replace, got {:?}", other),
        }
    }

    #[test]
    fn diff_detects_attribute_removal_with_prev_desired_keys() {
        // User previously had "region" and "tags" in .crn, now only has "region"
        let desired = Resource::new("s3.bucket", "test")
            .with_attribute("region", Value::String("ap-northeast-1".to_string()));

        let mut current_attrs = HashMap::new();
        current_attrs.insert(
            "region".to_string(),
            Value::String("ap-northeast-1".to_string()),
        );
        current_attrs.insert(
            "tags".to_string(),
            Value::Map(HashMap::from([(
                "Name".to_string(),
                Value::String("test".to_string()),
            )])),
        );
        let current = State::existing(ResourceId::new("s3.bucket", "test"), current_attrs);

        // Previous desired state had both "region" and "tags"
        let prev_keys = vec!["region".to_string(), "tags".to_string()];

        let result = diff(&desired, &current, None, Some(&prev_keys), None);
        match result {
            Diff::Update {
                changed_attributes, ..
            } => {
                assert!(
                    changed_attributes.contains(&"tags".to_string()),
                    "Should detect 'tags' removal, got: {:?}",
                    changed_attributes
                );
            }
            _ => panic!("Expected Update, got {:?}", result),
        }
    }

    #[test]
    fn diff_ignores_attributes_not_in_prev_desired_keys() {
        // Current state has "arn" and "region" from provider, but user only ever
        // specified "region" — "arn" was never in prev_desired_keys
        let desired = Resource::new("s3.bucket", "test");

        let mut current_attrs = HashMap::new();
        current_attrs.insert(
            "region".to_string(),
            Value::String("ap-northeast-1".to_string()),
        );
        current_attrs.insert(
            "arn".to_string(),
            Value::String("arn:aws:s3:::test".to_string()),
        );
        let current = State::existing(ResourceId::new("s3.bucket", "test"), current_attrs);

        // User previously only specified "region", not "arn"
        let prev_keys = vec!["region".to_string()];

        let result = diff(&desired, &current, None, Some(&prev_keys), None);
        match result {
            Diff::Update {
                changed_attributes, ..
            } => {
                assert!(
                    changed_attributes.contains(&"region".to_string()),
                    "Should detect 'region' removal"
                );
                assert!(
                    !changed_attributes.contains(&"arn".to_string()),
                    "Should NOT detect 'arn' removal since it was never in desired"
                );
            }
            _ => panic!("Expected Update, got {:?}", result),
        }
    }

    #[test]
    fn diff_no_change_without_prev_desired_keys() {
        // Without prev_desired_keys, removed attributes should NOT be detected
        let desired = Resource::new("s3.bucket", "test")
            .with_attribute("region", Value::String("ap-northeast-1".to_string()));

        let mut current_attrs = HashMap::new();
        current_attrs.insert(
            "region".to_string(),
            Value::String("ap-northeast-1".to_string()),
        );
        current_attrs.insert(
            "tags".to_string(),
            Value::Map(HashMap::from([(
                "Name".to_string(),
                Value::String("test".to_string()),
            )])),
        );
        let current = State::existing(ResourceId::new("s3.bucket", "test"), current_attrs);

        let result = diff(&desired, &current, None, None, None);
        assert!(
            matches!(result, Diff::NoChange(_)),
            "Without prev_desired_keys, extra attributes in current should not trigger Update, got {:?}",
            result
        );
    }

    #[test]
    fn create_plan_detects_attribute_removal() {
        // Resource in .crn has no "tags", but current state (from AWS) has tags.
        // prev_desired_keys indicates user previously had "region" and "tags".
        let resources = vec![
            Resource::new("s3.bucket", "test")
                .with_attribute("region", Value::String("ap-northeast-1".to_string())),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "region".to_string(),
            Value::String("ap-northeast-1".to_string()),
        );
        attrs.insert(
            "tags".to_string(),
            Value::Map(HashMap::from([(
                "Name".to_string(),
                Value::String("test".to_string()),
            )])),
        );
        current_states.insert(
            ResourceId::new("s3.bucket", "test"),
            State::existing(ResourceId::new("s3.bucket", "test"), attrs),
        );

        let mut prev_desired_keys = HashMap::new();
        prev_desired_keys.insert(
            ResourceId::new("s3.bucket", "test"),
            vec!["region".to_string(), "tags".to_string()],
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &prev_desired_keys,
        );

        assert_eq!(plan.effects().len(), 1);
        assert!(
            matches!(&plan.effects()[0], Effect::Update { .. }),
            "Expected Update effect for attribute removal, got {:?}",
            plan.effects()[0]
        );
    }

    #[test]
    fn create_plan_filters_non_removable_attribute_removal() {
        use crate::schema::{AttributeSchema, AttributeType};
        // When schema is available, only removable attributes should trigger removal.
        // "region" is not removable, "tags" is removable.
        let resources = vec![
            Resource::new("s3.bucket", "test")
                .with_attribute("region", Value::String("ap-northeast-1".to_string())),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "region".to_string(),
            Value::String("ap-northeast-1".to_string()),
        );
        attrs.insert(
            "tags".to_string(),
            Value::Map(HashMap::from([(
                "Name".to_string(),
                Value::String("test".to_string()),
            )])),
        );
        current_states.insert(
            ResourceId::new("s3.bucket", "test"),
            State::existing(ResourceId::new("s3.bucket", "test"), attrs),
        );

        let mut prev_desired_keys = HashMap::new();
        prev_desired_keys.insert(
            ResourceId::new("s3.bucket", "test"),
            vec!["region".to_string(), "tags".to_string()],
        );

        // Schema: tags is auto-removable (optional, not create-only),
        // region is explicitly non-removable (provider-inherited)
        let mut schemas = HashMap::new();
        schemas.insert(
            "s3.bucket".to_string(),
            ResourceSchema::new("s3.bucket")
                .attribute(AttributeSchema::new("region", AttributeType::String).non_removable())
                .attribute(AttributeSchema::new(
                    "tags",
                    AttributeType::Map(Box::new(AttributeType::String)),
                )),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &prev_desired_keys,
        );

        assert_eq!(plan.effects().len(), 1);
        match &plan.effects()[0] {
            Effect::Update {
                changed_attributes, ..
            } => {
                assert!(
                    changed_attributes.contains(&"tags".to_string()),
                    "Should detect removable 'tags' removal"
                );
                assert!(
                    !changed_attributes.contains(&"region".to_string()),
                    "Should NOT detect non-removable 'region' removal"
                );
            }
            _ => panic!("Expected Update effect"),
        }
    }

    #[test]
    fn create_plan_skips_update_when_only_non_removable_removal() {
        use crate::schema::{AttributeSchema, AttributeType};
        // When the only "change" is a non-removable attribute removal,
        // the plan should have no effects (no spurious Update).
        let resources = vec![
            Resource::new("s3.bucket", "test")
                .with_attribute("bucket", Value::String("my-bucket".to_string())),
        ];

        let mut current_states = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert("bucket".to_string(), Value::String("my-bucket".to_string()));
        attrs.insert(
            "region".to_string(),
            Value::String("ap-northeast-1".to_string()),
        );
        current_states.insert(
            ResourceId::new("s3.bucket", "test"),
            State::existing(ResourceId::new("s3.bucket", "test"), attrs),
        );

        let mut prev_desired_keys = HashMap::new();
        prev_desired_keys.insert(
            ResourceId::new("s3.bucket", "test"),
            vec!["bucket".to_string(), "region".to_string()],
        );

        // Schema: region is explicitly non-removable, bucket is required
        let mut schemas = HashMap::new();
        schemas.insert(
            "s3.bucket".to_string(),
            ResourceSchema::new("s3.bucket")
                .attribute(AttributeSchema::new("bucket", AttributeType::String).required())
                .attribute(AttributeSchema::new("region", AttributeType::String).non_removable()),
        );

        let plan = create_plan(
            &resources,
            &current_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &prev_desired_keys,
        );

        assert!(
            plan.effects().is_empty(),
            "Should not generate spurious Update for non-removable attribute removal, got {:?}",
            plan.effects()
        );
    }

    #[test]
    fn diff_skips_internal_attributes_in_removal_detection() {
        // prev_desired_keys includes "_internal" but it should be skipped
        let desired = Resource::new("s3.bucket", "test")
            .with_attribute("region", Value::String("ap-northeast-1".to_string()));

        let mut current_attrs = HashMap::new();
        current_attrs.insert(
            "region".to_string(),
            Value::String("ap-northeast-1".to_string()),
        );
        current_attrs.insert(
            "_internal".to_string(),
            Value::String("something".to_string()),
        );
        let current = State::existing(ResourceId::new("s3.bucket", "test"), current_attrs);

        let prev_keys = vec!["region".to_string(), "_internal".to_string()];

        let result = diff(&desired, &current, None, Some(&prev_keys), None);
        assert!(
            matches!(result, Diff::NoChange(_)),
            "Should skip internal attributes starting with '_', got {:?}",
            result
        );
    }

    // --- Type-aware comparison tests ---

    #[test]
    fn type_aware_int_float_coercion() {
        assert!(type_aware_equal(
            &Value::Int(42),
            &Value::Float(42.0),
            Some(&AttributeType::Float),
        ));
        assert!(type_aware_equal(
            &Value::Float(42.0),
            &Value::Int(42),
            Some(&AttributeType::Float),
        ));
        // Non-exact conversion should not be equal
        assert!(!type_aware_equal(
            &Value::Int(42),
            &Value::Float(42.5),
            Some(&AttributeType::Float),
        ));
        // Without type info, Int and Float are not equal
        assert!(!type_aware_equal(
            &Value::Int(42),
            &Value::Float(42.0),
            None,
        ));
    }

    #[test]
    fn type_aware_int_float_coercion_for_int_type() {
        // Int type also allows coercion (e.g., provider returns Float for an Int field)
        assert!(type_aware_equal(
            &Value::Int(10),
            &Value::Float(10.0),
            Some(&AttributeType::Int),
        ));
    }

    #[test]
    fn type_aware_string_enum_case_insensitive() {
        let enum_type = AttributeType::StringEnum {
            name: "Status".to_string(),
            values: vec!["Enabled".to_string(), "Disabled".to_string()],
            namespace: None,
            to_dsl: None,
        };
        // Case-insensitive match
        assert!(type_aware_equal(
            &Value::String("Enabled".to_string()),
            &Value::String("enabled".to_string()),
            Some(&enum_type),
        ));
        assert!(type_aware_equal(
            &Value::String("ENABLED".to_string()),
            &Value::String("enabled".to_string()),
            Some(&enum_type),
        ));
        // Different values are not equal
        assert!(!type_aware_equal(
            &Value::String("Enabled".to_string()),
            &Value::String("Disabled".to_string()),
            Some(&enum_type),
        ));
    }

    #[test]
    fn type_aware_string_enum_hyphen_underscore() {
        let enum_type = AttributeType::StringEnum {
            name: "Region".to_string(),
            values: vec!["ap-northeast-1".to_string()],
            namespace: None,
            to_dsl: Some(|s: &str| s.replace('-', "_")),
        };
        // Hyphen vs underscore should match via to_dsl normalization
        assert!(type_aware_equal(
            &Value::String("ap-northeast-1".to_string()),
            &Value::String("ap_northeast_1".to_string()),
            Some(&enum_type),
        ));
    }

    #[test]
    fn type_aware_list_with_inner_type() {
        let list_type = AttributeType::List(Box::new(AttributeType::Float));
        // List of Int vs Float with coercion
        assert!(type_aware_equal(
            &Value::List(vec![Value::Int(1), Value::Int(2)]),
            &Value::List(vec![Value::Float(2.0), Value::Float(1.0)]),
            Some(&list_type),
        ));
    }

    #[test]
    fn type_aware_struct_per_field() {
        use crate::schema::StructField;

        let struct_type = AttributeType::Struct {
            name: "Config".to_string(),
            fields: vec![
                StructField::new("count", AttributeType::Float),
                StructField::new("name", AttributeType::String),
            ],
        };
        let a = Value::Map(HashMap::from([
            ("count".to_string(), Value::Int(5)),
            ("name".to_string(), Value::String("test".to_string())),
        ]));
        let b = Value::Map(HashMap::from([
            ("count".to_string(), Value::Float(5.0)),
            ("name".to_string(), Value::String("test".to_string())),
        ]));
        assert!(type_aware_equal(&a, &b, Some(&struct_type)));
    }

    #[test]
    fn type_aware_union_numeric() {
        let union_type = AttributeType::Union(vec![AttributeType::Int, AttributeType::Float]);
        assert!(type_aware_equal(
            &Value::Int(7),
            &Value::Float(7.0),
            Some(&union_type),
        ));
    }

    #[test]
    fn type_aware_custom_delegates_to_base() {
        let custom_type = AttributeType::Custom {
            name: "Port".to_string(),
            base: Box::new(AttributeType::Float),
            validate: |_| Ok(()),
            namespace: None,
            to_dsl: None,
        };
        assert!(type_aware_equal(
            &Value::Int(8080),
            &Value::Float(8080.0),
            Some(&custom_type),
        ));
    }

    #[test]
    fn type_aware_diff_no_change_with_schema() {
        use crate::schema::{AttributeSchema, ResourceSchema};

        let mut schema = ResourceSchema::new("test.resource");
        schema.attributes.insert(
            "port".to_string(),
            AttributeSchema::new("port", AttributeType::Float),
        );

        let desired =
            Resource::new("test.resource", "test").with_attribute("port", Value::Int(443));

        let mut current_attrs = HashMap::new();
        current_attrs.insert("port".to_string(), Value::Float(443.0));
        let current = State::existing(ResourceId::new("test.resource", "test"), current_attrs);

        // Without schema: detects a change (Int != Float)
        let result = diff(&desired, &current, None, None, None);
        assert!(
            matches!(result, Diff::Update { .. }),
            "Without schema, Int(443) != Float(443.0) should be Update, got {:?}",
            result
        );

        // With schema: no change (type-aware coercion)
        let result = diff(&desired, &current, None, None, Some(&schema));
        assert!(
            matches!(result, Diff::NoChange(_)),
            "With schema, Int(443) and Float(443.0) should be NoChange, got {:?}",
            result
        );
    }
}
