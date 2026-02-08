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
use std::collections::{BTreeMap, HashSet};
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

    /// Print module name for the given type and exit
    /// e.g., AWS::EC2::SecurityGroupEgress -> security_group_egress
    #[arg(long)]
    print_module_name: bool,

    /// Print full resource name (service_resource) for the given type and exit
    /// e.g., AWS::EC2::SecurityGroupEgress -> ec2_security_group_egress
    #[arg(long)]
    print_full_resource_name: bool,

    /// Output format: rust (default) or markdown (for documentation)
    #[arg(long, default_value = "rust")]
    format: String,
}

/// CloudFormation Resource Schema
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CfnSchema {
    type_name: String,
    description: Option<String>,
    properties: BTreeMap<String, CfnProperty>,
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    read_only_properties: Vec<String>,
    #[serde(default)]
    create_only_properties: Vec<String>,
    #[serde(default)]
    write_only_properties: Vec<String>,
    primary_identifier: Option<Vec<String>>,
    definitions: Option<BTreeMap<String, CfnDefinition>>,
    tagging: Option<CfnTagging>,
}

/// CloudFormation Tagging metadata
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CfnTagging {
    #[serde(default)]
    taggable: bool,
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
    properties: Option<BTreeMap<String, CfnProperty>>,
    #[serde(default)]
    required: Vec<String>,
}

/// Compute module name from CloudFormation type name
/// e.g., "AWS::EC2::SecurityGroupEgress" -> "security_group_egress"
fn module_name_from_type(type_name: &str) -> Result<String> {
    let parts: Vec<&str> = type_name.split("::").collect();
    if parts.len() != 3 {
        anyhow::bail!("Invalid type name format: {}", type_name);
    }
    Ok(parts[2].to_snake_case())
}

/// Compute full resource name (service_resource) from CloudFormation type name
/// e.g., "AWS::EC2::SecurityGroupEgress" -> "ec2_security_group_egress"
fn full_resource_name_from_type(type_name: &str) -> Result<String> {
    let parts: Vec<&str> = type_name.split("::").collect();
    if parts.len() != 3 {
        anyhow::bail!("Invalid type name format: {}", type_name);
    }
    let service = parts[1].to_lowercase();
    let resource = parts[2].to_snake_case();
    Ok(format!("{}_{}", service, resource))
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.print_module_name {
        println!("{}", module_name_from_type(&args.type_name)?);
        return Ok(());
    }

    if args.print_full_resource_name {
        println!("{}", full_resource_name_from_type(&args.type_name)?);
        return Ok(());
    }

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

    // Generate output based on format
    let output = match args.format.as_str() {
        "markdown" | "md" => generate_markdown(&schema, &args.type_name)?,
        "rust" => generate_schema_code(&schema, &args.type_name)?,
        other => anyhow::bail!("Unknown format: {}. Use 'rust' or 'markdown'.", other),
    };

    // Output
    if let Some(output_path) = &args.output {
        std::fs::write(output_path, &output)
            .with_context(|| format!("Failed to write to: {}", output_path))?;
        eprintln!("Generated: {}", output_path);
    } else {
        println!("{}", output);
    }

    Ok(())
}

fn generate_markdown(schema: &CfnSchema, type_name: &str) -> Result<String> {
    let mut md = String::new();

    let full_resource = full_resource_name_from_type(type_name)?;
    let namespace = format!("awscc.{}", full_resource);

    // Build read-only properties set
    let read_only: HashSet<String> = schema
        .read_only_properties
        .iter()
        .map(|p| p.trim_start_matches("/properties/").to_string())
        .collect();

    let required: HashSet<String> = schema.required.iter().cloned().collect();

    // Collect enum info
    let mut enums: BTreeMap<String, EnumInfo> = BTreeMap::new();
    for (prop_name, prop) in &schema.properties {
        let (_, enum_info) = cfn_type_to_carina_type_with_enum(prop, prop_name, schema);
        if let Some(info) = enum_info {
            enums.insert(prop_name.clone(), info);
        }
    }

    // Title
    md.push_str(&format!("# awscc.{}\n\n", full_resource));
    md.push_str(&format!("CloudFormation Type: `{}`\n\n", type_name));

    // Description
    if let Some(desc) = &schema.description {
        md.push_str(&format!("{}\n\n", desc));
    }

    // Attributes table
    md.push_str("## Attributes\n\n");
    md.push_str("| Name | Type | Required | Description |\n");
    md.push_str("|------|------|----------|-------------|\n");

    for (prop_name, prop) in &schema.properties {
        let attr_name = prop_name.to_snake_case();
        let is_required = required.contains(prop_name) && !read_only.contains(prop_name);
        let is_read_only = read_only.contains(prop_name);

        // Determine type display string
        let type_display = if enums.contains_key(prop_name) {
            format!("Enum ({})", enums[prop_name].type_name)
        } else if prop_name == "Tags" {
            "Map".to_string()
        } else if let Some(ref_path) = &prop.ref_path {
            if ref_path.contains("/Tag") {
                "Map".to_string()
            } else {
                "String".to_string()
            }
        } else {
            match prop.prop_type.as_ref().and_then(|t| t.as_str()) {
                Some("string") => {
                    let prop_lower = prop_name.to_lowercase();
                    if prop_lower.contains("cidrblock") || prop_lower == "cidr_block" {
                        "CIDR".to_string()
                    } else {
                        "String".to_string()
                    }
                }
                Some("boolean") => "Bool".to_string(),
                Some("integer") | Some("number") => "Int".to_string(),
                Some("array") => "List".to_string(),
                Some("object") => "Map".to_string(),
                _ => "String".to_string(),
            }
        };

        let desc = if is_read_only {
            "(read-only)".to_string()
        } else if let Some(d) = &prop.description {
            d.replace('\n', " ").replace("  ", " ").replace('|', "\\|")
        } else {
            String::new()
        };

        let req_str = if is_read_only {
            ""
        } else if is_required {
            "Yes"
        } else {
            "No"
        };

        md.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            attr_name, type_display, req_str, desc
        ));
    }
    md.push('\n');

    // Enum values section
    if !enums.is_empty() {
        md.push_str("## Enum Values\n\n");
        for (prop_name, enum_info) in &enums {
            let attr_name = prop_name.to_snake_case();
            md.push_str(&format!("### {} ({})\n\n", attr_name, enum_info.type_name));
            md.push_str("| Value | DSL Identifier |\n");
            md.push_str("|-------|----------------|\n");
            for value in &enum_info.values {
                let dsl_id = format!("{}.{}.{}", namespace, enum_info.type_name, value);
                md.push_str(&format!("| `{}` | `{}` |\n", value, dsl_id));
            }
            md.push('\n');
            md.push_str(&format!(
                "Shorthand formats: `{}` or `{}.{}`\n\n",
                enum_info.values.first().unwrap_or(&String::new()),
                enum_info.type_name,
                enum_info.values.first().unwrap_or(&String::new()),
            ));
        }
    }

    Ok(md)
}

fn generate_schema_code(schema: &CfnSchema, type_name: &str) -> Result<String> {
    let mut code = String::new();

    // Parse type name: AWS::EC2::VPC -> (ec2, vpc)
    let parts: Vec<&str> = type_name.split("::").collect();
    if parts.len() != 3 {
        anyhow::bail!("Invalid type name format: {}", type_name);
    }
    let resource = parts[2].to_snake_case();
    let full_resource = full_resource_name_from_type(type_name)?;
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
    let mut enums: BTreeMap<String, EnumInfo> = BTreeMap::new();

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

    // Determine has_tags from tagging metadata
    let has_tags = schema.tagging.as_ref().map(|t| t.taggable).unwrap_or(false);

    // Generate header with conditional imports
    let types_import = if needs_types { ", types" } else { "" };
    code.push_str(&format!(
        r#"//! {} schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: {}
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use carina_core::schema::{{AttributeSchema, AttributeType, ResourceSchema{}}};
use super::AwsccSchemaConfig;
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

    // Generate config function
    let config_fn_name = format!("{}_config", full_resource);
    // Use awscc.service_resource format (e.g., awscc.ec2_vpc)
    let schema_name = format!("awscc.{}", full_resource);

    code.push_str(&format!(
        r#"/// Returns the schema config for {} ({})
pub fn {}() -> AwsccSchemaConfig {{
    AwsccSchemaConfig {{
        aws_type_name: "{}",
        resource_type_name: "{}",
        has_tags: {},
        schema: ResourceSchema::new("{}")
"#,
        full_resource, type_name, config_fn_name, type_name, full_resource, has_tags, schema_name
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

        // Add provider_name mapping (AWS property name)
        attr_code.push_str(&format!(
            "\n                .with_provider_name(\"{}\")",
            prop_name
        ));

        attr_code.push_str(",\n        )\n");
        code.push_str(&attr_code);
    }

    // Close the schema (ResourceSchema) and the AwsccSchemaConfig struct
    code.push_str("    }\n}\n");

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
            if let Some(desc) = &prop.description
                && let Some(enum_values) = extract_enum_from_description(desc)
            {
                let type_name = prop_name.to_pascal_case();
                let enum_info = EnumInfo {
                    type_name,
                    values: enum_values,
                };
                // Return placeholder - actual type will be generated using enum_info
                return ("/* enum */".to_string(), Some(enum_info));
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
