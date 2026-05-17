//! Module-call expansion, argument substitution, and intra-module
//! reference rewriting.

use std::collections::{BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;

use crate::parser::{
    ArgumentParameter, BindingName, DeferredForExpression, ModuleCall, ParsedFile, WaitBinding,
};
use crate::resource::{
    ConcreteValue, DeferredValue, Directives, Resource, ResourceId, ResourceKind, ResourceName,
    Value,
};

use super::error::ModuleError;
use super::resolver::ModuleResolver;
use super::typecheck::check_module_arg_type;
use super::validation::{evaluate_require_expr, evaluate_validate_expr, format_value_for_error};

impl ModuleResolver<'_> {
    /// Expand a module call into resources.
    ///
    /// If the module defines `attributes` and the call has a `binding_name`,
    /// a virtual resource is created to expose the module's attribute values.
    /// The virtual resource has `ResourceKind::Virtual` and is skipped by the differ.
    ///
    /// `enclosing_args` is the argument signature of the module the call
    /// lives inside (`None` for a top-level call). When this call is being
    /// expanded from inside another module, the inner type-check needs the
    /// enclosing module's declared types so a pass-through arg ref like
    /// `inner_arg = outer_arg` can be checked structurally before the
    /// parent's argument substitution erases the type tag (#2549).
    /// Returns the contribution as a concrete `ParsedFile` — module
    /// expansion is a parser-phase operation (it reads parser-phase
    /// module data from `imported_modules`), so the honest return type
    /// is `ParsedFile`, not a phantom `File<E>`.
    ///
    /// carina#3126 root fix: the caller folds this in via the *one*
    /// shared [`merge_parsed_file`](crate::config_loader) — there is no
    /// second hand-maintained field list that can silently diverge
    /// from `File<E>` (the carina#3061 / carina#3126 bug class: a
    /// module-internal `wait` / `for` dropped because the expansion
    /// path forgot a field). The contribution is built below with an
    /// **exhaustive struct literal**, so a new `File<E>` field cannot
    /// compile until it is classified as "populated from the module
    /// (instance-prefixed)" or "consumed during expansion, not
    /// propagated". The generic-`File<E>` caller
    /// (`resolve_modules_with_config<E>`) routes the contribution
    /// through [`relabel_export_phase`](crate::config_loader) before
    /// the merge so it stays phase-agnostic (today every caller is
    /// `E = ParsedExportParam`, making that a same-phase no-op;
    /// `export_params` is always empty so the relabel is total
    /// regardless). The recursive parser-phase caller
    /// (`resolve_nested_modules`) merges directly.
    pub fn expand_module_call(
        &self,
        call: &ModuleCall,
        instance_prefix: &str,
        enclosing_args: Option<&[ArgumentParameter]>,
    ) -> Result<ParsedFile, ModuleError> {
        let module = self
            .imported_modules
            .get(&call.module_name)
            .ok_or_else(|| ModuleError::UnknownModule(call.module_name.clone()))?;

        // Validate required arguments
        for arg in &module.arguments {
            if arg.default.is_none() && !call.arguments.contains_key(&arg.name) {
                return Err(ModuleError::MissingArgument {
                    module: call.module_name.clone(),
                    argument: arg.name.clone(),
                });
            }
        }

        // Validate no unknown arguments
        let declared_arg_names: HashSet<&str> =
            module.arguments.iter().map(|a| a.name.as_str()).collect();
        for arg_name in call.arguments.keys() {
            if !declared_arg_names.contains(arg_name.as_str()) {
                return Err(ModuleError::UnknownArgument {
                    module: call.module_name.clone(),
                    argument: arg_name.clone(),
                });
            }
        }

        // Build argument value map.
        //
        // Defaults may interpolate other arguments (#2393), e.g.
        // `subject_patterns: list(String) = ["repo:${github_repo}:*"]`.
        // The parser registers each argument as a placeholder ResourceRef
        // binding while parsing the block, so the default lands here as a
        // tree containing `Value::Deferred(DeferredValue::ResourceRef{ binding: "<other_arg>" })`
        // nodes.
        //
        // **Initial pass** seeds each entry with either the caller-
        // supplied value or the raw default. **Fix-point loop** then
        // re-substitutes every entry against the current map until an
        // iteration produces no change — this lets a forward-ref
        // default like `prefix: String = later` (where `later` is
        // declared after `prefix`) resolve to `later`'s value once
        // `later` itself is resolved. A hard iteration cap (one round
        // per argument plus one) bounds genuinely cyclic shapes
        // (`a = b`, `b = a`); leftover unresolved refs surface to the
        // post-merge scope check as undefined identifiers. #2817.
        let mut argument_values: HashMap<String, Value> = HashMap::new();
        for arg in &module.arguments {
            let value = call
                .arguments
                .get(&arg.name)
                .cloned()
                .or_else(|| arg.default.clone())
                .unwrap();
            argument_values.insert(arg.name.clone(), value);
        }
        let max_iterations = module.arguments.len() + 1;
        for _ in 0..max_iterations {
            let mut changed = false;
            // Snapshot the current map so each entry resolves against
            // the same generation — without this, the order in which
            // we iterate `module.arguments` would silently re-introduce
            // declaration-order coupling.
            let snapshot = argument_values.clone();
            for arg in &module.arguments {
                let current = argument_values.get(&arg.name).expect("seeded above");
                let mut next = substitute_arguments(current, &snapshot);
                next.canonicalize_in_place();
                if &next != current {
                    argument_values.insert(arg.name.clone(), next);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // Type-check argument values against declared types
        for arg in &module.arguments {
            let value = argument_values.get(&arg.name).unwrap();
            check_module_arg_type(
                &call.module_name,
                &arg.name,
                &arg.type_expr,
                value,
                self.config,
                enclosing_args,
            )?;
        }

        // Validate argument values against validate blocks
        for arg in &module.arguments {
            let value = argument_values.get(&arg.name).unwrap();
            for validation_block in &arg.validations {
                match evaluate_validate_expr(&validation_block.condition, &arg.name, value) {
                    Ok(true) => {} // Validation passed
                    Ok(false) => {
                        let message = validation_block.error_message.clone().unwrap_or_else(|| {
                            format!("validation failed for argument '{}'", arg.name)
                        });
                        return Err(ModuleError::ArgumentValidationFailed {
                            module: call.module_name.clone(),
                            argument: arg.name.clone(),
                            message,
                            actual: format_value_for_error(value),
                        });
                    }
                    Err(e) => {
                        return Err(ModuleError::ArgumentValidationFailed {
                            module: call.module_name.clone(),
                            argument: arg.name.clone(),
                            message: format!("error evaluating validate expression: {}", e),
                            actual: format_value_for_error(value),
                        });
                    }
                }
            }
        }

        // Evaluate require blocks (cross-argument constraints)
        for require in &module.requires {
            match evaluate_require_expr(&require.condition, &argument_values) {
                Ok(true) => {} // Constraint satisfied
                Ok(false) => {
                    return Err(ModuleError::RequireConstraintFailed {
                        module: call.module_name.clone(),
                        message: require.error_message.clone(),
                    });
                }
                Err(e) => {
                    return Err(ModuleError::RequireConstraintFailed {
                        module: call.module_name.clone(),
                        message: format!("error evaluating require expression: {}", e),
                    });
                }
            }
        }

        // Collect intra-module binding names so we can rewrite
        // ResourceRefs. Wait-binding names are included alongside
        // resource bindings so a downstream resource referencing
        // `<wait_binding>.<attr>` is instance-prefixed the same way —
        // otherwise its dependency edge to the wait is lost
        // (carina#3061; see `prefix_wait_binding`).
        let intra_module_bindings: HashSet<String> = module
            .resources
            .iter()
            .filter_map(|r| r.binding.clone())
            .chain(
                module
                    .wait_bindings
                    .iter()
                    .map(|w| w.binding.as_str().to_string()),
            )
            .collect();

        // Expand resources with substituted values
        let mut expanded_resources = Vec::new();
        for resource in &module.resources {
            expanded_resources.push(prefix_module_resource(
                resource,
                instance_prefix,
                &call.module_name,
                &intra_module_bindings,
                &argument_values,
            ));
        }

        // Create a virtual resource if the module has attributes and the call has a binding
        if !module.attribute_params.is_empty()
            && let Some(binding_name) = &call.binding_name
        {
            let mut virtual_attrs: IndexMap<String, Value> = IndexMap::new();

            // Copy attribute values from the module definition
            for attr_param in &module.attribute_params {
                if let Some(value) = &attr_param.value {
                    // Rewrite intra-module refs and substitute arguments
                    let rewritten =
                        rewrite_intra_module_refs(value, instance_prefix, &intra_module_bindings);
                    let mut substituted = substitute_arguments(&rewritten, &argument_values);
                    // Same post-substitution canonicalize as the
                    // resource-attribute path above (#2815 / #2817).
                    substituted.canonicalize_in_place();
                    virtual_attrs.insert(attr_param.name.clone(), substituted);
                }
            }

            let virtual_resource = Resource {
                id: ResourceId::new("_virtual", binding_name),
                attributes: virtual_attrs,
                kind: ResourceKind::Virtual {
                    module_name: call.module_name.clone(),
                    instance: instance_prefix.to_string(),
                },
                directives: Directives::default(),
                prefixes: HashMap::new(),
                binding: Some(binding_name.clone()),
                dependency_bindings: BTreeSet::new(),
                module_source: None,
                quoted_string_attrs: std::collections::HashSet::new(),
            };
            expanded_resources.push(virtual_resource);
        }

        // Propagate the module's `wait` declarations, instance-prefixed
        // (see `prefix_wait_binding`), so a module-internal `wait`
        // reaches the caller's plan (carina#3061).
        let expanded_wait_bindings: Vec<WaitBinding> = module
            .wait_bindings
            .iter()
            .map(|wb| prefix_wait_binding(wb, instance_prefix))
            .collect();

        // carina#3126: propagate the module's deferred for-expressions
        // (previously dropped at this boundary). Each entry is
        // instance-prefixed the same way `module.resources` are, so
        // the loop resolves against the prefixed module bindings and
        // its generated resources are isolated per module instance
        // (carina#3126 PR-B).
        let expanded_deferred_for_expressions: Vec<DeferredForExpression> = module
            .deferred_for_expressions
            .iter()
            .map(|d| {
                prefix_deferred_for_expression(
                    d,
                    instance_prefix,
                    &call.module_name,
                    &intra_module_bindings,
                    &argument_values,
                )
            })
            .collect();

        // The contribution is a full `ParsedFile` built with an
        // **exhaustive struct literal** (no `..Default::default()`):
        // adding a `File<E>` field breaks this until someone decides
        // whether a module instance contributes it. Fields a module
        // does *not* propagate are explicitly empty *here*, with the
        // reason — never silently absent (the carina#3126 fix).
        Ok(ParsedFile {
            // Populated from the module, instance-prefixed:
            resources: expanded_resources,
            wait_bindings: expanded_wait_bindings,
            deferred_for_expressions: expanded_deferred_for_expressions,

            // Consumed *inside* expansion, not propagated to the caller
            // (a module instance does not re-export these as raw
            // collections — they are inlined / surfaced via the
            // virtual attribute resource above):
            //   - `providers`: modules inherit the caller's providers.
            //   - `variables` / `user_functions`: module-local, already
            //     substituted into the expanded resources.
            //   - `uses` / `module_calls`: nested modules are resolved
            //     within `expand_module_call`, not re-emitted.
            //   - `arguments` / `attribute_params` / `export_params`:
            //     the call's args are bound here; outputs reach the
            //     caller through the `_virtual` attribute resource.
            //   - `requires`: evaluated against this call's args here.
            //   - `state_blocks` / `backend`: a module does not own
            //     caller state/backend config.
            //   - `structural_bindings` / `warnings`: scoped to the
            //     module's own parse, not merged upward.
            providers: Vec::new(),
            variables: IndexMap::new(),
            uses: Vec::new(),
            module_calls: Vec::new(),
            arguments: Vec::new(),
            attribute_params: Vec::new(),
            export_params: Vec::new(),
            backend: None,
            state_blocks: Vec::new(),
            user_functions: HashMap::new(),
            upstream_states: Vec::new(),
            requires: Vec::new(),
            structural_bindings: HashSet::new(),
            warnings: Vec::new(),
        })
    }
}

/// Join a module-call `instance_prefix` to an intra-module binding name
/// (`<prefix>.<name>`, dot-separated instance path).
///
/// This is the *single* definition of the instance-prefix spelling.
/// Every site that prefixes a binding — resource ids, resource
/// bindings, `rewrite_intra_module_refs`, and [`prefix_wait_binding`] —
/// routes through here so the format can never drift between binding
/// kinds (the carina#3061 class of bug was a binding kind that was
/// *not* prefixed at all; keeping one spelling makes "is this kind
/// prefixed?" a single, greppable call site per kind).
fn apply_instance_prefix(instance_prefix: &str, name: &str) -> String {
    format!("{instance_prefix}.{name}")
}

/// `BindingName`-typed instance-prefix: the wait path's binding-name
/// fields are `BindingName`, so prefixing them is a typed
/// `BindingName -> BindingName` transition (a raw `String` can't slip
/// into a binding-name position). Delegates to [`apply_instance_prefix`]
/// for the single spelling.
fn prefix_binding_name(instance_prefix: &str, name: &BindingName) -> BindingName {
    BindingName::new(apply_instance_prefix(instance_prefix, name.as_str()))
}

/// Instance-prefix every binding-name field of a [`WaitBinding`] so a
/// module-internal `wait` keeps referring to the same (now prefixed)
/// target / dependencies after expansion.
///
/// The `WaitBinding` is **destructured exhaustively** rather than
/// field-accessed: if a future field is added, this stops compiling
/// until someone decides whether that field is a binding name (prefix
/// it) or value/provenance (pass through). That compile-time forcing
/// function is the carina#3061 guard — the original bug was a
/// binding-carrying structure whose propagation silently skipped a
/// part. Today: `binding`, `target`, the predicate LHS root segment
/// (`lhs_segments[0]`, pinned to the target binding by
/// `parse_wait_expr`), and every `depends_on` entry are prefixed;
/// `until_raw` (verbatim user surface text), `until_predicate.rhs` (a
/// comparison value), `timeout_secs`, and `line` are not binding names
/// and pass through unchanged.
fn prefix_wait_binding(wb: &WaitBinding, instance_prefix: &str) -> WaitBinding {
    let WaitBinding {
        binding,
        target,
        until_raw,
        until_predicate,
        timeout_secs,
        depends_on,
        line,
    } = wb;
    let prefixed_name = |n: &BindingName| prefix_binding_name(instance_prefix, n);

    let mut until_predicate = until_predicate.clone();
    // `lhs_segments[0]` is the (string) target-binding segment of a
    // mixed path `[target, attr, ...]`; only that head is a binding
    // name, so it takes the string-level prefix spelling, not the
    // `BindingName` wrapper (the rest are attribute path segments).
    if let Some(root) = until_predicate.lhs_segments.first_mut() {
        *root = apply_instance_prefix(instance_prefix, root);
    }

    WaitBinding {
        binding: prefixed_name(binding),
        target: prefixed_name(target),
        until_raw: until_raw.clone(),
        until_predicate,
        timeout_secs: *timeout_secs,
        depends_on: depends_on.iter().map(prefixed_name).collect(),
        line: *line,
    }
}

/// Expand one attribute value crossing the module boundary:
/// `rewrite_intra_module_refs` (prefix intra-module refs; a
/// caller-provided ref that coincidentally shares a binding name is
/// left alone) → `substitute_arguments` → `canonicalize_in_place`
/// (collapse a fully-literal `Interpolation` back to a flat `String`,
/// #2815 / #2817).
///
/// The single definition of "expand a module attribute value", shared
/// by [`prefix_module_resource`] (a module resource's attrs) and
/// [`prefix_deferred_for_expression`] (a loop body's attrs) so the two
/// carriers can never drift — the exact carina#3126 bug class.
fn prefix_attr_value(
    value: &Value,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
    argument_values: &HashMap<String, Value>,
) -> Value {
    let rewritten = rewrite_intra_module_refs(value, instance_prefix, intra_module_bindings);
    let mut substituted = substitute_arguments(&rewritten, argument_values);
    substituted.canonicalize_in_place();
    substituted
}

/// Instance-prefix one module resource: `Bound` id name + binding take
/// the instance prefix, typed module-source is stamped, and every
/// attribute is `rewrite_intra_module_refs` → `substitute_arguments`
/// → `canonicalize_in_place` (the #2222 / #2815 / #2817 sequence).
///
/// The single definition of "expand a module resource", shared by the
/// `module.resources` loop and `prefix_deferred_for_expression`'s
/// `template_resource` so a loop body inside a module is prefixed
/// identically to a top-level module resource (carina#3126 PR-B).
fn prefix_module_resource(
    resource: &Resource,
    instance_prefix: &str,
    module_name: &str,
    intra_module_bindings: &HashSet<String>,
    argument_values: &HashMap<String, Value>,
) -> Resource {
    let mut new_resource = resource.clone();

    // Only Bound names take the prefix here. Pending names stay
    // Pending so `compute_anonymous_identifiers` can later attach both
    // the hash and the instance prefix in one shot (#2516).
    if let ResourceName::Bound(name) = &new_resource.id.name {
        let new_name = apply_instance_prefix(instance_prefix, name);
        new_resource.id.set_name(new_name);
    }

    if let Some(ref binding) = new_resource.binding {
        new_resource.binding = Some(apply_instance_prefix(instance_prefix, binding));
    }

    new_resource.module_source = Some(crate::resource::ModuleSource::Module {
        name: module_name.to_string(),
        instance: instance_prefix.to_string(),
    });

    // `IndexMap` preserves authored attribute order across expansion
    // (#2222); each value goes through the shared `prefix_attr_value`.
    let mut substituted_attrs: IndexMap<String, Value> = IndexMap::new();
    for (key, expr) in &new_resource.attributes {
        substituted_attrs.insert(
            key.clone(),
            prefix_attr_value(
                expr,
                instance_prefix,
                intra_module_bindings,
                argument_values,
            ),
        );
    }
    new_resource.attributes = substituted_attrs;

    new_resource
}

/// Instance-prefix a [`DeferredForExpression`] crossing a module
/// boundary, expanding its loop body the same way the
/// `module.resources` expansion ([`prefix_module_resource`]) does.
///
/// The struct is **destructured exhaustively** — the carina#3061 /
/// carina#3126 compile-time forcing function: a new
/// `DeferredForExpression` field stops this compiling until someone
/// classifies it as binding-name (prefix), value/provenance (pass
/// through), or loop-local (pass through).
///
/// carina#3126 PR-B: the loop body declared inside a module must be
/// expanded *as if it were a module resource* — otherwise its
/// generated resources collide across module instances and its
/// iterable resolves against the wrong (unprefixed) binding. Per
/// field:
/// - `binding_name` (the generated-resource address prefix, e.g.
///   `_domain_validation_options`) → instance-prefixed
///   unconditionally, exactly like `Resource.binding`, so each module
///   instance's loop resources are uniquely addressed.
/// - `iterable_binding` (the iterable's root binding, e.g. `cert`) →
///   instance-prefixed **only when it is a module-internal binding**,
///   the same caller-collision guard [`rewrite_intra_module_refs`]
///   applies to a `ResourceRef` head (a caller-provided binding that
///   coincidentally shares a name must not be prefixed). This is a
///   deliberate, more-correct divergence from the design doc's PR-B
///   table (which prescribed an unconditional prefix); see the
///   per-field comment in the body for why the design's cited
///   `prefix_wait_binding` mirror is unsafe to copy here.
/// - `attributes` and `template_resource` → the full module-resource
///   treatment via [`prefix_module_resource`] / the same
///   ref-rewrite + arg-substitute + canonicalize as `module.resources`.
/// - `file`/`line`/`header`/`resource_type`/`iterable_attr`/`binding`
///   → provenance / display / non-binding / loop-local: pass through.
fn prefix_deferred_for_expression(
    d: &DeferredForExpression,
    instance_prefix: &str,
    module_name: &str,
    intra_module_bindings: &HashSet<String>,
    argument_values: &HashMap<String, Value>,
) -> DeferredForExpression {
    let DeferredForExpression {
        file,
        line,
        header,
        resource_type,
        attributes,
        binding_name,
        iterable_binding,
        iterable_attr,
        binding,
        template_resource,
    } = d;

    DeferredForExpression {
        // provenance — pass through (like WaitBinding.line)
        file: file.clone(),
        line: *line,
        // verbatim user-surface display text — pass through
        // (like WaitBinding.until_raw)
        header: header.clone(),
        // provider-qualified type — not a binding
        resource_type: resource_type.clone(),
        // The loop body's attrs may reference module-internal bindings
        // / module arguments — same shared `prefix_attr_value` the
        // module resource attrs get.
        attributes: attributes
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    prefix_attr_value(v, instance_prefix, intra_module_bindings, argument_values),
                )
            })
            .collect(),
        // Generated-resource address prefix — `binding_name` is a name
        // the loop *synthesizes* for its own resources (e.g.
        // `_domain_validation_options`), NOT a reference to a
        // pre-existing binding, so the caller-collision concern does
        // not apply: prefix it unconditionally (same as
        // `Resource.binding`) to isolate each module instance's loop
        // resources.
        binding_name: apply_instance_prefix(instance_prefix, binding_name),
        // Iterable root binding — this IS a reference to an existing
        // binding (`cert` in `for _, opt in cert.dvo`), so it takes
        // the same caller-collision guard as a `ResourceRef` head in
        // `rewrite_intra_module_refs`: prefix only when it names a
        // module-internal binding. A caller-passed binding that
        // coincidentally shares the name must NOT be prefixed.
        //
        // NB: this is a *deliberate, more-correct* divergence from the
        // design doc's PR-B table, which prescribed an *unconditional*
        // prefix "mirroring `prefix_wait_binding`'s lhs_segments[0]".
        // That mirror is wrong: `prefix_wait_binding` prefixes the
        // `until` LHS root unconditionally (a known pre-existing
        // looseness, safe only because that root is virtually always
        // the module-internal wait/resource binding). Copying it here
        // would silently break a module that iterates a caller-passed
        // binding. The `rewrite_intra_module_refs` model is the right
        // one — do not "simplify" this back to unconditional.
        iterable_binding: if intra_module_bindings.contains(iterable_binding) {
            apply_instance_prefix(instance_prefix, iterable_binding)
        } else {
            iterable_binding.clone()
        },
        // attribute path tail — not a binding
        iterable_attr: iterable_attr.clone(),
        // loop-var pattern kind — loop-local, not a module binding
        binding: binding.clone(),
        // Full module-resource treatment — identical to a
        // `module.resources` entry.
        template_resource: prefix_module_resource(
            template_resource,
            instance_prefix,
            module_name,
            intra_module_bindings,
            argument_values,
        ),
    }
}

/// Substitute arguments references with actual values.
///
/// Argument parameter names are registered as lexical bindings in the
/// parser. A bare-name reference (`source_arn`) parses as
/// [`Value::Deferred(DeferredValue::BindingRef)`]; an attribute access (`source_arn.field`)
/// parses as [`Value::Deferred(DeferredValue::ResourceRef)`]. Both forms can target an argument
/// parameter, so substitution covers both.
pub(super) fn substitute_arguments(value: &Value, arguments: &HashMap<String, Value>) -> Value {
    match value {
        Value::Deferred(DeferredValue::BindingRef { binding })
            if arguments.contains_key(binding) =>
        {
            arguments
                .get(binding)
                .cloned()
                .unwrap_or_else(|| value.clone())
        }
        Value::Deferred(DeferredValue::ResourceRef { path })
            if arguments.contains_key(path.binding()) =>
        {
            arguments
                .get(path.binding())
                .cloned()
                .unwrap_or_else(|| value.clone())
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|v| substitute_arguments(v, arguments))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_arguments(v, arguments)))
                .collect(),
        )),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            let substituted_parts: Vec<InterpolationPart> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        InterpolationPart::Expr(substitute_arguments(v, arguments))
                    }
                    other => other.clone(),
                })
                .collect();
            Value::Deferred(DeferredValue::Interpolation(substituted_parts))
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            Value::Deferred(DeferredValue::FunctionCall {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|v| substitute_arguments(v, arguments))
                    .collect(),
            })
        }
        _ => value.clone(),
    }
}

/// Rewrite intra-module ResourceRef binding names with instance path.
///
/// When a ResourceRef's binding_name matches one of the module's own bindings,
/// prefix it with dot notation so that each module instance has isolated references.
pub(super) fn rewrite_intra_module_refs(
    value: &Value,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
) -> Value {
    match value {
        Value::Deferred(DeferredValue::BindingRef { binding })
            if intra_module_bindings.contains(binding) =>
        {
            Value::Deferred(DeferredValue::BindingRef {
                binding: apply_instance_prefix(instance_prefix, binding),
            })
        }
        Value::Deferred(DeferredValue::ResourceRef { path })
            if intra_module_bindings.contains(path.binding()) =>
        {
            Value::Deferred(DeferredValue::ResourceRef {
                path: crate::resource::AccessPath::with_segments(
                    apply_instance_prefix(instance_prefix, path.binding()),
                    path.attribute().to_string(),
                    path.segments().to_vec(),
                ),
            })
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|v| rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings),
                    )
                })
                .collect(),
        )),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            let rewritten_parts: Vec<InterpolationPart> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => InterpolationPart::Expr(
                        rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings),
                    ),
                    other => other.clone(),
                })
                .collect();
            Value::Deferred(DeferredValue::Interpolation(rewritten_parts))
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            Value::Deferred(DeferredValue::FunctionCall {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|v| rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings))
                    .collect(),
            })
        }
        _ => value.clone(),
    }
}

/// Compute the instance prefix for a module call. Named calls use the
/// binding name; anonymous calls get `<module>_<16hex>` where the hex is a
/// SimHash of the call's module name + flattened arguments.
///
/// SimHash is locality-sensitive, so editing one argument flips only a few
/// bits — `reconcile_anonymous_module_instances` can then find the matching
/// state entry by Hamming distance and preserve the resource address across
/// argument edits.
pub fn instance_prefix_for_call(call: &ModuleCall) -> String {
    use std::collections::BTreeMap;

    if let Some(name) = &call.binding_name {
        return name.clone();
    }

    let mut features: BTreeMap<String, String> = BTreeMap::new();
    features.insert("_module".to_string(), call.module_name.clone());
    for (k, v) in &call.arguments {
        crate::identifier::flatten_value_for_simhash(k, v, &mut features);
    }
    let simhash = crate::identifier::compute_simhash(&features);
    format!("{}_{:016x}", call.module_name, simhash)
}

/// Split a module-instance prefix into `(module_name, simhash)` when the
/// tail looks like a 16-hex SimHash. Returns `None` for non-synthetic prefixes
/// (user-written binding names, pre-SimHash state formats, etc.).
pub(super) fn parse_synthetic_instance_prefix(prefix: &str) -> Option<(&str, u64)> {
    let (module, hex) = prefix.rsplit_once('_')?;
    if hex.len() != 16 {
        return None;
    }
    let simhash = u64::from_str_radix(hex, 16).ok()?;
    if module.is_empty() {
        return None;
    }
    Some((module, simhash))
}

/// Split a resource name into `(instance_prefix, rest)` at the first `.`, or
/// return `None` if it has no dot (no module instance prefix at all).
pub(super) fn split_instance_prefix(name: &str) -> Option<(&str, &str)> {
    name.split_once('.')
}

/// Reconcile anonymous module-instance prefixes with existing state.
///
/// When a user edits an argument of an anonymous module call, its SimHash
/// prefix shifts a few bits. The expanded resources therefore live under a
/// new address (e.g. `thing_ab12….role` → `thing_cd34….role`) and would
/// otherwise look like destroy + create to the differ. This pass detects the
/// case by Hamming-distance matching: for each current DSL instance prefix
/// whose address is absent from state, find a state-only prefix for the same
/// module within `SIMHASH_HAMMING_THRESHOLD` bits; if exactly one candidate
/// qualifies, rewrite the current resources to use the state address.
///
/// `find_state_names_by_type` returns every state resource name for a given
/// `(provider, resource_type)` — the reconciler uses them to discover which
/// instance prefixes already exist in state.
pub fn reconcile_anonymous_module_instances(
    resources: &mut [Resource],
    find_state_names_by_type: &dyn Fn(&str, &str) -> Vec<String>,
) {
    use std::collections::{HashMap, HashSet};

    // Collect current (provider, resource_type) pairs that appear in the
    // expanded DSL — we'll query state for matching entries.
    let mut touched_types: HashSet<(String, String)> = HashSet::new();
    for r in resources.iter() {
        if split_instance_prefix(r.id.name_str()).is_none() {
            continue;
        }
        touched_types.insert((r.id.provider.clone(), r.id.resource_type.clone()));
    }

    if touched_types.is_empty() {
        return;
    }

    // Current DSL synthetic prefixes per module — only one entry per
    // distinct prefix (a multi-resource module instance shares one prefix
    // across all of its resources).
    let mut current_synthetic_by_module: HashMap<String, HashSet<u64>> = HashMap::new();
    for r in resources.iter() {
        let Some((prefix, _)) = split_instance_prefix(r.id.name_str()) else {
            continue;
        };
        let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
            continue;
        };
        current_synthetic_by_module
            .entry(module.to_string())
            .or_default()
            .insert(simhash);
    }

    // State synthetic prefixes per module. Use a set so a multi-resource
    // module instance — which contributes one state entry per resource
    // type, all under the same prefix — collapses to one candidate. With a
    // Vec the same hash would appear N times and the Hamming-distance
    // search below would mistake duplicates for ambiguous candidates and
    // refuse to remap (#2211).
    let mut state_synthetic_by_module: HashMap<String, HashSet<u64>> = HashMap::new();

    for (provider, resource_type) in &touched_types {
        for name in find_state_names_by_type(provider, resource_type) {
            let Some((prefix, _)) = split_instance_prefix(&name) else {
                continue;
            };
            let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
                continue;
            };
            state_synthetic_by_module
                .entry(module.to_string())
                .or_default()
                .insert(simhash);
        }
    }

    // For each current DSL prefix that has no matching state prefix, find the
    // closest orphan state prefix for the same module. Candidate state hashes
    // exclude any prefix already used by a current DSL instance — without
    // that filter, two distinct anonymous calls could collapse onto the same
    // state entry when only one of them existed before.
    let mut prefix_remap: HashMap<(String, u64), u64> = HashMap::new();
    for (module, current_hashes) in &current_synthetic_by_module {
        let Some(state_hashes) = state_synthetic_by_module.get(module) else {
            continue;
        };
        let orphan_state_hashes: Vec<u64> = state_hashes
            .iter()
            .copied()
            .filter(|h| !current_hashes.contains(h))
            .collect();
        if orphan_state_hashes.is_empty() {
            continue;
        }
        for current_hash in current_hashes {
            if state_hashes.contains(current_hash) {
                continue;
            }
            if let Some(state_hash) = crate::identifier::closest_unique_simhash_match(
                *current_hash,
                orphan_state_hashes.iter().copied(),
                |h| h,
            ) {
                prefix_remap.insert((module.clone(), *current_hash), state_hash);
            }
        }
    }

    if prefix_remap.is_empty() {
        return;
    }

    // Apply remaps: rewrite `id.name` and `binding` for every resource whose
    // instance prefix is in the remap table.
    for r in resources.iter_mut() {
        let Some((prefix, rest)) = split_instance_prefix(r.id.name_str()) else {
            continue;
        };
        let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
            continue;
        };
        if let Some(&target) = prefix_remap.get(&(module.to_string(), simhash)) {
            let new_prefix = format!("{}_{:016x}", module, target);
            let new_name = format!("{}.{}", new_prefix, rest);
            r.id.set_name(new_name);
            if let Some(ref binding) = r.binding
                && let Some((_, binding_rest)) = split_instance_prefix(binding)
            {
                r.binding = Some(format!("{}.{}", new_prefix, binding_rest));
            }
            if let Some(crate::resource::ModuleSource::Module { name, instance: _ }) =
                &r.module_source
            {
                r.module_source = Some(crate::resource::ModuleSource::Module {
                    name: name.clone(),
                    instance: new_prefix.clone(),
                });
            }
        }
    }

    // After remapping resource names, intra-module ResourceRefs also point at
    // bindings with the old prefix. Walk every value and rewrite those.
    for r in resources.iter_mut() {
        let mut replacements = Vec::new();
        for (key, value) in r.attributes.iter() {
            let rewritten = rewrite_ref_prefixes(value, &prefix_remap);
            if rewritten != *value {
                replacements.push((key.clone(), rewritten));
            }
        }
        for (key, new_value) in replacements {
            r.set_attr(key, new_value);
        }
    }
}

fn rewrite_ref_prefixes(
    value: &Value,
    remap: &std::collections::HashMap<(String, u64), u64>,
) -> Value {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let binding = path.binding();
            if let Some((prefix, rest)) = binding.split_once('.')
                && let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix)
                && let Some(&target) = remap.get(&(module.to_string(), simhash))
            {
                let new_binding = format!("{}_{:016x}.{}", module, target, rest);
                return Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::with_segments(
                        new_binding,
                        path.attribute().to_string(),
                        path.segments().to_vec(),
                    ),
                });
            }
            value.clone()
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|v| rewrite_ref_prefixes(v, remap))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), rewrite_ref_prefixes(v, remap)))
                .collect(),
        )),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            Value::Deferred(DeferredValue::Interpolation(
                parts
                    .iter()
                    .map(|p| match p {
                        InterpolationPart::Literal(s) => InterpolationPart::Literal(s.clone()),
                        InterpolationPart::Expr(v) => {
                            InterpolationPart::Expr(rewrite_ref_prefixes(v, remap))
                        }
                    })
                    .collect(),
            ))
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            Value::Deferred(DeferredValue::FunctionCall {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|v| rewrite_ref_prefixes(v, remap))
                    .collect(),
            })
        }
        _ => value.clone(),
    }
}
