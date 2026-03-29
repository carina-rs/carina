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
) -> Result<(), String> {
    // Save dependency bindings before resolution destroys ResourceRef values.
    // This metadata is used by plan tree building to recover parent-child
    // relationships (see build_plan_tree in display.rs and app.rs).
    for resource in resources.iter_mut() {
        let deps = get_resource_dependencies(resource);
        if !deps.is_empty() {
            let dep_list: Vec<String> = deps.into_iter().collect();
            resource.dependency_bindings = dep_list;
        }
    }

    // Build a map of binding_name -> attributes (merged from DSL and AWS state)
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

    for resource in resources.iter() {
        if let Some(ref binding_name) = resource.binding {
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
            resolved_attrs.insert(key.clone(), resolve_ref_value(value, &binding_map)?);
        }
        resource.attributes = resolved_attrs;
    }
    Ok(())
}

/// Recursively resolve a single Value, replacing ResourceRef with the referenced value.
///
/// If the referenced binding or attribute is not found, the value is returned as-is.
/// Returns an error if a builtin function fails with fully-resolved arguments.
pub fn resolve_ref_value(
    value: &Value,
    binding_map: &HashMap<String, HashMap<String, Value>>,
) -> Result<Value, String> {
    match value {
        Value::ResourceRef {
            binding_name,
            attribute_name,
            field_path,
        } => {
            if let Some(attrs) = binding_map.get(binding_name)
                && let Some(attr_value) = attrs.get(attribute_name)
            {
                // Resolve the initial attribute value
                let mut resolved = resolve_ref_value(attr_value, binding_map)?;

                // Traverse chained field path through nested maps
                for field in field_path {
                    match resolved {
                        Value::Map(ref map) => {
                            if let Some(nested) = map.get(field) {
                                resolved = resolve_ref_value(nested, binding_map)?;
                            } else {
                                // Field not found in nested map, keep original ref
                                return Ok(value.clone());
                            }
                        }
                        _ => {
                            // Cannot traverse non-map value, keep original ref
                            return Ok(value.clone());
                        }
                    }
                }

                return Ok(resolved);
            }
            // Keep as-is if not found
            Ok(value.clone())
        }
        Value::List(items) => {
            let resolved: Result<Vec<Value>, String> = items
                .iter()
                .map(|v| resolve_ref_value(v, binding_map))
                .collect();
            Ok(Value::List(resolved?))
        }
        Value::Map(map) => {
            let mut resolved = HashMap::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_ref_value(v, binding_map)?);
            }
            Ok(Value::Map(resolved))
        }
        Value::Interpolation(parts) => {
            let resolved_parts: Result<Vec<InterpolationPart>, String> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        Ok(InterpolationPart::Expr(resolve_ref_value(v, binding_map)?))
                    }
                    other => Ok(other.clone()),
                })
                .collect();
            let resolved_parts = resolved_parts?;

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
                Ok(Value::String(s))
            } else {
                Ok(Value::Interpolation(resolved_parts))
            }
        }
        Value::FunctionCall { name, args } => {
            // First, resolve all arguments
            let resolved_args: Result<Vec<Value>, String> = args
                .iter()
                .map(|a| resolve_ref_value(a, binding_map))
                .collect();
            let resolved_args = resolved_args?;

            // Check if all args are fully resolved (no remaining refs)
            let all_resolved = resolved_args.iter().all(|a| !contains_resource_ref(a));

            if all_resolved {
                // Evaluate the built-in function; propagate errors since args are resolved
                match crate::builtins::evaluate_builtin(name, &resolved_args) {
                    Ok(result) => Ok(result),
                    Err(e) => Err(format!("{}(): {}", name, e)),
                }
            } else {
                // Keep as FunctionCall with partially resolved args
                Ok(Value::FunctionCall {
                    name: name.clone(),
                    args: resolved_args,
                })
            }
        }
        Value::Secret(inner) => {
            let resolved_inner = resolve_ref_value(inner, binding_map)?;
            Ok(Value::Secret(Box::new(resolved_inner)))
        }
        _ => Ok(value.clone()),
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
        Value::FunctionCall { args, .. } => args.iter().any(contains_resource_ref),
        Value::Secret(inner) => contains_resource_ref(inner),
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
        Value::Secret(inner) => value_to_string(inner),
        _ => crate::value::format_value(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ResourceId;

    fn make_resource(name: &str, binding: Option<&str>, attrs: Vec<(&str, Value)>) -> Resource {
        let attributes: HashMap<String, Value> =
            attrs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let mut r = Resource::new("test.resource", name);
        r.attributes = attributes;
        r.binding = binding.map(|b| b.to_string());
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
            field_path: vec![],
        };

        let resolved = resolve_ref_value(&ref_value, &binding_map).unwrap();
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
                field_path: vec![],
            },
        ]);

        let resolved = resolve_ref_value(&list, &binding_map).unwrap();
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
                    field_path: vec![],
                },
            )]
            .into_iter()
            .collect(),
        );

        let resolved = resolve_ref_value(&map, &binding_map).unwrap();
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
            field_path: vec![],
        };

        let resolved = resolve_ref_value(&ref_value, &binding_map).unwrap();
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
                field_path: vec![],
            }),
        ]);

        let resolved = resolve_ref_value(&interp, &binding_map).unwrap();
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
                field_path: vec![],
            }),
        ]);

        let resolved = resolve_ref_value(&interp, &binding_map).unwrap();
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

        let resolved = resolve_ref_value(&interp, &binding_map).unwrap();
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
                        field_path: vec![],
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

        resolve_refs_with_state(&mut resources, &current_states).unwrap();

        // The subnet's vpc_id should be resolved from state
        assert_eq!(
            resources[1].attributes.get("vpc_id"),
            Some(&Value::String("vpc-abc".to_string()))
        );
    }

    #[test]
    fn test_resolve_function_call_join() {
        let binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let func = Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                    Value::String("c".to_string()),
                ]),
            ],
        };

        let resolved = resolve_ref_value(&func, &binding_map).unwrap();
        assert_eq!(resolved, Value::String("a-b-c".to_string()));
    }

    #[test]
    fn test_resolve_function_call_with_resource_ref() {
        let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
        binding_map.insert(
            "vpc".to_string(),
            HashMap::from([("id".to_string(), Value::String("vpc-123".to_string()))]),
        );

        // join("-", ["prefix", vpc.id]) should resolve vpc.id first, then evaluate
        let func = Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::List(vec![
                    Value::String("prefix".to_string()),
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "id".to_string(),
                        field_path: vec![],
                    },
                ]),
            ],
        };

        let resolved = resolve_ref_value(&func, &binding_map).unwrap();
        assert_eq!(resolved, Value::String("prefix-vpc-123".to_string()));
    }

    #[test]
    fn test_resolve_function_call_unresolved_ref_kept() {
        let binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

        // If a ResourceRef in the args can't be resolved, the FunctionCall is kept
        let func = Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::List(vec![Value::ResourceRef {
                    binding_name: "unknown".to_string(),
                    attribute_name: "id".to_string(),
                    field_path: vec![],
                }]),
            ],
        };

        let resolved = resolve_ref_value(&func, &binding_map).unwrap();
        assert!(matches!(resolved, Value::FunctionCall { .. }));
    }

    #[test]
    fn test_resolve_chained_field_access() {
        let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

        // web binding has a nested map: network = { vpc_id = "vpc-123" }
        let mut network_map = HashMap::new();
        network_map.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        let mut attrs = HashMap::new();
        attrs.insert("network".to_string(), Value::Map(network_map));
        binding_map.insert("web".to_string(), attrs);

        // web.network.vpc_id should resolve to "vpc-123"
        let ref_value = Value::ResourceRef {
            binding_name: "web".to_string(),
            attribute_name: "network".to_string(),
            field_path: vec!["vpc_id".to_string()],
        };

        let resolved = resolve_ref_value(&ref_value, &binding_map).unwrap();
        assert_eq!(resolved, Value::String("vpc-123".to_string()));
    }

    #[test]
    fn test_resolve_deeply_chained_field_access() {
        let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

        // web.output.network.vpc_id
        let mut inner_map = HashMap::new();
        inner_map.insert("vpc_id".to_string(), Value::String("vpc-456".to_string()));
        let mut output_map = HashMap::new();
        output_map.insert("network".to_string(), Value::Map(inner_map));
        let mut attrs = HashMap::new();
        attrs.insert("output".to_string(), Value::Map(output_map));
        binding_map.insert("web".to_string(), attrs);

        let ref_value = Value::ResourceRef {
            binding_name: "web".to_string(),
            attribute_name: "output".to_string(),
            field_path: vec!["network".to_string(), "vpc_id".to_string()],
        };

        let resolved = resolve_ref_value(&ref_value, &binding_map).unwrap();
        assert_eq!(resolved, Value::String("vpc-456".to_string()));
    }

    #[test]
    fn test_resolve_chained_field_missing_key_keeps_ref() {
        let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let mut network_map = HashMap::new();
        network_map.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        let mut attrs = HashMap::new();
        attrs.insert("network".to_string(), Value::Map(network_map));
        binding_map.insert("web".to_string(), attrs);

        // web.network.nonexistent should keep original ref
        let ref_value = Value::ResourceRef {
            binding_name: "web".to_string(),
            attribute_name: "network".to_string(),
            field_path: vec!["nonexistent".to_string()],
        };

        let resolved = resolve_ref_value(&ref_value, &binding_map).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn resolve_builtin_error_propagated_when_args_resolved() {
        // env() with a var name that is extremely unlikely to be set should propagate error
        let binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let value = Value::FunctionCall {
            name: "env".to_string(),
            args: vec![Value::String(
                "CARINA_RESOLVER_TEST_NONEXISTENT_VAR_12345".to_string(),
            )],
        };

        let result = resolve_ref_value(&value, &binding_map);
        assert!(
            result.is_err(),
            "Expected error for env() with missing var, got: {:?}",
            result
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("CARINA_RESOLVER_TEST_NONEXISTENT_VAR_12345"),
            "Error should mention the missing env var, got: {}",
            err_msg
        );
    }

    #[test]
    fn resolve_builtin_with_unresolved_ref_stays_as_function_call() {
        // join("-", vpc.tags) should stay as FunctionCall when vpc.tags is unresolved
        let binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let value = Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "tags".to_string(),
                    field_path: vec![],
                },
            ],
        };

        let result = resolve_ref_value(&value, &binding_map);
        assert!(result.is_ok(), "Unresolved ref should not cause error");
        match result.unwrap() {
            Value::FunctionCall { name, .. } => assert_eq!(name, "join"),
            other => panic!("Expected FunctionCall, got: {:?}", other),
        }
    }

    #[test]
    fn resolve_refs_with_state_propagates_builtin_error() {
        let mut resources = vec![make_resource(
            "test",
            None,
            vec![(
                "value",
                Value::FunctionCall {
                    name: "env".to_string(),
                    args: vec![Value::String(
                        "CARINA_RESOLVER_STATE_TEST_NONEXISTENT_VAR_12345".to_string(),
                    )],
                },
            )],
        )];

        let current_states: HashMap<ResourceId, State> = HashMap::new();
        let result = resolve_refs_with_state(&mut resources, &current_states);
        assert!(
            result.is_err(),
            "Expected error from resolve_refs_with_state, got Ok"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("CARINA_RESOLVER_STATE_TEST_NONEXISTENT_VAR_12345"),
            "Error should mention the missing env var, got: {}",
            err_msg
        );
    }
}
