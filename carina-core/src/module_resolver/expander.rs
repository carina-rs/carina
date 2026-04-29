//! Module-call expansion, argument substitution, and intra-module
//! reference rewriting.

use std::collections::{BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;

use crate::parser::ModuleCall;
use crate::resource::{Expr, LifecycleConfig, Resource, ResourceId, ResourceKind, Value};

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
    pub fn expand_module_call(
        &self,
        call: &ModuleCall,
        instance_prefix: &str,
    ) -> Result<Vec<Resource>, ModuleError> {
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

        // Build argument value map
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

        // Type-check argument values against declared types
        for arg in &module.arguments {
            let value = argument_values.get(&arg.name).unwrap();
            check_module_arg_type(
                &call.module_name,
                &arg.name,
                &arg.type_expr,
                value,
                self.config,
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

        // Collect intra-module binding names so we can rewrite ResourceRefs
        let intra_module_bindings: HashSet<String> = module
            .resources
            .iter()
            .filter_map(|r| r.binding.clone())
            .collect();

        // Expand resources with substituted values
        let mut expanded_resources = Vec::new();
        for resource in &module.resources {
            let mut new_resource = resource.clone();

            // Prefix the resource name with instance path (dot-separated)
            let new_name = format!("{}.{}", instance_prefix, new_resource.id.name_str());
            new_resource.id = ResourceId::with_provider(
                &new_resource.id.provider,
                &new_resource.id.resource_type,
                new_name.clone(),
            );

            // Rewrite binding with instance path (dot-separated)
            if let Some(ref binding) = new_resource.binding {
                let prefixed = format!("{}.{}", instance_prefix, binding);
                new_resource.binding = Some(prefixed);
            }

            // Set typed module source info
            new_resource.module_source = Some(crate::resource::ModuleSource::Module {
                name: call.module_name.clone(),
                instance: instance_prefix.to_string(),
            });

            // Rewrite intra-module ResourceRefs BEFORE substituting inputs.
            // This ensures that caller-provided ResourceRef values (which may
            // coincidentally share a binding name with a module-internal binding)
            // are not incorrectly prefixed.
            // Preserve user-authored attribute order across the
            // module-call expansion (#2222) — `IndexMap`, not `HashMap`.
            let mut substituted_attrs: IndexMap<String, Expr> = IndexMap::new();
            for (key, expr) in &new_resource.attributes {
                let rewritten =
                    rewrite_intra_module_refs(expr, instance_prefix, &intra_module_bindings);
                let substituted = substitute_arguments(&rewritten, &argument_values);
                substituted_attrs.insert(key.clone(), Expr(substituted));
            }
            new_resource.attributes = substituted_attrs;

            expanded_resources.push(new_resource);
        }

        // Create a virtual resource if the module has attributes and the call has a binding
        if !module.attribute_params.is_empty()
            && let Some(binding_name) = &call.binding_name
        {
            let mut virtual_attrs: IndexMap<String, Expr> = IndexMap::new();

            // Copy attribute values from the module definition
            for attr_param in &module.attribute_params {
                if let Some(value) = &attr_param.value {
                    // Rewrite intra-module refs and substitute arguments
                    let rewritten =
                        rewrite_intra_module_refs(value, instance_prefix, &intra_module_bindings);
                    let substituted = substitute_arguments(&rewritten, &argument_values);
                    virtual_attrs.insert(attr_param.name.clone(), Expr(substituted));
                }
            }

            let virtual_resource = Resource {
                id: ResourceId::new("_virtual", binding_name),
                attributes: virtual_attrs,
                kind: ResourceKind::Virtual {
                    module_name: call.module_name.clone(),
                    instance: instance_prefix.to_string(),
                },
                lifecycle: LifecycleConfig::default(),
                prefixes: HashMap::new(),
                binding: Some(binding_name.clone()),
                dependency_bindings: BTreeSet::new(),
                module_source: None,
                quoted_string_attrs: std::collections::HashSet::new(),
            };
            expanded_resources.push(virtual_resource);
        }

        Ok(expanded_resources)
    }
}

/// Substitute arguments references with actual values.
///
/// Argument parameter names are registered as lexical bindings in the parser,
/// so they appear as `ResourceRef { binding_name: "<param_name>", attribute_name: ... }`.
/// We match when `binding_name` is one of the argument keys.
pub(super) fn substitute_arguments(value: &Value, arguments: &HashMap<String, Value>) -> Value {
    match value {
        Value::ResourceRef { path } if arguments.contains_key(path.binding()) => arguments
            .get(path.binding())
            .cloned()
            .unwrap_or_else(|| value.clone()),
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| substitute_arguments(v, arguments))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_arguments(v, arguments)))
                .collect(),
        ),
        Value::Interpolation(parts) => {
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
            Value::Interpolation(substituted_parts)
        }
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|v| substitute_arguments(v, arguments))
                .collect(),
        },
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
        Value::ResourceRef { path } if intra_module_bindings.contains(path.binding()) => {
            Value::resource_ref(
                format!("{}.{}", instance_prefix, path.binding()),
                path.attribute().to_string(),
                path.field_path().iter().map(|s| s.to_string()).collect(),
            )
        }
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings),
                    )
                })
                .collect(),
        ),
        Value::Interpolation(parts) => {
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
            Value::Interpolation(rewritten_parts)
        }
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|v| rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings))
                .collect(),
        },
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
            r.id = ResourceId::with_provider(&r.id.provider, &r.id.resource_type, new_name.clone());
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
        for (key, expr) in r.attributes.iter() {
            let rewritten = rewrite_ref_prefixes(&expr.0, &prefix_remap);
            if rewritten != expr.0 {
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
        Value::ResourceRef { path } => {
            let binding = path.binding();
            if let Some((prefix, rest)) = binding.split_once('.')
                && let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix)
                && let Some(&target) = remap.get(&(module.to_string(), simhash))
            {
                let new_binding = format!("{}_{:016x}.{}", module, target, rest);
                return Value::resource_ref(
                    new_binding,
                    path.attribute().to_string(),
                    path.field_path().into_iter().map(String::from).collect(),
                );
            }
            value.clone()
        }
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| rewrite_ref_prefixes(v, remap))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), rewrite_ref_prefixes(v, remap)))
                .collect(),
        ),
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            Value::Interpolation(
                parts
                    .iter()
                    .map(|p| match p {
                        InterpolationPart::Literal(s) => InterpolationPart::Literal(s.clone()),
                        InterpolationPart::Expr(v) => {
                            InterpolationPart::Expr(rewrite_ref_prefixes(v, remap))
                        }
                    })
                    .collect(),
            )
        }
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|v| rewrite_ref_prefixes(v, remap))
                .collect(),
        },
        _ => value.clone(),
    }
}
