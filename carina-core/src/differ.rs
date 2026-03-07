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
use crate::schema::ResourceSchema;

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
pub fn diff(desired: &Resource, current: &State, saved: Option<&HashMap<String, Value>>) -> Diff {
    if !current.exists {
        return Diff::Create(desired.clone());
    }

    let changed = find_changed_attributes(&desired.attributes, &current.attributes, saved);

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

/// Find changed attributes between desired and current state.
/// If `saved` is provided, each desired value is merged with the saved value
/// before comparison, filling in unmanaged nested fields.
fn find_changed_attributes(
    desired: &HashMap<String, Value>,
    current: &HashMap<String, Value>,
    saved: Option<&HashMap<String, Value>>,
) -> Vec<String> {
    let mut changed = Vec::new();

    for (key, desired_value) in desired {
        // Skip internal attributes (starting with _)
        if key.starts_with('_') {
            continue;
        }

        let is_equal = match saved.and_then(|s| s.get(key)) {
            Some(saved_value) => {
                let effective_desired = merge_with_saved(desired_value, saved_value);
                current
                    .get(key)
                    .map(|cv| cv.semantically_equal(&effective_desired))
                    .unwrap_or(false)
            }
            None => current
                .get(key)
                .map(|cv| cv.semantically_equal(desired_value))
                .unwrap_or(false),
        };

        if !is_equal {
            changed.push(key.clone());
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
/// Returns `None` if no temporary name is needed (no name_attribute, or
/// the resource already uses name_prefix for that attribute).
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
pub fn create_plan(
    desired: &[Resource],
    current_states: &HashMap<ResourceId, State>,
    lifecycles: &HashMap<ResourceId, LifecycleConfig>,
    schemas: &HashMap<String, ResourceSchema>,
    saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
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
        let d = diff(resource, &current, saved);

        match d {
            Diff::Create(r) => plan.add(Effect::Create(r)),
            Diff::Update {
                id,
                from,
                to,
                changed_attributes,
            } => {
                // Check if any changed attributes are create-only
                let changed_create_only = find_changed_create_only(
                    &resource.id.provider,
                    &resource.id.resource_type,
                    &changed_attributes,
                    schemas,
                );

                if changed_create_only.is_empty() {
                    plan.add(Effect::Update { id, from, to });
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

        let result = diff(&desired, &current, None);
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

        let result = diff(&desired, &current, None);
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

        let result = diff(&desired, &current, None);
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

        let result = diff(&desired, &current, None);
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

        let result = diff(&desired, &current, None);
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
        );

        assert_eq!(plan.effects().len(), 1);
        assert!(
            matches!(plan.effects()[0], Effect::Update { .. }),
            "Expected Update, got {:?}",
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

        let result = diff(&desired, &current, None);
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

        let result = diff(&desired, &current, Some(&saved_map));
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

        let result = diff(&desired, &current, Some(&saved_map));
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

        let result = diff(&desired, &current, Some(&saved_map));
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
        let result = diff(&desired, &current, None);
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
}
