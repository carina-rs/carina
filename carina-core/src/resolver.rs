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
/// is replaced with `Value::Unknown(UnknownReason::UpstreamRef { path })`
/// so plan display can render it as `(known after upstream apply: <ref>)`
/// instead of the raw dot-form. `apply` continues to call the strict
/// variant. See #2366 / RFC #2371.
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
        Value::ResourceRef { path } if upstream_binding_names.contains(path.binding()) => {
            Value::Unknown(crate::resource::UnknownReason::UpstreamRef { path })
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
        // An already-stamped `Value::Unknown` (from an earlier pass)
        // is passed through unchanged — it cannot be resolved further.
        other @ Value::Unknown(_) => other,
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

                // For upstream bindings the loaded `Value::Map` is
                // concrete, so a missing field/key is a user typo and
                // gets a "key not found" error. For local bindings the
                // value may simply not be known yet (referenced
                // resource not created) and we keep the ref unchanged.
                let is_upstream = matches!(
                    bindings.source(binding_name),
                    Some(crate::binding_index::BindingValueSource::Upstream)
                );
                for field in field_path {
                    // Peel `Secret` wrappers so dot-form key access
                    // (`creds.db_pwd` against a `Secret(Map)` upstream)
                    // descends into the inner map and re-wraps the
                    // leaf — symmetric with the subscript walk below
                    // (#2439). Pre-fix the wildcard arm dropped any
                    // `Secret(Map)` ref silently.
                    let (peeled, secret_depth) = peel_secrets(resolved);
                    let next = match peeled {
                        Value::Map(ref map) => match map.get(field) {
                            Some(nested) => resolve_ref_value(nested, bindings)?,
                            None if is_upstream => {
                                return Err(missing_map_key_error(path, map));
                            }
                            None => return Ok(value.clone()),
                        },
                        _ => return Ok(value.clone()),
                    };
                    resolved = rewrap_secrets(next, secret_depth);
                }

                use crate::resource::Subscript;
                for sub in path.subscripts() {
                    // Peel `Secret` wrappers so the subscript addresses
                    // the inner container, then re-wrap so the secret
                    // tag survives end-to-end. #2439.
                    let (peeled, secret_depth) = peel_secrets(resolved);
                    let next = match (peeled, sub) {
                        (Value::List(items), Subscript::Int { index }) => {
                            let idx = usize::try_from(*index).ok().filter(|i| *i < items.len());
                            match idx {
                                Some(i) => resolve_ref_value(&items[i], bindings)?,
                                None => return Ok(value.clone()),
                            }
                        }
                        (Value::Map(map), Subscript::Str { key }) => match map.get(key) {
                            Some(nested) => resolve_ref_value(nested, bindings)?,
                            None if is_upstream => {
                                return Err(missing_map_key_error(path, &map));
                            }
                            None => return Ok(value.clone()),
                        },
                        _ => return Ok(value.clone()),
                    };
                    resolved = rewrap_secrets(next, secret_depth);
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
        // `Value::Unknown` is the result of stamping a previously-
        // unresolved upstream ref; it cannot be resolved further.
        Value::Unknown(_) => Ok(value.clone()),
        _ => Ok(value.clone()),
    }
}

/// Strip leading `Value::Secret` wrappers, returning the inner value
/// and the number of layers peeled. Pair with [`rewrap_secrets`] to
/// preserve the secret tag end-to-end through `field_path` /
/// `subscripts` projection (#2439). Plan-display redaction depends on
/// the tag.
fn peel_secrets(mut value: Value) -> (Value, usize) {
    let mut depth = 0;
    while let Value::Secret(inner) = value {
        value = *inner;
        depth += 1;
    }
    (value, depth)
}

/// Re-wrap `value` in `depth` layers of `Value::Secret`. Inverse of
/// [`peel_secrets`].
fn rewrap_secrets(value: Value, depth: usize) -> Value {
    (0..depth).fold(value, |acc, _| Value::Secret(Box::new(acc)))
}

/// Format the "key not found; available keys: ..." error for a missing
/// map entry. Shared by the `field_path` (dot-notation, #2447) and
/// `subscripts` (#2435) walks so the message format cannot drift.
fn missing_map_key_error(
    path: &crate::resource::AccessPath,
    map: &indexmap::IndexMap<String, Value>,
) -> String {
    let known_list = if map.is_empty() {
        "<no keys>".to_string()
    } else {
        map.keys()
            .map(|k| format!("{k:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("{path}: key not found; available keys: {known_list}")
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
    fn test_resolve_subscript_descends_into_secret_wrapped_map() {
        // #2439: subscript on `Secret(Map)` must descend and re-wrap
        // so plan-display redaction survives end-to-end.
        let map: indexmap::IndexMap<String, Value> = vec![
            ("db_pwd".to_string(), Value::String("hunter2".to_string())),
            ("api_key".to_string(), Value::String("xyz".to_string())),
        ]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "creds_binding",
            vec![("creds", Value::Secret(Box::new(Value::Map(map))))],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "creds_binding",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "db_pwd".to_string(),
            }],
        );
        let ref_value = Value::ResourceRef { path };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Secret(Box::new(Value::String("hunter2".to_string()))),
            "subscript on Secret(Map) must project the entry and re-wrap as Secret(String)"
        );
    }

    #[test]
    fn test_resolve_subscript_descends_into_secret_wrapped_list() {
        // Symmetric with the map test: `Value::Secret(Box::new(
        // Value::List(...)))` + integer subscript projects the element
        // and re-wraps the leaf as `Value::Secret`.
        let bindings = bindings_from(vec![(
            "secret_holder",
            vec![(
                "tokens",
                Value::Secret(Box::new(Value::List(vec![
                    Value::String("alpha".to_string()),
                    Value::String("beta".to_string()),
                ]))),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "secret_holder",
            "tokens",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 1 }],
        );
        let ref_value = Value::ResourceRef { path };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Secret(Box::new(Value::String("beta".to_string()))),
        );
    }

    #[test]
    fn test_resolve_field_path_descends_into_secret_wrapped_map() {
        // #2439 sibling: dot-form field access on `Secret(Map)` walks
        // the same blind spot as the subscript path. Pin the symmetric
        // fix so a regression that drops the field_path peel doesn't
        // silently keep the ref.
        let map: indexmap::IndexMap<String, Value> =
            vec![("db_pwd".to_string(), Value::String("hunter2".to_string()))]
                .into_iter()
                .collect();
        let bindings = bindings_from(vec![(
            "creds_binding",
            vec![("creds", Value::Secret(Box::new(Value::Map(map))))],
        )]);
        let path = crate::resource::AccessPath::with_fields(
            "creds_binding",
            "creds",
            vec!["db_pwd".to_string()],
        );
        let ref_value = Value::ResourceRef { path };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Secret(Box::new(Value::String("hunter2".to_string()))),
        );
    }

    #[test]
    fn test_resolve_subscript_descends_into_doubly_secret_wrapped_map() {
        // Defensive: stacked `Secret(Secret(Map))` (parser-rejected in
        // practice, but `peel_secrets` literally exists to handle it).
        // Re-wrap depth must match peel depth — pin the contract.
        let map: indexmap::IndexMap<String, Value> =
            vec![("k".to_string(), Value::String("v".to_string()))]
                .into_iter()
                .collect();
        let bindings = bindings_from(vec![(
            "b",
            vec![(
                "creds",
                Value::Secret(Box::new(Value::Secret(Box::new(Value::Map(map))))),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "b",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "k".to_string(),
            }],
        );
        let ref_value = Value::ResourceRef { path };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Secret(Box::new(Value::Secret(Box::new(Value::String(
                "v".to_string()
            ))))),
        );
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_list_out_of_range_keeps_ref() {
        // Parity with `test_resolve_subscript_out_of_range_keeps_ref`
        // for the `Secret(List)` shape: out-of-range index keeps the
        // original ref unchanged (the planner surfaces it as
        // unresolved). Pin so a future refactor of the rewrap path
        // can't accidentally wrap the kept ref in `Secret`.
        let bindings = bindings_from(vec![(
            "h",
            vec![(
                "tokens",
                Value::Secret(Box::new(Value::List(vec![Value::String(
                    "only".to_string(),
                )]))),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "h",
            "tokens",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 5 }],
        );
        let ref_value = Value::ResourceRef { path };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_map_missing_key_local_keeps_ref() {
        // Local binding (DSL-side `let`) with a `Secret(Map)` value:
        // a missing key keeps the ref unchanged because the value may
        // not be resolved yet. Mirrors the non-secret local-missing
        // path, just with the Secret peel.
        let map: indexmap::IndexMap<String, Value> =
            vec![("known".to_string(), Value::String("v".to_string()))]
                .into_iter()
                .collect();
        let bindings = bindings_from(vec![(
            "h",
            vec![("creds", Value::Secret(Box::new(Value::Map(map))))],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "h",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "missing".to_string(),
            }],
        );
        let ref_value = Value::ResourceRef { path };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_map_missing_key_upstream_errors() {
        // Upstream binding with a `Secret(Map)` value: a missing key
        // surfaces `missing_map_key_error` (concrete state, so a
        // missing key is a real typo). Pin the error path through the
        // peel so a refactor can't accidentally swallow it.
        use crate::binding_index::{BindingValueSource, ResolvedBindings};
        let map: indexmap::IndexMap<String, Value> =
            vec![("known".to_string(), Value::String("v".to_string()))]
                .into_iter()
                .collect();
        let mut attrs = HashMap::new();
        attrs.insert(
            "creds".to_string(),
            Value::Secret(Box::new(Value::Map(map))),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("h", attrs, BindingValueSource::Upstream);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "h",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "missing".to_string(),
            }],
        );
        let ref_value = Value::ResourceRef { path };
        let result = resolve_ref_value(&ref_value, &bindings);
        let err = result.expect_err("missing key in concrete upstream Secret(Map) must error");
        assert!(
            err.contains("key not found"),
            "expected missing-key error, got: {err}"
        );
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_unresolved_inner_ref_keeps_outer() {
        // Defensive: after the initial `resolve_ref_value(attr_value)`
        // call, the `resolved` value can be a `Secret(ResourceRef)` if
        // the inner ref points to an as-yet-unresolved binding. The
        // peel/rewrap loops then expose the inner ResourceRef and the
        // wildcard arm should keep the *outer* ref unchanged — never
        // silently strip the Secret tag and surface the bare inner
        // ref. Pin the contract.
        let mut h_attrs: HashMap<String, Value> = HashMap::new();
        h_attrs.insert(
            "creds".to_string(),
            Value::Secret(Box::new(Value::resource_ref(
                "missing".to_string(),
                "x".to_string(),
                vec![],
            ))),
        );
        let mut bindings = crate::binding_index::ResolvedBindings::default();
        bindings.set(
            "h",
            h_attrs,
            crate::binding_index::BindingValueSource::Local,
        );
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "h",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "k".to_string(),
            }],
        );
        let ref_value = Value::ResourceRef { path };
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved, ref_value,
            "Secret(ResourceRef) projection must keep the outer ref unchanged"
        );
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
    fn test_resolve_upstream_state_map_subscript_substitutes_value() {
        // Issue #2435: `${orgs.accounts['registry_dev']}` against an
        // upstream that exports `accounts: map(AwsAccountId)` must
        // resolve to the concrete account id, not the literal substring
        // or an unresolved ref.
        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "principal_arn",
                // orgs.accounts["registry_dev"]
                Value::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "registry_dev".to_string(),
                        }],
                    ),
                },
            )],
        )];

        let mut accounts_map: IndexMap<String, Value> = IndexMap::new();
        accounts_map.insert(
            "registry_prod".to_string(),
            Value::String("111111111111".to_string()),
        );
        accounts_map.insert(
            "registry_dev".to_string(),
            Value::String("222222222222".to_string()),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert("accounts".to_string(), Value::Map(accounts_map));
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        resolve_refs_with_state_and_remote(&mut resources, &HashMap::new(), &remote_bindings)
            .unwrap();

        assert_eq!(
            resources[0].get_attr("principal_arn"),
            Some(&Value::String("222222222222".to_string())),
        );
    }

    #[test]
    fn test_resolve_upstream_state_map_subscript_missing_key_errors() {
        // Issue #2435 acceptance: when the upstream map IS loaded
        // (concrete `Value::Map`) but the requested string key is not
        // among its entries, the resolver must surface a clear error
        // naming the binding, attribute, and key — otherwise a typo
        // silently degrades to "(known after upstream apply: …)".
        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "principal_arn",
                Value::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "registry_qa".to_string(),
                        }],
                    ),
                },
            )],
        )];

        let mut accounts_map: IndexMap<String, Value> = IndexMap::new();
        accounts_map.insert(
            "registry_prod".to_string(),
            Value::String("111111111111".to_string()),
        );
        accounts_map.insert(
            "registry_dev".to_string(),
            Value::String("222222222222".to_string()),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert("accounts".to_string(), Value::Map(accounts_map));
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let err =
            resolve_refs_with_state_and_remote(&mut resources, &HashMap::new(), &remote_bindings)
                .expect_err("missing key in concrete upstream map must error");
        assert!(
            err.contains("orgs.accounts") && err.contains("registry_qa"),
            "error must name the upstream path and the missing key, got: {err}"
        );
        // Should also list available keys so the user can fix the typo.
        assert!(
            err.contains("registry_prod") && err.contains("registry_dev"),
            "error should list known keys, got: {err}"
        );
    }

    #[test]
    fn test_resolve_upstream_state_map_subscript_empty_map_errors() {
        // Empty `Value::Map` exists but has no keys — the error must
        // still fire and the available-keys list must say so explicitly
        // ("<no keys>") rather than render an empty string.
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert("accounts".to_string(), Value::Map(IndexMap::new()));
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "principal_arn",
                Value::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "registry_dev".to_string(),
                        }],
                    ),
                },
            )],
        )];
        let err =
            resolve_refs_with_state_and_remote(&mut resources, &HashMap::new(), &remote_bindings)
                .expect_err("empty map subscript must error");
        assert!(
            err.contains("<no keys>"),
            "empty-map error must say '<no keys>', got: {err}"
        );
    }

    #[test]
    fn test_resolve_upstream_state_map_subscript_unloaded_keeps_ref() {
        // The new missing-key error must NOT fire when the upstream
        // binding itself isn't loaded yet (e.g. plan-time before the
        // upstream apply has run). The resolver should keep the ref so
        // `resolve_refs_for_plan` can stamp it as
        // `Unknown(UpstreamRef)`. Guards against a future refactor that
        // hoists the subscript walk above the binding-presence check.
        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "principal_arn",
                Value::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "anything".to_string(),
                        }],
                    ),
                },
            )],
        )];

        let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        resolve_refs_with_state_and_remote(&mut resources, &HashMap::new(), &remote_bindings)
            .expect("unloaded upstream subscript must not error");
        assert!(
            matches!(
                resources[0].get_attr("principal_arn"),
                Some(Value::ResourceRef { .. })
            ),
            "ref must stay as ResourceRef when upstream is not loaded"
        );
    }

    #[test]
    fn test_resolve_upstream_state_chained_subscripts() {
        // `orgs.regions['us'][0]` against a `map(list(String))` should
        // walk the map, then index into the list, returning the leaf.
        let inner_list = Value::List(vec![
            Value::String("aza".to_string()),
            Value::String("azb".to_string()),
        ]);
        let mut regions_map: IndexMap<String, Value> = IndexMap::new();
        regions_map.insert("us".to_string(), inner_list);
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert("regions".to_string(), Value::Map(regions_map));
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "az",
                Value::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "regions",
                        Vec::new(),
                        vec![
                            crate::resource::Subscript::Str {
                                key: "us".to_string(),
                            },
                            crate::resource::Subscript::Int { index: 0 },
                        ],
                    ),
                },
            )],
        )];
        resolve_refs_with_state_and_remote(&mut resources, &HashMap::new(), &remote_bindings)
            .unwrap();
        assert_eq!(
            resources[0].get_attr("az"),
            Some(&Value::String("aza".to_string()))
        );
    }

    #[test]
    fn test_resolve_upstream_state_chained_subscript_missing_key_errors() {
        // `orgs.regions['eu'][0]` where map has only 'us' — the missing
        // string key surfaces the same key-not-found error as the
        // single-subscript case.
        let mut regions_map: IndexMap<String, Value> = IndexMap::new();
        regions_map.insert(
            "us".to_string(),
            Value::List(vec![Value::String("aza".to_string())]),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert("regions".to_string(), Value::Map(regions_map));
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "az",
                Value::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "regions",
                        Vec::new(),
                        vec![
                            crate::resource::Subscript::Str {
                                key: "eu".to_string(),
                            },
                            crate::resource::Subscript::Int { index: 0 },
                        ],
                    ),
                },
            )],
        )];
        let err =
            resolve_refs_with_state_and_remote(&mut resources, &HashMap::new(), &remote_bindings)
                .expect_err("chained subscript with missing key must error");
        assert!(
            err.contains("eu") && err.contains("us"),
            "error must mention the missing key and known keys, got: {err}"
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
    /// root binding is in `remote_bindings.keys()` as
    /// `Value::Unknown(UnknownReason::UpstreamRef { path })`.
    #[test]
    fn test_resolve_refs_for_plan_stamps_top_level_unresolved_upstream() {
        use crate::resource::UnknownReason;
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
            Some(Value::Unknown(UnknownReason::UpstreamRef { path })) => {
                assert_eq!(path.to_dot_string(), "network.vpc.vpc_id");
            }
            other => panic!("expected Value::Unknown(UpstreamRef), got {:?}", other),
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
                    assert!(
                        matches!(item, Value::Unknown(_)),
                        "list[{}] should be Value::Unknown, got {:?}",
                        idx,
                        item
                    );
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
                Some(Value::Unknown(_)) => {}
                other => panic!("nested entry should be Value::Unknown, got {:?}", other),
            },
            other => panic!("expected Map, got {:?}", other),
        }
    }
}
