//! Validation utilities for resources and modules

use std::collections::HashMap;

use crate::parser::{ModuleCall, ParsedFile, TypeExpr};
use crate::provider::ProviderFactory;
use crate::resource::{Resource, Value};
use crate::schema::{AttributeType, ResourceSchema, validate_ipv4_cidr};

/// Validate resources against their schemas.
///
/// Checks that each resource's type is known, data sources use the `read` keyword,
/// and all attributes pass schema validation.
pub fn validate_resources(
    resources: &[Resource],
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    for resource in resources {
        let schema_key = schema_key_fn(resource);

        match schemas.get(&schema_key) {
            Some(schema) => {
                if schema.data_source && !resource.read_only {
                    all_errors.push(format!(
                        "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                        schema.resource_type, schema.resource_type
                    ));
                }
                if let Err(errors) = schema.validate(&resource.attributes) {
                    for error in errors {
                        all_errors.push(format!("{}: {}", resource.id, error));
                    }
                }
            }
            None => {
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
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    // Build binding_name -> resource map
    let mut binding_map: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
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

            let (ref_binding, ref_attr) = match attr_value {
                Value::ResourceRef {
                    binding_name,
                    attribute_name,
                    ..
                } => (binding_name, attribute_name),
                _ => continue,
            };

            // Get the expected type for this attribute
            let Some(attr_schema) = schema.attributes.get(attr_name) else {
                continue;
            };
            let expected_type_name = attr_schema.attr_type.type_name();

            // Look up the referenced binding's schema to get the type of the referenced attribute
            let Some(ref_resource) = binding_map.get(ref_binding.as_str()) else {
                continue; // Unknown binding, skip
            };
            let ref_schema_key_str = schema_key_fn(ref_resource);
            let Some(ref_schema) = schemas.get(&ref_schema_key_str) else {
                continue;
            };
            let Some(ref_attr_schema) = ref_schema.attributes.get(ref_attr.as_str()) else {
                continue; // Unknown attribute on referenced resource, skip
            };
            let ref_type_name = ref_attr_schema.attr_type.type_name();

            // Type compatibility check:
            // - Union type accepts any member type name -> OK
            // - Same type -> OK
            // - Either is "String" -> OK (String is the base type for Custom types)
            // - Different Custom types -> Error
            if attr_schema.attr_type.accepts_type_name(&ref_type_name) {
                continue;
            }
            if expected_type_name == "String" || ref_type_name == "String" {
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

/// Check if an AttributeType is string-compatible (can accept a string value).
pub fn is_string_compatible_type(attr_type: &AttributeType) -> bool {
    match attr_type {
        AttributeType::String | AttributeType::Custom { .. } | AttributeType::Enum(_) => true,
        AttributeType::Union { types, .. } => types.iter().all(is_string_compatible_type),
        _ => false,
    }
}

/// Validate provider configuration attributes via `ProviderFactory::validate_config()`.
pub fn validate_provider_config(
    parsed: &ParsedFile,
    factories: &[Box<dyn ProviderFactory>],
) -> Result<(), String> {
    for provider in &parsed.providers {
        if let Some(factory) = factories.iter().find(|f| f.name() == provider.name) {
            factory
                .validate_config(&provider.attributes)
                .map_err(|e| format!("provider {}: {}", provider.name, e))?;
        }
    }
    Ok(())
}

/// Validate module call arguments against module input types.
///
/// `imported_modules` maps module alias to its input parameter definitions.
pub fn validate_module_calls(
    module_calls: &[ModuleCall],
    imported_modules: &HashMap<String, Vec<crate::parser::InputParameter>>,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for call in module_calls {
        if let Some(module_inputs) = imported_modules.get(&call.module_name) {
            for (arg_name, arg_value) in &call.arguments {
                if let Some(input) = module_inputs.iter().find(|i| &i.name == arg_name)
                    && let Some(error) = validate_module_arg_type(&input.type_expr, arg_value)
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

/// Validate a module argument value against its expected type.
pub fn validate_module_arg_type(type_expr: &TypeExpr, value: &Value) -> Option<String> {
    match (type_expr, value) {
        (TypeExpr::Cidr, Value::String(s)) => validate_ipv4_cidr(s).err(),
        (TypeExpr::List(inner), Value::List(items)) => {
            if let TypeExpr::Cidr = inner.as_ref() {
                for (i, item) in items.iter().enumerate() {
                    if let Value::String(s) = item {
                        if let Err(e) = validate_ipv4_cidr(s) {
                            return Some(format!("element {}: {}", i, e));
                        }
                    } else {
                        return Some(format!("element {}: expected string", i));
                    }
                }
            }
            None
        }
        (TypeExpr::Bool, Value::String(s)) => Some(format!(
            "expected bool, got string \"{}\". Use true or false.",
            s
        )),
        (TypeExpr::Int, Value::String(s)) => Some(format!("expected int, got string \"{}\".", s)),
        _ => None,
    }
}
