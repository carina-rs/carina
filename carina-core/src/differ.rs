//! Differ - Compare desired state with current state to generate a Plan
//!
//! Compares the "desired state" declared in DSL with the "current state" fetched
//! from the Provider, and generates a list of required Effects (Plan).

use std::collections::HashMap;

use crate::effect::Effect;
use crate::plan::Plan;
use crate::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
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

/// Compare desired state with current state to compute a Diff
pub fn diff(desired: &Resource, current: &State) -> Diff {
    if !current.exists {
        return Diff::Create(desired.clone());
    }

    let changed = find_changed_attributes(&desired.attributes, &current.attributes);

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
    resource_type: &str,
    changed_attributes: &[String],
    schemas: &HashMap<String, ResourceSchema>,
) -> Vec<String> {
    // Try to find the schema — look up by resource_type directly,
    // then try with common provider prefixes
    let schema = schemas
        .get(resource_type)
        .or_else(|| schemas.get(&format!("awscc.{}", resource_type)))
        .or_else(|| schemas.get(&format!("aws.{}", resource_type)));

    let Some(schema) = schema else {
        return Vec::new();
    };

    let create_only_attrs = schema.create_only_attributes();
    changed_attributes
        .iter()
        .filter(|attr| create_only_attrs.contains(&attr.as_str()))
        .cloned()
        .collect()
}

/// Find changed attributes between desired and current state
fn find_changed_attributes(
    desired: &HashMap<String, Value>,
    current: &HashMap<String, Value>,
) -> Vec<String> {
    let mut changed = Vec::new();

    for (key, desired_value) in desired {
        // Skip internal attributes (starting with _)
        if key.starts_with('_') {
            continue;
        }

        match current.get(key) {
            Some(current_value) if current_value == desired_value => {}
            _ => changed.push(key.clone()),
        }
    }

    changed
}

/// Compute Diff for multiple resources and generate a Plan
///
/// The `lifecycles` map provides lifecycle configuration for orphaned resources
/// (resources in state but not in desired). For desired resources, the lifecycle
/// is read directly from the Resource struct.
pub fn create_plan(
    desired: &[Resource],
    current_states: &HashMap<ResourceId, State>,
    lifecycles: &HashMap<ResourceId, LifecycleConfig>,
    schemas: &HashMap<String, ResourceSchema>,
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

        let d = diff(resource, &current);

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
                    &resource.id.resource_type,
                    &changed_attributes,
                    schemas,
                );

                if changed_create_only.is_empty() {
                    plan.add(Effect::Update { id, from, to });
                } else {
                    plan.add(Effect::Replace {
                        id,
                        from,
                        to,
                        lifecycle: resource.lifecycle.clone(),
                        changed_create_only,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_create_when_not_exists() {
        let desired = Resource::new("bucket", "test");
        let current = State::not_found(ResourceId::new("bucket", "test"));

        let result = diff(&desired, &current);
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

        let result = diff(&desired, &current);
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

        let result = diff(&desired, &current);
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

        let result = diff(&desired, &current);
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

        let plan = create_plan(&desired, &current_states, &HashMap::new(), &HashMap::new());

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

        let result = diff(&desired, &current);
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

        let plan = create_plan(&resources, &current_states, &HashMap::new(), &schemas);

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

        let plan = create_plan(&resources, &current_states, &HashMap::new(), &schemas);

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

        let plan = create_plan(&resources, &current_states, &HashMap::new(), &schemas);

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

        let plan = create_plan(&resources, &current_states, &HashMap::new(), &schemas);

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
    fn replace_with_provider_prefixed_schema_key() {
        use crate::schema::{AttributeSchema, AttributeType};

        // In production, schemas are keyed by "awscc.ec2.vpc" but resource_type is "ec2.vpc"
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

        // Schema keyed with provider prefix (as in production)
        let mut schemas = HashMap::new();
        schemas.insert(
            "awscc.ec2.vpc".to_string(),
            crate::schema::ResourceSchema::new("awscc.ec2.vpc")
                .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only()),
        );

        let plan = create_plan(&resources, &current_states, &HashMap::new(), &schemas);

        assert_eq!(plan.effects().len(), 1);
        assert!(
            matches!(plan.effects()[0], Effect::Replace { .. }),
            "Expected Replace with awscc-prefixed schema key, got {:?}",
            plan.effects()[0]
        );
    }
}
