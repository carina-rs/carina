//! Validation utilities for resources and modules

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::binding_index::BindingIndex;
use crate::parser::{ModuleCall, ParsedFile, ProviderContext, TypeExpr, validate_custom_type};
use crate::provider::ProviderFactory;
use crate::resource::{Resource, Value};
use crate::schema::{AttributeType, ResourceSchema, suggest_similar_name};

/// Validate resources against their schemas.
///
/// Checks that each resource's type is known, data sources use the `read` keyword,
/// and all attributes pass schema validation.
pub fn validate_resources(
    parsed: &ParsedFile,
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
    known_providers: &HashSet<String>,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    for (_ctx, resource) in parsed.iter_all_resources() {
        // Skip virtual resources (module attribute containers)
        if resource.is_virtual() {
            continue;
        }

        let schema_key = schema_key_fn(resource);

        match schemas.get(&schema_key) {
            Some(schema) => {
                if schema.data_source && !resource.is_data_source() {
                    all_errors.push(format!(
                        "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                        schema.resource_type, schema.resource_type
                    ));
                }
                let is_string_literal = |attr: &str| resource.quoted_string_attrs.contains(attr);
                if let Err(errors) = schema
                    .validate_with_origins(&resource.resolved_attributes(), &is_string_literal)
                {
                    for error in errors {
                        all_errors.push(format!("{}: {}", resource.id, error));
                    }
                }
            }
            None => {
                // If no factory is registered for this provider, skip validation
                // (schemas are simply not available)
                if !resource.id.provider.is_empty()
                    && !known_providers.contains(&resource.id.provider)
                {
                    continue;
                }
                all_errors.push(format!("Unknown resource type: {}", schema_key));
            }
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// Validate that resource references have compatible types.
///
/// For example, if `ipv4_ipam_pool_id` expects `IpamPoolId` type,
/// a reference like `vpc.vpc_id` (which is `AwsResourceId`) should be an error.
pub fn validate_resource_ref_types(
    parsed: &ParsedFile,
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
    argument_names: &HashSet<String>,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    // Single source of truth for `binding_name → (resource, schema)` —
    // shared with the LSP via `BindingIndex` so the two paths cannot drift
    // (#2231).
    let bindings = BindingIndex::from_parsed(parsed, schemas, schema_key_fn);

    for (_ctx, resource) in parsed.iter_all_resources() {
        let schema_key = schema_key_fn(resource);

        let Some(schema) = schemas.get(&schema_key) else {
            continue;
        };

        for (attr_name, attr_value) in &resource.attributes {
            if attr_name.starts_with('_') {
                continue;
            }

            let (ref_binding, ref_attr) = match attr_value.as_value() {
                Value::ResourceRef { path } => {
                    (path.binding().to_string(), path.attribute().to_string())
                }
                _ => continue,
            };

            // Get the expected type for this attribute
            let Some(attr_schema) = schema.attributes.get(attr_name) else {
                continue;
            };
            let expected_type_name = attr_schema.attr_type.type_name();

            // Skip type checking for argument parameter references (resolved at call site)
            if argument_names.contains(ref_binding.as_str()) {
                continue;
            }

            // Look up the referenced binding's schema. `BindingIndex::get`
            // returns `Some` only when both the binding and its schema
            // resolved; `contains_name` distinguishes "unknown binding"
            // from "known binding, schema absent" so we keep the original
            // diagnostic shape (only the former gets reported here).
            let Some(ref_entry) = bindings.get(ref_binding.as_str()) else {
                if !bindings.is_declared(ref_binding.as_str()) {
                    all_errors.push(format!(
                        "{}: unknown binding '{}' in reference {}.{}",
                        resource.id, ref_binding, ref_binding, ref_attr,
                    ));
                }
                continue;
            };
            let ref_schema = ref_entry.schema;
            let Some(ref_attr_schema) = ref_schema.attributes.get(ref_attr.as_str()) else {
                let known_attrs: Vec<&str> =
                    ref_schema.attributes.keys().map(|s| s.as_str()).collect();
                let suggestion = suggest_similar_name(&ref_attr, &known_attrs)
                    .map(|s| format!(" Did you mean '{}'?", s))
                    .unwrap_or_default();
                all_errors.push(format!(
                    "{}: unknown attribute '{}' on '{}' in reference {}.{}{}",
                    resource.id, ref_attr, ref_binding, ref_binding, ref_attr, suggestion,
                ));
                continue;
            };
            let ref_type_name = ref_attr_schema.attr_type.type_name();

            // Directional check: source (the referenced attribute) must be
            // assignable to the sink (the current resource's attribute).
            if ref_attr_schema
                .attr_type
                .is_assignable_to(&attr_schema.attr_type)
            {
                continue;
            }

            all_errors.push(format!(
                "{}: cannot assign {} to '{}': expected {}, got {} (from {}.{})",
                resource.id,
                ref_type_name,
                attr_name,
                expected_type_name,
                ref_type_name,
                ref_binding,
                ref_attr,
            ));
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// Validate that attribute parameter ResourceRef values have types compatible
/// with their declared TypeExpr types.
///
/// For example, `attributes { role_arn: iam_role_arn = role.role_name }` should
/// be rejected because `role_name` is `String`, not `IamRoleArn`.
pub fn validate_attribute_param_ref_types(
    attribute_params: &[crate::parser::AttributeParameter],
    resources: &[Resource],
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
) -> Result<(), String> {
    let mut binding_map: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(ref binding_name) = resource.binding {
            binding_map.insert(binding_name.clone(), resource);
        }
    }

    let mut errors = Vec::new();

    for param in attribute_params {
        let Some(ref type_expr) = param.type_expr else {
            continue;
        };
        let Some(ref value) = param.value else {
            continue;
        };

        // Only check ResourceRef values
        let Value::ResourceRef { path } = value else {
            continue;
        };
        let ref_binding = path.binding().to_string();
        let ref_attr = path.attribute().to_string();

        // Get expected type name from TypeExpr
        let expected_type = match type_expr {
            crate::parser::TypeExpr::Simple(name) => name.as_str(),
            _ => continue, // String, Bool, etc. are handled by validate_type_expr_value
        };

        // Look up referenced resource's schema
        let Some(ref_resource) = binding_map.get(&ref_binding) else {
            continue;
        };
        let ref_schema_key = schema_key_fn(ref_resource);
        let Some(ref_schema) = schemas.get(&ref_schema_key) else {
            continue;
        };
        let Some(ref_attr_schema) = ref_schema.attributes.get(ref_attr.as_str()) else {
            continue;
        };

        let ref_type_name = ref_attr_schema.attr_type.type_name();
        let ref_type_snake = crate::parser::pascal_to_snake(&ref_type_name);

        if ref_type_snake == expected_type {
            continue;
        }

        errors.push(format!(
            "attribute '{}': type mismatch: expected {}, got {} (from {}.{})",
            param.name, expected_type, ref_type_snake, ref_binding, ref_attr
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Validate export parameter values that are ResourceRef against their declared
/// TypeExpr by looking up the referenced attribute's schema type.
///
/// This catches mismatches like `exports { x: list(bool) = [vpc.vpc_id] }` where
/// `vpc_id` is a string attribute but the export declares `bool`.
pub fn validate_export_param_ref_types(
    export_params: &[crate::parser::ExportParameter],
    resources: &[Resource],
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
) -> Result<(), String> {
    let mut binding_map: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(ref binding_name) = resource.binding {
            binding_map.insert(binding_name.clone(), resource);
        }
    }

    let mut errors = Vec::new();

    for param in export_params {
        let Some(ref type_expr) = param.type_expr else {
            continue;
        };
        let Some(ref value) = param.value else {
            continue;
        };

        collect_ref_type_errors(
            type_expr,
            value,
            &param.name,
            &binding_map,
            schemas,
            schema_key_fn,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Recursively check ResourceRef values in a value tree against their declared TypeExpr.
fn collect_ref_type_errors(
    type_expr: &crate::parser::TypeExpr,
    value: &Value,
    param_name: &str,
    binding_map: &HashMap<String, &Resource>,
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
    errors: &mut Vec<String>,
) {
    use crate::parser::TypeExpr;

    match (type_expr, value) {
        (_, Value::ResourceRef { path }) => {
            let ref_binding = path.binding();
            let ref_attr = path.attribute();

            let Some(ref_resource) = binding_map.get(ref_binding) else {
                return;
            };
            let ref_schema_key = schema_key_fn(ref_resource);
            let Some(ref_schema) = schemas.get(&ref_schema_key) else {
                return;
            };
            let Some(ref_attr_schema) = ref_schema.attributes.get(ref_attr) else {
                return;
            };

            let ref_type = &ref_attr_schema.attr_type;
            if !is_type_expr_compatible_with_schema(type_expr, ref_type) {
                let ref_type_name = ref_type.type_name();
                errors.push(format!(
                    "export '{}': type mismatch for '{}.{}': expected {}, got {}",
                    param_name, ref_binding, ref_attr, type_expr, ref_type_name,
                ));
            }
        }
        (TypeExpr::List(inner), Value::List(items)) => {
            for item in items {
                collect_ref_type_errors(
                    inner,
                    item,
                    param_name,
                    binding_map,
                    schemas,
                    schema_key_fn,
                    errors,
                );
            }
        }
        (TypeExpr::Map(inner), Value::Map(map)) => {
            for value in map.values() {
                collect_ref_type_errors(
                    inner,
                    value,
                    param_name,
                    binding_map,
                    schemas,
                    schema_key_fn,
                    errors,
                );
            }
        }
        (TypeExpr::Struct { fields }, Value::Map(map)) => {
            for (name, field_ty) in fields {
                if let Some(value) = map.get(name) {
                    collect_ref_type_errors(
                        field_ty,
                        value,
                        param_name,
                        binding_map,
                        schemas,
                        schema_key_fn,
                        errors,
                    );
                }
            }
        }
        _ => {}
    }
}

/// Check if a TypeExpr is compatible with an AttributeType from a schema.
pub fn is_type_expr_compatible_with_schema(
    type_expr: &crate::parser::TypeExpr,
    attr_type: &AttributeType,
) -> bool {
    use crate::parser::TypeExpr;

    match type_expr {
        TypeExpr::String => is_string_compatible_type(attr_type),
        TypeExpr::Bool => matches!(attr_type, AttributeType::Bool),
        TypeExpr::Int => matches!(attr_type, AttributeType::Int),
        TypeExpr::Float => matches!(attr_type, AttributeType::Float),
        TypeExpr::Simple(name) => {
            // Walk the base chain: if any type in the chain matches, it's compatible.
            // e.g., Simple("arn") accepts KmsKeyArn (chain: KmsKeyArn → Arn ✓)
            let mut current = attr_type;
            loop {
                let type_snake = crate::parser::pascal_to_snake(&current.type_name());
                if type_snake == *name {
                    return true;
                }
                match current {
                    AttributeType::Custom { base, .. } => current = base,
                    _ => return false,
                }
            }
        }
        TypeExpr::List(inner) => match attr_type {
            AttributeType::List {
                inner: schema_inner,
                ..
            } => is_type_expr_compatible_with_schema(inner, schema_inner),
            _ => false,
        },
        TypeExpr::Map(inner) => match attr_type {
            AttributeType::Map {
                value: schema_inner,
                ..
            } => is_type_expr_compatible_with_schema(inner, schema_inner),
            _ => false,
        },
        TypeExpr::Struct {
            fields: expr_fields,
        } => match attr_type {
            AttributeType::Struct {
                fields: schema_fields,
                ..
            } => {
                // Bijection: every schema field must match exactly one expr
                // field. We check schema ⇒ expr membership with equal
                // lengths; the parser's duplicate-name rejection keeps
                // expr_fields unique, which together forces a one-to-one
                // correspondence.
                if expr_fields.len() != schema_fields.len() {
                    return false;
                }
                schema_fields.iter().all(|sf| {
                    expr_fields.iter().any(|(n, t)| {
                        n == &sf.name && is_type_expr_compatible_with_schema(t, &sf.field_type)
                    })
                })
            }
            // A consumer annotated as `map(T)` may receive a `struct { a: T,
            // b: T }` value — the shape coerces as long as every field type
            // satisfies T.
            AttributeType::Map {
                value: schema_inner,
                ..
            } => expr_fields
                .iter()
                .all(|(_, ty)| is_type_expr_compatible_with_schema(ty, schema_inner)),
            _ => false,
        },
        _ => true, // Ref, SchemaType — conservatively accept
    }
}

/// Check if an AttributeType is string-compatible (can accept a string value).
pub fn is_string_compatible_type(attr_type: &AttributeType) -> bool {
    match attr_type {
        AttributeType::String | AttributeType::Custom { .. } | AttributeType::StringEnum { .. } => {
            true
        }
        AttributeType::Union(types) => types.iter().all(is_string_compatible_type),
        _ => false,
    }
}

/// Check that a root configuration does not contain `arguments` blocks.
///
/// `arguments` is a module-input declaration: it belongs on the module side
/// of a module boundary and is paired with `use` on the caller side. In a
/// root configuration there is no caller to pass values, so the block has
/// no meaning — its `default` would silently become a de-facto root
/// variable, which is not a documented feature (issue #2198).
///
/// A directory loaded via the CLI may be either a root config or a module
/// the user is validating in isolation. We only flag the misplaced block
/// when a `backend` or `provider` block is also present, since both are
/// root-only constructs and unambiguously identify a root configuration.
pub fn validate_no_arguments_in_root(parsed: &ParsedFile) -> Result<(), String> {
    let is_root = parsed.backend.is_some() || !parsed.providers.is_empty();
    if !parsed.arguments.is_empty() && is_root {
        return Err(
            "arguments blocks are only valid inside module definitions, not in root configurations.".to_string(),
        );
    }
    Ok(())
}

/// Check that a module file does not contain provider blocks.
///
/// Provider configuration should only be defined at the root configuration level,
/// not inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_provider_in_module(parsed: &ParsedFile) -> Result<(), String> {
    let is_module = !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty();
    if is_module && !parsed.providers.is_empty() {
        return Err(
            "provider blocks are not allowed inside modules. Define providers at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Validate provider configuration attributes.
///
/// Runs host-side type-level validation using
/// [`ProviderFactory::provider_config_attribute_types`] first, then
/// delegates to [`ProviderFactory::validate_config`] for any
/// provider-specific semantic checks. Keeping format validation
/// (namespace structure, enum membership) on the host side means fixes
/// in `carina-core` take effect without rebuilding provider binaries.
pub fn validate_provider_config(
    parsed: &ParsedFile,
    factories: &[Box<dyn ProviderFactory>],
) -> Result<(), String> {
    for provider in &parsed.providers {
        let Some(factory) = factories.iter().find(|f| f.name() == provider.name) else {
            continue;
        };
        // Host-side type-level validation.
        let attr_types = factory.provider_config_attribute_types();
        for (attr_name, value) in &provider.attributes {
            if let Some(attr_type) = attr_types.get(attr_name) {
                attr_type
                    .validate(value)
                    .map_err(|e| format!("provider {}: {}: {}", provider.name, attr_name, e))?;
            }
        }
        // Provider-specific validation.
        factory
            .validate_config(&provider.attributes)
            .map_err(|e| format!("provider {}: {}", provider.name, e))?;
    }
    Ok(())
}

/// Validate module call arguments against module argument types.
///
/// `imported_modules` maps module alias to its argument parameter definitions.
/// `config` provides custom type validators from providers.
pub fn validate_module_calls(
    module_calls: &[ModuleCall],
    imported_modules: &HashMap<String, Vec<crate::parser::ArgumentParameter>>,
    config: &ProviderContext,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for call in module_calls {
        if let Some(module_args) = imported_modules.get(&call.module_name) {
            for (arg_name, arg_value) in &call.arguments {
                if let Some(arg_param) = module_args.iter().find(|a| &a.name == arg_name)
                    && let Some(error) =
                        validate_type_expr_value(&arg_param.type_expr, arg_value, config)
                {
                    errors.push(format!(
                        "module {} argument '{}': {}",
                        call.module_name, arg_name, error
                    ));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Validate export parameter values against their declared type annotations.
///
/// For each export with both a `type_expr` and a `value`, validates the value
/// using `validate_type_expr_value`. Accumulates all errors.
pub fn validate_export_params(
    export_params: &[crate::parser::ExportParameter],
    config: &ProviderContext,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for param in export_params {
        if let (Some(type_expr), Some(value)) = (&param.type_expr, &param.value)
            && let Some(error) = validate_type_expr_value(type_expr, value, config)
        {
            errors.push(format!("export '{}': {}", param.name, error));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Check for unused `let` bindings and return the unused binding names.
///
/// A binding is unused if its name never appears as a `ResourceRef.binding_name`
/// in any resource attribute, module call argument, or attribute parameter value.
pub fn check_unused_bindings(parsed: &ParsedFile) -> Vec<String> {
    // Collect all defined binding names (skip discard pattern `_`).
    // Walk top-level and for-body resources so bindings declared inside a
    // `for` template are also tracked.
    let mut defined_bindings: Vec<String> = Vec::new();
    for (_ctx, resource) in parsed.iter_all_resources() {
        if let Some(ref binding_name) = resource.binding {
            if binding_name == "_" {
                continue;
            }
            defined_bindings.push(binding_name.clone());
        }
    }

    if defined_bindings.is_empty() {
        return Vec::new();
    }

    // Collect all referenced binding names. Walk both top-level resources
    // and for-body template resources so bindings referenced only inside a
    // `for` loop are counted as used.
    //
    // `collect_dot_notation_refs` also runs on resource attributes: when
    // a resource in file A references `binding.attr` where `binding` is
    // declared in sibling file B, per-file parse stores it as
    // `Value::String("binding.attr")`. `resolve_resource_refs_with_config`
    // lifts those to `ResourceRef` only when the value sits at the top
    // level of an attribute; inside a list / map / interpolation the
    // string form survives, so a reference nested in
    // `principals = [binding.attr]` would otherwise be missed.
    let mut referenced: HashSet<String> = HashSet::new();
    for (_ctx, resource) in parsed.iter_all_resources() {
        for (attr_name, value) in &resource.attributes {
            if attr_name.starts_with('_') {
                continue;
            }
            collect_resource_refs(value, &mut referenced);
            collect_dot_notation_refs(value, &mut referenced);
        }
    }
    for call in &parsed.module_calls {
        for value in call.arguments.values() {
            collect_resource_refs(value, &mut referenced);
            collect_dot_notation_refs(value, &mut referenced);
        }
    }
    for attr_param in &parsed.attribute_params {
        if let Some(value) = &attr_param.value {
            collect_resource_refs(value, &mut referenced);
        }
    }
    for export_param in &parsed.export_params {
        if let Some(value) = &export_param.value {
            collect_resource_refs(value, &mut referenced);
            // Cross-file: when exports.crn is parsed without the binding context,
            // "vpc.vpc_id" becomes String("vpc.vpc_id") instead of ResourceRef.
            // Extract the binding name from such dot-notation strings.
            collect_dot_notation_refs(value, &mut referenced);
        }
    }
    for attr_param in &parsed.attribute_params {
        if let Some(value) = &attr_param.value {
            collect_dot_notation_refs(value, &mut referenced);
        }
    }

    // Return unused binding names, skipping structurally-required bindings
    // (if/for/read expressions) and for-generated indexed bindings (e.g., vpcs[0])
    defined_bindings
        .into_iter()
        .filter(|binding| {
            !referenced.contains(binding)
                && !parsed.structural_bindings.contains(binding)
                && !binding.contains('[')
        })
        .collect()
}

/// Recursively collect all `ResourceRef` binding names from a value tree.
fn collect_resource_refs(value: &Value, refs: &mut HashSet<String>) {
    match value {
        Value::ResourceRef { path } => {
            refs.insert(path.binding().to_string());
        }
        Value::List(items) => {
            for item in items {
                collect_resource_refs(item, refs);
            }
        }
        Value::Map(map) => {
            for v in map.values() {
                collect_resource_refs(v, refs);
            }
        }
        _ => {}
    }
}

/// Extract binding names from dot-notation string values (e.g., "vpc.vpc_id" → "vpc").
///
/// When files are parsed independently, cross-file references like `vpc.vpc_id`
/// become `String("vpc.vpc_id")` instead of `ResourceRef`. This function extracts
/// the first component as a potential binding name.
fn collect_dot_notation_refs(value: &Value, refs: &mut HashSet<String>) {
    match value {
        Value::String(s) if s.contains('.') && !s.contains(' ') && !s.starts_with('/') => {
            if let Some(binding) = s.split('.').next()
                && !binding.is_empty()
                && binding
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                refs.insert(binding.to_string());
            }
        }
        Value::List(items) => {
            for item in items {
                collect_dot_notation_refs(item, refs);
            }
        }
        Value::Map(map) => {
            for v in map.values() {
                collect_dot_notation_refs(v, refs);
            }
        }
        _ => {}
    }
}

/// Validate a value against a TypeExpr, returning an error message if invalid.
///
/// Shared validation logic used by both CLI module call validation and LSP diagnostics.
/// `config` provides custom type validators from providers (e.g., `iam_policy_arn`).
pub fn validate_type_expr_value(
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> Option<String> {
    match (type_expr, value) {
        (TypeExpr::Simple(name), _) => validate_custom_type(name, value, config).err(),
        (TypeExpr::List(inner), Value::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                if let Some(e) = validate_type_expr_value(inner, item, config) {
                    return Some(format!("Element {}: {}", i, e));
                }
            }
            None
        }
        (TypeExpr::Struct { fields }, Value::Map(entries)) => {
            validate_struct_fields(fields, entries, config)
        }
        (TypeExpr::Struct { .. }, _) => Some(format!(
            "expected {}, got {}.",
            type_expr,
            crate::parser::value_type_name(value)
        )),
        (TypeExpr::Bool, Value::String(s)) => Some(format!(
            "expected {type_expr}, got string \"{s}\". Use true or false."
        )),
        (TypeExpr::Int, Value::String(s)) => {
            Some(format!("expected {type_expr}, got string \"{s}\"."))
        }
        (TypeExpr::Float, Value::String(s)) => {
            Some(format!("expected {type_expr}, got string \"{s}\"."))
        }
        (TypeExpr::String, Value::Bool(b)) => {
            Some(format!("expected {type_expr}, got bool ({b})."))
        }
        (TypeExpr::String, Value::Int(n)) => Some(format!("expected {type_expr}, got int ({n}).")),
        (TypeExpr::String, Value::Float(f)) => {
            Some(format!("expected {type_expr}, got float ({f})."))
        }
        (TypeExpr::Bool, Value::Int(n)) => Some(format!("expected {type_expr}, got int ({n}).")),
        (TypeExpr::Int, Value::Bool(b)) => Some(format!("expected {type_expr}, got bool ({b}).")),
        (TypeExpr::Float, Value::Bool(b)) => Some(format!("expected {type_expr}, got bool ({b}).")),
        // Schema types are string subtypes — reject non-string values
        (TypeExpr::SchemaType { .. }, Value::Bool(b)) => {
            Some(format!("expected {}, got bool ({}).", type_expr, b))
        }
        (TypeExpr::SchemaType { .. }, Value::Int(n)) => {
            Some(format!("expected {}, got int ({}).", type_expr, n))
        }
        (TypeExpr::SchemaType { .. }, Value::Float(f)) => {
            Some(format!("expected {}, got float ({}).", type_expr, f))
        }
        _ => None,
    }
}

/// Check shape-level problems of a `Value::Map` against a struct field
/// list: extra keys and missing keys. Returns `None` when the key sets
/// match. Callers then walk each field with their own type-check pass.
pub fn struct_field_shape_errors(
    fields: &[(String, TypeExpr)],
    entries: &IndexMap<String, Value>,
) -> Option<String> {
    // Sort unknown keys so the diagnostic is stable across HashMap's
    // per-process random hash seed.
    let mut unknown: Vec<&String> = entries
        .keys()
        .filter(|k| !fields.iter().any(|(name, _)| &name == k))
        .collect();
    unknown.sort();
    if let Some(key) = unknown.first() {
        return Some(format!("expected struct, unknown field '{key}'."));
    }
    for (name, _) in fields {
        if !entries.contains_key(name) {
            return Some(format!("expected struct, missing field '{}'.", name));
        }
    }
    None
}

fn validate_struct_fields(
    fields: &[(String, TypeExpr)],
    entries: &IndexMap<String, Value>,
    config: &ProviderContext,
) -> Option<String> {
    if let Some(e) = struct_field_shape_errors(fields, entries) {
        return Some(e);
    }
    for (name, ty) in fields {
        if let Some(v) = entries.get(name)
            && let Some(e) = validate_type_expr_value(ty, v, config)
        {
            return Some(format!("field '{}': {}", name, e));
        }
    }
    None
}

#[cfg(test)]
mod tests;
