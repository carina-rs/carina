//! Reference resolution for ResourceRef values
//!
//! Resolves `Value::ResourceRef` references by looking up bound resource attributes
//! from both the DSL definition and current infrastructure state.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::binding_index::ResolvedBindings;
use crate::resource::{
    Composition, ConcreteValue, DataSource, DeferredValue, InterpolationPart, Resource, ResourceId,
    State, Value, contains_resource_ref, peel_secrets, rewrap_secrets,
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
    let bindings =
        crate::binding_index::ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: resources,
            compositions: &[],
            data_sources: &[],
            current_states,
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
    resolve_refs_with_state_and_remote(resources, &bindings)
}

/// Resolve all `ResourceRef` values in `resources` against a
/// pre-built `ResolvedBindings` view. carina#3248: the caller is
/// responsible for assembling the bindings view via
/// [`ResolvedBindings::pre_apply`], so this entry point is a pure
/// transform that takes the view as input.
///
/// Use this on the apply path. Use [`resolve_refs_for_plan`] on the
/// plan path — it additionally stamps any surviving
/// `ResourceRef` whose root binding is named in
/// `bindings_upstream_keys` as
/// `Value::Unknown(UnknownReason::UpstreamRef { path })` so plan
/// display can render it as `(known after upstream apply: <ref>)`
/// instead of the raw dot-form.
pub fn resolve_refs_with_state_and_remote(
    resources: &mut [Resource],
    bindings: &ResolvedBindings,
) -> Result<(), String> {
    resolve_refs_inner(resources, bindings, &std::collections::HashSet::new())
}

/// Plan-only counterpart of [`resolve_refs_with_state_and_remote`].
/// Same input/output shape (pre-built `ResolvedBindings`), with the
/// addition of `unresolved_upstream_bindings`: the set of upstream-
/// state binding names whose surviving refs should be stamped as
/// `Value::Unknown(UnknownReason::UpstreamRef { path })` for display.
/// `apply` continues to call the strict variant. See #2366 / RFC #2371.
pub fn resolve_refs_for_plan(
    resources: &mut [Resource],
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
) -> Result<(), String> {
    resolve_refs_inner(resources, bindings, unresolved_upstream_bindings)
}

fn resolve_refs_inner(
    resources: &mut [Resource],
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
) -> Result<(), String> {
    // Save dependency bindings before resolution destroys ResourceRef values.
    // This metadata is used by plan tree building to recover parent-child
    // relationships (see build_plan_tree in display.rs and app.rs).
    for resource in resources.iter_mut() {
        let deps = crate::deps::get_resource_value_ref_dependencies(resource);
        if !deps.is_empty() {
            resource.dependency_bindings = deps.into_iter().collect();
        }
    }

    let mark_unresolved_upstream = !unresolved_upstream_bindings.is_empty();

    // Resolve ResourceRef values in all resources. Stay in `IndexMap`
    // so the user's authored attribute order survives resolution
    // (#2222).
    for resource in resources.iter_mut() {
        let mut resolved_attrs: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        for (key, value) in &resource.attributes {
            let resolved = resolve_ref_value(value, bindings)?;
            let final_value = if mark_unresolved_upstream {
                stamp_unresolved_upstream(resolved, unresolved_upstream_bindings)
            } else {
                resolved
            };
            resolved_attrs.insert(key.clone(), final_value);
        }
        resource.attributes = resolved_attrs;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Typed resolver entry points (#3175 / typestate split)
// ---------------------------------------------------------------------------

/// Pre-apply path: resolve `ResourceRef` values across a slice of
/// `Resource`s.
///
/// carina#3248: compositions and data sources are first-class binding
/// sources on the pre-apply path. `bindings` must therefore include
/// all kinds the configuration declares, via
/// [`ResolvedBindings::pre_apply`]. A managed attribute referencing
/// `<module_instance>.<attr>` (a composition binding) chains through the
/// composition's attribute map to the managed sibling literal that backs
/// it. The earlier "wait / upstream-state passthrough" guidance for
/// this shape (referring to cross-stack consumption) does not apply
/// to same-stack module attribute references — those resolve through
/// the in-process composition binding directly.
pub fn resolve_managed_refs_with_state_and_remote(
    managed: &mut [Resource],
    bindings: &ResolvedBindings,
) -> Result<(), String> {
    resolve_refs_inner(managed, bindings, &std::collections::HashSet::new())
}

/// Resolve `ResourceRef` values in a slice of [`DataSource`]s against
/// a pre-built `ResolvedBindings` view (carina#3248).
///
/// Data sources are read-only; their input attributes (`read aws.iam.user
/// { user_name = some_let.name }`) reference managed resources, compositions,
/// or other data sources, so the binding map the caller passes must
/// include all kinds via [`ResolvedBindings::pre_apply`]. Each data
/// source's `dependency_bindings` is recorded before resolution
/// destroys the `ResourceRef` values, mirroring [`resolve_refs_inner`].
pub fn resolve_data_source_refs(
    data_sources: &mut [DataSource],
    bindings: &ResolvedBindings,
) -> Result<(), String> {
    resolve_data_source_refs_inner(data_sources, bindings, &std::collections::HashSet::new())
}

/// Plan-only counterpart of [`resolve_data_source_refs`]. Mirrors
/// [`resolve_refs_for_plan`]: takes the set of upstream-binding
/// names whose surviving refs should be stamped as
/// `Value::Unknown(UnknownReason::UpstreamRef { path })`.
pub fn resolve_data_source_refs_for_plan(
    data_sources: &mut [DataSource],
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
) -> Result<(), String> {
    resolve_data_source_refs_inner(data_sources, bindings, unresolved_upstream_bindings)
}

fn resolve_data_source_refs_inner(
    data_sources: &mut [DataSource],
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
) -> Result<(), String> {
    for data_source in data_sources.iter_mut() {
        let deps = crate::deps::get_resource_value_ref_dependencies(data_source);
        if !deps.is_empty() {
            data_source.dependency_bindings = deps.into_iter().collect();
        }
    }
    let mark_unresolved_upstream = !unresolved_upstream_bindings.is_empty();
    for data_source in data_sources.iter_mut() {
        let mut resolved_attrs: IndexMap<String, Value> = IndexMap::new();
        for (key, value) in &data_source.attributes {
            let resolved = resolve_ref_value(value, bindings)?;
            let final_value = if mark_unresolved_upstream {
                stamp_unresolved_upstream(resolved, unresolved_upstream_bindings)
            } else {
                resolved
            };
            resolved_attrs.insert(key.clone(), final_value);
        }
        data_source.attributes = resolved_attrs;
    }
    Ok(())
}

/// Post-apply path: resolve `ResourceRef` values across a slice of
/// `Composition`s using a `ResolvedBindings` view that the caller
/// has already built against the post-apply state.
///
/// Calling this against pre-apply state would re-introduce the
/// #3169 exports-drift bug, so the caller is responsible for the
/// post-apply ordering. The signature is structurally distinct from
/// [`resolve_managed_refs_with_state_and_remote`] (takes a built
/// bindings view, not the raw state inputs) to make accidental
/// pre-apply use harder to write.
///
/// `dependency_bindings` is left untouched: composition→resource edges are
/// already recorded during the pre-apply pass, and the post-apply
/// resolution only needs to materialise the attribute values.
pub fn resolve_virtual_refs_post_apply(
    compositions: &mut [Composition],
    bindings: &ResolvedBindings,
) -> Result<(), String> {
    for v in compositions.iter_mut() {
        let mut resolved_attrs: IndexMap<String, Value> = IndexMap::new();
        for (key, value) in &v.signature.attributes {
            resolved_attrs.insert(key.clone(), resolve_ref_value(value, bindings)?);
        }
        v.signature.attributes = resolved_attrs;
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
        Value::Deferred(DeferredValue::ResourceRef { path })
            if upstream_binding_names.contains(path.binding()) =>
        {
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::UnknownReason::UpstreamRef { path },
            ))
        }
        Value::Deferred(DeferredValue::BindingRef { binding })
            if upstream_binding_names.contains(binding.as_str()) =>
        {
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::UnknownReason::UpstreamBareRef { binding },
            ))
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .into_iter()
                .map(|v| stamp_unresolved_upstream(v, upstream_binding_names))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut out: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                out.insert(k, stamp_unresolved_upstream(v, upstream_binding_names));
            }
            Value::Concrete(ConcreteValue::Map(out))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            Value::Deferred(DeferredValue::Interpolation(
                parts
                    .into_iter()
                    .map(|p| match p {
                        InterpolationPart::Expr(v) => InterpolationPart::Expr(
                            stamp_unresolved_upstream(v, upstream_binding_names),
                        ),
                        other => other,
                    })
                    .collect(),
            ))
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            Value::Deferred(DeferredValue::FunctionCall {
                name,
                args: args
                    .into_iter()
                    .map(|a| stamp_unresolved_upstream(a, upstream_binding_names))
                    .collect(),
            })
        }
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(stamp_unresolved_upstream(*inner, upstream_binding_names)),
        )),
        // An already-stamped `Value::Unknown` (from an earlier pass)
        // is passed through unchanged — it cannot be resolved further.
        other @ Value::Deferred(DeferredValue::Unknown(_)) => other,
        other => other,
    }
}

/// Recursively resolve a single Value, replacing ResourceRef with the referenced value.
///
/// If the referenced binding or attribute is not found, the value is returned as-is.
/// Returns an error if a builtin function fails with fully-resolved arguments.
pub fn resolve_ref_value(value: &Value, bindings: &ResolvedBindings) -> Result<Value, String> {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
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
                use crate::resource::{PathSegment, Subscript};
                for segment in path.segments() {
                    // Peel `Secret` wrappers so dot-form / subscript
                    // access descends into the inner container, then
                    // re-wraps the leaf so the secret tag survives
                    // end-to-end. #2439.
                    let (peeled, secret_depth) = peel_secrets(resolved);
                    let next = match (peeled, segment) {
                        (
                            Value::Concrete(ConcreteValue::Map(ref map)),
                            PathSegment::Field { name: field },
                        ) => match map.get(field) {
                            Some(nested) => resolve_ref_value(nested, bindings)?,
                            None if is_upstream && secret_depth > 0 => {
                                return Err(missing_map_key_error_redacted(path, map.len()));
                            }
                            None if is_upstream => {
                                return Err(missing_map_key_error(path, map));
                            }
                            None => return Ok(value.clone()),
                        },
                        (
                            Value::Concrete(ConcreteValue::List(items)),
                            PathSegment::Subscript {
                                index: Subscript::Int { index },
                            },
                        ) => {
                            let idx = usize::try_from(*index).ok().filter(|i| *i < items.len());
                            match idx {
                                Some(i) => resolve_ref_value(&items[i], bindings)?,
                                None => return Ok(value.clone()),
                            }
                        }
                        (
                            Value::Concrete(ConcreteValue::Map(map)),
                            PathSegment::Subscript {
                                index: Subscript::Str { key },
                            },
                        ) => match map.get(key) {
                            Some(nested) => resolve_ref_value(nested, bindings)?,
                            None if is_upstream && secret_depth > 0 => {
                                return Err(missing_map_key_error_redacted(path, map.len()));
                            }
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
        Value::Concrete(ConcreteValue::List(items)) => {
            let resolved: Result<Vec<Value>, String> = items
                .iter()
                .map(|v| resolve_ref_value(v, bindings))
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(resolved?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut resolved: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_ref_value(v, bindings)?);
            }
            Ok(Value::Concrete(ConcreteValue::Map(resolved)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let resolved_parts: Result<Vec<InterpolationPart>, String> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        Ok(InterpolationPart::Expr(resolve_ref_value(v, bindings)?))
                    }
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(resolved_parts?)).canonicalize())
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
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
                Ok(Value::Deferred(DeferredValue::FunctionCall {
                    name: name.clone(),
                    args: resolved_args,
                }))
            }
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            let resolved_inner = resolve_ref_value(inner, bindings)?;
            Ok(Value::Deferred(DeferredValue::Secret(Box::new(
                resolved_inner,
            ))))
        }
        // `Value::Unknown` is the result of stamping a previously-
        // unresolved upstream ref; it cannot be resolved further.
        Value::Deferred(DeferredValue::Unknown(_)) => Ok(value.clone()),
        _ => Ok(value.clone()),
    }
}

// `peel_secrets` / `rewrap_secrets` live in `crate::resource` (next to
// the `Secret` definition and `navigate_value_path`) so the
// secret-tunnel discipline (#2439) has one home shared with
// carina#3136's loop-var navigator.

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

/// Redacted variant of [`missing_map_key_error`] for `Secret`-wrapped
/// upstream maps (#2501). The signature deliberately takes only the
/// entry count, not the map itself, so the formatter cannot accidentally
/// include the keys: a `Secret(Map)`'s key names (`db_pwd`, `api_key`,
/// …) leak the shape of the credential set even when the values are
/// already redacted by the plan-display path.
fn missing_map_key_error_redacted(path: &crate::resource::AccessPath, key_count: usize) -> String {
    format!("{path}: key not found; available keys: <redacted, {key_count} entries>")
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
    /// map. Each entry is dropped in via `pre_apply` so the resulting
    /// view is identical to what production code constructs — there is
    /// no test-only back door into the type.
    fn bindings_from(entries: Vec<(&str, Vec<(&str, Value)>)>) -> ResolvedBindings {
        let resources: Vec<Resource> = entries
            .into_iter()
            .map(|(binding, attrs)| {
                make_resource(&format!("{}-resource", binding), Some(binding), attrs)
            })
            .collect();
        ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: &resources,
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        })
    }

    /// Test-only adapter that preserves the pre-#3248 four-arg
    /// signature so the bulky test corpus migrates mechanically.
    /// Builds the bindings view here, then delegates to the new
    /// signature.
    fn resolve_refs_with_state_and_remote_legacy(
        resources: &mut [Resource],
        current_states: &HashMap<ResourceId, State>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        wait_aliases: &[crate::binding_index::WaitAliasSpec],
    ) -> Result<(), String> {
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: resources,
            compositions: &[],
            data_sources: &[],
            current_states,
            remote_bindings,
            wait_aliases,
        });
        resolve_refs_with_state_and_remote(resources, &bindings)
    }

    /// Test-only adapter for the plan-path resolver (mirrors
    /// `resolve_refs_with_state_and_remote_legacy`).
    fn resolve_refs_for_plan_legacy(
        resources: &mut [Resource],
        current_states: &HashMap<ResourceId, State>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        wait_aliases: &[crate::binding_index::WaitAliasSpec],
    ) -> Result<(), String> {
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: resources,
            compositions: &[],
            data_sources: &[],
            current_states,
            remote_bindings,
            wait_aliases,
        });
        let upstream_keys: std::collections::HashSet<&str> =
            remote_bindings.keys().map(String::as_str).collect();
        resolve_refs_for_plan(resources, &bindings, &upstream_keys)
    }

    #[test]
    fn test_resolve_subscript_descends_into_list() {
        // `orgs.accounts[0]` against `accounts: [a, b]` resolves to `a`.
        let bindings = bindings_from(vec![(
            "orgs",
            vec![(
                "accounts",
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("alpha".to_string())),
                    Value::Concrete(ConcreteValue::String("beta".to_string())),
                ])),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 0 }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("alpha".to_string()))
        );
    }

    #[test]
    fn test_resolve_chained_subscript_then_field_descends_into_struct() {
        // carina#3025 reproduction: `cert.list[0].name` against
        // `list: [ { name = "alpha" }, { name = "beta" } ]` resolves
        // to `"alpha"`. The resolver walks segments in source order
        // (Subscript then Field), so the post-subscript field access
        // descends into the inner map.
        use crate::resource::{AccessPath, PathSegment, Subscript};
        let inner_alpha: indexmap::IndexMap<String, Value> = vec![(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("alpha".to_string())),
        )]
        .into_iter()
        .collect();
        let inner_beta: indexmap::IndexMap<String, Value> = vec![(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("beta".to_string())),
        )]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "cert",
            vec![(
                "list",
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::Map(inner_alpha)),
                    Value::Concrete(ConcreteValue::Map(inner_beta)),
                ])),
            )],
        )]);
        let path = AccessPath::with_segments(
            "cert",
            "list",
            vec![
                PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                },
                PathSegment::Field {
                    name: "name".to_string(),
                },
            ],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("alpha".to_string()))
        );
    }

    #[test]
    fn test_resolve_subscript_descends_into_map() {
        // `orgs.accounts["alpha"]` against `accounts: { alpha = "1", beta = "2" }`
        // resolves to `"1"`.
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "alpha".to_string(),
                Value::Concrete(ConcreteValue::String("1".to_string())),
            ),
            (
                "beta".to_string(),
                Value::Concrete(ConcreteValue::String("2".to_string())),
            ),
        ]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "orgs",
            vec![("accounts", Value::Concrete(ConcreteValue::Map(map)))],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "alpha".to_string(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("1".to_string()))
        );
    }

    #[test]
    fn test_resolve_subscript_descends_into_secret_wrapped_map() {
        // #2439: subscript on `Secret(Map)` must descend and re-wrap
        // so plan-display redaction survives end-to-end.
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "db_pwd".to_string(),
                Value::Concrete(ConcreteValue::String("hunter2".to_string())),
            ),
            (
                "api_key".to_string(),
                Value::Concrete(ConcreteValue::String("xyz".to_string())),
            ),
        ]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "creds_binding",
            vec![(
                "creds",
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::Map(map),
                )))),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "creds_binding",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "db_pwd".to_string(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hunter2".to_string())
            )))),
            "subscript on Secret(Map) must project the entry and re-wrap as Secret(String)"
        );
    }

    #[test]
    fn test_resolve_subscript_descends_into_secret_wrapped_list() {
        // Symmetric with the map test: `Value::Secret(Box::new(
        // Value::Concrete(ConcreteValue::List(...))))` + integer subscript projects the element
        // and re-wraps the leaf as `Value::Secret`.
        let bindings = bindings_from(vec![(
            "secret_holder",
            vec![(
                "tokens",
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::List(vec![
                        Value::Concrete(ConcreteValue::String("alpha".to_string())),
                        Value::Concrete(ConcreteValue::String("beta".to_string())),
                    ]),
                )))),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "secret_holder",
            "tokens",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 1 }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("beta".to_string())
            )))),
        );
    }

    #[test]
    fn test_resolve_field_path_descends_into_secret_wrapped_map() {
        // #2439 sibling: dot-form field access on `Secret(Map)` walks
        // the same blind spot as the subscript path. Pin the symmetric
        // fix so a regression that drops the field_path peel doesn't
        // silently keep the ref.
        let map: indexmap::IndexMap<String, Value> = vec![(
            "db_pwd".to_string(),
            Value::Concrete(ConcreteValue::String("hunter2".to_string())),
        )]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "creds_binding",
            vec![(
                "creds",
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::Map(map),
                )))),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields(
            "creds_binding",
            "creds",
            vec!["db_pwd".to_string()],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hunter2".to_string())
            )))),
        );
    }

    #[test]
    fn test_resolve_subscript_descends_into_doubly_secret_wrapped_map() {
        // Defensive: stacked `Secret(Secret(Map))` (parser-rejected in
        // practice, but `peel_secrets` literally exists to handle it).
        // Re-wrap depth must match peel depth — pin the contract.
        let map: indexmap::IndexMap<String, Value> = vec![(
            "k".to_string(),
            Value::Concrete(ConcreteValue::String("v".to_string())),
        )]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "b",
            vec![(
                "creds",
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
                    DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::Map(map)))),
                )))),
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
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
                DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::String(
                    "v".to_string()
                ))))
            )))),
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
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::List(vec![Value::Concrete(ConcreteValue::String(
                        "only".to_string(),
                    ))]),
                )))),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "h",
            "tokens",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 5 }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_map_missing_key_local_keeps_ref() {
        // Local binding (DSL-side `let`) with a `Secret(Map)` value:
        // a missing key keeps the ref unchanged because the value may
        // not be resolved yet. Mirrors the non-secret local-missing
        // path, just with the Secret peel.
        let map: indexmap::IndexMap<String, Value> = vec![(
            "known".to_string(),
            Value::Concrete(ConcreteValue::String("v".to_string())),
        )]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "h",
            vec![(
                "creds",
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::Map(map),
                )))),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "h",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "missing".to_string(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_map_missing_key_upstream_errors() {
        // Upstream binding with a `Secret(Map)` value: a missing key
        // surfaces `missing_map_key_error` (concrete state, so a
        // missing key is a real typo). Pin the error path through the
        // peel so a refactor can't accidentally swallow it.
        //
        // #2501: the available-keys list must be redacted (entry count
        // only, no key names) because the keys of a Secret(Map) leak
        // shape — e.g. "this binding exports a `db_pwd`, an `api_key`".
        use crate::binding_index::{BindingValueSource, ResolvedBindings};
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "db_pwd".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("v".to_string()),
                )))),
            ),
            (
                "api_key".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("v".to_string()),
                )))),
            ),
            (
                "slack_token".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("v".to_string()),
                )))),
            ),
        ]
        .into_iter()
        .collect();
        let mut attrs = HashMap::new();
        attrs.insert(
            "creds".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::Map(map),
            )))),
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
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let result = resolve_ref_value(&ref_value, &bindings);
        let err = result.expect_err("missing key in concrete upstream Secret(Map) must error");
        assert!(
            err.contains("key not found"),
            "expected missing-key error, got: {err}"
        );
        assert!(
            err.contains("<redacted, 3 entries>"),
            "Secret(Map) keys must be redacted, got: {err}"
        );
        for k in ["db_pwd", "api_key", "slack_token"] {
            assert!(
                !err.contains(k),
                "Secret(Map) key {k:?} leaked into error: {err}"
            );
        }
    }

    #[test]
    fn test_resolve_field_path_secret_wrapped_map_missing_key_upstream_redacted() {
        // #2501 sibling: dot-form field access (`creds.missing`) on a
        // `Secret(Map)` upstream binding must also redact the available
        // keys. The field_path walk and the subscript walk share the
        // same `missing_map_key_error_redacted` helper, so both must
        // emit the redacted message.
        use crate::binding_index::{BindingValueSource, ResolvedBindings};
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "db_pwd".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("v".to_string()),
                )))),
            ),
            (
                "api_key".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("v".to_string()),
                )))),
            ),
        ]
        .into_iter()
        .collect();
        let mut attrs = HashMap::new();
        attrs.insert(
            "creds".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::Map(map),
            )))),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("h", attrs, BindingValueSource::Upstream);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "h",
            "creds",
            vec!["missing".to_string()],
            Vec::new(),
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let err = resolve_ref_value(&ref_value, &bindings)
            .expect_err("missing dot-form field in concrete upstream Secret(Map) must error");
        assert!(
            err.contains("<redacted, 2 entries>"),
            "Secret(Map) field_path keys must be redacted, got: {err}"
        );
        for k in ["db_pwd", "api_key"] {
            assert!(
                !err.contains(k),
                "Secret(Map) key {k:?} leaked into field_path error: {err}"
            );
        }
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_empty_map_missing_key_upstream_redacted() {
        // Edge case: an empty `Secret(Map)` still emits the redacted
        // form (entry count = 0). Pins that the redacted helper does
        // not fall back to the literal "<no keys>" branch, which would
        // also be safe for an empty map but inconsistent with the
        // populated-Secret message format.
        use crate::binding_index::{BindingValueSource, ResolvedBindings};
        let mut attrs = HashMap::new();
        attrs.insert(
            "creds".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::Map(indexmap::IndexMap::new()),
            )))),
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
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let err = resolve_ref_value(&ref_value, &bindings)
            .expect_err("missing key in empty upstream Secret(Map) must error");
        assert!(
            err.contains("<redacted, 0 entries>"),
            "empty Secret(Map) must still use redacted form, got: {err}"
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
            Value::Deferred(DeferredValue::Secret(Box::new(Value::resource_ref(
                "missing".to_string(),
                "x".to_string(),
                vec![],
            )))),
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
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
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
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("a".to_string()),
                )])),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 5 }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path: path.clone() });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_simple_resource_ref() {
        let bindings = bindings_from(vec![(
            "my_vpc",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
            )],
        )]);

        let ref_value = Value::resource_ref("my_vpc".to_string(), "id".to_string(), vec![]);

        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("vpc-123".to_string()))
        );
    }

    #[test]
    fn test_resolve_nested_refs_in_list() {
        let bindings = bindings_from(vec![(
            "my_sg",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("sg-456".to_string())),
            )],
        )]);

        let list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("static".to_string())),
            Value::resource_ref("my_sg".to_string(), "id".to_string(), vec![]),
        ]));

        let resolved = resolve_ref_value(&list, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("static".to_string())),
                Value::Concrete(ConcreteValue::String("sg-456".to_string())),
            ]))
        );
    }

    #[test]
    fn test_resolve_nested_refs_in_map() {
        let bindings = bindings_from(vec![(
            "my_subnet",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("subnet-789".to_string())),
            )],
        )]);

        let map = Value::Concrete(ConcreteValue::Map(
            vec![(
                "subnet_id".to_string(),
                Value::resource_ref("my_subnet".to_string(), "id".to_string(), vec![]),
            )]
            .into_iter()
            .collect(),
        ));

        let resolved = resolve_ref_value(&map, &bindings).unwrap();
        if let Value::Concrete(ConcreteValue::Map(m)) = resolved {
            assert_eq!(
                m.get("subnet_id"),
                Some(&Value::Concrete(ConcreteValue::String(
                    "subnet-789".to_string()
                )))
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
            vec![(
                "vpc_id",
                Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
            )],
        )]);

        let interp = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("subnet-".to_string()),
            InterpolationPart::Expr(Value::resource_ref(
                "my_vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        ]));

        let resolved = resolve_ref_value(&interp, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("subnet-vpc-123".to_string()))
        );
    }

    #[test]
    fn test_resolve_interpolation_partially_unresolved() {
        let bindings = ResolvedBindings::default();

        let interp = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("subnet-".to_string()),
            InterpolationPart::Expr(Value::resource_ref(
                "my_vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        ]));

        let resolved = resolve_ref_value(&interp, &bindings).unwrap();
        // Should remain as Interpolation since the ref couldn't be resolved
        assert!(matches!(
            resolved,
            Value::Deferred(DeferredValue::Interpolation(_))
        ));
    }

    #[test]
    fn test_resolve_interpolation_with_non_string_types() {
        let bindings = ResolvedBindings::default();

        let interp = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("port-".to_string()),
            InterpolationPart::Expr(Value::Concrete(ConcreteValue::Int(8080))),
            InterpolationPart::Literal("-enabled-".to_string()),
            InterpolationPart::Expr(Value::Concrete(ConcreteValue::Bool(true))),
        ]));

        let resolved = resolve_ref_value(&interp, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("port-8080-enabled-true".to_string()))
        );
    }

    #[test]
    fn test_state_attributes_merged_into_binding_map() {
        let rid = ResourceId::new("test.resource", "my-vpc");
        let mut resources = vec![
            make_resource(
                "my-vpc",
                Some("my_vpc"),
                vec![(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
                )],
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
                attributes: vec![(
                    "vpc_id".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-abc".to_string())),
                )]
                .into_iter()
                .collect(),
                dependency_bindings: std::collections::BTreeSet::new(),
            },
        );

        resolve_refs_with_state(&mut resources, &current_states).unwrap();

        // The subnet's vpc_id should be resolved from state
        assert_eq!(
            resources[1].get_attr("vpc_id"),
            Some(&Value::Concrete(ConcreteValue::String(
                "vpc-abc".to_string()
            )))
        );
    }

    #[test]
    fn test_resolve_function_call_join() {
        let bindings = ResolvedBindings::default();

        let func = Value::Deferred(DeferredValue::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::Concrete(ConcreteValue::String("-".to_string())),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("a".to_string())),
                    Value::Concrete(ConcreteValue::String("b".to_string())),
                    Value::Concrete(ConcreteValue::String("c".to_string())),
                ])),
            ],
        });

        let resolved = resolve_ref_value(&func, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("a-b-c".to_string()))
        );
    }

    #[test]
    fn test_resolve_function_call_with_resource_ref() {
        let bindings = bindings_from(vec![(
            "vpc",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
            )],
        )]);

        // join("-", ["prefix", vpc.id]) should resolve vpc.id first, then evaluate
        let func = Value::Deferred(DeferredValue::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::Concrete(ConcreteValue::String("-".to_string())),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("prefix".to_string())),
                    Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
                ])),
            ],
        });

        let resolved = resolve_ref_value(&func, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("prefix-vpc-123".to_string()))
        );
    }

    #[test]
    fn test_resolve_function_call_unresolved_ref_kept() {
        let bindings = ResolvedBindings::default();

        // If a ResourceRef in the args can't be resolved, the FunctionCall is kept
        let func = Value::Deferred(DeferredValue::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::Concrete(ConcreteValue::String("-".to_string())),
                Value::Concrete(ConcreteValue::List(vec![Value::resource_ref(
                    "unknown".to_string(),
                    "id".to_string(),
                    vec![],
                )])),
            ],
        });

        let resolved = resolve_ref_value(&func, &bindings).unwrap();
        assert!(matches!(
            resolved,
            Value::Deferred(DeferredValue::FunctionCall { .. })
        ));
    }

    #[test]
    fn test_resolve_chained_field_access() {
        // web binding has a nested map: network = { vpc_id = "vpc-123" }
        let mut network_map = IndexMap::new();
        network_map.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
        );
        let bindings = bindings_from(vec![(
            "web",
            vec![("network", Value::Concrete(ConcreteValue::Map(network_map)))],
        )]);

        // web.network.vpc_id should resolve to "vpc-123"
        let ref_value = Value::resource_ref(
            "web".to_string(),
            "network".to_string(),
            vec!["vpc_id".to_string()],
        );

        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("vpc-123".to_string()))
        );
    }

    #[test]
    fn test_resolve_deeply_chained_field_access() {
        // web.output.network.vpc_id
        let mut inner_map = IndexMap::new();
        inner_map.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-456".to_string())),
        );
        let mut output_map = IndexMap::new();
        output_map.insert(
            "network".to_string(),
            Value::Concrete(ConcreteValue::Map(inner_map)),
        );
        let bindings = bindings_from(vec![(
            "web",
            vec![("output", Value::Concrete(ConcreteValue::Map(output_map)))],
        )]);

        let ref_value = Value::resource_ref(
            "web".to_string(),
            "output".to_string(),
            vec!["network".to_string(), "vpc_id".to_string()],
        );

        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("vpc-456".to_string()))
        );
    }

    #[test]
    fn test_resolve_chained_field_missing_key_keeps_ref() {
        let mut network_map = IndexMap::new();
        network_map.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
        );
        let bindings = bindings_from(vec![(
            "web",
            vec![("network", Value::Concrete(ConcreteValue::Map(network_map)))],
        )]);

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
        let value = Value::Deferred(DeferredValue::FunctionCall {
            name: "env".to_string(),
            args: vec![Value::Concrete(ConcreteValue::String(
                "CARINA_RESOLVER_TEST_NONEXISTENT_VAR_12345".to_string(),
            ))],
        });

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
        let value = Value::Deferred(DeferredValue::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::Concrete(ConcreteValue::String("-".to_string())),
                Value::resource_ref("vpc".to_string(), "tags".to_string(), vec![]),
            ],
        });

        let result = resolve_ref_value(&value, &bindings);
        assert!(result.is_ok(), "Unresolved ref should not cause error");
        match result.unwrap() {
            Value::Deferred(DeferredValue::FunctionCall { name, .. }) => assert_eq!(name, "join"),
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
                Value::Deferred(DeferredValue::FunctionCall {
                    name: "env".to_string(),
                    args: vec![Value::Concrete(ConcreteValue::String(
                        "CARINA_RESOLVER_STATE_TEST_NONEXISTENT_VAR_12345".to_string(),
                    ))],
                }),
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
        vpc_attrs.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
        );
        vpc_attrs.insert(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        );
        let mut network_map = HashMap::new();
        network_map.insert(
            "vpc".to_string(),
            Value::Concrete(ConcreteValue::Map(vpc_attrs)),
        );
        remote_bindings.insert("network".to_string(), network_map);

        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &current_states,
            &remote_bindings,
            &[],
        )
        .unwrap();

        assert_eq!(
            resources[0].get_attr("vpc_id"),
            Some(&Value::Concrete(ConcreteValue::String(
                "vpc-123".to_string()
            )))
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
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "registry_dev".to_string(),
                        }],
                    ),
                }),
            )],
        )];

        let mut accounts_map: IndexMap<String, Value> = IndexMap::new();
        accounts_map.insert(
            "registry_prod".to_string(),
            Value::Concrete(ConcreteValue::String("111111111111".to_string())),
        );
        accounts_map.insert(
            "registry_dev".to_string(),
            Value::Concrete(ConcreteValue::String("222222222222".to_string())),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::Concrete(ConcreteValue::Map(accounts_map)),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .unwrap();

        assert_eq!(
            resources[0].get_attr("principal_arn"),
            Some(&Value::Concrete(ConcreteValue::String(
                "222222222222".to_string()
            ))),
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
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "registry_qa".to_string(),
                        }],
                    ),
                }),
            )],
        )];

        let mut accounts_map: IndexMap<String, Value> = IndexMap::new();
        accounts_map.insert(
            "registry_prod".to_string(),
            Value::Concrete(ConcreteValue::String("111111111111".to_string())),
        );
        accounts_map.insert(
            "registry_dev".to_string(),
            Value::Concrete(ConcreteValue::String("222222222222".to_string())),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::Concrete(ConcreteValue::Map(accounts_map)),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let err = resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
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
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "principal_arn",
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "registry_dev".to_string(),
                        }],
                    ),
                }),
            )],
        )];
        let err = resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
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
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "anything".to_string(),
                        }],
                    ),
                }),
            )],
        )];

        let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .expect("unloaded upstream subscript must not error");
        assert!(
            matches!(
                resources[0].get_attr("principal_arn"),
                Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
            ),
            "ref must stay as ResourceRef when upstream is not loaded"
        );
    }

    #[test]
    fn test_resolve_upstream_state_chained_subscripts() {
        // `orgs.regions['us'][0]` against a `map(list(String))` should
        // walk the map, then index into the list, returning the leaf.
        let inner_list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("aza".to_string())),
            Value::Concrete(ConcreteValue::String("azb".to_string())),
        ]));
        let mut regions_map: IndexMap<String, Value> = IndexMap::new();
        regions_map.insert("us".to_string(), inner_list);
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "regions".to_string(),
            Value::Concrete(ConcreteValue::Map(regions_map)),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "az",
                Value::Deferred(DeferredValue::ResourceRef {
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
                }),
            )],
        )];
        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .unwrap();
        assert_eq!(
            resources[0].get_attr("az"),
            Some(&Value::Concrete(ConcreteValue::String("aza".to_string())))
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
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("aza".to_string()),
            )])),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "regions".to_string(),
            Value::Concrete(ConcreteValue::Map(regions_map)),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "az",
                Value::Deferred(DeferredValue::ResourceRef {
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
                }),
            )],
        )];
        let err = resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
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

        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &current_states,
            &remote_bindings,
            &[],
        )
        .unwrap();

        // Should remain as ResourceRef since "nonexistent" binding is not found
        assert!(matches!(
            resources[0].get_attr("vpc_id"),
            Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
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

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("vpc_id") {
            Some(Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef { path }))) => {
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

        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .unwrap();

        assert!(matches!(
            resources[0].get_attr("vpc_id"),
            Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
        ));
    }

    /// Bare-binding refs into an upstream_state binding must also be
    /// stamped — `let v = bootstrap` (no `.attr`) parses as
    /// `Value::BindingRef { binding: "bootstrap" }` since #2856.
    /// Without a dedicated arm in `stamp_unresolved_upstream`, the
    /// `BindingRef` would slip past the marker and surface in plan
    /// display as a plain identifier instead of the
    /// `(known after upstream apply: bootstrap)` form. #2876.
    #[test]
    fn test_resolve_refs_for_plan_stamps_bare_upstream_binding_ref() {
        use crate::resource::UnknownReason;
        let mut resources = vec![make_resource(
            "consumer",
            None,
            vec![(
                "raw",
                Value::Deferred(DeferredValue::BindingRef {
                    binding: "bootstrap".to_string(),
                }),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("bootstrap".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("raw") {
            Some(Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamBareRef {
                binding,
            }))) => {
                assert_eq!(binding, "bootstrap");
            }
            other => panic!("expected Value::Unknown(UpstreamBareRef), got {:?}", other),
        }
    }

    /// Bare upstream refs nested inside a `List` must reach the
    /// stamping arm via the existing recursion. Locks in the recursive
    /// contract symmetrically with the `UpstreamRef`-inside-list test
    /// above. #2876.
    #[test]
    fn test_resolve_refs_for_plan_stamps_bare_upstream_inside_list() {
        use crate::resource::UnknownReason;
        let mut resources = vec![make_resource(
            "consumer",
            None,
            vec![(
                "raws",
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: "bootstrap".to_string(),
                    }),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: "secondary".to_string(),
                    }),
                ])),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("bootstrap".to_string(), HashMap::new());
        remote_bindings.insert("secondary".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("raws") {
            Some(Value::Concrete(ConcreteValue::List(items))) => {
                assert_eq!(items.len(), 2);
                let bindings: Vec<&str> = items
                    .iter()
                    .map(|v| match v {
                        Value::Deferred(DeferredValue::Unknown(
                            UnknownReason::UpstreamBareRef { binding },
                        )) => binding.as_str(),
                        other => panic!("expected UpstreamBareRef, got {:?}", other),
                    })
                    .collect();
                assert_eq!(bindings, vec!["bootstrap", "secondary"]);
            }
            other => panic!("expected List, got {:?}", other),
        }
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

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        assert!(matches!(
            resources[0].get_attr("vpc_id"),
            Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
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
                Value::Concrete(ConcreteValue::List(vec![
                    Value::resource_ref("network".to_string(), "public_sg".to_string(), Vec::new()),
                    Value::resource_ref(
                        "network".to_string(),
                        "private_sg".to_string(),
                        Vec::new(),
                    ),
                ])),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("security_group_ids") {
            Some(Value::Concrete(ConcreteValue::List(items))) => {
                assert_eq!(items.len(), 2);
                for (idx, item) in items.iter().enumerate() {
                    assert!(
                        matches!(item, Value::Deferred(DeferredValue::Unknown(_))),
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

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

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
            vec![("tags", Value::Concrete(ConcreteValue::Map(tag_map)))],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("tags") {
            Some(Value::Concrete(ConcreteValue::Map(m))) => match m.get("VpcId") {
                Some(Value::Deferred(DeferredValue::Unknown(_))) => {}
                other => panic!("nested entry should be Value::Unknown, got {:?}", other),
            },
            other => panic!("expected Map, got {:?}", other),
        }
    }

    /// carina#3085 design Test plan item 3: `resolve_ref_value` on a
    /// `<wait-binding>.<attr>` ResourceRef resolves through the
    /// materialised wait alias to the target's value — not left as an
    /// unresolved ref. Exercises the resolver layer directly (the
    /// real-pipeline E2E lives in `tests/wait_downstream_apply.rs`).
    #[test]
    fn resolve_ref_value_resolves_wait_binding_passthrough() {
        let cert = make_resource(
            "cert",
            Some("cert"),
            vec![(
                "certificate_arn",
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:acm:us-east-1:1:certificate/abc".to_string(),
                )),
            )],
        );
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: &[cert],
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[crate::binding_index::WaitAliasSpec {
                binding: crate::parser::BindingName::new("cert_issued"),
                target: crate::parser::BindingName::new("cert"),
            }],
        });
        let path = crate::resource::AccessPath::new("cert_issued", "certificate_arn");
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String(
                "arn:aws:acm:us-east-1:1:certificate/abc".to_string()
            )),
            "cert_issued.certificate_arn must resolve through the wait \
             alias to cert's value, not stay an unresolved ResourceRef"
        );
    }

    /// A wait binding whose target is absent: the ref is left intact
    /// (the existing `Ok(value.clone())` fallthrough), so the existing
    /// PlanError path — not a panic — surfaces it.
    #[test]
    fn resolve_ref_value_wait_binding_absent_target_left_intact() {
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: &[],
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[crate::binding_index::WaitAliasSpec {
                binding: crate::parser::BindingName::new("cert_issued"),
                target: crate::parser::BindingName::new("nonexistent"),
            }],
        });
        let path = crate::resource::AccessPath::new("cert_issued", "certificate_arn");
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved, ref_value,
            "no alias when target absent → ref stays intact (existing PlanError surfaces it)"
        );
    }
}
