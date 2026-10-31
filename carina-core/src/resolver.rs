//! Reference resolution for ResourceRef values
//!
//! Resolves `Value::ResourceRef` references by looking up bound resource attributes
//! from both the DSL definition and current infrastructure state.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::binding_index::{BindingValueSource, ResolvedBindings};
#[cfg(test)]
use crate::resource::State;
use crate::resource::{
    AccessMissPolicy, ConcreteValue, DataSource, DeferredValue, InterpolationPart, LiveComposition,
    NoPlaceholder, PlaceholderKind, PlanInputCurrent, Resource, ResourceId, Value,
    ValueAccessError, ValuePathSegmentResult, apply_value_path_segment,
};

/// Resolve all ResourceRef values in resources using current state.
///
/// Builds a binding map from resource attributes and state, then resolves
/// all ResourceRef values across all resources.
///
/// Before resolving, saves dependency binding names in the typed
/// `Resource::dependency_bindings` field. This preserves dependency information
/// that would otherwise be lost when ResourceRef values are replaced with plain
/// strings.
///
/// `remote_bindings` provides external bindings from upstream state data sources.
/// Each entry maps an upstream_state binding name to a map of resource binding names
/// to their attributes. For example, `network -> { vpc -> { vpc_id -> "vpc-123" } }`.
pub fn resolve_refs_with_state(
    resources: &mut [Resource],
    managed_current: &HashMap<ResourceId, PlanInputCurrent>,
) -> Result<(), String> {
    let bindings = crate::binding_index::ResolvedBindings::pre_apply_live(
        crate::binding_index::LivePreApplyInputs {
            managed: resources,
            compositions: &[],
            managed_current,
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    );
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
/// `Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport { path }))` so plan
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
/// `Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport { path }))` for display.
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
        let deps = crate::deps::get_resource_value_ref_dependencies(
            crate::parser::ResourceRef::Resource(resource),
        );
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
/// `Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport { path }))`.
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
        let deps = crate::deps::get_resource_value_ref_dependencies(
            crate::parser::ResourceRef::DataSource(data_source),
        );
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
/// `LiveComposition`s using a `ResolvedBindings` view that the caller
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
pub fn resolve_live_composition_refs_post_apply(
    compositions: &mut [LiveComposition],
    bindings: &ResolvedBindings,
) -> Result<(), String> {
    for v in compositions.iter_mut() {
        v.signature.attributes = resolve_composition_attributes(&v.signature.attributes, bindings)?;
    }
    Ok(())
}

pub fn resolve_saved_composition_refs_post_apply(
    compositions: &mut [crate::resource::SavedComposition],
    bindings: &ResolvedBindings,
) -> Result<(), String> {
    for composition in compositions {
        composition.signature.attributes =
            resolve_composition_attributes(&composition.signature.attributes, bindings)?;
    }
    Ok(())
}

trait RuntimeCompositionAttribute: Sized {
    fn runtime_value(&self) -> Value;
    fn with_runtime_value(&self, value: Value) -> Self;
}

impl RuntimeCompositionAttribute for crate::resource::LiveCompositionAttribute {
    fn runtime_value(&self) -> Value {
        self.to_value()
    }

    fn with_runtime_value(&self, value: Value) -> Self {
        self.with_value(value)
    }
}

impl RuntimeCompositionAttribute for crate::resource::SavedCompositionAttribute {
    fn runtime_value(&self) -> Value {
        self.to_value()
    }

    fn with_runtime_value(&self, value: Value) -> Self {
        self.with_value(value)
    }
}

fn resolve_composition_attributes<A: RuntimeCompositionAttribute>(
    attributes: &IndexMap<String, A>,
    bindings: &ResolvedBindings,
) -> Result<IndexMap<String, A>, String> {
    attributes
        .iter()
        .map(|(key, attribute)| {
            // Reify the typed attribute into a value, resolve it, then
            // reclassify it in the same lifecycle. A forwarded reference that
            // resolves to a literal becomes derived; one that remains a
            // reference stays forwarded.
            let resolved = resolve_ref_value(&attribute.runtime_value(), bindings)?;
            Ok((key.clone(), attribute.with_runtime_value(resolved)))
        })
        .collect()
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
                crate::resource::ApplyTimeUnknown::UpstreamExport { path },
            ))
        }
        Value::Deferred(DeferredValue::BindingRef { binding })
            if upstream_binding_names.contains(binding.as_str()) =>
        {
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::UpstreamAllExports { binding },
            ))
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(stamp_unresolved_upstream(*value, upstream_binding_names)),
                default: Box::new(stamp_unresolved_upstream(*default, upstream_binding_names)),
            })
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
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => Value::Deferred(DeferredValue::FunctionCall {
            name,
            args: args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| {
                            Ok::<_, std::convert::Infallible>(stamp_unresolved_upstream(
                                value.clone(),
                                upstream_binding_names,
                            ))
                        })
                        .expect("infallible function-argument rewrite")
                })
                .collect(),
            source_line,
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args: args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| {
                            Ok::<_, std::convert::Infallible>(stamp_unresolved_upstream(
                                value.clone(),
                                upstream_binding_names,
                            ))
                        })
                        .expect("infallible function-argument rewrite")
                })
                .collect(),
            source_line,
        }),
        Value::Deferred(DeferredValue::Access(access)) => {
            let original = Value::Deferred(DeferredValue::Access(access.clone()));
            access
                .rebuild_with_base(
                    stamp_unresolved_upstream(access.base().clone(), upstream_binding_names),
                    AccessMissPolicy::Error,
                )
                .unwrap_or(original)
        }
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(stamp_unresolved_upstream(*inner, upstream_binding_names)),
        )),
        // Already-stamped or non-reference leaves pass through unchanged.
        value @ Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | value @ Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => value,
        Value::Deferred(DeferredValue::Placeholder(never)) => match never {},
    }
}

/// Plan-only single-value counterpart of [`resolve_refs_for_plan`].
///
/// Resolves `ResourceRef` segments against `bindings` and then stamps
/// any surviving ref whose root binding is in
/// `unresolved_upstream_bindings` as
/// `Value::Deferred(DeferredValue::Unknown(UpstreamExport { path }))` so
/// plan display renders it as `(known after upstream apply: …)`.
///
/// Used by `carina_cli::wiring::add_state_block_effects` for the `id`
/// value of an `import { … }` block: a `"${X.attr}|…"`
/// interpolation that references a deferred upstream-state value must
/// either fold to a concrete cloud identifier (when `X.attr` is in
/// scope) or carry the deferred marker through plan display (carina#3329).
/// Errors are converted into the input value untouched — same shape as
/// the resource-attribute path, where a non-evaluable interpolation
/// stays a `Value::Deferred(DeferredValue::Interpolation)` rather than
/// aborting the plan.
pub fn resolve_value_for_plan(
    value: &Value,
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
) -> Value {
    let resolved = resolve_ref_value(value, bindings).unwrap_or_else(|_| value.clone());
    if unresolved_upstream_bindings.is_empty() {
        resolved
    } else {
        stamp_unresolved_upstream(resolved, unresolved_upstream_bindings)
    }
}

/// Recursively resolve a single Value, replacing ResourceRef with the referenced value.
///
/// If the referenced binding or attribute is not found, the value is returned as-is.
/// Returns an error if a builtin function fails with fully-resolved arguments.
pub fn resolve_ref_value<P: PlaceholderKind>(
    value: &Value<P>,
    bindings: &ResolvedBindings<P>,
) -> Result<Value<P>, String> {
    resolve_ref_value_inner(value, bindings, None)
}

/// Three-state result used by null/missing coalescing. A missing value is not
/// interchangeable with a value whose producing snapshot is still incomplete.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueResolution<P: PlaceholderKind = NoPlaceholder> {
    Present(Value<P>),
    Absent,
    Unknown(Value<P>),
}

/// Resolve a value while preserving the distinction required by `??`.
pub fn resolve_ref_value_status<P: PlaceholderKind>(
    value: &Value<P>,
    bindings: &ResolvedBindings<P>,
) -> Result<ValueResolution<P>, String> {
    resolve_ref_value_status_inner(value, bindings, None, false)
}

/// Resolve a value using binding data and a provider-aware builtin context.
///
/// Export validation uses this entry point because a reference may become an
/// argument to `decrypt()` (or another context-sensitive builtin) only after
/// its binding has been substituted.
pub fn resolve_ref_value_with_config<P: PlaceholderKind>(
    value: &Value<P>,
    bindings: &ResolvedBindings<P>,
    config: &crate::parser::ProviderContext,
) -> Result<Value<P>, String> {
    resolve_ref_value_inner(value, bindings, Some(config))
}

/// Select the miss policy for a path-shaped reference. A missing source is not
/// evidence of a partially-known local value, so it uses the strict policy.
fn binding_access_miss_policy<P: PlaceholderKind>(
    binding: &str,
    bindings: &ResolvedBindings<P>,
) -> AccessMissPolicy {
    let Some(source) = bindings.source(binding) else {
        return AccessMissPolicy::Error;
    };
    match source {
        BindingValueSource::Local => AccessMissPolicy::Defer,
        BindingValueSource::Upstream => AccessMissPolicy::Error,
        BindingValueSource::WaitAlias { .. } => AccessMissPolicy::Defer,
    }
}

fn missing_access(error: &ValueAccessError) -> bool {
    matches!(
        error,
        ValueAccessError::MissingField { .. } | ValueAccessError::MissingKey { .. }
    )
}

fn resolve_ref_value_status_inner<P: PlaceholderKind>(
    value: &Value<P>,
    bindings: &ResolvedBindings<P>,
    config: Option<&crate::parser::ProviderContext>,
    allow_absent_default: bool,
) -> Result<ValueResolution<P>, String> {
    match value {
        Value::Deferred(DeferredValue::Coalesce {
            value: left,
            default,
        }) => {
            let left_resolution = if left.is_coalesce_optional_operand() {
                resolve_ref_value_status_inner(left, bindings, config, true)?
            } else {
                resolve_required_value_status(left, bindings, config)?
            };
            match left_resolution {
                ValueResolution::Present(value) => Ok(ValueResolution::Present(value)),
                ValueResolution::Absent
                    if allow_absent_default && default.is_coalesce_optional_operand() =>
                {
                    resolve_ref_value_status_inner(default, bindings, config, true)
                }
                ValueResolution::Absent => resolve_required_value_status(default, bindings, config),
                ValueResolution::Unknown(_) => Ok(ValueResolution::Unknown(value.clone())),
            }
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let binding = path.binding();
            let Some(attributes) = bindings.get(binding) else {
                return Ok(ValueResolution::Unknown(value.clone()));
            };
            let value_is_definitive =
                bindings.attribute_value_is_definitive(binding, path.attribute());
            let absence_is_definitive =
                bindings.attribute_absence_is_definitive(binding, path.attribute());
            let Some(attribute) = attributes.get(path.attribute()) else {
                return if absence_is_definitive {
                    Ok(ValueResolution::Absent)
                } else {
                    Ok(ValueResolution::Unknown(value.clone()))
                };
            };
            if !value_is_definitive {
                return Ok(ValueResolution::Unknown(value.clone()));
            }

            let mut resolved = resolve_ref_value_inner(attribute, bindings, config)?;
            if !resolved.is_fully_resolved() {
                return Ok(ValueResolution::Unknown(value.clone()));
            }
            for segment in path.segments() {
                match apply_value_path_segment(
                    resolved,
                    segment.clone(),
                    0,
                    AccessMissPolicy::Error,
                ) {
                    Ok(ValuePathSegmentResult::Resolved(next)) => {
                        resolved = resolve_ref_value_inner(&next, bindings, config)?;
                        if !resolved.is_fully_resolved() {
                            return Ok(ValueResolution::Unknown(value.clone()));
                        }
                    }
                    Ok(
                        ValuePathSegmentResult::Deferred(_)
                        | ValuePathSegmentResult::DeferredMiss(_),
                    ) => return Ok(ValueResolution::Unknown(value.clone())),
                    Err(error) if missing_access(&error) && absence_is_definitive => {
                        return Ok(ValueResolution::Absent);
                    }
                    Err(error) if missing_access(&error) => {
                        return Ok(ValueResolution::Unknown(value.clone()));
                    }
                    Err(error) => return Err(error.format_for_path(&path.to_string())),
                }
            }
            Ok(ValueResolution::Present(resolved))
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            let Some(attributes) = bindings.get(binding) else {
                return Ok(ValueResolution::Unknown(value.clone()));
            };
            if !bindings.binding_is_fully_definitive(binding) {
                return Ok(ValueResolution::Unknown(value.clone()));
            }
            let mut keys = attributes.keys().collect::<Vec<_>>();
            keys.sort();
            let values = keys
                .into_iter()
                .map(|key| (key.clone(), attributes[key].clone()))
                .collect();
            let resolved = Value::Concrete(ConcreteValue::Map(values));
            if resolved.is_fully_resolved() {
                Ok(ValueResolution::Present(resolved))
            } else {
                Ok(ValueResolution::Unknown(value.clone()))
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let resolved_base = match resolve_ref_value_status_inner(
                access.base(),
                bindings,
                config,
                allow_absent_default,
            )? {
                ValueResolution::Present(value) => value,
                ValueResolution::Absent => return Ok(ValueResolution::Absent),
                ValueResolution::Unknown(_) => {
                    return Ok(ValueResolution::Unknown(value.clone()));
                }
            };
            match access.rebuild_result(
                resolved_base,
                access.step().clone(),
                AccessMissPolicy::Error,
            ) {
                Ok(ValuePathSegmentResult::Resolved(value)) => {
                    resolve_ref_value_status_inner(&value, bindings, config, allow_absent_default)
                }
                Ok(
                    ValuePathSegmentResult::Deferred(_) | ValuePathSegmentResult::DeferredMiss(_),
                ) => Ok(ValueResolution::Unknown(value.clone())),
                Err(error) if missing_access(&error) => Ok(ValueResolution::Absent),
                Err(error) => Err(error.format_for_path(&crate::value::format_value(value))),
            }
        }
        Value::Deferred(DeferredValue::Unknown(_)) => Ok(ValueResolution::Unknown(value.clone())),
        Value::Deferred(DeferredValue::Unavailable { name, reason, .. }) => {
            Err(format!("{name}(): {reason}"))
        }
        Value::Concrete(_)
        | Value::Deferred(
            DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. },
        ) => {
            let resolved = resolve_ref_value_inner(value, bindings, config)?;
            if !resolved.is_fully_resolved() {
                Ok(ValueResolution::Unknown(resolved))
            } else {
                Ok(ValueResolution::Present(resolved))
            }
        }
        Value::Deferred(DeferredValue::Placeholder(_)) => {
            Ok(ValueResolution::Unknown(value.clone()))
        }
    }
}

/// Resolve a required expression without interpreting a direct miss as the
/// absence of an enclosing coalesce operand.
fn resolve_required_value_status<P: PlaceholderKind>(
    value: &Value<P>,
    bindings: &ResolvedBindings<P>,
    config: Option<&crate::parser::ProviderContext>,
) -> Result<ValueResolution<P>, String> {
    let resolved = resolve_ref_value_inner(value, bindings, config)?;
    if resolved.is_fully_resolved() {
        Ok(ValueResolution::Present(resolved))
    } else {
        Ok(ValueResolution::Unknown(resolved))
    }
}

fn parser_stage_function_arguments<P: PlaceholderKind>(
    args: &[crate::resource::FunctionArgument<P>],
) -> Option<Vec<crate::resource::FunctionArgument<crate::resource::ParserPlaceholder>>> {
    let expanded = args
        .iter()
        .cloned()
        .map(crate::resource::FunctionArgument::try_into_expanded)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(
        expanded
            .into_iter()
            .map(crate::resource::FunctionArgument::into_placeholder_type)
            .collect(),
    )
}

enum FunctionArgumentResolution<P: PlaceholderKind> {
    Deferred(Vec<crate::resource::FunctionArgument<P>>),
    FullyResolved(Vec<crate::resource::FunctionArgument<P>>),
}

fn resolve_function_arguments<P: PlaceholderKind>(
    args: &[crate::resource::FunctionArgument<P>],
    bindings: &ResolvedBindings<P>,
    config: Option<&crate::parser::ProviderContext>,
) -> Result<FunctionArgumentResolution<P>, String> {
    let args = args
        .iter()
        .map(|argument| {
            argument.try_map_value(|value| resolve_ref_value_inner(value, bindings, config))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if args.iter().all(|argument| {
        argument.handle().is_some() || argument.value().is_some_and(Value::is_fully_resolved)
    }) {
        Ok(FunctionArgumentResolution::FullyResolved(args))
    } else {
        Ok(FunctionArgumentResolution::Deferred(args))
    }
}

fn resolve_ref_value_inner<P: PlaceholderKind>(
    value: &Value<P>,
    bindings: &ResolvedBindings<P>,
    config: Option<&crate::parser::ProviderContext>,
) -> Result<Value<P>, String> {
    match value {
        Value::Deferred(DeferredValue::Coalesce { .. }) => {
            match resolve_ref_value_status_inner(value, bindings, config, false)? {
                ValueResolution::Present(value) | ValueResolution::Unknown(value) => Ok(value),
                // Keep the original expression if a future resolver adds an
                // absent default form. Residual coalesces are rejected at
                // provider/state boundaries, so this cannot silently cross a
                // concrete-value boundary.
                ValueResolution::Absent => Ok(value.clone()),
            }
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            if let Some(attrs) = bindings.get(binding_name)
                && let Some(attr_value) = attrs.get(attribute_name)
            {
                // Resolve the initial attribute value
                let mut resolved = resolve_ref_value_inner(attr_value, bindings, config)?;
                let miss_policy = binding_access_miss_policy(binding_name, bindings);

                for segment in path.segments() {
                    match apply_value_path_segment(resolved, segment.clone(), 0, miss_policy) {
                        Ok(ValuePathSegmentResult::Resolved(next)) => {
                            resolved = resolve_ref_value_inner(&next, bindings, config)?;
                        }
                        Ok(
                            ValuePathSegmentResult::Deferred(_)
                            | ValuePathSegmentResult::DeferredMiss(_),
                        ) => return Ok(value.clone()),
                        Err(error) => return Err(error.format_for_path(&path.to_string())),
                    }
                }

                return Ok(resolved);
            }
            // Keep as-is if not found
            Ok(value.clone())
        }
        Value::Deferred(DeferredValue::BindingRef { .. }) => Ok(value.clone()),
        Value::Concrete(ConcreteValue::List(items)) => {
            let resolved: Result<Vec<Value<P>>, String> = items
                .iter()
                .map(|v| resolve_ref_value_inner(v, bindings, config))
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(resolved?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut resolved: IndexMap<String, Value<P>> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_ref_value_inner(v, bindings, config)?);
            }
            Ok(Value::Concrete(ConcreteValue::Map(resolved)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let resolved_parts: Result<Vec<InterpolationPart<P>>, String> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => Ok(InterpolationPart::Expr(
                        resolve_ref_value_inner(v, bindings, config)?,
                    )),
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(resolved_parts?)).canonicalize())
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => {
            let resolved_args = match resolve_function_arguments(args, bindings, config)? {
                FunctionArgumentResolution::FullyResolved(args) => args,
                FunctionArgumentResolution::Deferred(args) => {
                    return Ok(Value::Deferred(DeferredValue::FunctionCall {
                        name: *name,
                        args,
                        source_line: *source_line,
                    }));
                }
            };

            // Evaluate the built-in function; propagate errors since args are resolved.
            // The evaluator boundary is `EvalValue::into_value` — a closure
            // returned here would mean `evaluate_builtin` saw fewer args
            // than the function's arity, which can only happen if a
            // partial application leaked through parsing. The parser is
            // supposed to surface that as a parse error, so treat any
            // closure here as a resolver-level invariant break.
            use crate::eval_value::EvalValue;
            let Some(parser_args) = parser_stage_function_arguments(&resolved_args) else {
                return Ok(Value::Deferred(DeferredValue::FunctionCall {
                    name: *name,
                    args: resolved_args,
                    source_line: *source_line,
                }));
            };
            let eval_args: Vec<EvalValue> = parser_args
                .iter()
                .map(crate::parser::function_argument_as_eval)
                .collect();
            let evaluated = match config {
                Some(config) => {
                    crate::builtins::evaluate_builtin_with_config(name, &eval_args, config)
                }
                None => crate::builtins::evaluate_builtin(name, &eval_args),
            };
            match evaluated {
                Ok(result) => result
                    .into_value()
                    .map_err(|error| format!("{name}(): {error}"))?
                    .into_expanded()
                    .map(Value::into_placeholder_type)
                    .map_err(|error| format!("{name}(): {error}")),
                Err(e) => Err(format!("{}(): {}", name, e)),
            }
        }
        Value::Deferred(DeferredValue::Unavailable { name, reason, .. }) => {
            Err(format!("{name}(): {reason}"))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => {
            let resolved_args = match resolve_function_arguments(args, bindings, config)? {
                FunctionArgumentResolution::FullyResolved(args) => args,
                FunctionArgumentResolution::Deferred(args) => {
                    return Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                        function: function.clone(),
                        functions: functions.clone(),
                        args,
                        source_line: *source_line,
                    }));
                }
            };

            let default_config = crate::parser::ProviderContext::default();
            let config = config.unwrap_or(&default_config);
            let Some(parser_args) = parser_stage_function_arguments(&resolved_args) else {
                return Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                    function: function.clone(),
                    functions: functions.clone(),
                    args: resolved_args,
                    source_line: *source_line,
                }));
            };
            crate::parser::evaluate_resolved_user_function(
                function,
                functions,
                &parser_args,
                config,
                source_line.get().unwrap_or(function.body.source_line),
            )
            .map_err(|error| error.to_string())?
            .into_expanded()
            .map(Value::into_placeholder_type)
            .map_err(|error| error.to_string())
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let access_expression = crate::value::format_value(value);
            let resolved_base = resolve_ref_value_inner(access.base(), bindings, config)?;
            // Construction canonicalizes reference-rooted access into a
            // ResourceRef/BindingRef. Every surviving Access therefore has a
            // computed container, whose missing element is a final error.
            access
                .rebuild_with_base(resolved_base, AccessMissPolicy::Error)
                .map_err(|error| {
                    let message = error.format_for_path(&access_expression);
                    if access.source_line() == 0 {
                        message
                    } else {
                        format!("at line {}: {message}", access.source_line())
                    }
                })
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            let resolved_inner = resolve_ref_value_inner(inner, bindings, config)?;
            Ok(Value::Deferred(DeferredValue::Secret(Box::new(
                resolved_inner,
            ))))
        }
        // `Value::Unknown` is the result of stamping a previously-
        // unresolved upstream ref; it cannot be resolved further.
        Value::Deferred(DeferredValue::Unknown(_)) => Ok(value.clone()),
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(DeferredValue::RedactedSecret { .. }) => Ok(value.clone()),
        Value::Deferred(DeferredValue::Placeholder(_)) => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding_index::{BindingCompleteness, SecretPreservingAttributes};
    use crate::resource::ResourceId;
    use std::collections::HashSet;

    fn make_resource(name: &str, binding: Option<&str>, attrs: Vec<(&str, Value)>) -> Resource {
        let mut r = Resource::new(
            "test.resource",
            crate::resource::ResourceIdentity::try_from(name.to_string()).unwrap(),
        );
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
        ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        })
    }

    fn coalesce(value: Value, default: Value) -> Value {
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(value),
            default: Box::new(default),
        })
    }

    #[test]
    fn coalesce_resolution_is_present_absent_or_unknown() {
        let default = Value::Concrete(ConcreteValue::List(Vec::new()));
        let accounts = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("a".into()),
        )]));
        let expression = coalesce(
            Value::resource_ref("up", "accounts", Vec::new()),
            default.clone(),
        );

        let mut present = ResolvedBindings::default();
        present.set(
            "up",
            HashMap::from([("accounts".to_string(), accounts.clone())]),
            BindingValueSource::Upstream,
        );
        assert_eq!(
            resolve_ref_value_status(&expression, &present),
            Ok(ValueResolution::Present(accounts)),
        );

        let mut absent = ResolvedBindings::default();
        absent.set("up", HashMap::new(), BindingValueSource::Upstream);
        assert_eq!(
            resolve_ref_value_status(&expression, &absent),
            Ok(ValueResolution::Present(default)),
        );

        let mut unknown = ResolvedBindings::default();
        unknown.set_with_completeness(
            "up",
            HashMap::new(),
            BindingValueSource::Upstream,
            BindingCompleteness::Incomplete,
        );
        assert_eq!(
            resolve_ref_value_status(&expression, &unknown),
            Ok(ValueResolution::Unknown(expression)),
        );
    }

    #[test]
    fn coalesce_groupings_propagate_absence_associatively() {
        let first = Value::resource_ref("r", "comment", Vec::new());
        let second = Value::resource_ref("r", "parent", Vec::new());
        let fallback = Value::Concrete(ConcreteValue::String("default".into()));
        let left_grouped = coalesce(coalesce(first.clone(), second.clone()), fallback.clone());
        let right_grouped = coalesce(first, coalesce(second, fallback.clone()));
        let cases = [
            (
                HashMap::from([
                    (
                        "comment".to_string(),
                        Value::Concrete(ConcreteValue::String("first".into())),
                    ),
                    (
                        "parent".to_string(),
                        Value::Concrete(ConcreteValue::String("second".into())),
                    ),
                ]),
                Value::Concrete(ConcreteValue::String("first".into())),
            ),
            (
                HashMap::from([(
                    "parent".to_string(),
                    Value::Concrete(ConcreteValue::String("second".into())),
                )]),
                Value::Concrete(ConcreteValue::String("second".into())),
            ),
            (HashMap::new(), fallback.clone()),
        ];

        for (attributes, expected) in cases {
            let mut bindings = ResolvedBindings::default();
            bindings.set_with_completeness(
                "r",
                attributes,
                BindingValueSource::Local,
                BindingCompleteness::Complete,
            );
            for expression in [&left_grouped, &right_grouped] {
                assert_eq!(
                    resolve_ref_value_status(expression, &bindings),
                    Ok(ValueResolution::Present(expected.clone())),
                );
            }
        }
    }

    #[test]
    fn coalesce_does_not_treat_explicit_unknown_as_absent() {
        let unknown_reason = crate::resource::ApplyTimeUnknown::PendingReRead(
            crate::resource::PendingReRead::incomplete("provider read incomplete"),
        );
        let expression = coalesce(
            Value::Deferred(DeferredValue::Unknown(unknown_reason)),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );

        let resolved = resolve_ref_value_status(&expression, &ResolvedBindings::default())
            .expect("unknown coalesce must remain resolvable");
        assert!(matches!(
            resolved,
            ValueResolution::Unknown(Value::Deferred(DeferredValue::Coalesce { .. }))
        ));
    }

    #[test]
    fn coalesce_does_not_evaluate_function_with_unknown_argument() {
        let function = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![
                Value::Deferred(DeferredValue::Unknown(
                    crate::resource::ApplyTimeUnknown::PendingReRead(
                        crate::resource::PendingReRead::incomplete("producer still running"),
                    ),
                ))
                .into(),
            ],
            source_line: 1.into(),
        });
        let expression = coalesce(
            function,
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );

        assert!(matches!(
            resolve_ref_value_status(&expression, &ResolvedBindings::default()),
            Ok(ValueResolution::Unknown(Value::Deferred(
                DeferredValue::Coalesce { .. }
            )))
        ));
    }

    #[test]
    fn coalesce_does_not_treat_function_error_as_absence() {
        let expression = coalesce(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::upper,
                args: vec![Value::Concrete(ConcreteValue::Int(1)).into()],
                source_line: 1.into(),
            }),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );

        let error = resolve_ref_value_status(&expression, &ResolvedBindings::default())
            .expect_err("function errors must not select the coalesce default");
        assert!(error.contains("upper()"), "unexpected error: {error}");
    }

    #[test]
    fn selected_coalesce_fallback_propagates_unavailable_builtin_error() {
        let missing = "CARINA_TEST_ISSUE_3974_SELECTED_FALLBACK_MISSING";
        let expression = coalesce(
            Value::resource_ref("r", "comment", Vec::new()),
            Value::Deferred(DeferredValue::Unavailable {
                name: crate::builtins::BuiltinFunction::env,
                args: vec![Value::Concrete(ConcreteValue::String(missing.into())).into()],
                reason: crate::resource::UnavailableReason::from_builtin_error(format!(
                    "environment variable '{missing}' is not set"
                )),
                source_line: 1.into(),
            }),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "r",
            HashMap::new(),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );

        assert_eq!(
            resolve_ref_value_status(&expression, &bindings),
            Err(format!(
                "env(): environment variable '{missing}' is not set"
            )),
        );
    }

    #[test]
    fn incomplete_binding_distinguishes_desired_values_from_provisional_outputs() {
        let resource = make_resource(
            "up-resource",
            Some("up"),
            vec![
                ("name", Value::Concrete(ConcreteValue::String("new".into()))),
                (
                    "stable",
                    Value::Concrete(ConcreteValue::String("same".into())),
                ),
            ],
        );
        let state = State::existing(
            resource.id.clone(),
            HashMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("old".into())),
                ),
                (
                    "accounts".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::String("old-account".into()),
                    )])),
                ),
                (
                    "stable".to_string(),
                    Value::Concrete(ConcreteValue::String("same".into())),
                ),
                (
                    "removed".to_string(),
                    Value::Concrete(ConcreteValue::String("old".into())),
                ),
            ]),
        )
        .with_identifier("up-id");
        let current = crate::resource::into_plan_input_current_map_from_states(
            HashMap::from([(resource.id.clone(), state)]),
            &crate::schema::SchemaRegistry::new(),
            std::slice::from_ref(&resource),
        );
        let stability = HashMap::from([(
            resource.id.clone(),
            crate::binding_index::ManagedBindingStability::Updating {
                changed_attributes: HashSet::from(["name".to_string(), "removed".to_string()]),
                refreshable_attributes: HashSet::from([
                    "accounts".to_string(),
                    "missing_output".to_string(),
                ]),
            },
        )]);
        let bindings = ResolvedBindings::pre_apply_live_with_stability(
            crate::binding_index::LivePreApplyInputs {
                managed: std::slice::from_ref(&resource),
                compositions: &[],
                managed_current: &current,
                data_source_values: &crate::data_source_value::DataSourceValues::empty(),
                remote_bindings: &HashMap::new(),
                wait_aliases: &[],
            },
            &stability,
        );

        let changed = coalesce(
            Value::resource_ref("up", "name", vec![]),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );
        assert_eq!(
            resolve_ref_value_status(&changed, &bindings),
            Ok(ValueResolution::Present(Value::Concrete(
                ConcreteValue::String("new".into()),
            ))),
            "a resolved authored desired value overrides refreshed state during its update",
        );

        let removed_default = Value::Concrete(ConcreteValue::String("removed-default".into()));
        assert_eq!(
            resolve_ref_value_status(
                &coalesce(
                    Value::resource_ref("up", "removed", vec![]),
                    removed_default.clone(),
                ),
                &bindings,
            ),
            Ok(ValueResolution::Present(removed_default)),
            "an attribute removed by the update is definitively absent",
        );

        let provisional = coalesce(
            Value::resource_ref("up", "accounts", vec![]),
            Value::Concrete(ConcreteValue::List(Vec::new())),
        );
        assert_eq!(
            resolve_ref_value_status(&provisional, &bindings),
            Ok(ValueResolution::Present(Value::Concrete(
                ConcreteValue::List(vec![Value::Concrete(ConcreteValue::String(
                    "old-account".into(),
                ))]),
            ))),
            "a present provider-populated value remains usable during an unrelated update",
        );

        let missing_refreshable = coalesce(
            Value::resource_ref("up", "missing_output", vec![]),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );
        assert_eq!(
            resolve_ref_value_status(&missing_refreshable, &bindings),
            Ok(ValueResolution::Unknown(missing_refreshable)),
            "a missing provider-populated value may still appear during refresh",
        );

        assert_eq!(
            resolve_ref_value_status(
                &coalesce(
                    Value::resource_ref("up", "stable", vec![]),
                    Value::Concrete(ConcreteValue::String("fallback".into())),
                ),
                &bindings,
            ),
            Ok(ValueResolution::Present(Value::Concrete(
                ConcreteValue::String("same".into()),
            ))),
            "an unchanged attribute remains definitive during an update",
        );
        let missing_default = Value::Concrete(ConcreteValue::String("fallback".into()));
        assert_eq!(
            resolve_ref_value_status(
                &coalesce(
                    Value::resource_ref("up", "unrelated_optional", vec![]),
                    missing_default.clone(),
                ),
                &bindings,
            ),
            Ok(ValueResolution::Present(missing_default)),
            "absence of an attribute the update cannot change remains definitive",
        );
    }

    #[test]
    fn unchanged_ref_authored_producer_has_complete_refreshed_outputs() {
        let source = make_resource(
            "source",
            Some("source"),
            vec![(
                "name",
                Value::Concrete(ConcreteValue::String("seed".into())),
            )],
        );
        let producer = make_resource(
            "producer",
            Some("producer"),
            vec![("parent_name", Value::resource_ref("source", "name", vec![]))],
        );
        let source_state = State::existing(
            source.id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("seed".into())),
            )]),
        )
        .with_identifier("source-id");
        let producer_state = State::existing(
            producer.id.clone(),
            HashMap::from([(
                "parent_name".to_string(),
                Value::Concrete(ConcreteValue::String("seed".into())),
            )]),
        )
        .with_identifier("producer-id");
        let resources = [source, producer];
        let current = crate::resource::into_plan_input_current_map_from_states(
            HashMap::from([
                (resources[0].id.clone(), source_state),
                (resources[1].id.clone(), producer_state),
            ]),
            &crate::schema::SchemaRegistry::new(),
            &resources,
        );
        let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &current,
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        let fallback = Value::Concrete(ConcreteValue::String("d".into()));
        let expression = coalesce(
            Value::resource_ref("producer", "comment", vec![]),
            fallback.clone(),
        );

        assert_eq!(
            resolve_ref_value_status(&expression, &bindings),
            Ok(ValueResolution::Present(fallback)),
            "a successful refresh with no planned effect makes a missing output definitively absent",
        );
    }

    #[test]
    fn post_apply_complete_binding_treats_missing_output_as_absent() {
        let resource = make_resource("producer", Some("producer"), vec![]);
        let state =
            State::existing(resource.id.clone(), HashMap::new()).with_identifier("provider-id");
        let mut bindings = ResolvedBindings::default();
        let empty = HashMap::new();
        let resolved = SecretPreservingAttributes::from_provider_values(&empty, &empty);
        bindings.record_applied(Some("producer"), &resolved, &state);
        let default = Value::Concrete(ConcreteValue::String("fallback".into()));
        let expression = coalesce(
            Value::resource_ref("producer", "optional", vec![]),
            default.clone(),
        );

        assert_eq!(
            resolve_ref_value_status(&expression, &bindings),
            Ok(ValueResolution::Present(default)),
        );
    }

    #[test]
    fn coalesce_propagates_wrong_shape_errors() {
        use crate::resource::{AccessPath, PathSegment};

        let mut bindings = ResolvedBindings::default();
        bindings.set(
            "up",
            HashMap::from([(
                "accounts".to_string(),
                Value::Concrete(ConcreteValue::String("not-a-map".into())),
            )]),
            BindingValueSource::Upstream,
        );
        let expression = coalesce(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::with_segments(
                    "up",
                    "accounts",
                    vec![PathSegment::Field {
                        name: "missing".to_string(),
                    }],
                ),
            }),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );

        let error = resolve_ref_value_status(&expression, &bindings)
            .expect_err("wrong-shaped access must remain an error");
        assert!(
            error.contains("cannot apply access"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn coalesce_does_not_treat_missing_field_on_computed_value_as_optional() {
        use crate::resource::{AccessMissPolicy, PathSegment, apply_postfix_access};

        let mut choices = IndexMap::new();
        choices.insert(
            "selected".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "present".to_string(),
                Value::Concrete(ConcreteValue::String("value".into())),
            )]))),
        );
        let computed = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::lookup,
            args: vec![
                Value::Concrete(ConcreteValue::Map(choices)).into(),
                Value::resource_ref("selector", "key", vec![]).into(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())).into(),
            ],
            source_line: 1.into(),
        });
        let missing = apply_postfix_access(
            computed,
            PathSegment::Field {
                name: "missing".to_string(),
            },
            1,
            AccessMissPolicy::Error,
        )
        .expect("deferred computed access");
        let expression = coalesce(
            missing,
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set(
            "selector",
            HashMap::from([(
                "key".to_string(),
                Value::Concrete(ConcreteValue::String("selected".into())),
            )]),
            BindingValueSource::Upstream,
        );

        let error = resolve_ref_value_status(&expression, &bindings)
            .expect_err("only an access chain rooted at a reference is optional");
        assert!(error.contains("missing"), "unexpected error: {error}");
    }

    #[test]
    fn local_bare_binding_ref_stays_typed_during_general_resolution() {
        let bindings = bindings_from(vec![("vpc", vec![])]);
        let reference = Value::Deferred(DeferredValue::BindingRef {
            binding: "vpc".to_string(),
        });

        assert_eq!(resolve_ref_value(&reference, &bindings), Ok(reference));
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
        let plan_input_states = crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        );
        let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: resources,
            compositions: &[],
            managed_current: &plan_input_states,
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
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
        let plan_input_states = crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        );
        let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: resources,
            compositions: &[],
            managed_current: &plan_input_states,
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
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
                    Value::Concrete(ConcreteValue::String("alpha".into())),
                    Value::Concrete(ConcreteValue::String("beta".into())),
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
            Value::Concrete(ConcreteValue::String("alpha".into()))
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
            Value::Concrete(ConcreteValue::String("alpha".into())),
        )]
        .into_iter()
        .collect();
        let inner_beta: indexmap::IndexMap<String, Value> = vec![(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("beta".into())),
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
            Value::Concrete(ConcreteValue::String("alpha".into()))
        );
    }

    #[test]
    fn test_resolve_subscript_descends_into_map() {
        // `orgs.accounts["alpha"]` against `accounts: { alpha = "1", beta = "2" }`
        // resolves to `"1"`.
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "alpha".to_string(),
                Value::Concrete(ConcreteValue::String("1".into())),
            ),
            (
                "beta".to_string(),
                Value::Concrete(ConcreteValue::String("2".into())),
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
                key: "alpha".into(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, Value::Concrete(ConcreteValue::String("1".into())));
    }

    #[test]
    fn test_resolve_subscript_descends_into_secret_wrapped_map() {
        // #2439: subscript on `Secret(Map)` must descend and re-wrap
        // so plan-display redaction survives end-to-end.
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "db_pwd".to_string(),
                Value::Concrete(ConcreteValue::String("hunter2".into())),
            ),
            (
                "api_key".to_string(),
                Value::Concrete(ConcreteValue::String("xyz".into())),
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
                key: "db_pwd".into(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hunter2".into())
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
                        Value::Concrete(ConcreteValue::String("alpha".into())),
                        Value::Concrete(ConcreteValue::String("beta".into())),
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
                ConcreteValue::String("beta".into())
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
            Value::Concrete(ConcreteValue::String("hunter2".into())),
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
                ConcreteValue::String("hunter2".into())
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
            Value::Concrete(ConcreteValue::String("v".into())),
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
            vec![crate::resource::Subscript::Str { key: "k".into() }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
                DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::String("v".into()))))
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
                        "only".into(),
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
            Value::Concrete(ConcreteValue::String("v".into())),
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
                key: "missing".into(),
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
                    ConcreteValue::String("v".into()),
                )))),
            ),
            (
                "api_key".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("v".into()),
                )))),
            ),
            (
                "slack_token".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("v".into()),
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
                key: "missing".into(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let result = resolve_ref_value(&ref_value, &bindings);
        let err = result.expect_err("missing key in concrete upstream Secret(Map) must error");
        assert_eq!(
            err,
            "h.creds['missing']: key not found; available keys: <redacted, 3 entries>"
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
                    ConcreteValue::String("v".into()),
                )))),
            ),
            (
                "api_key".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("v".into()),
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
        assert_eq!(
            err,
            "h.creds.missing: key not found; available keys: <redacted, 2 entries>"
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
                key: "missing".into(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let err = resolve_ref_value(&ref_value, &bindings)
            .expect_err("missing key in empty upstream Secret(Map) must error");
        assert_eq!(
            err,
            "h.creds['missing']: key not found; available keys: <redacted, 0 entries>"
        );
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_unresolved_inner_ref_stays_deferred() {
        // Defensive: after the initial `resolve_ref_value(attr_value)`
        // call, the `resolved` value can be a `Secret(ResourceRef)` if
        // the inner ref points to an as-yet-unresolved binding. The
        // peel/rewrap loops then expose the inner ResourceRef and the
        // shared access primitive must preserve the Secret wrapper and
        // retain the pending projection as Access — never strip the
        // Secret tag or pretend the projection is concrete.
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
            vec![crate::resource::Subscript::Str { key: "k".into() }],
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
        // `orgs.accounts[5]` against a one-element local list keeps the
        // reference: a local desired-side view may still be incomplete.
        let bindings = bindings_from(vec![(
            "orgs",
            vec![(
                "accounts",
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("a".into()),
                )])),
            )],
        )]);
        let path = crate::resource::AccessPath::with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 5 }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_simple_resource_ref() {
        let bindings = bindings_from(vec![(
            "my_vpc",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("vpc-123".into())),
            )],
        )]);

        let ref_value = Value::resource_ref("my_vpc".to_string(), "id".to_string(), vec![]);

        let resolved = resolve_ref_value(&ref_value, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("vpc-123".into()))
        );
    }

    #[test]
    fn test_resolve_nested_refs_in_list() {
        let bindings = bindings_from(vec![(
            "my_sg",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("sg-456".into())),
            )],
        )]);

        let list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("static".into())),
            Value::resource_ref("my_sg".to_string(), "id".to_string(), vec![]),
        ]));

        let resolved = resolve_ref_value(&list, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("static".into())),
                Value::Concrete(ConcreteValue::String("sg-456".into())),
            ]))
        );
    }

    #[test]
    fn test_resolve_nested_refs_in_map() {
        let bindings = bindings_from(vec![(
            "my_subnet",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("subnet-789".into())),
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
                Some(&Value::Concrete(ConcreteValue::String("subnet-789".into())))
            );
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_unresolved_ref_stays_as_is() {
        let bindings: ResolvedBindings = ResolvedBindings::default();

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
                Value::Concrete(ConcreteValue::String("vpc-123".into())),
            )],
        )]);

        let interp = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("subnet-".into()),
            InterpolationPart::Expr(Value::resource_ref(
                "my_vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        ]));

        let resolved = resolve_ref_value(&interp, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("subnet-vpc-123".into()))
        );
    }

    #[test]
    fn test_resolve_interpolation_partially_unresolved() {
        let bindings: ResolvedBindings = ResolvedBindings::default();

        let interp = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("subnet-".into()),
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
        let bindings: ResolvedBindings = ResolvedBindings::default();

        let interp = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("port-".into()),
            InterpolationPart::Expr(Value::Concrete(ConcreteValue::Int(8080))),
            InterpolationPart::Literal("-enabled-".into()),
            InterpolationPart::Expr(Value::Concrete(ConcreteValue::Bool(true))),
        ]));

        let resolved = resolve_ref_value(&interp, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("port-8080-enabled-true".into()))
        );
    }

    #[test]
    fn test_state_attributes_merged_into_binding_map() {
        let rid = ResourceId::with_identity("test.resource", "my-vpc");
        let mut resources = vec![
            make_resource(
                "my-vpc",
                Some("my_vpc"),
                vec![(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
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
                    Value::Concrete(ConcreteValue::String("vpc-abc".into())),
                )]
                .into_iter()
                .collect(),
                dependency_bindings: std::collections::BTreeSet::new(),

                partial_read: None,
            },
        );

        resolve_refs_with_state(
            &mut resources,
            &crate::resource::into_plan_input_current_map_from_states(
                current_states.clone(),
                &crate::schema::SchemaRegistry::new(),
                &[],
            ),
        )
        .unwrap();

        // The subnet's vpc_id should be resolved from state
        assert_eq!(
            resources[1].get_attr("vpc_id"),
            Some(&Value::Concrete(ConcreteValue::String("vpc-abc".into())))
        );
    }

    #[test]
    fn test_resolve_function_call_join() {
        let bindings: ResolvedBindings = ResolvedBindings::default();

        let func = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Concrete(ConcreteValue::String("-".into())).into(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("a".into())),
                    Value::Concrete(ConcreteValue::String("b".into())),
                    Value::Concrete(ConcreteValue::String("c".into())),
                ]))
                .into(),
            ],
            source_line: (0).into(),
        });

        let resolved = resolve_ref_value(&func, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("a-b-c".into()))
        );
    }

    #[test]
    fn test_resolve_function_call_with_resource_ref() {
        let bindings = bindings_from(vec![(
            "vpc",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("vpc-123".into())),
            )],
        )]);

        // join("-", ["prefix", vpc.id]) should resolve vpc.id first, then evaluate
        let func = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Concrete(ConcreteValue::String("-".into())).into(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("prefix".into())),
                    Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
                ]))
                .into(),
            ],
            source_line: (0).into(),
        });

        let resolved = resolve_ref_value(&func, &bindings).unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("prefix-vpc-123".into()))
        );
    }

    #[test]
    fn access_over_deferred_function_base_reduces_after_reference_resolution() {
        use crate::resource::{PathSegment, Subscript};

        let bindings = bindings_from(vec![(
            "queue",
            vec![(
                "arn",
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:sqs:region:account:name".into(),
                )),
            )],
        )]);
        let value = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String(":".into())).into(),
                    Value::resource_ref("queue", "arn", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 5 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        assert_eq!(
            resolve_ref_value(&value, &bindings).unwrap(),
            Value::Concrete(ConcreteValue::String("name".into()))
        );
    }

    #[test]
    fn access_over_deferred_function_base_reports_out_of_range_after_resolution() {
        use crate::binding_index::BindingValueSource;
        use crate::resource::{PathSegment, Subscript};

        let mut attrs = HashMap::new();
        attrs.insert(
            "arn".to_string(),
            Value::Concrete(ConcreteValue::String("a:b:c".into())),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("queue", attrs, BindingValueSource::Upstream);
        let value = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String(":".into())).into(),
                    Value::resource_ref("queue", "arn", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 5 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        let error = resolve_ref_value(&value, &bindings)
            .expect_err("apply-time out-of-range access must be an error");
        assert!(
            error.contains("index 5 out of range for list of length 3"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn computed_access_over_local_reference_reports_out_of_range_immediately() {
        use crate::binding_index::BindingValueSource;
        use crate::resource::{PathSegment, Subscript};

        let mut attrs = HashMap::new();
        attrs.insert(
            "identifier".to_string(),
            Value::Concrete(ConcreteValue::String("a-b-c".into())),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("q", attrs, BindingValueSource::Local);
        let value = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String("-".into())).into(),
                    Value::resource_ref("q", "identifier", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 5 },
            },
            7,
            AccessMissPolicy::Error,
        )
        .unwrap();

        let error = resolve_ref_value(&value, &bindings)
            .expect_err("a computed list is final even when an input ref is local");
        assert!(
            error.contains("index 5 out of range for list of length 3"),
            "unexpected error: {error}"
        );
        assert!(
            !error.contains("wait"),
            "range errors must not suggest wait: {error}"
        );
    }

    #[test]
    fn local_resource_path_and_postfix_access_both_defer_missing_map_key() {
        use crate::resource::{AccessPath, PathSegment, Subscript};

        let mut tags = IndexMap::new();
        tags.insert(
            "present".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        );
        let tags_value = Value::Concrete(ConcreteValue::Map(tags));
        let bindings = bindings_from(vec![("res", vec![("tags", tags_value.clone())])]);
        let step = PathSegment::Subscript {
            index: Subscript::Str {
                key: "missing".into(),
            },
        };
        let resource_path = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::with_segments("res", "tags", vec![step.clone()]),
        });
        let postfix_access = crate::resource::apply_postfix_access(
            Value::resource_ref("res".to_string(), "tags".to_string(), vec![]),
            step,
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        assert_eq!(postfix_access, resource_path);

        assert_eq!(
            resolve_ref_value(&resource_path, &bindings),
            Ok(resource_path.clone())
        );
        assert_eq!(
            resolve_ref_value(&postfix_access, &bindings),
            Ok(resource_path.clone())
        );
    }

    #[test]
    fn upstream_resource_path_and_postfix_access_both_error_on_missing_map_key() {
        use crate::binding_index::BindingValueSource;
        use crate::resource::{AccessPath, PathSegment, Subscript};

        let mut tags = IndexMap::new();
        tags.insert(
            "present".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        );
        let mut attrs = HashMap::new();
        attrs.insert(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(tags)),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("res", attrs, BindingValueSource::Upstream);
        let step = PathSegment::Subscript {
            index: Subscript::Str {
                key: "missing".into(),
            },
        };
        let resource_path = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::with_segments("res", "tags", vec![step.clone()]),
        });
        let postfix_access = crate::resource::apply_postfix_access(
            Value::resource_ref("res".to_string(), "tags".to_string(), vec![]),
            step,
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        assert_eq!(postfix_access, resource_path);

        let expected =
            Err("res.tags['missing']: key not found; available keys: 'present'".to_string());
        assert_eq!(resolve_ref_value(&resource_path, &bindings), expected);
        assert_eq!(resolve_ref_value(&postfix_access, &bindings), expected);
    }

    #[test]
    fn resource_path_and_postfix_access_both_index_string_lists() {
        use crate::resource::{AccessPath, PathSegment, Subscript};

        let bindings = bindings_from(vec![(
            "res",
            vec![(
                "names",
                Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()])),
            )],
        )]);
        let step = PathSegment::Subscript {
            index: Subscript::Int { index: 1 },
        };
        let resource_path = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::with_segments("res", "names", vec![step.clone()]),
        });
        let postfix_access = crate::resource::apply_postfix_access(
            Value::resource_ref("res".to_string(), "names".to_string(), vec![]),
            step,
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        assert_eq!(postfix_access, resource_path);

        let expected = Ok(Value::Concrete(ConcreteValue::String("b".into())));
        assert_eq!(resolve_ref_value(&resource_path, &bindings), expected);
        assert_eq!(resolve_ref_value(&postfix_access, &bindings), expected);
    }

    #[test]
    fn local_reference_wrong_shape_is_never_deferred_as_a_missing_value() {
        use crate::resource::{AccessPath, PathSegment, Subscript};

        let bindings = bindings_from(vec![(
            "q",
            vec![(
                "identifier",
                Value::Concrete(ConcreteValue::String("q-123".into())),
            )],
        )]);
        let value = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::with_segments(
                "q",
                "identifier",
                vec![PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                }],
            ),
        });

        let error = resolve_ref_value(&value, &bindings)
            .expect_err("a String cannot become indexable after apply");
        assert!(
            error.contains("cannot apply access [0] to string"),
            "unexpected apply-time error: {error}",
        );
        assert!(
            !error.contains("wait"),
            "wrong-shape access must never be reported as a deferred miss: {error}",
        );
    }

    #[test]
    fn test_resolve_function_call_unresolved_ref_kept() {
        let bindings: ResolvedBindings = ResolvedBindings::default();

        // If a ResourceRef in the args can't be resolved, the FunctionCall is kept
        let func = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Concrete(ConcreteValue::String("-".into())).into(),
                Value::Concrete(ConcreteValue::List(vec![Value::resource_ref(
                    "unknown".to_string(),
                    "id".to_string(),
                    vec![],
                )]))
                .into(),
            ],
            source_line: (0).into(),
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
            Value::Concrete(ConcreteValue::String("vpc-123".into())),
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
            Value::Concrete(ConcreteValue::String("vpc-123".into()))
        );
    }

    #[test]
    fn test_resolve_deeply_chained_field_access() {
        // web.output.network.vpc_id
        let mut inner_map = IndexMap::new();
        inner_map.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-456".into())),
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
            Value::Concrete(ConcreteValue::String("vpc-456".into()))
        );
    }

    #[test]
    fn test_resolve_chained_field_missing_key_keeps_ref() {
        let mut network_map = IndexMap::new();
        network_map.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-123".into())),
        );
        let bindings = bindings_from(vec![(
            "web",
            vec![("network", Value::Concrete(ConcreteValue::Map(network_map)))],
        )]);

        // A local desired-side view may not contain provider-populated fields yet.
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
        let bindings: ResolvedBindings = ResolvedBindings::default();
        let value = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![
                Value::Concrete(ConcreteValue::String(
                    "CARINA_RESOLVER_TEST_NONEXISTENT_VAR_12345"
                        .to_string()
                        .into(),
                ))
                .into(),
            ],
            source_line: (0).into(),
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
        let bindings: ResolvedBindings = ResolvedBindings::default();
        let value = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Concrete(ConcreteValue::String("-".into())).into(),
                Value::resource_ref("vpc".to_string(), "tags".to_string(), vec![]).into(),
            ],
            source_line: (0).into(),
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
                    name: crate::builtins::BuiltinFunction::env,
                    args: vec![
                        Value::Concrete(ConcreteValue::String(
                            "CARINA_RESOLVER_STATE_TEST_NONEXISTENT_VAR_12345"
                                .to_string()
                                .into(),
                        ))
                        .into(),
                    ],
                    source_line: (0).into(),
                }),
            )],
        )];

        let current_states: HashMap<ResourceId, State> = HashMap::new();
        let result = resolve_refs_with_state(
            &mut resources,
            &crate::resource::into_plan_input_current_map_from_states(
                current_states.clone(),
                &crate::schema::SchemaRegistry::new(),
                &[],
            ),
        );
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
            Value::Concrete(ConcreteValue::String("vpc-123".into())),
        );
        vpc_attrs.insert(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
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
            Some(&Value::Concrete(ConcreteValue::String("vpc-123".into())))
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
                            key: "registry_dev".into(),
                        }],
                    ),
                }),
            )],
        )];

        let mut accounts_map: IndexMap<String, Value> = IndexMap::new();
        accounts_map.insert(
            "registry_prod".to_string(),
            Value::Concrete(ConcreteValue::String("111111111111".into())),
        );
        accounts_map.insert(
            "registry_dev".to_string(),
            Value::Concrete(ConcreteValue::String("222222222222".into())),
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
                "222222222222".into()
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
                            key: "registry_qa".into(),
                        }],
                    ),
                }),
            )],
        )];

        let mut accounts_map: IndexMap<String, Value> = IndexMap::new();
        accounts_map.insert(
            "registry_prod".to_string(),
            Value::Concrete(ConcreteValue::String("111111111111".into())),
        );
        accounts_map.insert(
            "registry_dev".to_string(),
            Value::Concrete(ConcreteValue::String("222222222222".into())),
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
            err.contains("registry_qa"),
            "error must name the missing key, got: {err}"
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
                            key: "registry_dev".into(),
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
        // `Unknown(UpstreamExport)`. Guards against a future refactor that
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
                            key: "anything".into(),
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
            Value::Concrete(ConcreteValue::String("aza".into())),
            Value::Concrete(ConcreteValue::String("azb".into())),
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
                            crate::resource::Subscript::Str { key: "us".into() },
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
            Some(&Value::Concrete(ConcreteValue::String("aza".into())))
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
                ConcreteValue::String("aza".into()),
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
                            crate::resource::Subscript::Str { key: "eu".into() },
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
    /// `Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport { path }))`.
    #[test]
    fn test_resolve_refs_for_plan_stamps_top_level_unresolved_upstream() {
        use crate::resource::ApplyTimeUnknown;
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
            Some(Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
                path,
            }))) => {
                assert_eq!(path.to_dot_string(), "network.vpc.vpc_id");
            }
            other => panic!("expected Value::Unknown(UpstreamExport), got {:?}", other),
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
    fn test_resolve_refs_for_plan_stamps_upstream_all_exports_ref() {
        use crate::resource::ApplyTimeUnknown;
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
            Some(Value::Deferred(DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamAllExports { binding },
            ))) => {
                assert_eq!(binding, "bootstrap");
            }
            other => panic!(
                "expected Value::Unknown(UpstreamAllExports), got {:?}",
                other
            ),
        }
    }

    /// `UpstreamAllExports` unknowns nested inside a `List` must reach the
    /// stamping arm via the existing recursion. Locks in the recursive
    /// contract symmetrically with the `UpstreamExport`-inside-list test
    /// above. #2876.
    #[test]
    fn test_resolve_refs_for_plan_stamps_upstream_all_exports_inside_list() {
        use crate::resource::ApplyTimeUnknown;
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
                            ApplyTimeUnknown::UpstreamAllExports { binding },
                        )) => binding.as_str(),
                        other => panic!("expected UpstreamAllExports, got {:?}", other),
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
                    "arn:aws:acm:us-east-1:1:certificate/abc".into(),
                )),
            )],
        );
        let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: &[cert],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
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
                "arn:aws:acm:us-east-1:1:certificate/abc".into()
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
        let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
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
