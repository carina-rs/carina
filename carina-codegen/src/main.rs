//! CloudFormation Schema to Carina Schema Code Generator
//!
//! This tool generates Rust schema code for carina-provider-awscc
//! from AWS CloudFormation resource type schemas.
//!
//! Usage:
//!   # Generate from stdin (pipe from aws cli)
//!   aws-vault exec <profile> -- aws cloudformation describe-type \
//!     --type RESOURCE --type-name AWS::EC2::VPC --query 'Schema' --output text | \
//!     carina-codegen --type-name AWS::EC2::VPC
//!
//!   # Generate from file
//!   carina-codegen --file schema.json --type-name AWS::EC2::VPC

use anyhow::{Context, Result};
use clap::Parser;
use heck::{ToPascalCase, ToSnakeCase};
use regex::Regex;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};

/// Information about a detected enum type
#[derive(Debug, Clone)]
struct EnumInfo {
    /// Property name in PascalCase (e.g., "InstanceTenancy")
    type_name: String,
    /// Valid enum values (e.g., ["default", "dedicated", "host"])
    values: Vec<String>,
}

#[derive(Parser, Debug)]
#[command(name = "carina-codegen")]
#[command(about = "Generate Carina schema code from CloudFormation schemas")]
struct Args {
    /// CloudFormation type name (e.g., AWS::EC2::VPC)
    #[arg(long)]
    type_name: String,

    /// Input file (reads from stdin if not specified)
    #[arg(long)]
    file: Option<String>,

    /// Output file (writes to stdout if not specified)
    #[arg(long, short)]
    output: Option<String>,
}

/// CloudFormation Resource Schema
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CfnSchema {
    type_name: String,
    description: Option<String>,
    properties: HashMap<String, CfnProperty>,
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    read_only_properties: Vec<String>,
    #[serde(default)]
    create_only_properties: Vec<String>,
    #[serde(default)]
    write_only_properties: Vec<String>,
    primary_identifier: Option<Vec<String>>,
    definitions: Option<HashMap<String, CfnDefinition>>,
}

/// Type can be a string or an array of strings in JSON Schema
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TypeValue {
    Single(String),
    Multiple(Vec<String>),
}

impl TypeValue {
    fn as_str(&self) -> Option<&str> {
        match self {
            TypeValue::Single(s) => Some(s),
            TypeValue::Multiple(v) => v.first().map(|s| s.as_str()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CfnProperty {
    #[serde(rename = "type")]
    prop_type: Option<TypeValue>,
    description: Option<String>,
    #[serde(rename = "enum")]
    enum_values: Option<Vec<String>>,
    items: Option<Box<CfnProperty>>,
    #[serde(rename = "$ref")]
    ref_path: Option<String>,
    #[serde(default)]
    insertion_order: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CfnDefinition {
    #[serde(rename = "type")]
    def_type: Option<String>,
    properties: Option<HashMap<String, CfnProperty>>,
    #[serde(default)]
    required: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Read schema JSON
    let schema_json = if let Some(file_path) = &args.file {
        std::fs::read_to_string(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path))?
    } else {
        let mut buffer = String::new();
        io::stdin()
            .read_to_string(&mut buffer)
            .context("Failed to read from stdin")?;
        buffer
    };

    // Parse schema
    let schema: CfnSchema =
        serde_json::from_str(&schema_json).context("Failed to parse CloudFormation schema")?;

    // Generate code
    let code = generate_schema_code(&schema, &args.type_name)?;

    // Output
    if let Some(output_path) = &args.output {
        std::fs::write(output_path, &code)
            .with_context(|| format!("Failed to write to: {}", output_path))?;
        eprintln!("Generated: {}", output_path);
    } else {
        println!("{}", code);
    }

    Ok(())
}

fn generate_schema_code(schema: &CfnSchema, type_name: &str) -> Result<String> {
    let mut code = String::new();

    // Parse type name: AWS::EC2::VPC -> (ec2, vpc)
    let parts: Vec<&str> = type_name.split("::").collect();
    if parts.len() != 3 {
        anyhow::bail!("Invalid type name format: {}", type_name);
    }
    let service = parts[1].to_lowercase();
    let resource = parts[2].to_snake_case();
    // Combined format: ec2_vpc (service + underscore + resource)
    let full_resource = format!("{}_{}", service, resource);
    // Namespace for enums: awscc.ec2_vpc
    let namespace = format!("awscc.{}", full_resource);

    // Build read-only properties set
    let read_only: HashSet<String> = schema
        .read_only_properties
        .iter()
        .map(|p| p.trim_start_matches("/properties/").to_string())
        .collect();

    let required: HashSet<String> = schema.required.iter().cloned().collect();

    // Pre-scan properties to determine which imports are needed and collect enum info
    let mut needs_types = false;
    let mut needs_tags_type = false;
    let mut enums: HashMap<String, EnumInfo> = HashMap::new();

    for (prop_name, prop) in &schema.properties {
        let (attr_type, enum_info) = cfn_type_to_carina_type_with_enum(prop, prop_name, schema);
        if attr_type.contains("types::") {
            needs_types = true;
        }
        if attr_type.contains("tags_type()") {
            needs_tags_type = true;
        }
        if let Some(info) = enum_info {
            enums.insert(prop_name.clone(), info);
        }
    }

    let has_enums = !enums.is_empty();

    // Generate header with conditional imports
    let types_import = if needs_types { ", types" } else { "" };
    code.push_str(&format!(
        r#"//! {} schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: {}
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use carina_core::schema::{{AttributeSchema, AttributeType, ResourceSchema{}}};
"#,
        resource, type_name, types_import
    ));

    if has_enums {
        code.push_str("use carina_core::resource::Value;\n");
    }
    if needs_tags_type {
        code.push_str("use super::tags_type;\n");
    }
    if has_enums {
        code.push_str("use super::validate_namespaced_enum;\n");
    }
    code.push('\n');

    // Generate enum constants and validation functions
    for (prop_name, enum_info) in &enums {
        let const_name = format!("VALID_{}", prop_name.to_snake_case().to_uppercase());
        let fn_name = format!("validate_{}", prop_name.to_snake_case());

        // Generate constant
        let values_str = enum_info
            .values
            .iter()
            .map(|v| format!("\"{}\"", v))
            .collect::<Vec<_>>()
            .join(", ");
        code.push_str(&format!(
            "const {}: &[&str] = &[{}];\n\n",
            const_name, values_str
        ));

        // Generate validation function
        code.push_str(&format!(
            r#"fn {}(value: &Value) -> Result<(), String> {{
    validate_namespaced_enum(value, "{}", "{}", {})
}}

"#,
            fn_name, enum_info.type_name, namespace, const_name
        ));
    }

    // Generate schema function
    let fn_name = format!("{}_schema", full_resource);
    // Use awscc.service_resource format (e.g., awscc.ec2_vpc)
    let schema_name = format!("awscc.{}", full_resource);

    code.push_str(&format!(
        r#"/// Returns the schema for {} ({})
pub fn {}() -> ResourceSchema {{
    ResourceSchema::new("{}")
"#,
        full_resource, type_name, fn_name, schema_name
    ));

    // Add description
    if let Some(desc) = &schema.description {
        let escaped_desc = desc.replace('"', "\\\"").replace('\n', " ");
        let truncated = if escaped_desc.len() > 200 {
            format!("{}...", &escaped_desc[..200])
        } else {
            escaped_desc
        };
        code.push_str(&format!("        .with_description(\"{}\")\n", truncated));
    }

    // Generate attributes for each property
    for (prop_name, prop) in &schema.properties {
        let attr_name = prop_name.to_snake_case();
        let is_required = required.contains(prop_name) && !read_only.contains(prop_name);
        let is_read_only = read_only.contains(prop_name);

        let attr_type = if let Some(enum_info) = enums.get(prop_name) {
            // Use AttributeType::Custom for enums
            let validate_fn = format!("validate_{}", prop_name.to_snake_case());
            format!(
                r#"AttributeType::Custom {{
                name: "{}".to_string(),
                base: Box::new(AttributeType::String),
                validate: {},
                namespace: Some("{}".to_string()),
            }}"#,
                enum_info.type_name, validate_fn, namespace
            )
        } else {
            let (attr_type, _) = cfn_type_to_carina_type_with_enum(prop, prop_name, schema);
            attr_type
        };

        let mut attr_code = format!(
            "        .attribute(\n            AttributeSchema::new(\"{}\", {})",
            attr_name, attr_type
        );

        if is_required {
            attr_code.push_str("\n                .required()");
        }

        if let Some(desc) = &prop.description {
            let escaped = desc
                .replace('"', "\\\"")
                .replace('\n', " ")
                .replace("  ", " ");
            let truncated = if escaped.len() > 150 {
                format!("{}...", &escaped[..150])
            } else {
                escaped
            };
            let suffix = if is_read_only { " (read-only)" } else { "" };
            attr_code.push_str(&format!(
                "\n                .with_description(\"{}{}\")",
                truncated, suffix
            ));
        } else if is_read_only {
            attr_code.push_str("\n                .with_description(\"(read-only)\")");
        }

        attr_code.push_str(",\n        )\n");
        code.push_str(&attr_code);
    }

    code.push_str("}\n");

    Ok(code)
}

/// Check if a string looks like a property name (CamelCase or PascalCase)
/// rather than an enum value (lowercase, kebab-case, or UPPER_CASE)
fn looks_like_property_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Property names typically start with uppercase and contain mixed case
    // e.g., "InstanceTenancy", "VpcId"
    let first_char = s.chars().next().unwrap();
    if first_char.is_uppercase() {
        // Check if it has lowercase letters too (CamelCase)
        let has_lowercase = s.chars().any(|c| c.is_lowercase());
        return has_lowercase;
    }
    false
}

/// Extract enum values from description text.
/// Looks for patterns like ``value`` (double backticks) which CloudFormation uses
/// to indicate allowed values in descriptions.
fn extract_enum_from_description(description: &str) -> Option<Vec<String>> {
    let re = Regex::new(r"``([^`]+)``").ok()?;
    let values: Vec<String> = re
        .captures_iter(description)
        .map(|cap| cap[1].to_string())
        // Filter out property names (CamelCase) as they're not enum values
        .filter(|v| !looks_like_property_name(v))
        .collect();

    // Only return if we have at least 2 distinct values (indicating an enum)
    if values.len() >= 2 {
        // Deduplicate while preserving order
        let mut seen = HashSet::new();
        let unique: Vec<String> = values
            .into_iter()
            .filter(|v| seen.insert(v.clone()))
            .collect();
        if unique.len() >= 2 {
            return Some(unique);
        }
    }
    None
}

/// Returns (type_string, Option<EnumInfo>)
/// EnumInfo is Some if this property is an enum that should use AttributeType::Custom
fn cfn_type_to_carina_type_with_enum(
    prop: &CfnProperty,
    prop_name: &str,
    _schema: &CfnSchema,
) -> (String, Option<EnumInfo>) {
    // Tags property is special - it's a Map in Carina (Terraform-style)
    if prop_name == "Tags" {
        return ("tags_type()".to_string(), None);
    }

    // Handle $ref
    if let Some(ref_path) = &prop.ref_path {
        if ref_path.contains("/Tag") {
            return ("tags_type()".to_string(), None);
        }
        // Default to String for unknown refs
        return ("AttributeType::String".to_string(), None);
    }

    // Handle explicit enum
    if let Some(enum_values) = &prop.enum_values {
        let type_name = prop_name.to_pascal_case();
        let enum_info = EnumInfo {
            type_name,
            values: enum_values.clone(),
        };
        // Return placeholder - actual type will be generated using enum_info
        return ("/* enum */".to_string(), Some(enum_info));
    }

    // Handle type
    match prop.prop_type.as_ref().and_then(|t| t.as_str()) {
        Some("string") => {
            // Check property name for specific types
            let prop_lower = prop_name.to_lowercase();

            // CIDR type - only for properties that are actually CIDRs
            if prop_lower.contains("cidrblock") || prop_lower == "cidr_block" {
                return ("types::cidr()".to_string(), None);
            }

            // IDs are always strings
            if prop_lower.ends_with("id") || prop_lower.ends_with("_id") {
                return ("AttributeType::String".to_string(), None);
            }

            // ARNs are always strings
            if prop_lower.ends_with("arn") || prop_lower.contains("_arn") {
                return ("AttributeType::String".to_string(), None);
            }

            // Zone/Region are strings
            if prop_lower.contains("zone") || prop_lower.contains("region") {
                return ("AttributeType::String".to_string(), None);
            }

            // Try to extract enum values from description
            if let Some(desc) = &prop.description {
                if let Some(enum_values) = extract_enum_from_description(desc) {
                    let type_name = prop_name.to_pascal_case();
                    let enum_info = EnumInfo {
                        type_name,
                        values: enum_values,
                    };
                    // Return placeholder - actual type will be generated using enum_info
                    return ("/* enum */".to_string(), Some(enum_info));
                }
            }

            ("AttributeType::String".to_string(), None)
        }
        Some("boolean") => ("AttributeType::Bool".to_string(), None),
        Some("integer") => ("AttributeType::Int".to_string(), None),
        Some("number") => ("AttributeType::Int".to_string(), None),
        Some("array") => {
            if let Some(items) = &prop.items {
                let (item_type, _) = cfn_type_to_carina_type_with_enum(items, prop_name, _schema);
                (
                    format!("AttributeType::List(Box::new({}))", item_type),
                    None,
                )
            } else {
                (
                    "AttributeType::List(Box::new(AttributeType::String))".to_string(),
                    None,
                )
            }
        }
        Some("object") => (
            "AttributeType::Map(Box::new(AttributeType::String))".to_string(),
            None,
        ),
        _ => ("AttributeType::String".to_string(), None),
    }
}

/// Tags type helper (to be included in generated module)
#[allow(dead_code)]
fn tags_type_helper() -> &'static str {
    r#"
/// Tags type for AWS resources
pub fn tags_type() -> AttributeType {
    AttributeType::Map(Box::new(AttributeType::String))
}
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_looks_like_property_name() {
        // CamelCase property names should be detected
        assert!(looks_like_property_name("InstanceTenancy"));
        assert!(looks_like_property_name("VpcId"));
        assert!(looks_like_property_name("CidrBlock"));

        // Enum values should not be detected as property names
        assert!(!looks_like_property_name("default"));
        assert!(!looks_like_property_name("dedicated"));
        assert!(!looks_like_property_name("host"));

        // Edge cases
        assert!(!looks_like_property_name(""));
        assert!(!looks_like_property_name("UPPERCASE")); // All uppercase, no lowercase
    }

    #[test]
    fn test_extract_enum_from_description_instance_tenancy() {
        let description = r#"The allowed tenancy of instances launched into the VPC.
  +  ``default``: An instance launched into the VPC runs on shared hardware by default.
  +  ``dedicated``: An instance launched into the VPC runs on dedicated hardware by default.
  +  ``host``: Some description.
 Updating ``InstanceTenancy`` requires no replacement."#;

        let result = extract_enum_from_description(description);
        assert!(result.is_some());
        let values = result.unwrap();
        assert_eq!(values, vec!["default", "dedicated", "host"]);
    }

    #[test]
    fn test_extract_enum_from_description_single_value() {
        // Only one value should not be treated as enum
        let description = "Set to ``true`` to enable.";
        let result = extract_enum_from_description(description);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_enum_from_description_no_backticks() {
        let description = "A regular description without any special formatting.";
        let result = extract_enum_from_description(description);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_enum_from_description_deduplication() {
        // Same value mentioned multiple times should be deduplicated
        let description =
            r#"Use ``enabled`` or ``disabled``. When ``enabled`` is set, the feature activates."#;
        let result = extract_enum_from_description(description);
        assert!(result.is_some());
        let values = result.unwrap();
        assert_eq!(values, vec!["enabled", "disabled"]);
    }
}
