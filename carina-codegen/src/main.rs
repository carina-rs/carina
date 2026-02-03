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
use heck::ToSnakeCase;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};

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

    // Build read-only properties set
    let read_only: HashSet<String> = schema
        .read_only_properties
        .iter()
        .map(|p| p.trim_start_matches("/properties/").to_string())
        .collect();

    let required: HashSet<String> = schema.required.iter().cloned().collect();

    // Pre-scan properties to determine which imports are needed
    let mut needs_types = false;
    let mut needs_tags_type = false;
    for (prop_name, prop) in &schema.properties {
        let attr_type = cfn_type_to_carina_type(prop, prop_name, schema);
        if attr_type.contains("types::") {
            needs_types = true;
        }
        if attr_type.contains("tags_type()") {
            needs_tags_type = true;
        }
    }

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

    if needs_tags_type {
        code.push_str("use super::tags_type;\n");
    }
    code.push('\n');

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

        let attr_type = cfn_type_to_carina_type(prop, prop_name, schema);

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

fn cfn_type_to_carina_type(prop: &CfnProperty, prop_name: &str, _schema: &CfnSchema) -> String {
    // Tags property is special - it's a Map in Carina (Terraform-style)
    if prop_name == "Tags" {
        return "tags_type()".to_string();
    }

    // Handle $ref
    if let Some(ref_path) = &prop.ref_path {
        if ref_path.contains("/Tag") {
            return "tags_type()".to_string();
        }
        // Default to String for unknown refs
        return "AttributeType::String".to_string();
    }

    // Handle enum
    if let Some(enum_values) = &prop.enum_values {
        let values: Vec<String> = enum_values.iter().map(|v| format!("\"{}\"", v)).collect();
        return format!(
            "AttributeType::Enum(vec![{}.to_string()])",
            values.join(".to_string(), ")
        );
    }

    // Handle type
    match prop.prop_type.as_ref().and_then(|t| t.as_str()) {
        Some("string") => {
            // Check property name for specific types
            let prop_lower = prop_name.to_lowercase();

            // CIDR type - only for properties that are actually CIDRs
            if prop_lower.contains("cidrblock") || prop_lower == "cidr_block" {
                return "types::cidr()".to_string();
            }

            // IDs are always strings
            if prop_lower.ends_with("id") || prop_lower.ends_with("_id") {
                return "AttributeType::String".to_string();
            }

            // ARNs are always strings
            if prop_lower.ends_with("arn") || prop_lower.contains("_arn") {
                return "AttributeType::String".to_string();
            }

            // Zone/Region are strings
            if prop_lower.contains("zone") || prop_lower.contains("region") {
                return "AttributeType::String".to_string();
            }

            "AttributeType::String".to_string()
        }
        Some("boolean") => "AttributeType::Bool".to_string(),
        Some("integer") => "AttributeType::Int".to_string(),
        Some("number") => "AttributeType::Int".to_string(),
        Some("array") => {
            if let Some(items) = &prop.items {
                let item_type = cfn_type_to_carina_type(items, prop_name, _schema);
                format!("AttributeType::List(Box::new({}))", item_type)
            } else {
                "AttributeType::List(Box::new(AttributeType::String))".to_string()
            }
        }
        Some("object") => "AttributeType::Map(Box::new(AttributeType::String))".to_string(),
        _ => "AttributeType::String".to_string(),
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
