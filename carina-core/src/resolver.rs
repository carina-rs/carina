//! Reference resolution for ResourceRef values
//!
//! Resolves `Value::ResourceRef` references by looking up bound resource attributes
//! from both the DSL definition and current infrastructure state.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::binding_index::ResolvedBindings;
use crate::deps::get_resource_dependencies;
use crate::resource::{
    InterpolationPart, Resource, ResourceId, State, Value, contains_resource_ref,
};

/// Resolve all ResourceRef values in resources using current state.
///
/// Builds a binding map from resource attributes and state, then resolves
/// all ResourceRef values across all resources.
///
/// Before resolving, saves dependency binding names as `_dependency_bindings`
/// metadata on each resource. This preserves dependency information that would
/// otherwise be lost when ResourceRef values are replaced with plain strings.
///
/// `remote_bindings` provides external bindings from upstream state data sources.
/// Each entry maps an upstream_state binding name to a map of resource binding names
/// to their attributes. For example, `network -> { vpc -> { vpc_id -> "vpc-123" } }`.
pub fn resolve_refs_with_state(
    resources: &mut [Resource],
    current_states: &HashMap<ResourceId, State>,
) -> Result<(), String> {
    resolve_refs_with_state_and_remote(resources, current_states, &HashMap::new())
}

/// Resolve all ResourceRef values in resources using current state and upstream state bindings.
pub fn resolve_refs_with_state_and_remote(
    resources: &mut [Resource],
    current_states: &HashMap<ResourceId, State>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
) -> Result<(), String> {
    resolve_refs_inner(resources, current_states, remote_bindings, false)
}

/// Plan-only counterpart used when an upstream's state file is missing or
/// its export is absent. Behaves like
/// [`resolve_refs_with_state_and_remote`], but any surviving
/// `Value::ResourceRef` whose root binding is named in `remote_bindings`
/// is replaced with a marker `Value::String` (built via
/// `crate::parser::encode_unresolved_upstream_marker`) so plan display
/// can render it as `(known after upstream apply: <ref>)` instead of the
/// raw dot-form. `apply` continues to call the strict variant. See #2366.
pub fn resolve_refs_for_plan(
    resources: &mut [Resource],
    current_states: &HashMap<ResourceId, State>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
) -> Result<(), String> {
    resolve_refs_inner(resources, current_states, remote_bindings, true)
}

fn resolve_refs_inner(
    resources: &mut [Resource],
    current_states: &HashMap<ResourceId, State>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    mark_unresolved_upstream: bool,
) -> Result<(), String> {
    // Save dependency bindings before resolution destroys ResourceRef values.
    // This metadata is used by plan tree building to recover parent-child
    // relationships (see build_plan_tree in display.rs and app.rs).
    for resource in resources.iter_mut() {
        let deps = get_resource_dependencies(resource);
        if !deps.is_empty() {
            resource.dependency_bindings = deps.into_iter().collect();
        }
    }

    let bindings =
        ResolvedBindings::from_resources_with_state(resources, current_states, remote_bindings);

    let upstream_binding_names: std::collections::HashSet<&str> = if mark_unresolved_upstream {
        remote_bindings.keys().map(String::as_str).collect()
    } else {
        std::collections::HashSet::new()
    };

    // Resolve ResourceRef values in all resources. Stay in `IndexMap`
    // so the user's authored attribute order survives resolution
    // (#2222).
    for resource in resources.iter_mut() {
        let mut resolved_attrs: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        for (key, value) in &resource.attributes {
            let resolved = resolve_ref_value(value, &bindings)?;
            let final_value = if mark_unresolved_upstream {
                stamp_unresolved_upstream(resolved, &upstream_binding_names)
            } else {
                resolved
            };
            resolved_attrs.insert(key.clone(), final_value);
        }
        resource.attributes = resolved_attrs;
    }
    Ok(())
}

/// Replace any `Value::ResourceRef` whose root binding is in
/// `upstream_binding_names` with the unresolved-upstream marker string.
/// Recurses through `List` / `Map` / `Interpolation` / `FunctionCall` /
/// `Secret` so nested upstream refs (e.g. inside a `tags` map) are also
/// stamped. Subtrees containing no upstream refs are returned by move
/// without rebuilding.
fn stamp_unresolved_upstream(
    value: Value,
    upstream_binding_names: &std::collections::HashSet<&str>,
) -> Value {
    match value {
        Value::ResourceRef { ref path } if upstream_binding_names.contains(path.binding()) => {
            Value::String(crate::parser::encode_unresolved_upstream_marker(
                &path.to_dot_string(),
            ))
        }
        Value::List(items) => Value::List(
            items
                .into_iter()
                .map(|v| stamp_unresolved_upstream(v, upstream_binding_names))
                .collect(),
        ),
        Value::Map(map) => {
            let mut out: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                out.insert(k, stamp_unresolved_upstream(v, upstream_binding_names));
            }
            Value::Map(out)
        }
        Value::Interpolation(parts) => Value::Interpolation(
            parts
                .into_iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => InterpolationPart::Expr(
                        stamp_unresolved_upstream(v, upstream_binding_names),
                    ),
                    other => other,
                })
                .collect(),
        ),
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name,
            args: args
                .into_iter()
                .map(|a| stamp_unresolved_upstream(a, upstream_binding_names))
                .collect(),
        },
        Value::Secret(inner) => Value::Secret(Box::new(stamp_unresolved_upstream(
            *inner,
            upstream_binding_names,
        ))),
        // Stage 2 (RFC #2371) replaces this whole function — the
        // current `Value::String("\0upstream_unresolved:...")` output
        // becomes `Value::Unknown(UnknownReason::UpstreamRef { ... })`.
        // Until then no producer creates `Value::Unknown`.
        Value::Unknown(_) => {
            unimplemented!("Value::Unknown handling lands in RFC #2371 stage 2/3")
        }
        other => other,
    }
}

/// Recursively resolve a single Value, replacing ResourceRef with the referenced value.
///
/// If the referenced binding or attribute is not found, the value is returned as-is.
/// Returns an error if a builtin function fails with fully-resolved arguments.
pub fn resolve_ref_value(value: &Value, bindings: &ResolvedBindings) -> Result<Value, String> {
    match value {
        Value::ResourceRef { path } => {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            let field_path = path.field_path();
            if let Some(attrs) = bindings.get(binding_name)
                && let Some(attr_value) = attrs.get(attribute_name)
            {
                // Resolve the initial attribute value
                let mut resolved = resolve_ref_value(attr_value, bindings)?;

                // Traverse chained field path through nested maps
                for field in field_path {
                    match resolved {
                        Value::Map(ref map) => {
                            if let Some(nested) = map.get(field) {
                                resolved = resolve_ref_value(nested, bindings)?;
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

                // Descend into the resolved value by each post-field
                // subscript (`orgs.accounts[0]`, `orgs.matrix[0][1]`).
                // The validate-time shape check
                // (`check_upstream_state_subscript_shapes`) already
                // rejects kind mismatches against typed exports, so by
                // the time we get here the subscripts should fit the
                // shape — but the resolver still has to handle the
                // happy path and bail out cleanly when an out-of-range
                // index or missing key is encountered (e.g. resources
                // produced by `for` whose count differs from the
                // upstream's declared length).
                use crate::resource::Subscript;
                for sub in path.subscripts() {
                    match (resolved, sub) {
                        (Value::List(items), Subscript::Int { index }) => {
                            let idx = usize::try_from(*index).ok().filter(|i| *i < items.len());
                            match idx {
                                Some(i) => {
                                    resolved = resolve_ref_value(&items[i], bindings)?;
                                }
                                None => return Ok(value.clone()),
                            }
                        }
                        (Value::Map(map), Subscript::Str { key }) => match map.get(key) {
                            Some(nested) => {
                                resolved = resolve_ref_value(nested, bindings)?;
                            }
                            None => return Ok(value.clone()),
                        },
                        _ => return Ok(value.clone()),
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
                .map(|v| resolve_ref_value(v, bindings))
                .collect();
            Ok(Value::List(resolved?))
        }
        Value::Map(map) => {
            let mut resolved: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_ref_value(v, bindings)?);
            }
            Ok(Value::Map(resolved))
        }
        Value::Interpolation(parts) => {
            let resolved_parts: Result<Vec<InterpolationPart>, String> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        Ok(InterpolationPart::Expr(resolve_ref_value(v, bindings)?))
                    }
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Interpolation(resolved_parts?).canonicalize())
        }
        Value::FunctionCall { name, args } => {
            // First, resolve all arguments
            let resolved_args: Result<Vec<Value>, String> = args
                .iter()
                .map(|a| resolve_ref_value(a, bindings))
                .collect();
            let resolved_args = resolved_args?;

            // Check if all args are fully resolved (no remaining refs)
            let all_resolved = resolved_args.iter().all(|a| !contains_resource_ref(a));

            if all_resolved {
                // Evaluate the built-in function; propagate errors since args are resolved.
                // The evaluator boundary is `EvalValue::into_value` — a closure
                // returned here would mean `evaluate_builtin` saw fewer args
                // than the function's arity, which can only happen if a
                // partial application leaked through parsing. The parser is
                // supposed to surface that as a parse error, so treat any
                // closure here as a resolver-level invariant break.
                use crate::eval_value::EvalValue;
                let eval_args: Vec<EvalValue> = resolved_args
                    .iter()
                    .cloned()
                    .map(EvalValue::from_value)
                    .collect();
                match crate::builtins::evaluate_builtin(name, &eval_args) {
                    Ok(result) => result.into_value().map_err(|leak| {
                        format!(
                            "{}(): produced a closure '{}' (still needs {} arg(s)); \
                             this should have been caught at parse time",
                            name, leak.name, leak.remaining_arity
                        )
                    }),
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
            let resolved_inner = resolve_ref_value(inner, bindings)?;
            Ok(Value::Secret(Box::new(resolved_inner)))
        }
        // RFC #2371: stage 1 has no producer; stage 2 will route the
        // `Value::Unknown` cases through this resolver explicitly.
        Value::Unknown(_) => {
            unimplemented!("Value::Unknown handling lands in RFC #2371 stage 2/3")
        }
        _ => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ResourceId;

    fn make_resource(name: &str, binding: Option<&str>, attrs: Vec<(&str, Value)>) -> Resource {
        let mut r = Resource::new("test.resource", name);
        r.attributes = attrs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        r.binding = binding.map(|b| b.to_string());
        r
    }

    /// Build a `ResolvedBindings` from a flat `binding_name → attributes`
    /// map. Each entry is dropped in via `from_resources_with_state` so
    /// the resulting view is identical to what production code constructs
    /// — there is no test-only back door into the type.
    fn bindings_from(entries: Vec<(&str, Vec<(&str, Value)>)>) -> ResolvedBindings {
        let resources: Vec<Resource> = entries
            .into_iter()
            .map(|(binding, attrs)| {
                make_resource(&format!("{}-resource", binding), Some(binding), attrs)
            })
            .collect();
        ResolvedBindings::from_resources_with_state(&resources, &HashMap::new(), &HashMap::new())
    }

    #[test]
    fn test_resolve_subscript_descends_into_list() {
        // `orgs.accounts[0]` against `accounts: [a, b]` resolves to `a`.
        let bindings = bindings_from(vec![(
            "orgs",
            vec![(
                "accounts",
                Value::List(vec![
                    Value::String("alpha".to_string()),
                    Value::String("beta".to_string()),
                ]),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 0 }],
        );
        let ref_value = Value::ResourceRef { path };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, Value::String("alpha".to_string()));
    }

    #[test]
    fn test_resolve_subscript_descends_into_map() {
        // `orgs.accounts["alpha"]` against `accounts: { alpha = "1", beta = "2" }`
        // resolves to `"1"`.
        let map: indexmap::IndexMap<String, Value> = vec![
            ("alpha".to_string(), Value::String("1".to_string())),
            ("beta".to_string(), Value::String("2".to_string())),
        ]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![("orgs", vec![("accounts", Value::Map(map))])]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "alpha".to_string(),
            }],
        );
        let ref_value = Value::ResourceRef { path };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, Value::String("1".to_string()));
    }

    #[test]
    fn test_resolve_subscript_out_of_range_keeps_ref() {
        // `orgs.accounts[5]` against a 2-element list — out of range,
        // keep as-is so the planner can surface the unresolved ref.
        let bindings = bindings_from(vec![(
            "orgs",
            vec![(
                "accounts",
                Value::List(vec![Value::String("a".to_string())]),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 5 }],
        );
        let ref_value = Value::ResourceRef { path: path.clone() };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_simple_resource_ref() {
        let bindings = bindings_from(vec![(
            "my_vpc",
            vec![("id", Value::String("vpc-123".to_string()))],
        )]);

        let ref_value = Value::resource_ref("my_vpc".to_string(), "id".to_string(), vec![]);

        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, Value::String("vpc-123".to_string()));
    }

    #[test]
    fn test_resolve_nested_refs_in_list() {
        let bindings = bindings_from(vec![(
            "my_sg",
            vec![("id", Value::String("sg-456".to_string()))],
        )]);

        let list = Value::List(vec![
            Value::String("static".to_string()),
            Value::resource_ref("my_sg".to_string(), "id".to_string(), vec![]),
        ]);

        let resolved = resolve_ref_value(&list, &bindings).unwrap();
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
        let bindings = bindings_from(vec![(
            "my_subnet",
            vec![("id", Value::String("subnet-789".to_string()))],
        )]);

        let map = Value::Map(
            vec![(
                "subnet_id".to_string(),
                Value::resource_ref("my_subnet".to_string(), "id".to_string(), vec![]),
            )]
            .into_iter()
            .collect(),
        );

        let resolved = resolve_ref_value(&map, &bindings).unwrap();
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
        let bindings = ResolvedBindings::default();

        let ref_value = Value::resource_ref("nonexistent".to_string(), "id".to_string(), vec![]);

        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_interpolation_all_resolved() {
        let bindings = bindings_from(vec![(
            "my_vpc",
            vec![("vpc_id", Value::String("vpc-123".to_string()))],
        )]);

        let interp = Value::Interpolation(vec![
            InterpolationPart::Literal("subnet-".to_string()),
            InterpolationPart::Expr(Value::resource_ref(
                "my_vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        ]);

        let resolved = resolve_ref_value(&interp, &bindings).unwrap();
        assert_eq!(resolved, Value::String("subnet-vpc-123".to_string()));
    }

    #[test]
    fn test_resolve_interpolation_partially_unresolved() {
        let bindings = ResolvedBindings::default();

        let interp = Value::Interpolation(vec![
            InterpolationPart::Literal("subnet-".to_string()),
            InterpolationPart::Expr(Value::resource_ref(
                "my_vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        ]);

        let resolved = resolve_ref_value(&interp, &bindings).unwrap();
        // Should remain as Interpolation since the ref couldn't be resolved
        assert!(matches!(resolved, Value::Interpolation(_)));
    }

    #[test]
    fn test_resolve_interpolation_with_non_string_types() {
        let bindings = ResolvedBindings::default();

        let interp = Value::Interpolation(vec![
            InterpolationPart::Literal("port-".to_string()),
            InterpolationPart::Expr(Value::Int(8080)),
            InterpolationPart::Literal("-enabled-".to_string()),
            InterpolationPart::Expr(Value::Bool(true)),
        ]);

        let resolved = resolve_ref_value(&interp, &bindings).unwrap();
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
                    Value::resource_ref("my_vpc".to_string(), "vpc_id".to_string(), vec![]),
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
                dependency_bindings: std::collections::BTreeSet::new(),
            },
        );

        resolve_refs_with_state(&mut resources, &current_states).unwrap();

        // The subnet's vpc_id should be resolved from state
        assert_eq!(
            resources[1].get_attr("vpc_id"),
            Some(&Value::String("vpc-abc".to_string()))
        );
    }

    #[test]
    fn test_resolve_function_call_join() {
        let bindings = ResolvedBindings::default();

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

        let resolved = resolve_ref_value(&func, &bindings).unwrap();
        assert_eq!(resolved, Value::String("a-b-c".to_string()));
    }

    #[test]
    fn test_resolve_function_call_with_resource_ref() {
        let bindings = bindings_from(vec![(
            "vpc",
            vec![("id", Value::String("vpc-123".to_string()))],
        )]);

        // join("-", ["prefix", vpc.id]) should resolve vpc.id first, then evaluate
        let func = Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::List(vec![
                    Value::String("prefix".to_string()),
                    Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
                ]),
            ],
        };

        let resolved = resolve_ref_value(&func, &bindings).unwrap();
        assert_eq!(resolved, Value::String("prefix-vpc-123".to_string()));
    }

    #[test]
    fn test_resolve_function_call_unresolved_ref_kept() {
        let bindings = ResolvedBindings::default();

        // If a ResourceRef in the args can't be resolved, the FunctionCall is kept
        let func = Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::List(vec![Value::resource_ref(
                    "unknown".to_string(),
                    "id".to_string(),
                    vec![],
                )]),
            ],
        };

        let resolved = resolve_ref_value(&func, &bindings).unwrap();
        assert!(matches!(resolved, Value::FunctionCall { .. }));
    }

    #[test]
    fn test_resolve_chained_field_access() {
        // web binding has a nested map: network = { vpc_id = "vpc-123" }
        let mut network_map = IndexMap::new();
        network_map.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        let bindings = bindings_from(vec![("web", vec![("network", Value::Map(network_map))])]);

        // web.network.vpc_id should resolve to "vpc-123"
        let ref_value = Value::resource_ref(
            "web".to_string(),
            "network".to_string(),
            vec!["vpc_id".to_string()],
        );

        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, Value::String("vpc-123".to_string()));
    }

    #[test]
    fn test_resolve_deeply_chained_field_access() {
        // web.output.network.vpc_id
        let mut inner_map = IndexMap::new();
        inner_map.insert("vpc_id".to_string(), Value::String("vpc-456".to_string()));
        let mut output_map = IndexMap::new();
        output_map.insert("network".to_string(), Value::Map(inner_map));
        let bindings = bindings_from(vec![("web", vec![("output", Value::Map(output_map))])]);

        let ref_value = Value::resource_ref(
            "web".to_string(),
            "output".to_string(),
            vec!["network".to_string(), "vpc_id".to_string()],
        );

        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, Value::String("vpc-456".to_string()));
    }

    #[test]
    fn test_resolve_chained_field_missing_key_keeps_ref() {
        let mut network_map = IndexMap::new();
        network_map.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        let bindings = bindings_from(vec![("web", vec![("network", Value::Map(network_map))])]);

        // web.network.nonexistent should keep original ref
        let ref_value = Value::resource_ref(
            "web".to_string(),
            "network".to_string(),
            vec!["nonexistent".to_string()],
        );

        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn resolve_builtin_error_propagated_when_args_resolved() {
        // env() with a var name that is extremely unlikely to be set should propagate error
        let bindings = ResolvedBindings::default();
        let value = Value::FunctionCall {
            name: "env".to_string(),
            args: vec![Value::String(
                "CARINA_RESOLVER_TEST_NONEXISTENT_VAR_12345".to_string(),
            )],
        };

        let result = resolve_ref_value(&value, &bindings);
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
        let bindings = ResolvedBindings::default();
        let value = Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::resource_ref("vpc".to_string(), "tags".to_string(), vec![]),
            ],
        };

        let result = resolve_ref_value(&value, &bindings);
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

    #[test]
    fn test_resolve_upstream_state_binding() {
        // Simulate a resource that references an upstream_state binding:
        // network.vpc.vpc_id where network is an upstream_state
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                // network.vpc.vpc_id -> ResourceRef { binding: "network", attribute: "vpc", field_path: ["vpc_id"] }
                Value::resource_ref(
                    "network".to_string(),
                    "vpc".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];

        let current_states: HashMap<ResourceId, State> = HashMap::new();

        // Build remote bindings: network -> { vpc -> Map { vpc_id -> "vpc-123" } }
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut vpc_attrs = IndexMap::new();
        vpc_attrs.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        vpc_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let mut network_map = HashMap::new();
        network_map.insert("vpc".to_string(), Value::Map(vpc_attrs));
        remote_bindings.insert("network".to_string(), network_map);

        resolve_refs_with_state_and_remote(&mut resources, &current_states, &remote_bindings)
            .unwrap();

        assert_eq!(
            resources[0].get_attr("vpc_id"),
            Some(&Value::String("vpc-123".to_string()))
        );
    }

    #[test]
    fn test_resolve_upstream_state_unresolved_keeps_ref() {
        // If the upstream state doesn't have the referenced resource, the ref stays as-is
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::resource_ref(
                    "network".to_string(),
                    "nonexistent".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];

        let current_states: HashMap<ResourceId, State> = HashMap::new();
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_with_state_and_remote(&mut resources, &current_states, &remote_bindings)
            .unwrap();

        // Should remain as ResourceRef since "nonexistent" binding is not found
        assert!(matches!(
            resources[0].get_attr("vpc_id"),
            Some(Value::ResourceRef { .. })
        ));
    }

    /// `resolve_refs_for_plan` stamps any surviving `ResourceRef` whose
    /// root binding is in `remote_bindings.keys()` with the unresolved-
    /// upstream marker. Plan display detects the marker via
    /// `parser::decode_unresolved_upstream_marker`.
    #[test]
    fn test_resolve_refs_for_plan_stamps_top_level_unresolved_upstream() {
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::resource_ref(
                    "network".to_string(),
                    "vpc".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan(&mut resources, &HashMap::new(), &remote_bindings).unwrap();

        match resources[0].get_attr("vpc_id") {
            Some(Value::String(s)) => {
                let payload = crate::parser::decode_unresolved_upstream_marker(s)
                    .expect("expected marker prefix");
                assert_eq!(payload, "network.vpc.vpc_id");
            }
            other => panic!("expected marker String, got {:?}", other),
        }
    }

    /// The apply-side `resolve_refs_with_state_and_remote` must leave a
    /// surviving upstream `ResourceRef` intact (no marker String). Apply
    /// still requires every upstream value to be resolved at apply time;
    /// the resolver-level guarantee is "no stamping unless `for_plan`".
    #[test]
    fn test_resolve_refs_with_state_and_remote_leaves_unresolved_ref_intact() {
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::resource_ref(
                    "network".to_string(),
                    "vpc".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_with_state_and_remote(&mut resources, &HashMap::new(), &remote_bindings)
            .unwrap();

        assert!(matches!(
            resources[0].get_attr("vpc_id"),
            Some(Value::ResourceRef { .. })
        ));
    }

    /// Refs to non-upstream bindings must not be marked, so existing
    /// diagnostics for in-DSL `let` typos keep working.
    #[test]
    fn test_resolve_refs_for_plan_leaves_non_upstream_refs() {
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::resource_ref("main".to_string(), "vpc_id".to_string(), Vec::new()),
            )],
        )];
        let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();

        resolve_refs_for_plan(&mut resources, &HashMap::new(), &remote_bindings).unwrap();

        assert!(matches!(
            resources[0].get_attr("vpc_id"),
            Some(Value::ResourceRef { .. })
        ));
    }

    /// Upstream refs nested inside a `List` must be reached too — e.g.
    /// `security_group_ids = [network.public_sg, network.private_sg]`.
    #[test]
    fn test_resolve_refs_for_plan_stamps_inside_list() {
        let mut resources = vec![make_resource(
            "instance",
            None,
            vec![(
                "security_group_ids",
                Value::List(vec![
                    Value::resource_ref("network".to_string(), "public_sg".to_string(), Vec::new()),
                    Value::resource_ref(
                        "network".to_string(),
                        "private_sg".to_string(),
                        Vec::new(),
                    ),
                ]),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan(&mut resources, &HashMap::new(), &remote_bindings).unwrap();

        match resources[0].get_attr("security_group_ids") {
            Some(Value::List(items)) => {
                assert_eq!(items.len(), 2);
                for (idx, item) in items.iter().enumerate() {
                    match item {
                        Value::String(s) => assert!(
                            crate::parser::decode_unresolved_upstream_marker(s).is_some(),
                            "list[{}] should be marker String, got {:?}",
                            idx,
                            s
                        ),
                        other => panic!("list[{}] should be marker String, got {:?}", idx, other),
                    }
                }
            }
            other => panic!("expected List, got {:?}", other),
        }
    }

    /// Stamping must not destroy the `dependency_bindings` saved by the
    /// resolver before stamping. Plan-tree building and `Delete` effect
    /// linkage rely on this metadata; without it, a deleted resource that
    /// referenced an unresolved upstream would lose the parent-child link.
    #[test]
    fn test_resolve_refs_for_plan_preserves_dependency_bindings() {
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::resource_ref(
                    "network".to_string(),
                    "vpc".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan(&mut resources, &HashMap::new(), &remote_bindings).unwrap();

        assert!(
            resources[0].dependency_bindings.contains("network"),
            "dependency_bindings must record the upstream binding even after stamping"
        );
    }

    /// Upstream refs nested inside `Map` must be reached — a
    /// `tags = { id = network.vpc.vpc_id }` attribute would otherwise
    /// render the raw dot-form when only the top level gets stamped.
    #[test]
    fn test_resolve_refs_for_plan_recurses_into_collections() {
        let inner = Value::resource_ref(
            "network".to_string(),
            "vpc".to_string(),
            vec!["vpc_id".to_string()],
        );
        let mut tag_map: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        tag_map.insert("VpcId".to_string(), inner);
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![("tags", Value::Map(tag_map))],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan(&mut resources, &HashMap::new(), &remote_bindings).unwrap();

        match resources[0].get_attr("tags") {
            Some(Value::Map(m)) => match m.get("VpcId") {
                Some(Value::String(s)) => assert!(
                    crate::parser::decode_unresolved_upstream_marker(s).is_some(),
                    "expected marker prefix, got {:?}",
                    s
                ),
                other => panic!("nested entry should be marker String, got {:?}", other),
            },
            other => panic!("expected Map, got {:?}", other),
        }
    }
}
