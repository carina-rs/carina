//! Forward-reference resolution and identifier-scope checks.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::ast::{AttributeParameter, ExportParameter, ModuleCall, ParsedFile};
use super::entry::BindingSeed;
use super::error::{ParseError, undefined_identifier_error};
use super::static_eval::is_static_value;
use crate::eval_value::EvalValue;
use crate::resource::{ConcreteValue, DataSource, DeferredValue, Resource, Value};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Mutable pre-apply traversal — the resolver mutates `attributes` and
/// `dependency_bindings` of every resource whose references are resolved
/// **before** apply: managed [`Resource`]s and [`DataSource`]s.
///
/// `Composition` is excluded on purpose — a composition resource's
/// attributes may carry refs whose resolution is deferred to the
/// post-apply path, so the pre-apply resolver must never rewrite them
/// (carina#3181; see the `composition.rs` module doc).
/// Pre-apply mutable accessors. Self-contained since #3308 retired
/// the `ResourceLike` trait — this trait keeps the read+write surface
/// the pre-apply resolver needs while staying scoped to the two
/// types that actually go through it ([`Resource`] and [`DataSource`]).
trait PreApplyResourceMut {
    fn attributes(&self) -> &IndexMap<String, Value>;
    fn dependency_bindings(&self) -> &BTreeSet<String>;
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value>;
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<String>;
}

impl PreApplyResourceMut for Resource {
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependency_bindings
    }
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value> {
        &mut self.attributes
    }
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<String> {
        &mut self.dependency_bindings
    }
}

impl PreApplyResourceMut for DataSource {
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependency_bindings
    }
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value> {
        &mut self.attributes
    }
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<String> {
        &mut self.dependency_bindings
    }
}

/// Mutable iterator over the pre-apply resolvable resources — managed
/// resources then data sources (carina#3181). Used by the resolver to
/// rewrite refs in place across both typed slices in one walk.
fn iter_pre_apply_resources_mut(
    parsed: &mut ParsedFile,
) -> impl Iterator<Item = &mut dyn PreApplyResourceMut> + '_ {
    parsed
        .resources
        .iter_mut()
        .map(|r| r as &mut dyn PreApplyResourceMut)
        .chain(
            parsed
                .data_sources
                .iter_mut()
                .map(|d| d as &mut dyn PreApplyResourceMut),
        )
}

/// Resolve forward references after the full binding set is known.
///
/// During single-pass parsing, `identifier.member` forms where `identifier` is
/// not yet a known binding are stored as `String("identifier.member")`.
/// This function walks all resource attributes, module call arguments, and attribute
/// parameter values, converting matching strings to `ResourceRef`.
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_forward_references(
    resource_bindings: &HashSet<String>,
    resources: &mut [Resource],
    data_sources: &mut [DataSource],
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
            let placeholder = Value::Concrete(ConcreteValue::Bool(false));
            let value = std::mem::replace(attr, placeholder);
            *attr = resolve_forward_ref_in_value(value, resource_bindings);
        }
    }
    for data_source in data_sources.iter_mut() {
        for (_, attr) in data_source.attributes.iter_mut() {
            let placeholder = Value::Concrete(ConcreteValue::Bool(false));
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
fn resolve_forward_ref_in_value(value: Value, resource_bindings: &HashSet<String>) -> Value {
    match value {
        Value::Concrete(ConcreteValue::String(ref s)) => {
            // A dotted string like "vpc.vpc_id" or "vpc.attr.nested" may be a
            // forward reference that was stored as a string during single-pass
            // parsing. Resolve it to ResourceRef if the first segment is a known
            // resource binding. Parts after the second become field_path.
            let parts: Vec<&str> = s.splitn(3, '.').collect();
            if parts.len() >= 2 && resource_bindings.contains(parts[0]) {
                let field_path = parts
                    .get(2)
                    .map(|rest| rest.split('.').map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                return Value::resource_ref(parts[0].to_string(), parts[1].to_string(), field_path);
            }
            value
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .into_iter()
                .map(|v| resolve_forward_ref_in_value(v, resource_bindings))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.into_iter()
                .map(|(k, v)| (k, resolve_forward_ref_in_value(v, resource_bindings)))
                .collect(),
        )),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            Value::Deferred(DeferredValue::Interpolation(
                parts
                    .into_iter()
                    .map(|p| match p {
                        InterpolationPart::Expr(v) => InterpolationPart::Expr(
                            resolve_forward_ref_in_value(v, resource_bindings),
                        ),
                        other => other,
                    })
                    .collect(),
            ))
            .canonicalize()
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            Value::Deferred(DeferredValue::FunctionCall {
                name,
                args: args
                    .into_iter()
                    .map(|v| resolve_forward_ref_in_value(v, resource_bindings))
                    .collect(),
            })
        }
        other => other,
    }
}

/// Resolve resource references in a ParsedFile
/// This replaces ResourceRef values with the actual attribute values from referenced resources
pub fn resolve_resource_refs(parsed: &mut ParsedFile) -> Result<(), ParseError> {
    resolve_resource_refs_with_config(parsed, &ProviderContext::default())
}

/// Resolve resource references with the given parser configuration.
///
/// `parsed.user_functions` is consulted when resolving `Value::Deferred(DeferredValue::FunctionCall)`
/// values — a name that isn't a builtin but is a user-defined function in
/// the merged directory parse triggers user-fn evaluation here, even when
/// the per-file parse couldn't evaluate it because the `fn` declaration
/// lived in a sibling `.crn` (#2444).
pub fn resolve_resource_refs_with_config(
    parsed: &mut ParsedFile,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    // Save dependency bindings before resolution may change ResourceRef binding names.
    // This preserves direct dependencies that would be lost by recursive resolution
    // (e.g., tgw_attach.transit_gateway_id resolves to tgw.id, losing the tgw_attach dep).
    //
    // carina#3181: walk managed resources *and* data sources — a data
    // source's attributes can carry refs too, and `parsed.resources` is
    // now managed-only.
    for resource in iter_pre_apply_resources_mut(parsed) {
        // #3308: compute the value-ref dependency snapshot inline from
        // the trait's read accessors. The previous shared helper took
        // `&dyn ResourceLike`; with that trait gone, the equivalent is
        // a one-line attribute walk + dependency_bindings copy.
        let mut deps: std::collections::HashSet<String> = std::collections::HashSet::new();
        for value in resource.attributes().values() {
            crate::deps::collect_dependencies(value, &mut deps);
        }
        for name in resource.dependency_bindings() {
            deps.insert(name.clone());
        }
        if !deps.is_empty() {
            *resource.dependency_bindings_mut() = deps.into_iter().collect();
        }
    }

    // Build a map of binding_name -> attributes for quick lookup. Walk
    // every top-level resource (managed, data source, composition) so a
    // `ResourceRef` to any binding resolves. `binding_map` only needs
    // key-based lookup, not source order, so the inner map stays a plain
    // `HashMap`.
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for rref in parsed.iter_top_level_resources() {
        if let Some(binding_name) = rref.binding() {
            binding_map.insert(binding_name.to_string(), rref.resolved_attributes());
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

    // Register `wait` bindings the same way: downstream `<wait>.<attr>`
    // refs are passthrough of the target's snapshot, resolved at apply time.
    for wb in &parsed.wait_bindings {
        binding_map
            .entry(wb.binding.as_str().to_string())
            .or_default();
    }

    // Build a `ParseContext` once, populated with the merged
    // directory's user functions so a `Value::Deferred(DeferredValue::FunctionCall)` whose name
    // is a sibling-defined user-fn (visible only in the merged parse —
    // #2444) is evaluated here rather than rejected as "Unknown
    // built-in function". The HashMap is small (typically a handful of
    // user-defined functions). Restructuring the surrounding
    // `&mut parsed` borrow to avoid the clone is hairy on the error
    // path (would need to restore on every `?` exit), so we accept a
    // single map clone per call.
    let mut fn_ctx = super::ParseContext::new(config);
    fn_ctx.user_functions = parsed.user_functions.clone();

    // Resolve references in each resource. Keep `IndexMap` to preserve
    // the user's source order through resolution (#2222).
    //
    // carina#3181: managed resources and data sources both go through
    // pre-apply ref resolution; `Composition` is excluded — its refs
    // are resolved on the post-apply path.
    for resource in iter_pre_apply_resources_mut(parsed) {
        let mut resolved_attrs: IndexMap<String, Value> = IndexMap::new();

        for (key, expr) in resource.attributes().iter() {
            let resolved = resolve_value_with_config(expr, &binding_map, &fn_ctx)?;
            resolved_attrs.insert(key.clone(), resolved);
        }

        *resource.attributes_mut() = resolved_attrs;
    }

    // Resolve top-level `let v = ...` bindings that contain
    // `Value::Deferred(DeferredValue::FunctionCall)` placeholders deferred by the per-file
    // parse — typically `fn X(...)` lives in a sibling `.crn` (#2444).
    // The variant below only mutates `FunctionCall` arms, leaving
    // other shapes (`Value::Concrete(ConcreteValue::String("${vpc}"))` placeholder,
    // `Value::Deferred(DeferredValue::ResourceRef)`, etc.) untouched so earlier passes
    // (`upstream_exports`, `BindingNameSet`) keep seeing the raw
    // unresolved forms they expect.
    let var_keys: Vec<String> = parsed.variables.keys().cloned().collect();
    for key in var_keys {
        let expr = parsed.variables[&key].clone();
        let resolved = resolve_function_calls_only(&expr, &binding_map, &fn_ctx)?;
        parsed.variables[&key] = resolved;
    }

    // Resolve cross-file forward references in export_params.
    // During per-file parsing, "binding.attribute" strings from sibling files
    // remain as Value::Concrete(ConcreteValue::String). Convert them to ResourceRef now that the full
    // binding map is available.
    // carina#3181: include data sources and compositions — a sibling-file
    // export can forward-reference any top-level binding. Only the
    // binding-name set is needed (forward-ref resolution is keyed on
    // the name).
    let resource_bindings: HashSet<String> = parsed
        .iter_top_level_resources()
        .filter_map(|rref| rref.binding().map(|b| b.to_string()))
        .collect();
    for export_param in &mut parsed.export_params {
        if let Some(value) = export_param.value.take() {
            export_param.value = Some(resolve_forward_ref_in_value(value, &resource_bindings));
        }
    }

    // Provider attribute resolution lives in
    // `resolve_provider_unresolved_attributes` (generic over export-
    // parameter shape so it works on both `ParsedFile` and `InferredFile`),
    // and is called by consumers after `module_resolver::resolve_modules_with_config`
    // produces composition resources. See #2717.

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
    let mut known = parsed.structural_binding_names();
    known.extend(parsed.variables.keys().map(String::as_str));
    known
}

/// Directory-wide parse seeds for Pass 2.
///
/// Plain value-shaped `let` bindings carry their pass-1 value so bare
/// sibling identifiers parse to that value instead of a deferred
/// placeholder. Structural bindings may also have parser placeholder
/// entries in `parsed.variables` (`"${binding}"`); those must not become
/// value seeds, so structural names continue to use the placeholder
/// installed by `seed_bindings`.
pub(crate) fn collect_seed_bindings_merged(parsed: &ParsedFile) -> Vec<BindingSeed<'_>> {
    let structural = parsed.structural_binding_names();

    collect_known_bindings_merged(parsed)
        .into_iter()
        .map(|name| {
            if structural.contains(name) {
                BindingSeed::structural(name)
            } else if let Some(value) = parsed.variables.get(name) {
                BindingSeed::value(name, value)
            } else {
                unreachable!(
                    "collect_known_bindings_merged names are either structural or in variables"
                )
            }
        })
        .collect()
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

/// Validate every `directives { provider = <ident> }` reference and
/// every implicit default-instance lookup against `parsed.providers`.
///
/// Emits an error for each:
///
/// - `provider = <binding>` whose binding is not a declared
///   `ProviderInstance` (typo or undeclared name).
/// - `provider = <binding>` whose binding's kind does not match the
///   resource's kind (e.g. `aws.s3.Bucket { directives { provider =
///   <awscc-binding> } }`).
/// - Resource that omits `directives { provider = ... }` but whose
///   kind has no default instance (the kind was registered via a
///   top-level `provider <kind> { source = ..., version = ... }`
///   block with no instance attributes, and every instance of that
///   kind is named).
///
/// The check is a sibling of `check_identifier_scope` rather than
/// folded into it: the errors are provider-specific, the inputs
/// (`parsed.providers`) and outputs (kind-aware diagnostics) differ
/// from the generic "is this identifier in scope" walk, and folding
/// them together would force callers that only want one kind of
/// validation to pay for both.
pub fn check_provider_instance_routing(parsed: &ParsedFile) -> Vec<ParseError> {
    let mut errors = Vec::new();
    // carina#3181: walk the typed top-level slices. `directives()` is
    // `None` for the composition arm (no `directives` on a synthetic node),
    // which routes to the `None` match arm — and a composition's `id` has an
    // empty provider, so it is skipped there, matching prior behaviour.
    for rref in parsed.iter_top_level_resources() {
        let id = rref.id();
        let provider_instance = rref.directives().and_then(|d| d.provider_instance.as_ref());
        match provider_instance {
            Some(binding) => {
                let Some(instance) = parsed
                    .providers
                    .iter()
                    .find(|p| p.binding.as_deref() == Some(binding.as_str()))
                else {
                    errors.push(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "directives.provider: `{binding}` is not a known \
                             provider instance. Declare it with `let {binding} \
                             = provider <kind> {{ ... }}`."
                        ),
                    });
                    continue;
                };
                if instance.name != id.provider {
                    errors.push(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "directives.provider: instance `{binding}` has kind \
                             `{}`, but resource `{}.{}` requires kind `{}`",
                            instance.name, id.provider, id.resource_type, id.provider,
                        ),
                    });
                }
            }
            None => {
                // Mock / synthetic resources (no provider prefix) have no
                // kind to route by; skip them.
                if id.provider.is_empty() {
                    continue;
                }
                let (kind_has_any, kind_has_default) =
                    parsed
                        .providers
                        .iter()
                        .fold((false, false), |(any, def), p| {
                            let matches = p.name == id.provider;
                            (any || matches, def || (matches && p.is_default))
                        });
                if !kind_has_any {
                    continue;
                }
                if !kind_has_default {
                    errors.push(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "resource `{}.{}` has no default provider \
                             instance for kind `{}`: either add instance \
                             attributes to the top-level `provider {}` \
                             block, or set `directives {{ provider = \
                             <instance> }}` explicitly.",
                            id.provider, id.resource_type, id.provider, id.provider,
                        ),
                    });
                }
            }
        }
    }
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

    // carina#3181: walk the typed top-level slices (managed, composition,
    // data source) — `parsed.resources` is managed-only now. Deferred
    // for-expression templates are handled by
    // `accumulate_deferred_iterable_errors`, so they are excluded here.
    for rref in parsed.iter_top_level_resources() {
        for value in rref.attributes().values() {
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
    for provider in &parsed.providers {
        for value in provider.attributes.values() {
            check(value);
        }
        for value in provider.unresolved_attributes.values() {
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
    fn_ctx: &super::ParseContext<'_>,
) -> Result<Value, ParseError> {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            match binding_map.get(path.binding()) {
                Some(attributes) => match attributes.get(path.attribute()) {
                    Some(attr_value) => {
                        // Recursively resolve in case the attribute itself is a reference
                        resolve_value_with_config(attr_value, binding_map, fn_ctx)
                    }
                    None => {
                        // Attribute not found, keep as reference (might be resolved at runtime)
                        Ok(value.clone())
                    }
                },
                // Binding not found in *this* file's binding_map. The same
                // resolver runs both per-file (config_loader.rs L93) and on
                // the merged `ParsedFile` (L178); a per-file miss may be a
                // legitimate cross-file ref (`upstream_state` declared in a
                // sibling `.crn`, resource binding from another file).
                // Keep the ref as-is and let the post-merge
                // `check_identifier_scope` walk surface any genuine
                // undefined identifiers — that walk has full directory
                // context plus did-you-mean suggestions.
                None => Ok(value.clone()),
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let resolved: Result<Vec<Value>, ParseError> = items
                .iter()
                .map(|item| resolve_value_with_config(item, binding_map, fn_ctx))
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(resolved?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut resolved: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(
                    k.clone(),
                    resolve_value_with_config(v, binding_map, fn_ctx)?,
                );
            }
            Ok(Value::Concrete(ConcreteValue::Map(resolved)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            let resolved: Result<Vec<InterpolationPart>, ParseError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => Ok(InterpolationPart::Expr(
                        resolve_value_with_config(v, binding_map, fn_ctx)?,
                    )),
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(resolved?)).canonicalize())
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            let resolved_args: Result<Vec<Value>, ParseError> = args
                .iter()
                .map(|a| resolve_value_with_config(a, binding_map, fn_ctx))
                .collect();
            let resolved_args = resolved_args?;

            let all_args_resolved = resolved_args.iter().all(is_static_value);

            let eval_args: Vec<EvalValue> = resolved_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            match crate::builtins::evaluate_builtin_with_config(name, &eval_args, fn_ctx.config) {
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
                    // Builtin lookup failed. The name may be a user-fn
                    // that became visible only after directory merge
                    // (#2444): evaluate against the merged `fn_ctx`
                    // built once at the top of the resolver pass.
                    // Truly-undefined names with all-static args still
                    // error.
                    if let Some(user_fn) = fn_ctx.user_functions.get(name) {
                        return super::functions::evaluate_user_function(
                            user_fn,
                            &resolved_args,
                            fn_ctx,
                        );
                    }
                    if all_args_resolved {
                        // All args are resolved but builtin failed — propagate the error
                        Err(ParseError::InvalidExpression {
                            line: 0,
                            message: format!("{}(): {}", name, e),
                        })
                    } else {
                        // Args contain unresolved refs — keep as FunctionCall for later resolution
                        Ok(Value::Deferred(DeferredValue::FunctionCall {
                            name: name.clone(),
                            args: resolved_args,
                        }))
                    }
                }
            }
        }
        _ => Ok(value.clone()),
    }
}

/// Walk a value tree and finalize any `Value::Deferred(DeferredValue::FunctionCall)` placeholder
/// that the per-file parse deferred (typically because the user-fn
/// lives in a sibling `.crn` — #2444). Recurses through `List` / `Map`
/// / `Interpolation` containers so deferred calls inside them surface
/// too. Non-FunctionCall arms (`String`, `ResourceRef`,
/// `Interpolation` literals, etc.) pass through unchanged so earlier
/// passes that read raw forms keep working.
fn resolve_function_calls_only(
    value: &Value,
    binding_map: &HashMap<String, HashMap<String, Value>>,
    fn_ctx: &super::ParseContext<'_>,
) -> Result<Value, ParseError> {
    use crate::resource::InterpolationPart;
    match value {
        Value::Deferred(DeferredValue::FunctionCall { .. }) => {
            resolve_value_with_config(value, binding_map, fn_ctx)
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let resolved: Result<Vec<Value>, ParseError> = items
                .iter()
                .map(|v| resolve_function_calls_only(v, binding_map, fn_ctx))
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(resolved?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut resolved: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(
                    k.clone(),
                    resolve_function_calls_only(v, binding_map, fn_ctx)?,
                );
            }
            Ok(Value::Concrete(ConcreteValue::Map(resolved)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let resolved: Result<Vec<InterpolationPart>, ParseError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => Ok(InterpolationPart::Expr(
                        resolve_function_calls_only(v, binding_map, fn_ctx)?,
                    )),
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(resolved?)).canonicalize())
        }
        _ => Ok(value.clone()),
    }
}

/// Resolve `ResourceRef` values inside provider blocks'
/// `unresolved_attributes` against the merged binding map produced by
/// `parsed.resources` + `parsed.arguments` + `parsed.module_calls` +
/// `parsed.upstream_states`.
///
/// Call this **after** `module_resolver::resolve_modules_with_config`
/// has produced composition resources for module-call bindings, so that
/// `default_tags = mod.tags` can see the module's exported attribute
/// values. Then call [`finalize_provider_configs`] to drain the
/// (now-literal) values into the typed `default_tags` field.
///
/// Generic over the export-parameter shape so callers can pass either
/// `ParsedFile` or `InferredFile` — we only touch
/// `provider.unresolved_attributes`, never `export_params`.
pub fn resolve_provider_unresolved_attributes<E>(
    parsed: &mut super::File<E>,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    // carina#3181: walk every top-level resource — managed, data source,
    // and composition. This runs after module expansion, so composition resources
    // exist and a provider attr like `default_tags = mod.tags` resolves
    // through the module-call composition.
    for rref in parsed.iter_top_level_resources() {
        if let Some(binding_name) = rref.binding() {
            binding_map.insert(binding_name.to_string(), rref.resolved_attributes());
        }
    }
    for arg in &parsed.arguments {
        binding_map.entry(arg.name.clone()).or_default();
    }
    for call in &parsed.module_calls {
        if let Some(ref name) = call.binding_name {
            binding_map.entry(name.clone()).or_default();
        }
    }
    for us in &parsed.upstream_states {
        binding_map.entry(us.binding.clone()).or_default();
    }
    for wb in &parsed.wait_bindings {
        binding_map
            .entry(wb.binding.as_str().to_string())
            .or_default();
    }
    let mut fn_ctx = super::ParseContext::new(config);
    fn_ctx.user_functions = parsed.user_functions.clone();

    for provider in &mut parsed.providers {
        let mut resolved: IndexMap<String, Value> = IndexMap::new();
        for (key, expr) in &provider.unresolved_attributes {
            let r = resolve_value_with_config(expr, &binding_map, &fn_ctx)?;
            resolved.insert(key.clone(), r);
        }
        provider.unresolved_attributes = resolved;
    }
    Ok(())
}

/// Resolve `ResourceRef` values inside provider blocks' `attributes`
/// against a binding map built from `parsed`'s resources and `remote_bindings`
/// (the values fetched by `load_upstream_states` at plan/apply time).
///
/// This is the plan/apply-time counterpart to
/// [`resolve_provider_unresolved_attributes`], which only touches the
/// parse-time `unresolved_attributes` bucket (default_tags etc.). The
/// regular `provider.attributes` map can contain refs nested inside
/// struct literals — `assume_role = { role_arn = upstream.arn }` is the
/// motivating case (carina#3182) — and those refs need to be substituted
/// before the attributes cross the WASM provider boundary in
/// `create_provider`.
///
/// Refs whose binding is unknown (neither a top-level resource nor an
/// upstream binding) are left in place so the caller can decide whether
/// that is a fatal error or expected (validate path tolerates it; the
/// post-resolution WASM boundary rejects it on plan/apply).
pub fn resolve_provider_attributes_with_remote<E>(
    parsed: &mut super::File<E>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for rref in parsed.iter_top_level_resources() {
        if let Some(binding_name) = rref.binding() {
            binding_map.insert(binding_name.to_string(), rref.resolved_attributes());
        }
    }
    for (binding, attrs) in remote_bindings {
        binding_map.insert(binding.clone(), attrs.clone());
    }

    let mut fn_ctx = super::ParseContext::new(config);
    fn_ctx.user_functions = parsed.user_functions.clone();

    for provider in &mut parsed.providers {
        let mut resolved: IndexMap<String, Value> = IndexMap::new();
        for (key, expr) in &provider.attributes {
            let r = resolve_value_with_config(expr, &binding_map, &fn_ctx)?;
            resolved.insert(key.clone(), r);
        }
        provider.attributes = resolved;
    }
    Ok(())
}

/// Drain `ProviderConfig.unresolved_attributes` and promote resolved
/// well-known attributes into their typed fields. Run **after**
/// [`resolve_resource_refs`] (and, for module-call deferrals,
/// [`resolve_provider_unresolved_attributes`]) so deferred references
/// have a chance to resolve to literals first.
///
/// Today only `default_tags` is handled; `source` / `version` /
/// `revision` peel sites still use the legacy parse-time shape (#2757).
///
/// Errors when a resolved value has the wrong shape (e.g.
/// `default_tags = "string"`).
pub fn finalize_provider_configs<E>(parsed: &mut super::File<E>) -> Result<(), ParseError> {
    for provider in parsed.providers.iter_mut() {
        if let Some(value) = provider.unresolved_attributes.shift_remove("default_tags") {
            match value {
                Value::Concrete(ConcreteValue::Map(tags)) => {
                    provider.default_tags = tags;
                }
                other => {
                    return Err(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "Provider '{}': default_tags must resolve to a map, got {other:?}",
                            provider.name
                        ),
                    });
                }
            }
        }
        debug_assert!(
            provider.unresolved_attributes.is_empty(),
            "unresolved_attributes must be drained by finalize",
        );
    }
    Ok(())
}

#[cfg(test)]
mod seed_precedence_tests {
    use super::*;
    use crate::parser::entry::SeedKind;

    #[test]
    fn structural_let_with_placeholder_in_variables_seeds_as_structural() {
        // A `let cluster = mock.compute.Instance {}` shape: `cluster` appears
        // in `parsed.resources` and in `parsed.variables`, where let_binding.rs
        // stores the parser placeholder string "${cluster}".
        let mut parsed = ParsedFile::default();

        let mut resource = Resource::new("mock.compute.Instance", "instance");
        resource.binding = Some("cluster".to_string());
        parsed.resources.push(resource); // allow: direct — fixture test inspection

        parsed.variables.insert(
            "cluster".to_string(),
            Value::Concrete(ConcreteValue::String("${cluster}".to_string())),
        );

        let seeds = collect_seed_bindings_merged(&parsed);
        let cluster_seed = seeds
            .iter()
            .find(|seed| seed.name() == "cluster")
            .expect("cluster seed missing");

        match cluster_seed.kind() {
            SeedKind::Structural => {}
            SeedKind::Value(value) => panic!(
                "structural binding `cluster` with placeholder in variables \
                 must seed as Structural, not Value({value:?}). The structural-first \
                 precedence in collect_seed_bindings_merged is load-bearing for \
                 carina#3391's fix: if Value wins, the parser placeholder \
                 \"${{cluster}}\" leaks as the seed value and sibling-file `cluster.attr` \
                 references can resolve to the placeholder string instead of \
                 going through the resource-binding path."
            ),
        }
    }

    #[test]
    fn plain_let_literal_seeds_as_value() {
        // A `let bad_name = 'literal'` shape: `bad_name` appears in
        // `parsed.variables` only, not in any structural source.
        let mut parsed = ParsedFile::default();
        parsed.variables.insert(
            "bad_name".to_string(),
            Value::Concrete(ConcreteValue::String("literal".to_string())),
        );

        let seeds = collect_seed_bindings_merged(&parsed);
        let bad_seed = seeds
            .iter()
            .find(|seed| seed.name() == "bad_name")
            .expect("bad_name seed missing");

        match bad_seed.kind() {
            SeedKind::Value(value) => {
                assert_eq!(
                    value,
                    &Value::Concrete(ConcreteValue::String("literal".to_string())),
                    "Value seed must carry the actual literal"
                );
            }
            SeedKind::Structural => panic!("plain value let must seed as Value, not Structural"),
        }
    }
}
