//! Validation utilities for resources and modules

use std::collections::{HashMap, HashSet};

use crate::parser::{ModuleCall, ParsedFile, TypeExpr};
use crate::provider::ProviderFactory;
use crate::resource::{Resource, Value};
use crate::schema::{
    AttributeType, ResourceSchema, suggest_similar_name, validate_ipv4_address, validate_ipv4_cidr,
    validate_ipv6_address, validate_ipv6_cidr,
};

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
        // Skip virtual resources (module attribute containers)
        if resource.is_virtual() {
            continue;
        }

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
    argument_names: &HashSet<String>,
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
                let suggestion = suggest_similar_name(ref_attr, &known_attrs)
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

/// Validate module call arguments against module argument types.
///
/// `imported_modules` maps module alias to its argument parameter definitions.
pub fn validate_module_calls(
    module_calls: &[ModuleCall],
    imported_modules: &HashMap<String, Vec<crate::parser::ArgumentParameter>>,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for call in module_calls {
        if let Some(module_args) = imported_modules.get(&call.module_name) {
            for (arg_name, arg_value) in &call.arguments {
                if let Some(arg_param) = module_args.iter().find(|a| &a.name == arg_name)
                    && let Some(error) = validate_type_expr_value(&arg_param.type_expr, arg_value)
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

/// Check for unused `let` bindings and return the unused binding names.
///
/// A binding is unused if its name never appears as a `ResourceRef.binding_name`
/// in any resource attribute, module call argument, or attribute parameter value.
pub fn check_unused_bindings(parsed: &ParsedFile) -> Vec<String> {
    // Collect all defined binding names
    let mut defined_bindings: Vec<String> = Vec::new();
    for resource in &parsed.resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
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

    // Return unused binding names
    defined_bindings
        .into_iter()
        .filter(|binding| !referenced.contains(binding))
        .collect()
}

/// Recursively collect all `ResourceRef` binding names from a value tree.
fn collect_resource_refs(value: &Value, refs: &mut HashSet<String>) {
    match value {
        Value::ResourceRef { binding_name, .. } => {
            refs.insert(binding_name.clone());
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

/// Validate a value against a TypeExpr, returning an error message if invalid.
///
/// Shared validation logic used by both CLI module call validation and LSP diagnostics.
pub fn validate_type_expr_value(type_expr: &TypeExpr, value: &Value) -> Option<String> {
    match (type_expr, value) {
        (TypeExpr::Simple(name), Value::String(s)) => {
            simple_type_validator(name).and_then(|validate_fn| validate_fn(s).err())
        }
        (TypeExpr::List(inner), Value::List(items)) => {
            if let TypeExpr::Simple(name) = inner.as_ref()
                && let Some(validate_fn) = simple_type_validator(name)
            {
                for (i, item) in items.iter().enumerate() {
                    if let Value::String(s) = item {
                        if let Err(e) = validate_fn(s) {
                            return Some(format!("Element {}: {}", i, e));
                        }
                    } else {
                        return Some(format!("Element {}: expected string, got {:?}", i, item));
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
        (TypeExpr::Float, Value::String(s)) => {
            Some(format!("expected float, got string \"{}\".", s))
        }
        (TypeExpr::String, Value::Bool(b)) => Some(format!("expected string, got bool ({}).", b)),
        (TypeExpr::String, Value::Int(n)) => Some(format!("expected string, got int ({}).", n)),
        (TypeExpr::String, Value::Float(f)) => Some(format!("expected string, got float ({}).", f)),
        (TypeExpr::Bool, Value::Int(n)) => Some(format!("expected bool, got int ({}).", n)),
        (TypeExpr::Int, Value::Bool(b)) => Some(format!("expected int, got bool ({}).", b)),
        (TypeExpr::Float, Value::Bool(b)) => Some(format!("expected float, got bool ({}).", b)),
        _ => None,
    }
}

type ValidateFn = fn(&str) -> Result<(), String>;

/// Return the validator function for a custom simple type name, if any.
fn simple_type_validator(name: &str) -> Option<ValidateFn> {
    match name {
        "cidr" => Some(validate_ipv4_cidr),
        "ipv4_address" => Some(validate_ipv4_address),
        "ipv6_cidr" => Some(validate_ipv6_cidr),
        "ipv6_address" => Some(validate_ipv6_address),
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
            backend: None,
            state_blocks: Vec::new(),
            user_functions: HashMap::new(),
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
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));
        parsed.resources.push(vpc);

        // Resource that references the binding
        let subnet = Resource::with_provider("awscc", "ec2.subnet", "web-subnet").with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
                field_path: vec![],
            },
        );
        parsed.resources.push(subnet);

        assert!(check_unused_bindings(&parsed).is_empty());
    }

    #[test]
    fn unused_binding_warns() {
        let mut parsed = empty_parsed();

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
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

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()));
        parsed.resources.push(vpc);

        // Reference inside a Map inside a List
        let mut map = HashMap::new();
        map.insert(
            "vpc_id".to_string(),
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
                field_path: vec![],
            },
        );
        let sg = Resource::with_provider("awscc", "ec2.security_group", "web-sg")
            .with_attribute("tags", Value::List(vec![Value::Map(map)]));
        parsed.resources.push(sg);

        assert!(check_unused_bindings(&parsed).is_empty());
    }

    #[test]
    fn binding_referenced_in_module_call() {
        let mut parsed = empty_parsed();

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()));
        parsed.resources.push(vpc);

        let mut args = HashMap::new();
        args.insert(
            "vpc_id".to_string(),
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
                field_path: vec![],
            },
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

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()));
        parsed.resources.push(vpc);

        let sg = Resource::with_provider("awscc", "ec2.security_group", "web-sg")
            .with_attribute("_binding", Value::String("web_sg".to_string()));
        parsed.resources.push(sg);

        // Only vpc is referenced
        let subnet = Resource::with_provider("awscc", "ec2.subnet", "web-subnet").with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
                field_path: vec![],
            },
        );
        parsed.resources.push(subnet);

        let unused = check_unused_bindings(&parsed);
        assert_eq!(unused, vec!["web_sg"]);
    }

    #[test]
    fn binding_referenced_in_attributes_not_warned() {
        let mut parsed = empty_parsed();

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()));
        parsed.resources.push(vpc);

        parsed
            .attribute_params
            .push(crate::parser::AttributeParameter {
                name: "vpc_id".to_string(),
                type_expr: Some(TypeExpr::String),
                value: Some(Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                    field_path: vec![],
                }),
            });

        assert!(check_unused_bindings(&parsed).is_empty());
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
        let gateway_id = route.attributes.get("gateway_id").unwrap();
        match gateway_id {
            Value::ResourceRef {
                binding_name,
                attribute_name,
                ..
            } => {
                assert_eq!(binding_name, "igw_attachment");
                assert_eq!(attribute_name, "internet_gateway_id");
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
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
                field_path: vec![],
            },
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
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));

        // Subnet references vpc.nonexistent_attr which doesn't exist on the VPC schema
        let subnet = Resource::with_provider("awscc", "ec2.subnet", "web-subnet").with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "nonexistent_attr".to_string(),
                field_path: vec![],
            },
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

        let igw = Resource::with_provider("awscc", "ec2.internet_gateway", "igw")
            .with_attribute("_binding", Value::String("igw".to_string()));

        // Typo: internet_gateway_idd instead of internet_gateway_id
        let route = Resource::with_provider("awscc", "ec2.route", "main-route").with_attribute(
            "gateway_id",
            Value::ResourceRef {
                binding_name: "igw".to_string(),
                attribute_name: "internet_gateway_idd".to_string(),
                field_path: vec![],
            },
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

        let vpc = Resource::with_provider("awscc", "ec2.vpc", "main-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()));

        // Completely unrelated attribute name - no suggestion expected
        let subnet = Resource::with_provider("awscc", "ec2.subnet", "web-subnet").with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "completely_wrong_name".to_string(),
                field_path: vec![],
            },
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
        });
        parsed.arguments.push(crate::parser::ArgumentParameter {
            name: "vpc_cidr".to_string(),
            type_expr: TypeExpr::String,
            default: None,
            description: None,
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
        });

        let result = validate_no_provider_in_module(&parsed);
        assert!(result.is_ok());
    }

    // --- validate_type_expr_value tests ---

    #[test]
    fn validate_type_expr_value_cidr_valid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("cidr".to_string()),
            &Value::String("10.0.0.0/16".to_string()),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_cidr_invalid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("cidr".to_string()),
            &Value::String("not-a-cidr".to_string()),
        );
        assert!(result.is_some());
    }

    #[test]
    fn validate_type_expr_value_ipv4_address_valid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv4_address".to_string()),
            &Value::String("192.168.1.1".to_string()),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_ipv4_address_invalid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv4_address".to_string()),
            &Value::String("999.999.999.999".to_string()),
        );
        assert!(result.is_some());
    }

    #[test]
    fn validate_type_expr_value_ipv6_cidr_valid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv6_cidr".to_string()),
            &Value::String("2001:db8::/32".to_string()),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_ipv6_cidr_invalid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv6_cidr".to_string()),
            &Value::String("not-ipv6-cidr".to_string()),
        );
        assert!(result.is_some());
    }

    #[test]
    fn validate_type_expr_value_ipv6_address_valid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv6_address".to_string()),
            &Value::String("2001:db8::1".to_string()),
        );
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_ipv6_address_invalid() {
        let result = validate_type_expr_value(
            &TypeExpr::Simple("ipv6_address".to_string()),
            &Value::String("zzz::zzz".to_string()),
        );
        assert!(result.is_some());
    }

    #[test]
    fn validate_type_expr_value_bool_mismatch() {
        let result = validate_type_expr_value(&TypeExpr::Bool, &Value::String("yes".to_string()));
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected bool"));
    }

    #[test]
    fn validate_type_expr_value_int_mismatch() {
        let result = validate_type_expr_value(&TypeExpr::Int, &Value::String("42".to_string()));
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected int"));
    }

    #[test]
    fn validate_type_expr_value_float_mismatch() {
        let result = validate_type_expr_value(&TypeExpr::Float, &Value::String("3.14".to_string()));
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
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("Element 1"));
    }

    #[test]
    fn validate_type_expr_value_string_type_accepts_string() {
        let result =
            validate_type_expr_value(&TypeExpr::String, &Value::String("hello".to_string()));
        assert!(result.is_none());
    }

    #[test]
    fn validate_type_expr_value_string_got_bool() {
        let result = validate_type_expr_value(&TypeExpr::String, &Value::Bool(true));
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected string, got bool"));
    }

    #[test]
    fn validate_type_expr_value_string_got_int() {
        let result = validate_type_expr_value(&TypeExpr::String, &Value::Int(42));
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected string, got int"));
    }

    #[test]
    fn validate_type_expr_value_string_got_float() {
        let result = validate_type_expr_value(&TypeExpr::String, &Value::Float(1.5));
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected string, got float"));
    }

    #[test]
    fn validate_type_expr_value_bool_got_int() {
        let result = validate_type_expr_value(&TypeExpr::Bool, &Value::Int(1));
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected bool, got int"));
    }

    #[test]
    fn validate_type_expr_value_int_got_bool() {
        let result = validate_type_expr_value(&TypeExpr::Int, &Value::Bool(true));
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected int, got bool"));
    }

    #[test]
    fn validate_type_expr_value_float_got_bool() {
        let result = validate_type_expr_value(&TypeExpr::Float, &Value::Bool(false));
        assert!(result.is_some());
        assert!(result.unwrap().contains("expected float, got bool"));
    }
}
