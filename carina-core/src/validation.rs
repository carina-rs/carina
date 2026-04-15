//! Validation utilities for resources and modules

use std::collections::{HashMap, HashSet};

use crate::parser::{ModuleCall, ParsedFile, ProviderContext, TypeExpr, validate_custom_type};
use crate::provider::ProviderFactory;
use crate::resource::{Resource, Value};
use crate::schema::{AttributeType, ResourceSchema, suggest_similar_name};

/// Validate resources against their schemas.
///
/// Checks that each resource's type is known, data sources use the `read` keyword,
/// and all attributes pass schema validation.
pub fn validate_resources(
    resources: &[Resource],
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
    known_providers: &HashSet<String>,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    for resource in resources {
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
                if let Err(errors) = schema.validate(&resource.resolved_attributes()) {
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
    resources: &[Resource],
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
    argument_names: &HashSet<String>,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    // Build binding_name -> resource map
    let mut binding_map: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(ref binding_name) = resource.binding {
            binding_map.insert(binding_name.clone(), resource);
        }
    }

    for resource in resources {
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

            // Look up the referenced binding's schema to get the type of the referenced attribute
            let Some(ref_resource) = binding_map.get(ref_binding.as_str()) else {
                all_errors.push(format!(
                    "{}: unknown binding '{}' in reference {}.{}",
                    resource.id, ref_binding, ref_binding, ref_attr,
                ));
                continue;
            };
            let ref_schema_key_str = schema_key_fn(ref_resource);
            let Some(ref_schema) = schemas.get(&ref_schema_key_str) else {
                continue;
            };
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

            // Type compatibility check:
            // - Union type accepts any member type name -> OK
            // - Same type -> OK
            // - Either is "String" -> OK (String is the base type for Custom types)
            // - Both are String-based Custom types -> OK (different length/pattern constraints
            //   for the same logical identifier, or semantic type assigned to generic pattern)
            // - Different Custom types -> Error
            if attr_schema.attr_type.accepts_type_name(&ref_type_name) {
                continue;
            }
            if expected_type_name == "String" || ref_type_name == "String" {
                continue;
            }
            if attr_schema.attr_type.is_string_based_custom()
                && ref_attr_schema.attr_type.is_string_based_custom()
            {
                continue;
            }

            all_errors.push(format!(
                "{}: type mismatch for '{}': expected {}, got {} (from {}.{})",
                resource.id, attr_name, expected_type_name, ref_type_name, ref_binding, ref_attr,
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
            let ref_type_snake = crate::parser::pascal_to_snake(&attr_type.type_name());
            // String-typed schema attributes are compatible with any Simple type
            // (the value-level validator handles the rest)
            is_string_compatible_type(attr_type) || &ref_type_snake == name
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
    // Collect all defined binding names (skip discard pattern `_`)
    let mut defined_bindings: Vec<String> = Vec::new();
    for resource in &parsed.resources {
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

    // Collect all referenced binding names
    let mut referenced: HashSet<String> = HashSet::new();
    for resource in &parsed.resources {
        for (attr_name, value) in &resource.attributes {
            if attr_name.starts_with('_') {
                continue;
            }
            collect_resource_refs(value, &mut referenced);
        }
    }
    for call in &parsed.module_calls {
        for value in call.arguments.values() {
            collect_resource_refs(value, &mut referenced);
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
        (TypeExpr::Bool, Value::String(s)) => Some(format!(
            "expected bool, got string \"{}\". Use true or false.",
            s
        )),
        (TypeExpr::Int, Value::String(s)) => Some(format!("expected int, got string \"{}\".", s)),
        (TypeExpr::Float, Value::String(s)) => {
            Some(format!("expected float, got string \"{}\".", s))
        }
        (TypeExpr::String, Value::Bool(b)) => Some(format!("expected string, got bool ({}).", b)),
        (TypeExpr::String, Value::Int(n)) => Some(format!("expected string, got int ({}).", n)),
        (TypeExpr::String, Value::Float(f)) => Some(format!("expected string, got float ({}).", f)),
        (TypeExpr::Bool, Value::Int(n)) => Some(format!("expected bool, got int ({}).", n)),
        (TypeExpr::Int, Value::Bool(b)) => Some(format!("expected int, got bool ({}).", b)),
        (TypeExpr::Float, Value::Bool(b)) => Some(format!("expected float, got bool ({}).", b)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ParsedFile, ProviderContext};
    use crate::resource::Resource;

    fn empty_parsed() -> ParsedFile {
        ParsedFile {
            providers: Vec::new(),
            resources: Vec::new(),
            variables: HashMap::new(),
            imports: Vec::new(),
            module_calls: Vec::new(),
            arguments: Vec::new(),
            attribute_params: Vec::new(),
            export_params: vec![],
            backend: None,
            state_blocks: Vec::new(),
            user_functions: HashMap::new(),
            remote_states: Vec::new(),
            requires: Vec::new(),
            structural_bindings: HashSet::new(),
            warnings: Vec::new(),
            deferred_for_expressions: Vec::new(),
        }
    }

    fn context_with_iam_policy_arn_validator() -> ProviderContext {
        use crate::parser::ValidatorFn;

        let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
        validators.insert(
            "iam_policy_arn".to_string(),
            Box::new(|s: &str| {
                if s.starts_with("arn:aws:iam::") {
                    Ok(())
                } else {
                    Err(format!("invalid IAM policy ARN: '{s}'"))
                }
            }),
        );
        ProviderContext {
            decryptor: None,
            validators,
            custom_type_validator: None,
        }
    }

    #[test]
    fn no_bindings_no_warnings() {
        let parsed = empty_parsed();
        assert!(check_unused_bindings(&parsed).is_empty());
    }

    #[test]
    fn used_binding_no_warning() {
        let mut parsed = empty_parsed();

        // Resource with a binding
        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc")
            .with_binding("vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));
        parsed.resources.push(vpc);

        // Resource that references the binding
        let subnet = Resource::with_provider("awscc", "ec2.subnet", "web-subnet").with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );
        parsed.resources.push(subnet);

        assert!(check_unused_bindings(&parsed).is_empty());
    }

    #[test]
    fn unused_binding_warns() {
        let mut parsed = empty_parsed();

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc")
            .with_binding("vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));
        parsed.resources.push(vpc);

        let unused = check_unused_bindings(&parsed);
        assert_eq!(unused, vec!["vpc"]);
    }

    #[test]
    fn anonymous_resource_no_warning() {
        let mut parsed = empty_parsed();

        // Anonymous resource (no _binding attribute)
        let bucket = Resource::with_provider("awscc", "s3.bucket", "my-bucket")
            .with_attribute("bucket_name", Value::String("my-bucket".to_string()));
        parsed.resources.push(bucket);

        assert!(check_unused_bindings(&parsed).is_empty());
    }

    #[test]
    fn binding_referenced_in_nested_value() {
        let mut parsed = empty_parsed();

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc").with_binding("vpc");
        parsed.resources.push(vpc);

        // Reference inside a Map inside a List
        let mut map = HashMap::new();
        map.insert(
            "vpc_id".to_string(),
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );
        let sg = Resource::with_provider("awscc", "ec2.security_group", "web-sg")
            .with_attribute("tags", Value::List(vec![Value::Map(map)]));
        parsed.resources.push(sg);

        assert!(check_unused_bindings(&parsed).is_empty());
    }

    #[test]
    fn binding_referenced_in_module_call() {
        let mut parsed = empty_parsed();

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc").with_binding("vpc");
        parsed.resources.push(vpc);

        let mut args = HashMap::new();
        args.insert(
            "vpc_id".to_string(),
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );
        parsed.module_calls.push(ModuleCall {
            module_name: "web_tier".to_string(),
            binding_name: None,
            arguments: args,
        });

        assert!(check_unused_bindings(&parsed).is_empty());
    }

    #[test]
    fn multiple_bindings_some_unused() {
        let mut parsed = empty_parsed();

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc").with_binding("vpc");
        parsed.resources.push(vpc);

        let sg =
            Resource::with_provider("awscc", "ec2.security_group", "web-sg").with_binding("web_sg");
        parsed.resources.push(sg);

        // Only vpc is referenced
        let subnet = Resource::with_provider("awscc", "ec2.subnet", "web-subnet").with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );
        parsed.resources.push(subnet);

        let unused = check_unused_bindings(&parsed);
        assert_eq!(unused, vec!["web_sg"]);
    }

    #[test]
    fn binding_referenced_in_attributes_not_warned() {
        let mut parsed = empty_parsed();

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc").with_binding("vpc");
        parsed.resources.push(vpc);

        parsed
            .attribute_params
            .push(crate::parser::AttributeParameter {
                name: "vpc_id".to_string(),
                type_expr: Some(TypeExpr::String),
                value: Some(Value::resource_ref(
                    "vpc".to_string(),
                    "vpc_id".to_string(),
                    vec![],
                )),
            });

        assert!(check_unused_bindings(&parsed).is_empty());
    }

    #[test]
    fn binding_referenced_in_exports_not_warned() {
        let mut parsed = empty_parsed();

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc").with_binding("vpc");
        parsed.resources.push(vpc);

        parsed.export_params.push(crate::parser::ExportParameter {
            name: "vpc_id".to_string(),
            type_expr: Some(TypeExpr::String),
            value: Some(Value::resource_ref(
                "vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        });

        assert!(
            check_unused_bindings(&parsed).is_empty(),
            "binding referenced in exports should not be warned"
        );
    }

    #[test]
    fn igw_route_crn_unused_detection() {
        let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
  cidr_block = "10.0.0.0/16"
}

let igw = awscc.ec2.internet_gateway {
}

let igw_attachment = awscc.ec2.vpc_gateway_attachment {
  vpc_id              = vpc.vpc_id
  internet_gateway_id = igw.internet_gateway_id
}

let rt = awscc.ec2.route_table {
  vpc_id = vpc.vpc_id
}

let route = awscc.ec2.route {
  route_table_id         = rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = igw_attachment.internet_gateway_id
}
"#;
        let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();

        // Check the route resource's gateway_id is a ResourceRef to igw_attachment
        let route = parsed
            .resources
            .iter()
            .find(|r| r.id.name == "route")
            .unwrap();
        let gateway_id = route.get_attr("gateway_id").unwrap();
        match gateway_id {
            Value::ResourceRef { path } => {
                assert_eq!(path.binding(), "igw_attachment");
                assert_eq!(path.attribute(), "internet_gateway_id");
            }
            other => panic!("Expected ResourceRef, got {:?}", other),
        }

        let unused = check_unused_bindings(&parsed);
        // igw_attachment is referenced by route, so should NOT be unused
        // route is the last resource and not referenced, so IS unused
        assert!(
            !unused.contains(&"igw_attachment".to_string()),
            "igw_attachment should not be unused"
        );
        assert_eq!(unused, vec!["route"]);
    }

    #[test]
    fn if_expression_binding_not_warned() {
        let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let enabled = true

let vpc = if enabled {
  awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
  }
}
"#;
        let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();
        assert!(
            parsed.structural_bindings.contains("vpc"),
            "vpc should be in structural_bindings"
        );
        let unused = check_unused_bindings(&parsed);
        assert!(
            !unused.contains(&"vpc".to_string()),
            "if-expression binding should not be warned as unused"
        );
    }

    #[test]
    fn for_expression_binding_not_warned() {
        let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpcs = for (i, env) in ["dev", "stg"] {
  awscc.ec2.vpc {
    cidr_block = cidr_subnet("10.0.0.0/8", 8, i)
  }
}
"#;
        let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();
        assert!(
            parsed.structural_bindings.contains("vpcs"),
            "vpcs should be in structural_bindings"
        );
        let unused = check_unused_bindings(&parsed);
        assert!(
            unused.is_empty(),
            "for-expression bindings should not be warned as unused, got: {:?}",
            unused
        );
    }

    #[test]
    fn read_expression_binding_not_warned() {
        let input = r#"
provider aws {
  region = aws.Region.ap_northeast_1
}

let caller = read aws.sts.caller_identity {}
"#;
        let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();
        assert!(
            parsed.structural_bindings.contains("caller"),
            "caller should be in structural_bindings"
        );
        let unused = check_unused_bindings(&parsed);
        assert!(
            !unused.contains(&"caller".to_string()),
            "read-expression binding should not be warned as unused"
        );
    }

    #[test]
    fn genuinely_unused_binding_still_warns() {
        let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
  cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();
        let unused = check_unused_bindings(&parsed);
        assert_eq!(
            unused,
            vec!["vpc"],
            "genuinely unused binding should still be warned"
        );
    }

    /// Helper to create a simple ResourceSchema with given attributes.
    fn make_schema(resource_type: &str, attrs: Vec<(&str, AttributeType)>) -> ResourceSchema {
        let mut attributes = HashMap::new();
        for (name, attr_type) in attrs {
            attributes.insert(
                name.to_string(),
                crate::schema::AttributeSchema {
                    name: name.to_string(),
                    attr_type,
                    required: false,
                    default: None,
                    description: None,
                    completions: None,
                    provider_name: None,
                    create_only: false,
                    read_only: false,
                    removable: None,
                    block_name: None,
                    write_only: false,
                    identity: false,
                },
            );
        }
        ResourceSchema {
            resource_type: resource_type.to_string(),
            attributes,
            description: None,
            validator: None,
            data_source: false,
            name_attribute: None,
            force_replace: false,
            operation_config: None,
        }
    }

    fn test_schema_key_fn(r: &Resource) -> String {
        r.id.resource_type.clone()
    }

    #[test]
    fn unknown_binding_reference_reports_error() {
        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.subnet".to_string(),
            make_schema("ec2.subnet", vec![("vpc_id", AttributeType::String)]),
        );

        // Subnet references "vpc" binding which doesn't exist
        let subnet = Resource::with_provider("awscc", "ec2.subnet", "web-subnet").with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );

        let result =
            validate_resource_ref_types(&[subnet], &schemas, &test_schema_key_fn, &HashSet::new());
        assert_eq!(
            result.unwrap_err(),
            "awscc.ec2.subnet.web-subnet: unknown binding 'vpc' in reference vpc.vpc_id"
        );
    }

    #[test]
    fn unknown_attribute_reference_reports_error() {
        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.vpc".to_string(),
            make_schema("ec2.vpc", vec![("cidr_block", AttributeType::String)]),
        );
        schemas.insert(
            "ec2.subnet".to_string(),
            make_schema("ec2.subnet", vec![("vpc_id", AttributeType::String)]),
        );

        // VPC resource with binding
        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc")
            .with_binding("vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));

        // Subnet references vpc.nonexistent_attr which doesn't exist on the VPC schema
        let subnet = Resource::with_provider("awscc", "ec2.subnet", "web-subnet").with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "nonexistent_attr".to_string(), vec![]),
        );

        let result = validate_resource_ref_types(
            &[vpc, subnet],
            &schemas,
            &test_schema_key_fn,
            &HashSet::new(),
        );
        assert_eq!(
            result.unwrap_err(),
            "awscc.ec2.subnet.web-subnet: unknown attribute 'nonexistent_attr' on 'vpc' in reference vpc.nonexistent_attr"
        );
    }

    #[test]
    fn unknown_attribute_reference_suggests_similar_name() {
        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.internet_gateway".to_string(),
            make_schema(
                "ec2.internet_gateway",
                vec![("internet_gateway_id", AttributeType::String)],
            ),
        );
        schemas.insert(
            "ec2.route".to_string(),
            make_schema(
                "ec2.route",
                vec![
                    ("route_table_id", AttributeType::String),
                    ("gateway_id", AttributeType::String),
                ],
            ),
        );

        let igw =
            Resource::with_provider("awscc", "ec2.internet_gateway", "igw").with_binding("igw");

        // Typo: internet_gateway_idd instead of internet_gateway_id
        let route = Resource::with_provider("awscc", "ec2.route", "main-route").with_attribute(
            "gateway_id",
            Value::resource_ref(
                "igw".to_string(),
                "internet_gateway_idd".to_string(),
                vec![],
            ),
        );

        let result = validate_resource_ref_types(
            &[igw, route],
            &schemas,
            &test_schema_key_fn,
            &HashSet::new(),
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("Did you mean 'internet_gateway_id'?"),
            "Expected 'did you mean' suggestion, got: {}",
            err
        );
    }

    #[test]
    fn unknown_attribute_reference_no_suggestion_when_too_different() {
        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.vpc".to_string(),
            make_schema("ec2.vpc", vec![("cidr_block", AttributeType::String)]),
        );
        schemas.insert(
            "ec2.subnet".to_string(),
            make_schema("ec2.subnet", vec![("vpc_id", AttributeType::String)]),
        );

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc").with_binding("vpc");

        // Completely unrelated attribute name - no suggestion expected
        let subnet = Resource::with_provider("awscc", "ec2.subnet", "web-subnet").with_attribute(
            "vpc_id",
            Value::resource_ref(
                "vpc".to_string(),
                "completely_wrong_name".to_string(),
                vec![],
            ),
        );

        let result = validate_resource_ref_types(
            &[vpc, subnet],
            &schemas,
            &test_schema_key_fn,
            &HashSet::new(),
        );
        let err = result.unwrap_err();
        assert!(
            !err.contains("Did you mean"),
            "Should not suggest when name is too different, got: {}",
            err
        );
    }

    #[test]
    fn provider_in_module_with_arguments_errors() {
        let mut parsed = empty_parsed();
        parsed.providers.push(crate::parser::ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
            source: None,
            version: None,
            revision: None,
        });
        parsed.arguments.push(crate::parser::ArgumentParameter {
            name: "vpc_cidr".to_string(),
            type_expr: TypeExpr::String,
            default: None,
            description: None,
            validations: Vec::new(),
        });

        let result = validate_no_provider_in_module(&parsed);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "provider blocks are not allowed inside modules. Define providers at the root configuration level."
        );
    }

    #[test]
    fn provider_in_module_with_attributes_errors() {
        let mut parsed = empty_parsed();
        parsed.providers.push(crate::parser::ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
            source: None,
            version: None,
            revision: None,
        });
        parsed
            .attribute_params
            .push(crate::parser::AttributeParameter {
                name: "vpc_id".to_string(),
                type_expr: Some(TypeExpr::String),
                value: Some(Value::String("dummy".to_string())),
            });

        let result = validate_no_provider_in_module(&parsed);
        assert!(result.is_err());
    }

    #[test]
    fn provider_without_module_markers_ok() {
        let mut parsed = empty_parsed();
        parsed.providers.push(crate::parser::ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
            source: None,
            version: None,
            revision: None,
        });

        let result = validate_no_provider_in_module(&parsed);
        assert!(result.is_ok());
    }

    #[test]
    fn module_without_provider_ok() {
        let mut parsed = empty_parsed();
        parsed.arguments.push(crate::parser::ArgumentParameter {
            name: "vpc_cidr".to_string(),
            type_expr: TypeExpr::String,
            default: None,
            description: None,
            validations: Vec::new(),
        });

        let result = validate_no_provider_in_module(&parsed);
        assert!(result.is_ok());
    }

    // --- validate_type_expr_value tests ---

    #[test]
    fn validate_type_expr_value_ipv4_cidr_valid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv4_cidr".to_string()),
            &Value::String("10.0.0.0/16".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_ipv4_cidr_invalid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv4_cidr".to_string()),
            &Value::String("not-a-cidr".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
    }

    #[test]
    fn validate_type_expr_value_ipv4_address_valid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv4_address".to_string()),
            &Value::String("192.168.1.1".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_ipv4_address_invalid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv4_address".to_string()),
            &Value::String("999.999.999.999".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
    }

    #[test]
    fn validate_type_expr_value_ipv6_cidr_valid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv6_cidr".to_string()),
            &Value::String("2001:db8::/32".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_ipv6_cidr_invalid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv6_cidr".to_string()),
            &Value::String("not-ipv6-cidr".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
    }

    #[test]
    fn validate_type_expr_value_ipv6_address_valid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv6_address".to_string()),
            &Value::String("2001:db8::1".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_ipv6_address_invalid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv6_address".to_string()),
            &Value::String("zzz::zzz".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
    }

    #[test]
    fn validate_type_expr_value_bool_mismatch() {
        let result = validate_type_expr_value(
            &TypeExpr::Bool,
            &Value::String("yes".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected bool"));
    }

    #[test]
    fn validate_type_expr_value_int_mismatch() {
        let result = validate_type_expr_value(
            &TypeExpr::Int,
            &Value::String("42".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected int"));
    }

    #[test]
    fn validate_type_expr_value_float_mismatch() {
        let result = validate_type_expr_value(
            &TypeExpr::Float,
            &Value::String("3.14".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected float"));
    }

    #[test]
    fn validate_type_expr_value_list_of_ipv4_address() {
        let items = vec![
            Value::String("192.168.1.1".to_string()),
            Value::String("999.0.0.1".to_string()),
        ];
        let result = validate_type_expr_value(
            &TypeExpr::List(Box::new(TypeExpr::Simple("ipv4_address".to_string()))),
            &Value::List(items),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("Element 1"));
    }

    #[test]
    fn validate_type_expr_value_string_type_accepts_string() {
        let result = validate_type_expr_value(
            &TypeExpr::String,
            &Value::String("hello".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_string_got_bool() {
        let result = validate_type_expr_value(
            &TypeExpr::String,
            &Value::Bool(true),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected string, got bool"));
    }

    #[test]
    fn validate_type_expr_value_string_got_int() {
        let result = validate_type_expr_value(
            &TypeExpr::String,
            &Value::Int(42),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected string, got int"));
    }

    #[test]
    fn validate_type_expr_value_string_got_float() {
        let result = validate_type_expr_value(
            &TypeExpr::String,
            &Value::Float(1.5),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected string, got float"));
    }

    #[test]
    fn validate_type_expr_value_bool_got_int() {
        let result =
            validate_type_expr_value(&TypeExpr::Bool, &Value::Int(1), &ProviderContext::default());
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected bool, got int"));
    }

    #[test]
    fn validate_type_expr_value_int_got_bool() {
        let result = validate_type_expr_value(
            &TypeExpr::Int,
            &Value::Bool(true),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected int, got bool"));
    }

    #[test]
    fn validate_type_expr_value_float_got_bool() {
        let result = validate_type_expr_value(
            &TypeExpr::Float,
            &Value::Bool(false),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected float, got bool"));
    }

    #[test]
    fn validate_type_expr_value_schema_type_accepts_string() {
        let schema_type = TypeExpr::SchemaType {
            provider: "awscc".to_string(),
            path: "ec2".to_string(),
            type_name: "VpcId".to_string(),
        };
        let result = validate_type_expr_value(
            &schema_type,
            &Value::String("vpc-12345678".to_string()),
            &ProviderContext::default(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_schema_type_rejects_bool() {
        let schema_type = TypeExpr::SchemaType {
            provider: "awscc".to_string(),
            path: "ec2".to_string(),
            type_name: "VpcId".to_string(),
        };
        let result = validate_type_expr_value(
            &schema_type,
            &Value::Bool(true),
            &ProviderContext::default(),
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected awscc.ec2.VpcId"));
    }

    #[test]
    fn validate_type_expr_value_schema_type_rejects_int() {
        let schema_type = TypeExpr::SchemaType {
            provider: "awscc".to_string(),
            path: "ec2".to_string(),
            type_name: "VpcId".to_string(),
        };
        let result =
            validate_type_expr_value(&schema_type, &Value::Int(42), &ProviderContext::default());
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected awscc.ec2.VpcId"));
    }

    #[test]
    fn discard_binding_no_warning() {
        let mut parsed = empty_parsed();

        let caller = Resource::with_provider("aws", "sts.caller_identity", "caller_identity")
            .with_binding("_");
        parsed.resources.push(caller);

        assert!(check_unused_bindings(&parsed).is_empty());
    }

    #[test]
    fn validate_type_expr_custom_type_rejects_invalid() {
        let config = context_with_iam_policy_arn_validator();

        let result = validate_type_expr_value(
            &TypeExpr::Simple("iam_policy_arn".to_string()),
            &Value::String("aaaa".to_string()),
            &config,
        );
        assert!(result.is_some(), "Expected validation error for 'aaaa'");
        assert!(result.unwrap().contains("invalid IAM policy ARN"));

        let result = validate_type_expr_value(
            &TypeExpr::Simple("iam_policy_arn".to_string()),
            &Value::String("arn:aws:iam::123456789012:policy/MyPolicy".to_string()),
            &config,
        );
        assert!(result.is_none(), "Expected no error for valid ARN");
    }

    #[test]
    fn validate_type_expr_list_custom_type_rejects_invalid() {
        let config = context_with_iam_policy_arn_validator();

        let result = validate_type_expr_value(
            &TypeExpr::List(Box::new(TypeExpr::Simple("iam_policy_arn".to_string()))),
            &Value::List(vec![Value::String("aaaa".to_string())]),
            &config,
        );
        assert!(
            result.is_some(),
            "Expected validation error for list element"
        );
        assert!(result.unwrap().contains("Element 0"));
    }

    #[test]
    fn validate_module_calls_rejects_custom_type() {
        use crate::parser::ArgumentParameter;

        let config = context_with_iam_policy_arn_validator();

        let mut args = HashMap::new();
        args.insert(
            "managed_policy_arns".to_string(),
            Value::List(vec![Value::String("aaaa".to_string())]),
        );

        let module_calls = vec![ModuleCall {
            module_name: "github".to_string(),
            binding_name: None,
            arguments: args,
        }];

        let mut imported_modules = HashMap::new();
        imported_modules.insert(
            "github".to_string(),
            vec![ArgumentParameter {
                name: "managed_policy_arns".to_string(),
                type_expr: TypeExpr::List(Box::new(TypeExpr::Simple("iam_policy_arn".to_string()))),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
        );

        let result = validate_module_calls(&module_calls, &imported_modules, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid IAM policy ARN"));
    }

    #[test]
    fn attribute_param_ref_type_mismatch_detected() {
        use crate::parser::AttributeParameter;
        use crate::schema::{AttributeSchema, ResourceSchema};

        // Build a resource with schema: role_name is String, arn is IamRoleArn (Custom)
        let role = Resource::with_provider("awscc", "iam.role", "github-role")
            .with_binding("role")
            .with_attribute("role_name", Value::String("my-role".to_string()))
            .with_attribute(
                "arn",
                Value::String("arn:aws:iam::123456789012:role/my-role".to_string()),
            );

        let mut role_schema = ResourceSchema::new("iam.role");
        role_schema =
            role_schema.attribute(AttributeSchema::new("role_name", AttributeType::String));
        role_schema = role_schema.attribute(AttributeSchema::new(
            "arn",
            AttributeType::Custom {
                name: "IamRoleArn".to_string(),
                base: Box::new(AttributeType::String),
                validate: |_| Ok(()),
                namespace: None,
                to_dsl: None,
            },
        ));

        let mut schemas = HashMap::new();
        schemas.insert("iam.role".to_string(), role_schema);

        let resources = vec![role];

        // Attribute param: role_arn: iam_role_arn = role.role_name (MISMATCH: String vs iam_role_arn)
        let params_mismatch = vec![AttributeParameter {
            name: "role_arn".to_string(),
            type_expr: Some(TypeExpr::Simple("iam_role_arn".to_string())),
            value: Some(Value::resource_ref(
                "role".to_string(),
                "role_name".to_string(),
                vec![],
            )),
        }];

        let result = validate_attribute_param_ref_types(
            &params_mismatch,
            &resources,
            &schemas,
            &|r: &Resource| r.id.resource_type.clone(),
        );
        assert!(
            result.is_err(),
            "Should reject String assigned to iam_role_arn"
        );
        let err = result.unwrap_err();
        assert!(err.contains("type mismatch"), "Error: {err}");
        assert!(err.contains("iam_role_arn"), "Error: {err}");

        // Attribute param: role_arn: iam_role_arn = role.arn (MATCH: IamRoleArn matches iam_role_arn)
        let params_match = vec![AttributeParameter {
            name: "role_arn".to_string(),
            type_expr: Some(TypeExpr::Simple("iam_role_arn".to_string())),
            value: Some(Value::resource_ref(
                "role".to_string(),
                "arn".to_string(),
                vec![],
            )),
        }];

        let result = validate_attribute_param_ref_types(
            &params_match,
            &resources,
            &schemas,
            &|r: &Resource| r.id.resource_type.clone(),
        );
        assert!(
            result.is_ok(),
            "Should accept IamRoleArn assigned to iam_role_arn"
        );
    }

    #[test]
    fn validate_export_params_rejects_invalid_custom_type() {
        use crate::parser::ExportParameter;

        let config = context_with_iam_policy_arn_validator();
        let exports = vec![ExportParameter {
            name: "policy".to_string(),
            type_expr: Some(TypeExpr::Simple("iam_policy_arn".to_string())),
            value: Some(Value::String("not-an-arn".to_string())),
        }];
        let result = validate_export_params(&exports, &config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("export 'policy'"), "err={err}");
        assert!(err.contains("invalid IAM policy ARN"), "err={err}");
    }

    #[test]
    fn validate_export_params_rejects_invalid_list_element() {
        use crate::parser::ExportParameter;

        let config = context_with_iam_policy_arn_validator();
        let exports = vec![ExportParameter {
            name: "policies".to_string(),
            type_expr: Some(TypeExpr::List(Box::new(TypeExpr::Simple(
                "iam_policy_arn".to_string(),
            )))),
            value: Some(Value::List(vec![
                Value::String("arn:aws:iam::123456789012:policy/valid".to_string()),
                Value::String("garbage".to_string()),
            ])),
        }];
        let result = validate_export_params(&exports, &config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("export 'policies'"), "err={err}");
        assert!(err.contains("Element 1"), "err={err}");
    }

    #[test]
    fn validate_export_params_accepts_valid_values() {
        use crate::parser::ExportParameter;

        let config = context_with_iam_policy_arn_validator();
        let exports = vec![ExportParameter {
            name: "policy".to_string(),
            type_expr: Some(TypeExpr::Simple("iam_policy_arn".to_string())),
            value: Some(Value::String(
                "arn:aws:iam::123456789012:policy/admin".to_string(),
            )),
        }];
        let result = validate_export_params(&exports, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_export_params_skips_no_type_annotation() {
        use crate::parser::ExportParameter;

        let config = ProviderContext::default();
        let exports = vec![ExportParameter {
            name: "raw".to_string(),
            type_expr: None,
            value: Some(Value::String("anything".to_string())),
        }];
        let result = validate_export_params(&exports, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_export_params_rejects_type_mismatch() {
        use crate::parser::ExportParameter;

        let config = ProviderContext::default();
        let exports = vec![ExportParameter {
            name: "flag".to_string(),
            type_expr: Some(TypeExpr::Bool),
            value: Some(Value::String("not-a-bool".to_string())),
        }];
        let result = validate_export_params(&exports, &config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("export 'flag'"), "err={err}");
        assert!(err.contains("expected bool"), "err={err}");
    }
}
