//! Forward-reference resolution and identifier-scope checks.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::ast::{AttributeParameter, ExportParameter, ModuleCall, ParsedFile};
use super::error::{ParseError, undefined_identifier_error};
use super::static_eval::is_static_value;
use crate::eval_value::EvalValue;
use crate::resource::{Resource, Value};
use indexmap::IndexMap;
use std::collections::HashMap;

/// Resolve forward references after the full binding set is known.
///
/// During single-pass parsing, `identifier.member` forms where `identifier` is
/// not yet a known binding are stored as `String("identifier.member")`.
/// This function walks all resource attributes, module call arguments, and attribute
/// parameter values, converting matching strings to `ResourceRef`.
pub(super) fn resolve_forward_references(
    resource_bindings: &HashMap<String, Resource>,
    resources: &mut [Resource],
    attribute_params: &mut [AttributeParameter],
    module_calls: &mut [ModuleCall],
    export_params: &mut [ExportParameter],
) {
    for resource in resources.iter_mut() {
        // In-place replace via `iter_mut`: avoids the O(n²) cost of
        // `shift_remove` + re-insert per key, and naturally preserves
        // the user-authored attribute order without a key-collection
        // round-trip. The placeholder is overwritten on the next line,
        // so its identity doesn't matter.
        for (_, attr) in resource.attributes.iter_mut() {
            let placeholder = Value::Bool(false);
            let value = std::mem::replace(attr, placeholder);
            *attr = resolve_forward_ref_in_value(value, resource_bindings);
        }
    }
    for attr_param in attribute_params.iter_mut() {
        if let Some(value) = attr_param.value.take() {
            attr_param.value = Some(resolve_forward_ref_in_value(value, resource_bindings));
        }
    }
    for call in module_calls.iter_mut() {
        let keys: Vec<String> = call.arguments.keys().cloned().collect();
        for key in keys {
            if let Some(value) = call.arguments.remove(&key) {
                let resolved = resolve_forward_ref_in_value(value, resource_bindings);
                call.arguments.insert(key, resolved);
            }
        }
    }
    for export_param in export_params.iter_mut() {
        if let Some(value) = export_param.value.take() {
            export_param.value = Some(resolve_forward_ref_in_value(value, resource_bindings));
        }
    }
}

/// Recursively resolve forward references in a single Value.
///
/// Strings in `"name.member"` format where `name` is a known resource binding
/// are resolved to `ResourceRef`. This handles forward references that were
/// stored as strings during single-pass parsing.
fn resolve_forward_ref_in_value(
    value: Value,
    resource_bindings: &HashMap<String, Resource>,
) -> Value {
    match value {
        Value::String(ref s) => {
            // A dotted string like "vpc.vpc_id" or "vpc.attr.nested" may be a
            // forward reference that was stored as a string during single-pass
            // parsing. Resolve it to ResourceRef if the first segment is a known
            // resource binding. Parts after the second become field_path.
            let parts: Vec<&str> = s.splitn(3, '.').collect();
            if parts.len() >= 2 && resource_bindings.contains_key(parts[0]) {
                let field_path = parts
                    .get(2)
                    .map(|rest| rest.split('.').map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                return Value::resource_ref(parts[0].to_string(), parts[1].to_string(), field_path);
            }
            value
        }
        Value::List(items) => Value::List(
            items
                .into_iter()
                .map(|v| resolve_forward_ref_in_value(v, resource_bindings))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| (k, resolve_forward_ref_in_value(v, resource_bindings)))
                .collect(),
        ),
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            Value::Interpolation(
                parts
                    .into_iter()
                    .map(|p| match p {
                        InterpolationPart::Expr(v) => InterpolationPart::Expr(
                            resolve_forward_ref_in_value(v, resource_bindings),
                        ),
                        other => other,
                    })
                    .collect(),
            )
            .canonicalize()
        }
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name,
            args: args
                .into_iter()
                .map(|v| resolve_forward_ref_in_value(v, resource_bindings))
                .collect(),
        },
        other => other,
    }
}

/// Resolve resource references in a ParsedFile
/// This replaces ResourceRef values with the actual attribute values from referenced resources
pub fn resolve_resource_refs(parsed: &mut ParsedFile) -> Result<(), ParseError> {
    resolve_resource_refs_with_config(parsed, &ProviderContext::default())
}

/// Resolve resource references with the given parser configuration.
pub fn resolve_resource_refs_with_config(
    parsed: &mut ParsedFile,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    // Save dependency bindings before resolution may change ResourceRef binding names.
    // This preserves direct dependencies that would be lost by recursive resolution
    // (e.g., tgw_attach.transit_gateway_id resolves to tgw.id, losing the tgw_attach dep).
    for resource in &mut parsed.resources {
        let deps = crate::deps::get_resource_dependencies(resource);
        if !deps.is_empty() {
            resource.dependency_bindings = deps.into_iter().collect();
        }
    }

    // Build a map of binding_name -> attributes for quick lookup
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in &parsed.resources {
        if let Some(ref binding_name) = resource.binding {
            // `binding_map` only needs key-based lookup, not source order
            // (callers consume it via `.get(name)` for ResourceRef
            // resolution), so the inner map stays `HashMap`.
            binding_map.insert(binding_name.clone(), resource.resolved_attributes());
        }
    }

    // Register argument parameters so they're recognized as valid bindings
    for arg in &parsed.arguments {
        binding_map.entry(arg.name.clone()).or_default();
    }

    // Register module call bindings so ResourceRefs to them are not rejected.
    // The actual attribute values will be resolved after module expansion.
    for call in &parsed.module_calls {
        if let Some(ref name) = call.binding_name {
            binding_map.entry(name.clone()).or_default();
        }
    }

    // Register upstream_state bindings so ResourceRefs to them are not rejected.
    // The actual attribute values will be resolved at plan time when the state file is loaded.
    for us in &parsed.upstream_states {
        binding_map.entry(us.binding.clone()).or_default();
    }

    // Resolve references in each resource. Keep `IndexMap` to preserve
    // the user's source order through resolution (#2222).
    for resource in &mut parsed.resources {
        let mut resolved_attrs: IndexMap<String, Value> = IndexMap::new();

        for (key, expr) in &resource.attributes {
            let resolved = resolve_value_with_config(expr, &binding_map, config)?;
            resolved_attrs.insert(key.clone(), resolved);
        }

        resource.attributes = resolved_attrs;
    }

    // Resolve cross-file forward references in export_params.
    // During per-file parsing, "binding.attribute" strings from sibling files
    // remain as Value::String. Convert them to ResourceRef now that the full
    // binding map is available.
    let resource_bindings: HashMap<String, Resource> = parsed
        .resources
        .iter()
        .filter_map(|r| r.binding.as_ref().map(|b| (b.clone(), r.clone())))
        .collect();
    for export_param in &mut parsed.export_params {
        if let Some(value) = export_param.value.take() {
            export_param.value = Some(resolve_forward_ref_in_value(value, &resource_bindings));
        }
    }

    Ok(())
}

/// Every binding name declared in the merged `ParsedFile`: resources,
/// arguments, module calls, upstream states, imports, user functions,
/// variables, and for/if structural bindings.
///
/// This is the canonical answer to "is this identifier in scope?" for
/// directory-wide checks. The same set feeds [`check_identifier_scope`]
/// and the LSP borrows it (via `carina_lsp::diagnostics::checks`) to
/// keep diagnostic suggestions consistent with the CLI.
///
/// Thin wrapper over [`crate::binding_index::BindingNameSet::from_parsed`]
/// (#2301). Prefer the new type at fresh call sites; this helper is kept
/// because `accumulate_*` helpers below still take `&HashSet<&str>` as a
/// borrowed view (changing that signature would force `undefined_identifier_error`
/// to rebuild a borrowed view per call).
pub fn collect_known_bindings_merged(parsed: &ParsedFile) -> std::collections::HashSet<&str> {
    let mut known: std::collections::HashSet<&str> = std::collections::HashSet::new();
    known.extend(parsed.resources.iter().filter_map(|r| r.binding.as_deref())); // allow: direct — parser-internal, pre-expansion
    known.extend(parsed.arguments.iter().map(|a| a.name.as_str()));
    known.extend(
        parsed
            .module_calls
            .iter()
            .filter_map(|c| c.binding_name.as_deref()),
    );
    known.extend(parsed.upstream_states.iter().map(|u| u.binding.as_str()));
    known.extend(parsed.uses.iter().map(|i| i.alias.as_str()));
    known.extend(parsed.user_functions.keys().map(String::as_str));
    known.extend(parsed.variables.keys().map(String::as_str));
    known.extend(parsed.structural_bindings.iter().map(String::as_str));
    known
}

/// Directory-wide identifier-scope validation for a merged [`ParsedFile`].
///
/// Emits one flat list of `UndefinedIdentifier` errors covering:
///
/// - Every `ResourceRef` whose root binding is not in scope (roots in
///   resource attributes, attribute-parameter values, module-call
///   arguments, and export-parameter values).
/// - Every deferred for-expression iterable whose root is not in scope.
///
/// Errors are returned in a deterministic order: ResourceRef findings
/// first (in resource / attribute / module / export order), then
/// deferred-iterable findings. The caller (CLI `load_configuration_with_config`,
/// LSP analysis pipeline) just inspects the returned `Vec` — both
/// checks share the same `collect_known_bindings_merged` pass, so there
/// is no performance reason to split them at the callsite.
///
/// **This is the canonical entry point for "is this identifier in
/// scope?" checks.** Follow the #2104 rule: any new semantic check in
/// that family gets added *here*, not as a new sibling function.
pub fn check_identifier_scope(parsed: &ParsedFile) -> Vec<ParseError> {
    let known = collect_known_bindings_merged(parsed);
    let mut errors = Vec::new();
    accumulate_undefined_reference_errors(parsed, &known, &mut errors);
    accumulate_deferred_iterable_errors(parsed, &known, &mut errors);
    errors
}

fn accumulate_undefined_reference_errors(
    parsed: &ParsedFile,
    known: &std::collections::HashSet<&str>,
    errors: &mut Vec<ParseError>,
) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut check = |value: &Value| {
        value.visit_refs(&mut |path| {
            let root = path.binding();
            let root_ident = root.split(['[', ']']).next().unwrap_or(root);
            if !known.contains(root_ident) && seen.insert(root_ident.to_string()) {
                errors.push(undefined_identifier_error(known, root_ident.to_string(), 0));
            }
        });
    };

    for resource in &parsed.resources {
        for value in resource.attributes.values() {
            check(value);
        }
    }
    for attr in &parsed.attribute_params {
        if let Some(value) = &attr.value {
            check(value);
        }
    }
    for call in &parsed.module_calls {
        for v in call.arguments.values() {
            check(v);
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = &export.value {
            check(value);
        }
    }
}

fn accumulate_deferred_iterable_errors(
    parsed: &ParsedFile,
    known: &std::collections::HashSet<&str>,
    errors: &mut Vec<ParseError>,
) {
    for d in &parsed.deferred_for_expressions {
        if !known.contains(d.iterable_binding.as_str()) {
            errors.push(undefined_identifier_error(
                known,
                d.iterable_binding.clone(),
                d.line,
            ));
        }
    }
}

fn resolve_value_with_config(
    value: &Value,
    binding_map: &HashMap<String, HashMap<String, Value>>,
    config: &ProviderContext,
) -> Result<Value, ParseError> {
    match value {
        Value::ResourceRef { path } => match binding_map.get(path.binding()) {
            Some(attributes) => match attributes.get(path.attribute()) {
                Some(attr_value) => {
                    // Recursively resolve in case the attribute itself is a reference
                    resolve_value_with_config(attr_value, binding_map, config)
                }
                None => {
                    // Attribute not found, keep as reference (might be resolved at runtime)
                    Ok(value.clone())
                }
            },
            None => Err(ParseError::UndefinedVariable(format!(
                "{}.{}",
                path.binding(),
                path.attribute()
            ))),
        },
        Value::List(items) => {
            let resolved: Result<Vec<Value>, ParseError> = items
                .iter()
                .map(|item| resolve_value_with_config(item, binding_map, config))
                .collect();
            Ok(Value::List(resolved?))
        }
        Value::Map(map) => {
            let mut resolved: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(
                    k.clone(),
                    resolve_value_with_config(v, binding_map, config)?,
                );
            }
            Ok(Value::Map(resolved))
        }
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            let resolved: Result<Vec<InterpolationPart>, ParseError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => Ok(InterpolationPart::Expr(
                        resolve_value_with_config(v, binding_map, config)?,
                    )),
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Interpolation(resolved?).canonicalize())
        }
        Value::FunctionCall { name, args } => {
            let resolved_args: Result<Vec<Value>, ParseError> = args
                .iter()
                .map(|a| resolve_value_with_config(a, binding_map, config))
                .collect();
            let resolved_args = resolved_args?;

            let all_args_resolved = resolved_args.iter().all(is_static_value);

            let eval_args: Vec<EvalValue> = resolved_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            match crate::builtins::evaluate_builtin_with_config(name, &eval_args, config) {
                Ok(result) => result
                    .into_value()
                    .map_err(|leak| ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "{}(): produced a closure '{}' (still needs {} arg(s)); \
                         finish the partial application before using the result as data",
                            name, leak.name, leak.remaining_arity
                        ),
                    }),
                Err(e) => {
                    if all_args_resolved {
                        // All args are resolved but builtin failed — propagate the error
                        Err(ParseError::InvalidExpression {
                            line: 0,
                            message: format!("{}(): {}", name, e),
                        })
                    } else {
                        // Args contain unresolved refs — keep as FunctionCall for later resolution
                        Ok(Value::FunctionCall {
                            name: name.clone(),
                            args: resolved_args,
                        })
                    }
                }
            }
        }
        _ => Ok(value.clone()),
    }
}
