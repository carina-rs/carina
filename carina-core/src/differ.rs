//! Differ - Compare desired state with current state to generate a Plan
//!
//! Compares the "desired state" declared in DSL with the "current state" fetched
//! from the Provider, and generates a list of required Effects (Plan).

use std::collections::HashMap;

use crate::effect::Effect;
use crate::plan::Plan;
use crate::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
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
    provider: &str,
    resource_type: &str,
    changed_attributes: &[String],
    schemas: &HashMap<String, ResourceSchema>,
) -> Vec<String> {
    // Try to find the schema — look up by resource_type directly,
    // then try with provider prefix
    let schema = schemas.get(resource_type).or_else(|| {
        if !provider.is_empty() {
            schemas.get(&format!("{}.{}", provider, resource_type))
        } else {
            None
        }
    });

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

/// Normalize a Value based on its AttributeType.
///
/// The parser produces `Value::Map(...)` for `= { ... }` syntax and
/// `Value::List(vec![Value::Map(...)])` for block syntax. The read path
/// (`aws_value_to_dsl`) always produces `Value::List(vec![Value::Map(...)])`
/// for bare Struct types. This function normalizes `Value::Map` to
/// `Value::List(vec![Value::Map])` for bare Struct types so both syntaxes
/// compare correctly against the read result.
fn normalize_value_for_type(value: &Value, attr_type: &AttributeType) -> Value {
    match (attr_type, value) {
        // Bare Struct + Map → wrap in List to match read path convention
        (AttributeType::Struct { fields, .. }, Value::Map(map)) => {
            let normalized_map: HashMap<String, Value> = map
                .iter()
                .map(|(k, v)| {
                    let field_type = fields.iter().find(|f| f.name == *k).map(|f| &f.field_type);
                    let normalized_v = if let Some(ft) = field_type {
                        normalize_value_for_type(v, ft)
                    } else {
                        v.clone()
                    };
                    (k.clone(), normalized_v)
                })
                .collect();
            Value::List(vec![Value::Map(normalized_map)])
        }
        // Bare Struct + List → normalize inner maps recursively
        (AttributeType::Struct { fields, .. }, Value::List(items)) => {
            let normalized: Vec<Value> = items
                .iter()
                .map(|item| {
                    if let Value::Map(map) = item {
                        let normalized_map: HashMap<String, Value> = map
                            .iter()
                            .map(|(k, v)| {
                                let field_type =
                                    fields.iter().find(|f| f.name == *k).map(|f| &f.field_type);
                                let normalized_v = if let Some(ft) = field_type {
                                    normalize_value_for_type(v, ft)
                                } else {
                                    v.clone()
                                };
                                (k.clone(), normalized_v)
                            })
                            .collect();
                        Value::Map(normalized_map)
                    } else {
                        item.clone()
                    }
                })
                .collect();
            Value::List(normalized)
        }
        // List(inner) → recurse into list items
        (AttributeType::List(inner), Value::List(items)) => {
            let normalized: Vec<Value> = items
                .iter()
                .map(|item| normalize_value_for_type(item, inner))
                .collect();
            Value::List(normalized)
        }
        _ => value.clone(),
    }
}

/// Normalize resource attributes based on schema types.
///
/// Converts `Value::Map` to `Value::List(vec![Value::Map])` for bare Struct
/// typed attributes, ensuring consistent representation between the two
/// equivalent DSL syntaxes (`= { ... }` and block `{ ... }`).
fn normalize_resource_attributes(
    resource: &Resource,
    schemas: &HashMap<String, ResourceSchema>,
) -> Resource {
    let schema = schemas.get(&resource.id.resource_type).or_else(|| {
        schemas.get(&format!(
            "{}.{}",
            resource.id.provider, resource.id.resource_type
        ))
    });

    let Some(schema) = schema else {
        return resource.clone();
    };

    let mut normalized = resource.clone();
    for (attr_name, value) in &resource.attributes {
        if let Some(attr_schema) = schema.attributes.get(attr_name) {
            let normalized_value = normalize_value_for_type(value, &attr_schema.attr_type);
            normalized
                .attributes
                .insert(attr_name.clone(), normalized_value);
        }
    }
    normalized
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

        // Normalize desired attributes so both `= { ... }` and block syntax
        // produce the same Value representation for Struct types
        let normalized_resource = normalize_resource_attributes(resource, schemas);
        let d = diff(&normalized_resource, &current);

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

        let plan = create_plan(&resources, &current_states, &HashMap::new(), &schemas);

        assert_eq!(plan.effects().len(), 1);
        assert!(
            matches!(plan.effects()[0], Effect::Replace { .. }),
            "Expected Replace with awscc-prefixed schema key, got {:?}",
            plan.effects()[0]
        );
    }

    #[test]
    fn normalize_map_to_list_for_bare_struct() {
        use crate::schema::StructField;

        let attr_type = AttributeType::Struct {
            name: "TestStruct".to_string(),
            fields: vec![
                StructField::new("name", AttributeType::String),
                StructField::new("value", AttributeType::String),
            ],
        };

        let mut map = HashMap::new();
        map.insert("name".to_string(), Value::String("foo".to_string()));
        map.insert("value".to_string(), Value::String("bar".to_string()));
        let input = Value::Map(map.clone());

        let result = normalize_value_for_type(&input, &attr_type);
        assert_eq!(result, Value::List(vec![Value::Map(map)]));
    }

    #[test]
    fn normalize_list_unchanged_for_bare_struct() {
        use crate::schema::StructField;

        let attr_type = AttributeType::Struct {
            name: "TestStruct".to_string(),
            fields: vec![StructField::new("name", AttributeType::String)],
        };

        let mut map = HashMap::new();
        map.insert("name".to_string(), Value::String("foo".to_string()));
        let input = Value::List(vec![Value::Map(map.clone())]);

        let result = normalize_value_for_type(&input, &attr_type);
        assert_eq!(result, Value::List(vec![Value::Map(map)]));
    }

    #[test]
    fn create_plan_normalizes_map_syntax_for_struct() {
        use crate::schema::{AttributeSchema, StructField};

        // Simulate: user wrote `config = { name = "test" }` (Map syntax)
        // for a Struct-typed attribute
        let struct_type = AttributeType::Struct {
            name: "Config".to_string(),
            fields: vec![StructField::new("name", AttributeType::String)],
        };

        let mut desired_map = HashMap::new();
        desired_map.insert("name".to_string(), Value::String("test".to_string()));

        let resource = Resource::with_provider("awscc", "test.resource", "my-res")
            .with_attribute("config", Value::Map(desired_map.clone()));

        // Simulate: aws_value_to_dsl returns List([Map]) for bare Struct
        let mut current_map = HashMap::new();
        current_map.insert("name".to_string(), Value::String("test".to_string()));
        let mut current_attrs = HashMap::new();
        current_attrs.insert(
            "config".to_string(),
            Value::List(vec![Value::Map(current_map)]),
        );
        let mut current_states = HashMap::new();
        current_states.insert(
            ResourceId::with_provider("awscc", "test.resource", "my-res"),
            State::existing(
                ResourceId::with_provider("awscc", "test.resource", "my-res"),
                current_attrs,
            ),
        );

        let mut schemas = HashMap::new();
        schemas.insert(
            "test.resource".to_string(),
            ResourceSchema::new("test.resource")
                .attribute(AttributeSchema::new("config", struct_type)),
        );

        let plan = create_plan(&[resource], &current_states, &HashMap::new(), &schemas);

        // Should detect NO change — Map and List([Map]) are equivalent for Struct
        assert!(
            plan.effects().is_empty(),
            "Expected no effects (no spurious diff), got {:?}",
            plan.effects()
        );
    }
}
