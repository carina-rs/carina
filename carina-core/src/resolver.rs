//! Reference resolution for ResourceRef values
//!
//! Resolves `Value::ResourceRef` references by looking up bound resource attributes
//! from both the DSL definition and current infrastructure state.

use std::collections::HashMap;

use crate::deps::get_resource_dependencies;
use crate::resource::{InterpolationPart, Resource, ResourceId, State, Value};

/// Resolve all ResourceRef values in resources using current state.
///
/// Builds a binding map from resource attributes and state, then resolves
/// all ResourceRef values across all resources.
///
/// Before resolving, saves dependency binding names as `_dependency_bindings`
/// metadata on each resource. This preserves dependency information that would
/// otherwise be lost when ResourceRef values are replaced with plain strings.
pub fn resolve_refs_with_state(
    resources: &mut [Resource],
    current_states: &HashMap<ResourceId, State>,
) {
    // Save dependency bindings before resolution destroys ResourceRef values.
    // This metadata is used by plan tree building to recover parent-child
    // relationships (see build_plan_tree in display.rs and app.rs).
    for resource in resources.iter_mut() {
        let deps = get_resource_dependencies(resource);
        if !deps.is_empty() {
            let dep_list: Vec<Value> = deps.into_iter().map(Value::String).collect();
            resource
                .attributes
                .insert("_dependency_bindings".to_string(), Value::List(dep_list));
        }
    }

    // Build a map of binding_name -> attributes (merged from DSL and AWS state)
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

    for resource in resources.iter() {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            let mut attrs = resource.attributes.clone();

            // Merge AWS state attributes (like `id`) if available
            if let Some(state) = current_states.get(&resource.id)
                && state.exists
            {
                for (k, v) in &state.attributes {
                    if !attrs.contains_key(k) {
                        attrs.insert(k.clone(), v.clone());
                    }
                }
            }

            binding_map.insert(binding_name.clone(), attrs);
        }
    }

    // Resolve ResourceRef values in all resources
    for resource in resources.iter_mut() {
        let mut resolved_attrs = HashMap::new();
        for (key, value) in &resource.attributes {
            resolved_attrs.insert(key.clone(), resolve_ref_value(value, &binding_map));
        }
        resource.attributes = resolved_attrs;
    }
}

/// Recursively resolve a single Value, replacing ResourceRef with the referenced value.
///
/// If the referenced binding or attribute is not found, the value is returned as-is.
pub fn resolve_ref_value(
    value: &Value,
    binding_map: &HashMap<String, HashMap<String, Value>>,
) -> Value {
    match value {
        Value::ResourceRef {
            binding_name,
            attribute_name,
            ..
        } => {
            if let Some(attrs) = binding_map.get(binding_name)
                && let Some(attr_value) = attrs.get(attribute_name)
            {
                // Recursively resolve
                return resolve_ref_value(attr_value, binding_map);
            }
            // Keep as-is if not found
            value.clone()
        }
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| resolve_ref_value(v, binding_map))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve_ref_value(v, binding_map)))
                .collect(),
        ),
        Value::Interpolation(parts) => {
            let resolved_parts: Vec<InterpolationPart> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        InterpolationPart::Expr(resolve_ref_value(v, binding_map))
                    }
                    other => other.clone(),
                })
                .collect();

            // Check if all parts are now resolved (no remaining ResourceRef)
            let all_resolved = resolved_parts.iter().all(|p| match p {
                InterpolationPart::Expr(v) => !contains_resource_ref(v),
                InterpolationPart::Literal(_) => true,
            });

            if all_resolved {
                // Concatenate all parts into a single String
                let s = resolved_parts
                    .iter()
                    .map(|p| match p {
                        InterpolationPart::Literal(s) => s.clone(),
                        InterpolationPart::Expr(v) => value_to_string(v),
                    })
                    .collect::<String>();
                Value::String(s)
            } else {
                Value::Interpolation(resolved_parts)
            }
        }
        _ => value.clone(),
    }
}

/// Check if a Value contains any ResourceRef (possibly nested)
fn contains_resource_ref(value: &Value) -> bool {
    match value {
        Value::ResourceRef { .. } => true,
        Value::List(items) => items.iter().any(contains_resource_ref),
        Value::Map(map) => map.values().any(contains_resource_ref),
        Value::Interpolation(parts) => parts.iter().any(|p| match p {
            InterpolationPart::Expr(v) => contains_resource_ref(v),
            _ => false,
        }),
        _ => false,
    }
}

/// Convert a Value to its string representation for interpolation
fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => crate::value::format_value(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ResourceId;

    fn make_resource(name: &str, binding: Option<&str>, attrs: Vec<(&str, Value)>) -> Resource {
        let mut attributes: HashMap<String, Value> =
            attrs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        if let Some(b) = binding {
            attributes.insert("_binding".to_string(), Value::String(b.to_string()));
        }
        let mut r = Resource::new("test.resource", name);
        r.attributes = attributes;
        r
    }

    #[test]
    fn test_resolve_simple_resource_ref() {
        let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert("id".to_string(), Value::String("vpc-123".to_string()));
        binding_map.insert("my_vpc".to_string(), attrs);

        let ref_value = Value::ResourceRef {
            binding_name: "my_vpc".to_string(),
            attribute_name: "id".to_string(),
        };

        let resolved = resolve_ref_value(&ref_value, &binding_map);
        assert_eq!(resolved, Value::String("vpc-123".to_string()));
    }

    #[test]
    fn test_resolve_nested_refs_in_list() {
        let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert("id".to_string(), Value::String("sg-456".to_string()));
        binding_map.insert("my_sg".to_string(), attrs);

        let list = Value::List(vec![
            Value::String("static".to_string()),
            Value::ResourceRef {
                binding_name: "my_sg".to_string(),
                attribute_name: "id".to_string(),
            },
        ]);

        let resolved = resolve_ref_value(&list, &binding_map);
        assert_eq!(
            resolved,
            Value::List(vec![
                Value::String("static".to_string()),
                Value::String("sg-456".to_string()),
            ])
        );
    }

    #[test]
    fn test_resolve_nested_refs_in_map() {
        let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert("id".to_string(), Value::String("subnet-789".to_string()));
        binding_map.insert("my_subnet".to_string(), attrs);

        let map = Value::Map(
            vec![(
                "subnet_id".to_string(),
                Value::ResourceRef {
                    binding_name: "my_subnet".to_string(),
                    attribute_name: "id".to_string(),
                },
            )]
            .into_iter()
            .collect(),
        );

        let resolved = resolve_ref_value(&map, &binding_map);
        if let Value::Map(m) = resolved {
            assert_eq!(
                m.get("subnet_id"),
                Some(&Value::String("subnet-789".to_string()))
            );
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_unresolved_ref_stays_as_is() {
        let binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let ref_value = Value::ResourceRef {
            binding_name: "nonexistent".to_string(),
            attribute_name: "id".to_string(),
        };

        let resolved = resolve_ref_value(&ref_value, &binding_map);
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_interpolation_all_resolved() {
        let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        binding_map.insert("my_vpc".to_string(), attrs);

        let interp = Value::Interpolation(vec![
            InterpolationPart::Literal("subnet-".to_string()),
            InterpolationPart::Expr(Value::ResourceRef {
                binding_name: "my_vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            }),
        ]);

        let resolved = resolve_ref_value(&interp, &binding_map);
        assert_eq!(resolved, Value::String("subnet-vpc-123".to_string()));
    }

    #[test]
    fn test_resolve_interpolation_partially_unresolved() {
        let binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let interp = Value::Interpolation(vec![
            InterpolationPart::Literal("subnet-".to_string()),
            InterpolationPart::Expr(Value::ResourceRef {
                binding_name: "my_vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            }),
        ]);

        let resolved = resolve_ref_value(&interp, &binding_map);
        // Should remain as Interpolation since the ref couldn't be resolved
        assert!(matches!(resolved, Value::Interpolation(_)));
    }

    #[test]
    fn test_resolve_interpolation_with_non_string_types() {
        let binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let interp = Value::Interpolation(vec![
            InterpolationPart::Literal("port-".to_string()),
            InterpolationPart::Expr(Value::Int(8080)),
            InterpolationPart::Literal("-enabled-".to_string()),
            InterpolationPart::Expr(Value::Bool(true)),
        ]);

        let resolved = resolve_ref_value(&interp, &binding_map);
        assert_eq!(
            resolved,
            Value::String("port-8080-enabled-true".to_string())
        );
    }

    #[test]
    fn test_state_attributes_merged_into_binding_map() {
        let rid = ResourceId::new("test.resource", "my-vpc");
        let mut resources = vec![
            make_resource(
                "my-vpc",
                Some("my_vpc"),
                vec![("cidr_block", Value::String("10.0.0.0/16".to_string()))],
            ),
            make_resource(
                "my-subnet",
                None,
                vec![(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "my_vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                    },
                )],
            ),
        ];

        let mut current_states = HashMap::new();
        current_states.insert(
            rid.clone(),
            State {
                id: rid,
                identifier: None,
                exists: true,
                attributes: vec![("vpc_id".to_string(), Value::String("vpc-abc".to_string()))]
                    .into_iter()
                    .collect(),
            },
        );

        resolve_refs_with_state(&mut resources, &current_states);

        // The subnet's vpc_id should be resolved from state
        assert_eq!(
            resources[1].attributes.get("vpc_id"),
            Some(&Value::String("vpc-abc".to_string()))
        );
    }
}
