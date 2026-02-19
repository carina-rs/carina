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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Read};
use std::sync::LazyLock;

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
#[derive(Debug, Clone, Deserialize)]
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

/// Enum value can be a string or an integer in JSON Schema
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum EnumValue {
    Str(String),
    Int(i64),
}

impl EnumValue {
    fn to_string_value(&self) -> String {
        match self {
            EnumValue::Str(s) => s.clone(),
            EnumValue::Int(i) => i.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CfnProperty {
    #[serde(rename = "type")]
    prop_type: Option<TypeValue>,
    description: Option<String>,
    #[serde(rename = "enum")]
    enum_values: Option<Vec<EnumValue>>,
    items: Option<Box<CfnProperty>>,
    #[serde(rename = "$ref")]
    ref_path: Option<String>,
    #[serde(default)]
    insertion_order: Option<bool>,
    /// Inline object properties (for nested objects)
    properties: Option<BTreeMap<String, CfnProperty>>,
    /// Required fields for inline objects
    #[serde(default)]
    required: Vec<String>,
    /// Minimum value constraint (for integer/number types)
    #[serde(default)]
    minimum: Option<i64>,
    /// Maximum value constraint (for integer/number types)
    #[serde(default)]
    maximum: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
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

/// Information about a resolved struct definition for markdown docs
struct StructDefInfo {
    /// Definition name (e.g., "Ingress")
    def_name: String,
    /// Properties of the definition
    properties: BTreeMap<String, CfnProperty>,
    /// Required fields
    required: Vec<String>,
}

/// Display string for List element types based on items property type and property name.
/// The `prop_name` is used for name-based type inference (e.g., SubnetIds -> `List<SubnetId>`).
fn list_element_type_display(items: &CfnProperty, prop_name: &str) -> String {
    match items.prop_type.as_ref().and_then(|t| t.as_str()) {
        Some("string") => {
            let element_type = infer_string_type_display(prop_name);
            format!("`List<{}>`", element_type)
        }
        Some("integer") | Some("number") => "`List<Int>`".to_string(),
        Some("boolean") => "`List<Bool>`".to_string(),
        _ => "`List<String>`".to_string(),
    }
}

/// Infer a display type name for a string property based on its name.
/// Used by both scalar `type_display_string()` and `list_element_type_display()`.
/// Handles both singular (e.g., "SubnetId") and plural (e.g., "SubnetIds") property names.
fn infer_string_type_display(prop_name: &str) -> String {
    // Check known string type overrides first
    let string_overrides = known_string_type_overrides();
    if let Some(&override_type) = string_overrides.get(prop_name) {
        // Extract display name from override type string
        // e.g., "super::security_group_id()" -> "SecurityGroupId"
        //       "super::iam_role_arn()" -> "IamRoleArn"
        return override_type_to_display_name(override_type).to_string();
    }

    // Normalize plural forms to singular for type inference
    // e.g., "SubnetIds" -> "SubnetId", "CidrBlocks" -> "CidrBlock"
    let singular_name = if prop_name.ends_with("Ids")
        || prop_name.ends_with("ids")
        || prop_name.ends_with("Arns")
        || prop_name.ends_with("arns")
    {
        &prop_name[..prop_name.len() - 1]
    } else {
        prop_name
    };

    // Check overrides for singular form too (e.g., list items)
    if let Some(&override_type) = string_overrides.get(singular_name) {
        return override_type_to_display_name(override_type).to_string();
    }

    let prop_lower = singular_name.to_lowercase();
    if prop_lower.contains("cidr") {
        if prop_lower.contains("ipv6") {
            "Ipv6Cidr".to_string()
        } else {
            "Ipv4Cidr".to_string()
        }
    } else if (prop_lower.contains("ipaddress")
        || prop_lower.ends_with("ip")
        || prop_lower.contains("ipaddresses"))
        && !prop_lower.contains("cidr")
        && !prop_lower.contains("count")
        && !prop_lower.contains("type")
    {
        if prop_lower.contains("ipv6") {
            "Ipv6Address".to_string()
        } else {
            "Ipv4Address".to_string()
        }
    } else if prop_lower == "availabilityzone" || prop_lower == "availabilityzones" {
        "AvailabilityZone".to_string()
    } else if prop_lower.ends_with("arn")
        || prop_lower.ends_with("arns")
        || prop_lower.contains("_arn")
    {
        "Arn".to_string()
    } else if is_ipam_pool_id_property(singular_name) {
        "IpamPoolId".to_string()
    } else if is_aws_resource_id_property(singular_name) {
        get_resource_id_display_name(singular_name).to_string()
    } else {
        "String".to_string()
    }
}

/// Convert an override type string to a display name
/// e.g., "super::security_group_id()" -> "SecurityGroupId"
fn override_type_to_display_name(override_type: &str) -> &str {
    match override_type {
        "super::security_group_id()" => "SecurityGroupId",
        "super::aws_resource_id()" => "AwsResourceId",
        "super::iam_role_arn()" => "IamRoleArn",
        "super::iam_policy_arn()" => "IamPolicyArn",
        "super::kms_key_arn()" => "KmsKeyArn",
        "super::kms_key_id()" => "KmsKeyId",
        _ => "String",
    }
}

/// Determine the display string for a property's type in markdown docs
fn type_display_string(
    prop_name: &str,
    prop: &CfnProperty,
    schema: &CfnSchema,
    enums: &BTreeMap<String, EnumInfo>,
) -> String {
    if enums.contains_key(prop_name) {
        format!(
            "[Enum ({})](#{}-{})",
            enums[prop_name].type_name,
            prop_name.to_snake_case(),
            enums[prop_name].type_name.to_lowercase()
        )
    } else if prop_name == "Tags" {
        "Map".to_string()
    } else if let Some(ref_path) = &prop.ref_path {
        if ref_path.contains("/Tag") {
            "Map".to_string()
        } else if let Some(def_name) = ref_def_name(ref_path)
            && resolve_ref(schema, ref_path)
                .and_then(|d| d.properties.as_ref())
                .map(|p| !p.is_empty())
                .unwrap_or(false)
        {
            format!("[Struct({})](#{})", def_name, def_name.to_lowercase())
        } else {
            // Apply name-based heuristics for unresolvable $ref
            infer_string_type_display(prop_name)
        }
    } else {
        match prop.prop_type.as_ref().and_then(|t| t.as_str()) {
            Some("string") => {
                if prop_name.ends_with("PolicyDocument") {
                    "IamPolicyDocument".to_string()
                } else {
                    infer_string_type_display(prop_name)
                }
            }
            Some("boolean") => "Bool".to_string(),
            Some("integer") | Some("number") => {
                let range = if let (Some(min), Some(max)) = (prop.minimum, prop.maximum) {
                    Some((min, max))
                } else {
                    known_int_range_overrides().get(prop_name).copied()
                };
                if let Some((min, max)) = range {
                    format!("Int({}..={})", min, max)
                } else {
                    "Int".to_string()
                }
            }
            Some("array") => {
                if let Some(items) = &prop.items {
                    if let Some(ref_path) = &items.ref_path {
                        if !ref_path.contains("/Tag") {
                            if let Some(def_name) = ref_def_name(ref_path)
                                && resolve_ref(schema, ref_path)
                                    .and_then(|d| d.properties.as_ref())
                                    .map(|p| !p.is_empty())
                                    .unwrap_or(false)
                            {
                                format!("[List\\<{}\\>](#{})", def_name, def_name.to_lowercase())
                            } else {
                                "`List<String>`".to_string()
                            }
                        } else {
                            "`List<Map>`".to_string()
                        }
                    } else {
                        list_element_type_display(items, prop_name)
                    }
                } else {
                    "`List<String>`".to_string()
                }
            }
            Some("object") => {
                if let Some(props) = &prop.properties
                    && !props.is_empty()
                {
                    format!("[Struct({})](#{})", prop_name, prop_name.to_lowercase())
                } else if prop_name.ends_with("PolicyDocument") {
                    "IamPolicyDocument".to_string()
                } else {
                    "Map".to_string()
                }
            }
            _ => "String".to_string(),
        }
    }
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

    // Collect enum info and struct definitions
    let mut enums: BTreeMap<String, EnumInfo> = BTreeMap::new();
    let mut struct_defs: BTreeMap<String, StructDefInfo> = BTreeMap::new();

    for (prop_name, prop) in &schema.properties {
        let (_, enum_info) = cfn_type_to_carina_type_with_enum(prop, prop_name, schema);
        if let Some(info) = enum_info {
            enums.insert(prop_name.clone(), info);
        }
        // Collect struct definitions from $ref
        collect_struct_defs(prop, prop_name, schema, &mut struct_defs);
    }

    // Title
    md.push_str(&format!("# awscc.{}\n\n", full_resource));
    md.push_str(&format!("CloudFormation Type: `{}`\n\n", type_name));

    // Description
    if let Some(desc) = &schema.description {
        md.push_str(&format!("{}\n\n", desc));
    }

    // Argument Reference (writable attributes)
    md.push_str("## Argument Reference\n\n");

    for (prop_name, prop) in &schema.properties {
        if read_only.contains(prop_name) {
            continue;
        }

        let attr_name = prop_name.to_snake_case();
        let is_required = required.contains(prop_name);
        let type_display = type_display_string(prop_name, prop, schema, &enums);

        md.push_str(&format!("### `{}`\n\n", attr_name));
        md.push_str(&format!("- **Type:** {}\n", type_display));
        if is_required {
            md.push_str("- **Required:** Yes\n");
        } else {
            md.push_str("- **Required:** No\n");
        }
        md.push('\n');

        if let Some(d) = &prop.description {
            let desc = d.replace('\n', " ").replace("  ", " ");
            md.push_str(&format!("{}\n\n", desc));
        }
    }

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

    // Struct Definitions section
    if !struct_defs.is_empty() {
        md.push_str("## Struct Definitions\n\n");
        for def_info in struct_defs.values() {
            md.push_str(&format!("### {}\n\n", def_info.def_name));
            md.push_str("| Field | Type | Required | Description |\n");
            md.push_str("|-------|------|----------|-------------|\n");
            let required_set: HashSet<&str> =
                def_info.required.iter().map(|s| s.as_str()).collect();
            let overrides = known_enum_overrides();
            for (field_name, field_prop) in &def_info.properties {
                let snake_name = field_name.to_snake_case();
                let is_req = required_set.contains(field_name.as_str());
                let field_type_display: String = if overrides.contains_key(field_name.as_str()) {
                    "Enum".to_string()
                } else {
                    match field_prop.prop_type.as_ref().and_then(|t| t.as_str()) {
                        Some("string") => {
                            if field_name.ends_with("PolicyDocument") {
                                "IamPolicyDocument".to_string()
                            } else {
                                infer_string_type_display(field_name)
                            }
                        }
                        Some("boolean") => "Bool".to_string(),
                        Some("integer") | Some("number") => {
                            let range = if let (Some(min), Some(max)) =
                                (field_prop.minimum, field_prop.maximum)
                            {
                                Some((min, max))
                            } else {
                                known_int_range_overrides()
                                    .get(field_name.as_str())
                                    .copied()
                            };
                            if let Some((min, max)) = range {
                                format!("Int({}..={})", min, max)
                            } else {
                                "Int".to_string()
                            }
                        }
                        Some("array") => {
                            if let Some(items) = &field_prop.items {
                                list_element_type_display(items, field_name)
                            } else {
                                "`List<String>`".to_string()
                            }
                        }
                        Some("object") => "Map".to_string(),
                        _ => infer_string_type_display(field_name),
                    }
                };
                let desc = field_prop
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .replace('\n', " ")
                    .replace("  ", " ");
                let truncated = if desc.len() > 100 {
                    format!("{}...", &desc[..100])
                } else {
                    desc
                };
                md.push_str(&format!(
                    "| `{}` | {} | {} | {} |\n",
                    snake_name,
                    field_type_display,
                    if is_req { "Yes" } else { "No" },
                    truncated
                ));
            }
            md.push('\n');
        }
    }

    // Attribute Reference (read-only attributes)
    let has_read_only = schema
        .properties
        .keys()
        .any(|name| read_only.contains(name));
    if has_read_only {
        md.push_str("## Attribute Reference\n\n");

        for (prop_name, prop) in &schema.properties {
            if !read_only.contains(prop_name) {
                continue;
            }

            let attr_name = prop_name.to_snake_case();
            let type_display = type_display_string(prop_name, prop, schema, &enums);

            md.push_str(&format!("### `{}`\n\n", attr_name));
            md.push_str(&format!("- **Type:** {}\n\n", type_display));
        }
    }

    Ok(md)
}

/// Collect struct definitions from properties for markdown documentation
fn collect_struct_defs(
    prop: &CfnProperty,
    prop_name: &str,
    schema: &CfnSchema,
    struct_defs: &mut BTreeMap<String, StructDefInfo>,
) {
    // Handle $ref
    if let Some(ref_path) = &prop.ref_path
        && !ref_path.contains("/Tag")
        && let Some(def_name) = ref_def_name(ref_path)
        && let Some(def) = resolve_ref(schema, ref_path)
        && let Some(props) = &def.properties
        && !props.is_empty()
    {
        struct_defs
            .entry(def_name.to_string())
            .or_insert_with(|| StructDefInfo {
                def_name: def_name.to_string(),
                properties: props.clone(),
                required: def.required.clone(),
            });
    }
    // Handle array items with $ref
    if let Some(items) = &prop.items
        && let Some(ref_path) = &items.ref_path
        && !ref_path.contains("/Tag")
        && let Some(def_name) = ref_def_name(ref_path)
        && let Some(def) = resolve_ref(schema, ref_path)
        && let Some(props) = &def.properties
        && !props.is_empty()
    {
        struct_defs
            .entry(def_name.to_string())
            .or_insert_with(|| StructDefInfo {
                def_name: def_name.to_string(),
                properties: props.clone(),
                required: def.required.clone(),
            });
    }
    // Handle inline object with properties
    if let Some(type_val) = &prop.prop_type
        && type_val.as_str() == Some("object")
        && let Some(props) = &prop.properties
        && !props.is_empty()
    {
        struct_defs
            .entry(prop_name.to_string())
            .or_insert_with(|| StructDefInfo {
                def_name: prop_name.to_string(),
                properties: props.clone(),
                required: prop.required.clone(),
            });
    }
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

    // Build create-only properties set
    let create_only: HashSet<String> = schema
        .create_only_properties
        .iter()
        .map(|p| p.trim_start_matches("/properties/").to_string())
        .collect();

    let required: HashSet<String> = schema.required.iter().cloned().collect();

    // Pre-scan properties to determine which imports are needed and collect enum info
    let mut needs_types = false;
    let mut needs_attribute_type = false;
    let mut needs_tags_type = false;
    let mut needs_struct_field = false;
    let mut enums: BTreeMap<String, EnumInfo> = BTreeMap::new();
    let mut ranged_ints: BTreeMap<String, (i64, i64)> = BTreeMap::new();

    for (prop_name, prop) in &schema.properties {
        let (attr_type, enum_info) = cfn_type_to_carina_type_with_enum(prop, prop_name, schema);
        if attr_type.contains("types::") {
            needs_types = true;
        }
        if attr_type.contains("AttributeType::") {
            needs_attribute_type = true;
        }
        if attr_type.contains("tags_type()") {
            needs_tags_type = true;
        }
        if attr_type.contains("StructField::") {
            needs_struct_field = true;
        }
        if let Some(info) = enum_info {
            enums.insert(prop_name.clone(), info);
        }
        // Collect ranged integer properties
        if matches!(
            prop.prop_type.as_ref().and_then(|t| t.as_str()),
            Some("integer") | Some("number")
        ) {
            if let (Some(min), Some(max)) = (prop.minimum, prop.maximum) {
                ranged_ints.insert(prop_name.clone(), (min, max));
            } else if let Some(&(min, max)) = known_int_range_overrides().get(prop_name.as_str()) {
                ranged_ints.insert(prop_name.clone(), (min, max));
            }
        }
    }

    // Also scan definitions for struct field integer properties matching overrides
    let int_overrides = known_int_range_overrides();
    if let Some(definitions) = &schema.definitions {
        for def in definitions.values() {
            if let Some(props) = &def.properties {
                for (field_name, field_prop) in props {
                    if matches!(
                        field_prop.prop_type.as_ref().and_then(|t| t.as_str()),
                        Some("integer") | Some("number")
                    ) && field_prop.minimum.is_none()
                        && field_prop.maximum.is_none()
                        && int_overrides.contains_key(field_name.as_str())
                    {
                        // Mark that we need ranged ints (for imports)
                        if !ranged_ints.contains_key(field_name) {
                            let (min, max) = int_overrides[field_name.as_str()];
                            ranged_ints.insert(field_name.clone(), (min, max));
                        }
                    }
                }
            }
        }
    }

    let has_enums = !enums.is_empty();
    let has_ranged_ints = !ranged_ints.is_empty();

    // Enums use AttributeType::Custom with AttributeType::String base
    if has_enums {
        needs_attribute_type = true;
    }

    // Ranged ints use AttributeType::Custom with AttributeType::Int base
    if has_ranged_ints {
        needs_attribute_type = true;
    }

    // Determine has_tags from tagging metadata
    let has_tags = schema.tagging.as_ref().map(|t| t.taggable).unwrap_or(false);

    // Generate header with conditional imports
    let mut schema_imports = vec!["AttributeSchema", "ResourceSchema"];
    if needs_attribute_type {
        schema_imports.insert(1, "AttributeType");
    }
    if needs_struct_field {
        schema_imports.push("StructField");
    }
    if needs_types {
        schema_imports.push("types");
    }
    let schema_imports_str = schema_imports.join(", ");
    code.push_str(&format!(
        r#"//! {} schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: {}
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use carina_core::schema::{{{}}};
use super::AwsccSchemaConfig;
"#,
        resource, type_name, schema_imports_str
    ));

    if has_enums || has_ranged_ints {
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
        .map_err(|reason| {{
            if let Value::String(s) = value {{
                format!("Invalid {} '{{}}': {{}}", s, reason)
            }} else {{
                reason
            }}
        }})
}}

"#,
            fn_name, enum_info.type_name, namespace, const_name, enum_info.type_name
        ));
    }

    // Generate range validation functions for integer properties
    for (prop_name, (min, max)) in &ranged_ints {
        let fn_name = format!("validate_{}_range", prop_name.to_snake_case());
        code.push_str(&format!(
            r#"fn {}(value: &Value) -> Result<(), String> {{
    if let Value::Int(n) = value {{
        if *n < {} || *n > {} {{
            Err(format!("Value {{}} is out of range {}..={}", n))
        }} else {{
            Ok(())
        }}
    }} else {{
        Err("Expected integer".to_string())
    }}
}}

"#,
            fn_name, min, max, min, max
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
        let is_create_only = create_only.contains(prop_name);

        let attr_type = if let Some(enum_info) = enums.get(prop_name) {
            // Use AttributeType::Custom for enums
            let validate_fn = format!("validate_{}", prop_name.to_snake_case());
            format!(
                r#"AttributeType::Custom {{
                name: "{}".to_string(),
                base: Box::new(AttributeType::String),
                validate: {},
                namespace: Some("{}".to_string()),
                to_dsl: None,
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

        if is_create_only {
            attr_code.push_str("\n                .create_only()");
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

    // Generate enum_valid_values() function that exposes VALID_* constants
    code.push_str(&format!(
        "\n/// Returns the resource type name and all enum valid values for this module\n\
         pub fn enum_valid_values() -> (&'static str, &'static [(&'static str, &'static [&'static str])]) {{\n\
         {}\
         }}\n",
        if enums.is_empty() {
            format!("    (\"{}\", &[])\n", full_resource)
        } else {
            let entries: Vec<String> = enums
                .keys()
                .map(|prop_name| {
                    let attr_name = prop_name.to_snake_case();
                    let const_name = format!("VALID_{}", attr_name.to_uppercase());
                    format!("        (\"{}\", {}),", attr_name, const_name)
                })
                .collect();
            format!(
                "    (\"{}\", &[\n{}\n    ])\n",
                full_resource,
                entries.join("\n")
            )
        }
    ));

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

/// Check if a string looks like a valid enum value (not a code example, unicode escape, etc.)
fn looks_like_enum_value(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.contains('{') || s.contains('}') {
        return false;
    }
    if s.contains("\\u") {
        return false;
    }
    if s.len() > 50 {
        return false;
    }
    if s.contains(' ') {
        return false;
    }
    true
}

/// Extract enum values from description text.
/// Looks for patterns like ``value`` (double backticks) which CloudFormation uses
/// to indicate allowed values in descriptions.
fn extract_enum_from_description(description: &str) -> Option<Vec<String>> {
    // Strategy 1: Look for double-backtick values (existing behavior)
    let backtick_re = Regex::new(r"``([^`]+)``").ok()?;
    let mut values: Vec<String> = backtick_re
        .captures_iter(description)
        .map(|cap| cap[1].to_string())
        .filter(|v| !looks_like_property_name(v) && looks_like_enum_value(v))
        .collect();

    // If we found enum values with backticks, use them
    if values.len() >= 2 {
        return deduplicate_enum_values(values);
    }

    // Strategy 2: Look for "Valid values: X | Y | Z" or "Options: X | Y" patterns
    if let Ok(pipe_re) = Regex::new(r"(?i)(?:valid values?|options?):\s*([^\n.]+)")
        && let Some(cap) = pipe_re.captures(description)
    {
        let list = cap[1].trim();
        // Split by pipe or comma
        let candidates: Vec<String> = if list.contains('|') {
            list.split('|').map(|s| s.trim().to_string()).collect()
        } else if list.contains(',') {
            list.split(',').map(|s| s.trim().to_string()).collect()
        } else {
            vec![]
        };

        values = candidates
            .into_iter()
            .filter(|v| !v.is_empty() && !looks_like_property_name(v))
            .collect();

        if values.len() >= 2 {
            return deduplicate_enum_values(values);
        }
    }

    // Strategy 3: Look for "Options are X, Y, Z" or "Can be X, Y, or Z" patterns
    if let Ok(list_re) =
        Regex::new(r"(?i)(?:options (?:here )?are|can be|either)\s+(.+?)(?:\.|\n|$)")
        && let Some(cap) = list_re.captures(description)
    {
        let list = cap[1].trim();

        // Extract the enum list part before any trailing explanatory text
        // e.g., "default, dedicated, or host for instances" -> "default, dedicated, or host"
        let enum_list = if let Some(idx) = list
            .find(" for ")
            .or_else(|| list.find(" when "))
            .or_else(|| list.find(" where "))
            .or_else(|| list.find(" by "))
            .or_else(|| list.find(" with "))
            .or_else(|| list.find(" that "))
        {
            &list[..idx]
        } else {
            list
        };

        // Split by comma and "or"
        let mut candidates: Vec<String> = vec![];
        for part in enum_list.split(',') {
            let part = part.trim();
            // Handle "or X" pattern
            if let Some(stripped) = part.strip_prefix("or ") {
                candidates.push(stripped.trim().to_string());
            } else if part.contains(" or ") {
                // Split on " or " within a part
                for subpart in part.split(" or ") {
                    candidates.push(subpart.trim().to_string());
                }
            } else {
                candidates.push(part.to_string());
            }
        }

        values = candidates
            .into_iter()
            .filter(|v| !v.is_empty() && !looks_like_property_name(v) && !v.contains(' '))
            .collect();

        if values.len() >= 2 {
            return deduplicate_enum_values(values);
        }
    }

    None
}

/// Deduplicate enum values while preserving order
fn deduplicate_enum_values(values: Vec<String>) -> Option<Vec<String>> {
    let mut seen = HashSet::new();
    let unique: Vec<String> = values
        .into_iter()
        .filter(|v| seen.insert(v.clone()))
        .collect();
    if unique.len() >= 2 {
        Some(unique)
    } else {
        None
    }
}

/// Resolve a $ref path to a CfnDefinition
/// e.g., "#/definitions/Ingress" -> Some(&CfnDefinition)
fn resolve_ref<'a>(schema: &'a CfnSchema, ref_path: &str) -> Option<&'a CfnDefinition> {
    let def_name = ref_path.strip_prefix("#/definitions/")?;
    schema.definitions.as_ref()?.get(def_name)
}

/// Extract the definition name from a $ref path
/// e.g., "#/definitions/Ingress" -> Some("Ingress")
fn ref_def_name(ref_path: &str) -> Option<&str> {
    ref_path.strip_prefix("#/definitions/")
}

/// Generate Rust code for an AttributeType::Struct from a set of properties
fn generate_struct_type(
    def_name: &str,
    properties: &BTreeMap<String, CfnProperty>,
    required: &[String],
    schema: &CfnSchema,
) -> String {
    let required_set: HashSet<&str> = required.iter().map(|s| s.as_str()).collect();

    let fields: Vec<String> = properties
        .iter()
        .map(|(field_name, field_prop)| {
            let snake_name = field_name.to_snake_case();
            let (field_type, enum_info) =
                cfn_type_to_carina_type_with_enum(field_prop, field_name, schema);
            // If enum detected in struct field, use Enum variant directly
            let field_type = if let Some(info) = enum_info {
                let values_str = info
                    .values
                    .iter()
                    .map(|v| format!("\"{}\".to_string()", v))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("AttributeType::Enum(vec![{}])", values_str)
            } else {
                field_type
            };
            let is_required = required_set.contains(field_name.as_str());

            let mut field_code = format!("StructField::new(\"{}\", {})", snake_name, field_type);
            if is_required {
                field_code.push_str(".required()");
            }
            if let Some(desc) = &field_prop.description {
                let escaped = desc
                    .replace('"', "\\\"")
                    .replace('\n', " ")
                    .replace("  ", " ");
                let truncated = if escaped.len() > 150 {
                    format!("{}...", &escaped[..150])
                } else {
                    escaped
                };
                field_code.push_str(&format!(".with_description(\"{}\")", truncated));
            }
            field_code.push_str(&format!(".with_provider_name(\"{}\")", field_name));
            field_code
        })
        .collect();

    let fields_str = fields.join(",\n                    ");
    format!(
        "AttributeType::Struct {{\n                    name: \"{}\".to_string(),\n                    fields: vec![\n                    {}\n                    ],\n                }}",
        def_name, fields_str
    )
}

/// Known enum overrides for properties where `extract_enum_from_description()` fails
/// due to inconsistent description formatting in CloudFormation schemas.
fn known_enum_overrides() -> &'static HashMap<&'static str, Vec<&'static str>> {
    static OVERRIDES: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("IpProtocol", vec!["tcp", "udp", "icmp", "icmpv6", "-1"]);
        m.insert("ConnectivityType", vec!["public", "private"]);
        m.insert("AvailabilityMode", vec!["zonal", "regional"]);
        m.insert("AddressFamily", vec!["IPv4", "IPv6"]);
        m.insert("Domain", vec!["vpc", "standard"]);
        // HostnameType enum values are in parent struct description, not field description
        m.insert("HostnameType", vec!["ip-name", "resource-name"]);
        // InternetGatewayBlockMode removed - now auto-detected via "Options here are" pattern
        // Transit gateway enable/disable properties
        m.insert("AutoAcceptSharedAttachments", vec!["enable", "disable"]);
        m.insert("DefaultRouteTableAssociation", vec!["enable", "disable"]);
        m.insert("DefaultRouteTablePropagation", vec!["enable", "disable"]);
        m.insert("DnsSupport", vec!["enable", "disable"]);
        m.insert("MulticastSupport", vec!["enable", "disable"]);
        m.insert("SecurityGroupReferencingSupport", vec!["enable", "disable"]);
        m.insert("VpnEcmpSupport", vec!["enable", "disable"]);
        m
    });
    &OVERRIDES
}

/// Known integer range overrides for properties where CloudFormation schemas
/// don't include min/max constraints but the ranges are well-known.
fn known_int_range_overrides() -> &'static HashMap<&'static str, (i64, i64)> {
    static OVERRIDES: LazyLock<HashMap<&'static str, (i64, i64)>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("Ipv4NetmaskLength", (0, 32));
        m.insert("Ipv6NetmaskLength", (0, 128));
        m.insert("FromPort", (-1, 65535));
        m.insert("ToPort", (-1, 65535));
        m.insert("MaxSessionDuration", (3600, 43200));
        m
    });
    &OVERRIDES
}

/// Known string type overrides for properties where the CloudFormation type is
/// plain "string" but should use a more specific type.
fn known_string_type_overrides() -> &'static HashMap<&'static str, &'static str> {
    static OVERRIDES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("DefaultSecurityGroup", "super::security_group_id()");
        m.insert("DefaultNetworkAcl", "super::aws_resource_id()");
        m.insert("DeliverCrossAccountRole", "super::iam_role_arn()");
        m.insert("DeliverLogsPermissionArn", "super::iam_role_arn()");
        m.insert("PeerRoleArn", "super::iam_role_arn()");
        m.insert("PermissionsBoundary", "super::iam_policy_arn()");
        m.insert("ManagedPolicyArns", "super::iam_policy_arn()");
        m.insert("KmsKeyId", "super::kms_key_arn()");
        m.insert("KMSMasterKeyID", "super::kms_key_id()");
        m.insert("ReplicaKmsKeyID", "super::kms_key_id()");
        m.insert("KmsKeyArn", "super::kms_key_arn()");
        m
    });
    &OVERRIDES
}

/// Resource-specific property type overrides.
/// Maps (CloudFormation type name, property name) to a specific type.
/// Use this when the same property name should have different types on different resources.
fn resource_specific_type_overrides() -> &'static HashMap<(&'static str, &'static str), &'static str>
{
    static OVERRIDES: LazyLock<HashMap<(&'static str, &'static str), &'static str>> =
        LazyLock::new(|| {
            let mut m = HashMap::new();
            // IAM Role's Arn is always an IAM Role ARN
            m.insert(("AWS::IAM::Role", "Arn"), "super::iam_role_arn()");
            m
        });
    &OVERRIDES
}

/// Infer the Carina type string for a property based on its name.
/// Checks resource-specific overrides, known string type overrides, ARN patterns,
/// and resource ID patterns.
/// Returns None if no heuristic matches (caller should default to String).
fn infer_string_type(prop_name: &str, resource_type: &str) -> Option<String> {
    // Check resource-specific overrides first
    if let Some(&override_type) =
        resource_specific_type_overrides().get(&(resource_type, prop_name))
    {
        return Some(override_type.to_string());
    }
    // Check known string type overrides
    if let Some(&override_type) = known_string_type_overrides().get(prop_name) {
        return Some(override_type.to_string());
    }
    // Check ARN pattern
    let prop_lower = prop_name.to_lowercase();
    if prop_lower.ends_with("arn") || prop_lower.ends_with("arns") || prop_lower.contains("_arn") {
        return Some("super::arn()".to_string());
    }
    // Check resource ID pattern
    if is_aws_resource_id_property(prop_name) {
        return Some(get_resource_id_type(prop_name).to_string());
    }
    None
}

/// Check if a property name represents an AWS resource ID with the standard
/// prefix-hex format (e.g., vpc-1a2b3c4d, subnet-0123456789abcdef0)
fn is_aws_resource_id_property(prop_name: &str) -> bool {
    let lower = prop_name.to_lowercase();
    // Known resource ID suffixes that use prefix-hex format
    let resource_id_suffixes = [
        "vpcid",
        "subnetid",
        "groupid",
        "gatewayid",
        "routetableid",
        "allocationid",
        "networkinterfaceid",
        "instanceid",
        "endpointid",
        "connectionid",
        "prefixlistid",
        "eniid",
    ];
    // Exclude properties that don't follow prefix-hex format
    if lower.contains("owner") || lower.contains("availabilityzone") || lower == "resourceid" {
        return false;
    }
    // Strip trailing "s" for plural forms (e.g., "RouteTableIds" -> "routetableid")
    let singular = if lower.ends_with("ids") {
        &lower[..lower.len() - 1]
    } else {
        &lower
    };
    resource_id_suffixes
        .iter()
        .any(|suffix| lower.ends_with(suffix) || singular.ends_with(suffix))
}

/// Classification of AWS resource ID types.
/// Used to derive both the type function name and display name from a single
/// matching logic, avoiding duplication (see #243).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceIdKind {
    VpcId,
    SubnetId,
    SecurityGroupId,
    EgressOnlyInternetGatewayId,
    InternetGatewayId,
    RouteTableId,
    NatGatewayId,
    VpcPeeringConnectionId,
    TransitGatewayId,
    VpnGatewayId,
    VpcEndpointId,
    Generic,
}

/// Classify a property name into a specific resource ID kind.
/// The matching order matters: more specific patterns (e.g., EgressOnlyInternetGateway)
/// must be checked before more general ones (e.g., InternetGateway).
fn classify_resource_id(prop_name: &str) -> ResourceIdKind {
    let lower = prop_name.to_lowercase();

    // VPC IDs
    if lower.ends_with("vpcid") || lower == "vpcid" {
        return ResourceIdKind::VpcId;
    }
    // Subnet IDs
    if lower.ends_with("subnetid") || lower == "subnetid" {
        return ResourceIdKind::SubnetId;
    }
    // Security Group IDs (including DestinationSecurityGroupId, SourceSecurityGroupId, etc.)
    if (lower.contains("securitygroup") || lower.contains("groupid")) && lower.ends_with("id") {
        return ResourceIdKind::SecurityGroupId;
    }
    // Egress Only Internet Gateway IDs (must be checked before Internet Gateway IDs)
    if lower.contains("egressonlyinternetgateway") && lower.ends_with("id") {
        return ResourceIdKind::EgressOnlyInternetGatewayId;
    }
    // Internet Gateway IDs
    if lower.contains("internetgateway") && lower.ends_with("id") {
        return ResourceIdKind::InternetGatewayId;
    }
    // Route Table IDs
    if lower.contains("routetable") && lower.ends_with("id") {
        return ResourceIdKind::RouteTableId;
    }
    // NAT Gateway IDs
    if lower.contains("natgateway") && lower.ends_with("id") {
        return ResourceIdKind::NatGatewayId;
    }
    // VPC Peering Connection IDs
    if lower.contains("peeringconnection") && lower.ends_with("id") {
        return ResourceIdKind::VpcPeeringConnectionId;
    }
    // Transit Gateway IDs
    if lower.contains("transitgateway") && lower.ends_with("id") {
        return ResourceIdKind::TransitGatewayId;
    }
    // VPN Gateway IDs
    if lower.contains("vpngateway") && lower.ends_with("id") {
        return ResourceIdKind::VpnGatewayId;
    }
    // VPC Endpoint IDs
    if lower.contains("vpcendpoint") && lower.ends_with("id") {
        return ResourceIdKind::VpcEndpointId;
    }

    ResourceIdKind::Generic
}

/// Get the specific resource ID type function for a property name.
/// Returns the function name (e.g., "super::vpc_id()") or generic aws_resource_id.
fn get_resource_id_type(prop_name: &str) -> &'static str {
    match classify_resource_id(prop_name) {
        ResourceIdKind::VpcId => "super::vpc_id()",
        ResourceIdKind::SubnetId => "super::subnet_id()",
        ResourceIdKind::SecurityGroupId => "super::security_group_id()",
        ResourceIdKind::EgressOnlyInternetGatewayId => "super::egress_only_internet_gateway_id()",
        ResourceIdKind::InternetGatewayId => "super::internet_gateway_id()",
        ResourceIdKind::RouteTableId => "super::route_table_id()",
        ResourceIdKind::NatGatewayId => "super::nat_gateway_id()",
        ResourceIdKind::VpcPeeringConnectionId => "super::vpc_peering_connection_id()",
        ResourceIdKind::TransitGatewayId => "super::transit_gateway_id()",
        ResourceIdKind::VpnGatewayId => "super::vpn_gateway_id()",
        ResourceIdKind::VpcEndpointId => "super::vpc_endpoint_id()",
        ResourceIdKind::Generic => "super::aws_resource_id()",
    }
}

/// Get the display name for a resource ID type (for markdown documentation).
fn get_resource_id_display_name(prop_name: &str) -> &'static str {
    match classify_resource_id(prop_name) {
        ResourceIdKind::VpcId => "VpcId",
        ResourceIdKind::SubnetId => "SubnetId",
        ResourceIdKind::SecurityGroupId => "SecurityGroupId",
        ResourceIdKind::EgressOnlyInternetGatewayId => "EgressOnlyInternetGatewayId",
        ResourceIdKind::InternetGatewayId => "InternetGatewayId",
        ResourceIdKind::RouteTableId => "RouteTableId",
        ResourceIdKind::NatGatewayId => "NatGatewayId",
        ResourceIdKind::VpcPeeringConnectionId => "VpcPeeringConnectionId",
        ResourceIdKind::TransitGatewayId => "TransitGatewayId",
        ResourceIdKind::VpnGatewayId => "VpnGatewayId",
        ResourceIdKind::VpcEndpointId => "VpcEndpointId",
        ResourceIdKind::Generic => "AwsResourceId",
    }
}

/// Check if a property name represents an IPAM Pool ID
/// (e.g., IpamPoolId, Ipv4IpamPoolId, Ipv6IpamPoolId, SourceIpamPoolId)
fn is_ipam_pool_id_property(prop_name: &str) -> bool {
    let lower = prop_name.to_lowercase();
    // Exclude properties that don't follow prefix-hex format
    if lower.contains("owner") || lower.contains("availabilityzone") || lower == "resourceid" {
        return false;
    }
    lower.ends_with("poolid")
}

/// Returns (type_string, Option<EnumInfo>)
/// EnumInfo is Some if this property is an enum that should use AttributeType::Custom
fn cfn_type_to_carina_type_with_enum(
    prop: &CfnProperty,
    prop_name: &str,
    schema: &CfnSchema,
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
        // Try to resolve the $ref to a definition and generate Struct type
        if let Some(def) = resolve_ref(schema, ref_path)
            && let Some(props) = &def.properties
            && !props.is_empty()
        {
            let def_name = ref_def_name(ref_path).unwrap_or(prop_name);
            return (
                generate_struct_type(def_name, props, &def.required, schema),
                None,
            );
        }
        // Apply name-based heuristics for unresolvable $ref
        if let Some(inferred) = infer_string_type(prop_name, &schema.type_name) {
            return (inferred, None);
        }
        return ("AttributeType::String".to_string(), None);
    }

    // Handle explicit enum
    if let Some(enum_values) = &prop.enum_values {
        // If all enum values are integers, skip enum treatment and use the base type
        let all_ints = enum_values.iter().all(|v| matches!(v, EnumValue::Int(_)));
        if all_ints {
            return match prop.prop_type.as_ref().and_then(|t| t.as_str()) {
                Some("integer") | Some("number") => ("AttributeType::Int".to_string(), None),
                _ => ("AttributeType::String".to_string(), None),
            };
        }

        let type_name = prop_name.to_pascal_case();
        let string_values: Vec<String> = enum_values.iter().map(|v| v.to_string_value()).collect();
        let enum_info = EnumInfo {
            type_name,
            values: string_values,
        };
        // Return placeholder - actual type will be generated using enum_info
        return ("/* enum */".to_string(), Some(enum_info));
    }

    // Check known enum overrides (for properties with inconsistent description formatting)
    let overrides = known_enum_overrides();
    if let Some(values) = overrides.get(prop_name) {
        let type_name = prop_name.to_pascal_case();
        let enum_info = EnumInfo {
            type_name,
            values: values.iter().map(|s| s.to_string()).collect(),
        };
        return ("/* enum */".to_string(), Some(enum_info));
    }

    // Handle type
    match prop.prop_type.as_ref().and_then(|t| t.as_str()) {
        Some("string") => {
            // Check known string type overrides first
            if let Some(inferred) = infer_string_type(prop_name, &schema.type_name) {
                return (inferred, None);
            }

            // Check if this is a policy document field (CFN sometimes types these as "string")
            if prop_name.ends_with("PolicyDocument") {
                return ("super::iam_policy_document()".to_string(), None);
            }

            // Check property name for specific types
            let prop_lower = prop_name.to_lowercase();

            // CIDR types - differentiate IPv4 vs IPv6 based on property name
            // Any property containing "cidr" is a CIDR field.
            // If it also contains "ipv6", it's IPv6 CIDR; otherwise IPv4 CIDR.
            if prop_lower.contains("cidr") {
                if prop_lower.contains("ipv6") {
                    return ("types::ipv6_cidr()".to_string(), None);
                }
                return ("types::ipv4_cidr()".to_string(), None);
            }

            // IP address types (not CIDR) - e.g., PrivateIpAddress, PublicIp
            if (prop_lower.contains("ipaddress")
                || prop_lower.ends_with("ip")
                || prop_lower.contains("ipaddresses"))
                && !prop_lower.contains("cidr")
                && !prop_lower.contains("count")
                && !prop_lower.contains("type")
            {
                if prop_lower.contains("ipv6") {
                    return ("types::ipv6_address()".to_string(), None);
                }
                return ("types::ipv4_address()".to_string(), None);
            }

            // IPAM Pool IDs with ipam-pool-{hex} format
            if is_ipam_pool_id_property(prop_name) {
                return ("super::ipam_pool_id()".to_string(), None);
            }

            // Other IDs are plain strings (AZ IDs, owner IDs, etc.)
            // Note: resource IDs and ARNs are already handled by infer_string_type() above
            if prop_lower.ends_with("id") {
                return ("AttributeType::String".to_string(), None);
            }

            // Availability zone uses format validation (e.g., "us-east-1a")
            // but AvailabilityZoneId stays as String (e.g., "use1-az1")
            if prop_lower == "availabilityzone" {
                return ("super::availability_zone()".to_string(), None);
            }

            // Other zone/region fields are strings
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
        Some("integer") | Some("number") => {
            // Use CF min/max if available, otherwise check known overrides
            let range = if let (Some(min), Some(max)) = (prop.minimum, prop.maximum) {
                Some((min, max))
            } else {
                known_int_range_overrides().get(prop_name).copied()
            };
            if let Some((min, max)) = range {
                // Generate a ranged int type with validation
                let validate_fn = format!("validate_{}_range", prop_name.to_snake_case());
                (
                    format!(
                        r#"AttributeType::Custom {{
                name: "Int({}..={})".to_string(),
                base: Box::new(AttributeType::Int),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        min, max, validate_fn
                    ),
                    None,
                )
            } else {
                ("AttributeType::Int".to_string(), None)
            }
        }
        Some("array") => {
            if let Some(items) = &prop.items {
                // Check if items has a $ref to a definition
                if let Some(ref_path) = &items.ref_path
                    && !ref_path.contains("/Tag")
                    && let Some(def) = resolve_ref(schema, ref_path)
                    && let Some(props) = &def.properties
                    && !props.is_empty()
                {
                    let def_name = ref_def_name(ref_path).unwrap_or(prop_name);
                    let struct_type = generate_struct_type(def_name, props, &def.required, schema);
                    return (
                        format!("AttributeType::List(Box::new({}))", struct_type),
                        None,
                    );
                }
                let (item_type, item_enum) =
                    cfn_type_to_carina_type_with_enum(items, prop_name, schema);
                // If array items are enum values, use String as the item type
                // (enum validation happens at the attribute level, not item level)
                let effective_item_type = if item_enum.is_some() {
                    "AttributeType::String".to_string()
                } else {
                    item_type
                };
                (
                    format!("AttributeType::List(Box::new({}))", effective_item_type),
                    None,
                )
            } else {
                (
                    "AttributeType::List(Box::new(AttributeType::String))".to_string(),
                    None,
                )
            }
        }
        Some("object") => {
            // Check if this object has inline properties -> Struct
            if let Some(props) = &prop.properties
                && !props.is_empty()
            {
                return (
                    generate_struct_type(prop_name, props, &prop.required, schema),
                    None,
                );
            }
            // Check if this is an IAM policy document
            if prop_name.ends_with("PolicyDocument") {
                return ("super::iam_policy_document()".to_string(), None);
            }
            (
                "AttributeType::Map(Box::new(AttributeType::String))".to_string(),
                None,
            )
        }
        _ => {
            // Fallback: apply name-based heuristics for properties with no explicit type
            if let Some(inferred) = infer_string_type(prop_name, &schema.type_name) {
                (inferred, None)
            } else {
                ("AttributeType::String".to_string(), None)
            }
        }
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

    #[test]
    fn test_extract_enum_from_description_valid_values_pipe() {
        // "Valid values: X | Y | Z" pattern
        let description = "The connectivity type. Valid values: public | private";
        let result = extract_enum_from_description(description);
        assert!(result.is_some());
        let values = result.unwrap();
        assert_eq!(values, vec!["public", "private"]);
    }

    #[test]
    fn test_extract_enum_from_description_options_colon() {
        // "Options: X, Y, Z" pattern
        let description =
            "Block mode for internet gateway. Options: off, block-bidirectional, block-ingress";
        let result = extract_enum_from_description(description);
        assert!(result.is_some());
        let values = result.unwrap();
        assert_eq!(values, vec!["off", "block-bidirectional", "block-ingress"]);
    }

    #[test]
    fn test_extract_enum_from_description_options_here_are() {
        // "Options here are X, Y, Z" pattern (real CloudFormation format)
        let description =
            "The mode of VPC BPA. Options here are off, block-bidirectional, block-ingress";
        let result = extract_enum_from_description(description);
        assert!(result.is_some());
        let values = result.unwrap();
        assert_eq!(values, vec!["off", "block-bidirectional", "block-ingress"]);
    }

    #[test]
    fn test_extract_enum_from_description_options_are() {
        // "Options are X, Y, or Z" pattern
        let description =
            "The tenancy options. Options are default, dedicated, or host for instances.";
        let result = extract_enum_from_description(description);
        assert!(result.is_some());
        let values = result.unwrap();
        assert_eq!(values, vec!["default", "dedicated", "host"]);
    }

    #[test]
    fn test_extract_enum_from_description_can_be() {
        // "Can be X or Y" pattern
        let description = "The allocation strategy can be zonal or regional";
        let result = extract_enum_from_description(description);
        assert!(result.is_some());
        let values = result.unwrap();
        assert_eq!(values, vec!["zonal", "regional"]);
    }

    #[test]
    fn test_extract_enum_from_description_either() {
        // "Either X or Y" pattern
        let description = "Set the mode to either enabled or disabled";
        let result = extract_enum_from_description(description);
        assert!(result.is_some());
        let values = result.unwrap();
        assert_eq!(values, vec!["enabled", "disabled"]);
    }

    #[test]
    fn test_known_enum_overrides() {
        let overrides = known_enum_overrides();

        // IpProtocol should be overridden
        let ip_protocol = overrides.get("IpProtocol");
        assert!(ip_protocol.is_some(), "IpProtocol should be in overrides");
        assert_eq!(
            ip_protocol.unwrap(),
            &vec!["tcp", "udp", "icmp", "icmpv6", "-1"]
        );

        // ConnectivityType should be overridden
        let connectivity = overrides.get("ConnectivityType");
        assert!(
            connectivity.is_some(),
            "ConnectivityType should be in overrides"
        );
        assert_eq!(connectivity.unwrap(), &vec!["public", "private"]);
    }

    #[test]
    fn test_known_enum_override_used_in_codegen() {
        // IpProtocol with plain description (no double backticks) should still
        // produce an EnumInfo via known_enum_overrides
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some(
                "The IP protocol name (tcp, udp, icmp, icmpv6) or number.".to_string(),
            ),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::SecurityGroupIngress".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (_, enum_info) = cfn_type_to_carina_type_with_enum(&prop, "IpProtocol", &schema);
        assert!(
            enum_info.is_some(),
            "IpProtocol should produce EnumInfo via overrides"
        );
        let info = enum_info.unwrap();
        assert_eq!(info.type_name, "IpProtocol");
        assert_eq!(info.values, vec!["tcp", "udp", "icmp", "icmpv6", "-1"]);
    }

    #[test]
    fn test_cidr_ip_detected_as_ipv4_cidr() {
        // CidrIp (PascalCase from CloudFormation) should be detected as IPv4 CIDR
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("The IPv4 address range, in CIDR format.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::SecurityGroupIngress".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "CidrIp", &schema);
        assert_eq!(
            type_str, "types::ipv4_cidr()",
            "CidrIp should produce types::ipv4_cidr()"
        );
    }

    #[test]
    fn test_cidr_ipv6_detected_as_ipv6_cidr() {
        // CidrIpv6 (PascalCase from CloudFormation) should be detected as IPv6 CIDR
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("The IPv6 address range, in CIDR format.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::SecurityGroupIngress".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "CidrIpv6", &schema);
        assert_eq!(
            type_str, "types::ipv6_cidr()",
            "CidrIpv6 should produce types::ipv6_cidr()"
        );
    }

    #[test]
    fn test_ip_address_detected_as_ipv4_address() {
        // PrivateIpAddress should be detected as IPv4 address
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("The private IPv4 address.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::NatGateway".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "PrivateIpAddress", &schema);
        assert_eq!(
            type_str, "types::ipv4_address()",
            "PrivateIpAddress should produce types::ipv4_address()"
        );
    }

    #[test]
    fn test_public_ip_detected_as_ipv4_address() {
        // PublicIp should be detected as IPv4 address
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("The public IP address.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::EIP".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "PublicIp", &schema);
        assert_eq!(
            type_str, "types::ipv4_address()",
            "PublicIp should produce types::ipv4_address()"
        );
    }

    #[test]
    fn test_ip_address_count_stays_int() {
        // SecondaryPrivateIpAddressCount should stay Int, not become IP address
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("The number of secondary private IPv4 addresses.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::NatGateway".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) =
            cfn_type_to_carina_type_with_enum(&prop, "SecondaryPrivateIpAddressCount", &schema);
        assert_eq!(
            type_str, "AttributeType::Int",
            "SecondaryPrivateIpAddressCount should stay Int"
        );
    }

    #[test]
    fn test_availability_zone_detected() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("The Availability Zone.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::Subnet".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        // AvailabilityZone should use super::availability_zone()
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "AvailabilityZone", &schema);
        assert_eq!(type_str, "super::availability_zone()");

        // AvailabilityZoneId should stay String
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "AvailabilityZoneId", &schema);
        assert_eq!(type_str, "AttributeType::String");
    }

    #[test]
    fn test_is_aws_resource_id_property() {
        // Known resource ID properties
        assert!(is_aws_resource_id_property("VpcId"));
        assert!(is_aws_resource_id_property("SubnetId"));
        assert!(is_aws_resource_id_property("GroupId"));
        assert!(is_aws_resource_id_property("RouteTableId"));
        assert!(is_aws_resource_id_property("InternetGatewayId"));
        assert!(is_aws_resource_id_property("AllocationId"));
        assert!(is_aws_resource_id_property("NetworkInterfaceId"));
        assert!(is_aws_resource_id_property("InstanceId"));
        assert!(is_aws_resource_id_property("DestinationSecurityGroupId"));
        assert!(is_aws_resource_id_property("DestinationPrefixListId"));
        assert!(is_aws_resource_id_property("VpcEndpointId"));

        // Non-resource ID properties (should stay String)
        assert!(!is_aws_resource_id_property("AvailabilityZoneId"));
        assert!(!is_aws_resource_id_property("SourceSecurityGroupOwnerId"));
        assert!(!is_aws_resource_id_property("ResourceId"));

        // IPAM Pool ID properties should NOT match AwsResourceId
        assert!(!is_aws_resource_id_property("IpamPoolId"));
        assert!(!is_aws_resource_id_property("Ipv4IpamPoolId"));
        assert!(!is_aws_resource_id_property("Ipv6IpamPoolId"));
        assert!(!is_aws_resource_id_property("SourceIpamPoolId"));
    }

    #[test]
    fn test_is_ipam_pool_id_property() {
        // Known IPAM Pool ID properties
        assert!(is_ipam_pool_id_property("IpamPoolId"));
        assert!(is_ipam_pool_id_property("Ipv4IpamPoolId"));
        assert!(is_ipam_pool_id_property("Ipv6IpamPoolId"));
        assert!(is_ipam_pool_id_property("SourceIpamPoolId"));

        // Non-IPAM Pool ID properties
        assert!(!is_ipam_pool_id_property("VpcId"));
        assert!(!is_ipam_pool_id_property("SubnetId"));
        assert!(!is_ipam_pool_id_property("AllocationId"));
    }

    #[test]
    fn test_list_element_type_display() {
        // String items
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: None,
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "GenericProp"),
            "`List<String>`"
        );

        // Integer items
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: None,
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "GenericProp"),
            "`List<Int>`"
        );

        // Boolean items
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("boolean".to_string())),
            description: None,
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "GenericProp"),
            "`List<Bool>`"
        );

        // No type (fallback)
        let prop = CfnProperty {
            prop_type: None,
            description: None,
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "GenericProp"),
            "`List<String>`"
        );
    }

    #[test]
    fn test_list_element_type_display_with_name_inference() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: None,
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "SubnetIds"),
            "`List<SubnetId>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "SecurityGroupIds"),
            "`List<SecurityGroupId>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "RouteTableIds"),
            "`List<RouteTableId>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "NetworkInterfaceIds"),
            "`List<AwsResourceId>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "VpcEndpointIds"),
            "`List<VpcEndpointId>`"
        );
        assert_eq!(list_element_type_display(&prop, "RoleArns"), "`List<Arn>`");
        assert_eq!(
            list_element_type_display(&prop, "CidrBlocks"),
            "`List<Ipv4Cidr>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "Ipv6CidrBlocks"),
            "`List<Ipv6Cidr>`"
        );
        assert_eq!(list_element_type_display(&prop, "Names"), "`List<String>`");
        assert_eq!(
            list_element_type_display(&prop, "SubnetId"),
            "`List<SubnetId>`"
        );
    }

    #[test]
    fn test_type_display_string_array_tag_ref() {
        // Array with Tag $ref should display List<Map>
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            description: None,
            enum_values: None,
            items: Some(Box::new(CfnProperty {
                prop_type: None,
                description: None,
                enum_values: None,
                items: None,
                ref_path: Some("#/definitions/Tag".to_string()),
                insertion_order: None,
                properties: None,
                required: vec![],
                minimum: None,
                maximum: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPC".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let enums = BTreeMap::new();
        assert_eq!(
            type_display_string("ResourceTags", &prop, &schema, &enums),
            "`List<Map>`"
        );
    }

    #[test]
    fn test_type_display_string_array_unresolvable_ref() {
        // Array with $ref that cannot be resolved should display List<String>
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            description: None,
            enum_values: None,
            items: Some(Box::new(CfnProperty {
                prop_type: None,
                description: None,
                enum_values: None,
                items: None,
                ref_path: Some("#/definitions/NonExistent".to_string()),
                insertion_order: None,
                properties: None,
                required: vec![],
                minimum: None,
                maximum: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPC".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let enums = BTreeMap::new();
        assert_eq!(
            type_display_string("Items", &prop, &schema, &enums),
            "`List<String>`"
        );
    }

    #[test]
    fn test_type_display_string_array_no_items() {
        // Array with no items should display List<String>
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            description: None,
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPC".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let enums = BTreeMap::new();
        assert_eq!(
            type_display_string("SomeList", &prop, &schema, &enums),
            "`List<String>`"
        );
    }

    #[test]
    fn test_list_element_type_display_always_includes_element_type() {
        // Regression guard: list_element_type_display must always return "List<...>"
        // with element type info, never bare "List".
        let test_cases: Vec<Option<TypeValue>> = vec![
            Some(TypeValue::Single("string".to_string())),
            Some(TypeValue::Single("integer".to_string())),
            Some(TypeValue::Single("number".to_string())),
            Some(TypeValue::Single("boolean".to_string())),
            Some(TypeValue::Single("object".to_string())),
            Some(TypeValue::Single("unknown".to_string())),
            None,
        ];
        for type_val in test_cases {
            let prop = CfnProperty {
                prop_type: type_val.clone(),
                description: None,
                enum_values: None,
                items: None,
                ref_path: None,
                insertion_order: None,
                properties: None,
                required: vec![],
                minimum: None,
                maximum: None,
            };
            let result = list_element_type_display(&prop, "GenericProp");
            assert!(
                result.contains('<') && result.contains('>'),
                "list_element_type_display should include element type for {:?}, got: {}",
                type_val,
                result
            );
        }
    }

    #[test]
    fn test_type_display_string_array_never_bare_list() {
        // Regression guard: type_display_string for array types must never return
        // bare "List" without element type information.
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPC".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let enums = BTreeMap::new();

        // Case 1: array with no items
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            description: None,
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let result = type_display_string("Prop1", &prop, &schema, &enums);
        assert_ne!(
            result, "List",
            "array with no items should not return bare 'List'"
        );

        // Case 2: array with Tag ref items
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            description: None,
            enum_values: None,
            items: Some(Box::new(CfnProperty {
                prop_type: None,
                description: None,
                enum_values: None,
                items: None,
                ref_path: Some("#/definitions/Tag".to_string()),
                insertion_order: None,
                properties: None,
                required: vec![],
                minimum: None,
                maximum: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let result = type_display_string("Prop2", &prop, &schema, &enums);
        assert_ne!(
            result, "List",
            "array with Tag ref should not return bare 'List'"
        );

        // Case 3: array with unresolvable ref items
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            description: None,
            enum_values: None,
            items: Some(Box::new(CfnProperty {
                prop_type: None,
                description: None,
                enum_values: None,
                items: None,
                ref_path: Some("#/definitions/Missing".to_string()),
                insertion_order: None,
                properties: None,
                required: vec![],
                minimum: None,
                maximum: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let result = type_display_string("Prop3", &prop, &schema, &enums);
        assert_ne!(
            result, "List",
            "array with unresolvable ref should not return bare 'List'"
        );

        // Case 4: array with items that have no type
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            description: None,
            enum_values: None,
            items: Some(Box::new(CfnProperty {
                prop_type: None,
                description: None,
                enum_values: None,
                items: None,
                ref_path: None,
                insertion_order: None,
                properties: None,
                required: vec![],
                minimum: None,
                maximum: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let result = type_display_string("Prop4", &prop, &schema, &enums);
        assert_ne!(
            result, "List",
            "array with typeless items should not return bare 'List'"
        );
    }

    #[test]
    fn test_integer_with_range_produces_custom_type() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("The netmask length.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: Some(0),
            maximum: Some(32),
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPC".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "Ipv4NetmaskLength", &schema);
        assert!(
            type_str.contains("AttributeType::Custom"),
            "Integer with min/max should produce Custom type, got: {}",
            type_str
        );
        assert!(
            type_str.contains("Int(0..=32)"),
            "Custom type name should include range, got: {}",
            type_str
        );
        assert!(
            type_str.contains("validate_ipv4_netmask_length_range"),
            "Should reference range validation function, got: {}",
            type_str
        );
    }

    #[test]
    fn test_integer_without_range_produces_plain_int() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("Some integer value.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPC".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "SomeCount", &schema);
        assert_eq!(type_str, "AttributeType::Int");
    }

    #[test]
    fn test_integer_with_only_minimum_produces_plain_int() {
        // Only minimum set, no maximum - should remain plain Int
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("Some integer value.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: Some(0),
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPC".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "SomeCount", &schema);
        assert_eq!(type_str, "AttributeType::Int");
    }

    #[test]
    fn test_type_display_string_ranged_int_markdown() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("The netmask length.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: Some(0),
            maximum: Some(32),
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPC".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let enums = BTreeMap::new();
        let result = type_display_string("Ipv4NetmaskLength", &prop, &schema, &enums);
        assert_eq!(result, "Int(0..=32)");
    }

    #[test]
    fn test_known_int_range_overrides() {
        let overrides = known_int_range_overrides();
        assert_eq!(overrides.get("Ipv4NetmaskLength"), Some(&(0, 32)));
        assert_eq!(overrides.get("Ipv6NetmaskLength"), Some(&(0, 128)));
        assert_eq!(overrides.get("FromPort"), Some(&(-1, 65535)));
        assert_eq!(overrides.get("ToPort"), Some(&(-1, 65535)));
        assert_eq!(overrides.get("MaxSessionDuration"), Some(&(3600, 43200)));
    }

    #[test]
    fn test_known_string_type_overrides() {
        let overrides = known_string_type_overrides();
        assert_eq!(
            overrides.get("DefaultSecurityGroup"),
            Some(&"super::security_group_id()")
        );
        assert_eq!(
            overrides.get("DeliverLogsPermissionArn"),
            Some(&"super::iam_role_arn()")
        );
        assert_eq!(overrides.get("KmsKeyId"), Some(&"super::kms_key_arn()"));
        assert_eq!(overrides.get("KmsKeyArn"), Some(&"super::kms_key_arn()"));
        assert_eq!(
            overrides.get("KMSMasterKeyID"),
            Some(&"super::kms_key_id()")
        );
        assert_eq!(
            overrides.get("ReplicaKmsKeyID"),
            Some(&"super::kms_key_id()")
        );
        assert_eq!(
            overrides.get("PermissionsBoundary"),
            Some(&"super::iam_policy_arn()")
        );
    }

    #[test]
    fn test_string_type_override_applied() {
        // DefaultSecurityGroup should use security_group_id() via override
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("The ID of the default security group.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPC".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) =
            cfn_type_to_carina_type_with_enum(&prop, "DefaultSecurityGroup", &schema);
        assert_eq!(type_str, "super::security_group_id()");
    }

    #[test]
    fn test_int_range_override_applied() {
        // FromPort without CF min/max should use override (-1..=65535)
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("The start of port range.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::SecurityGroupIngress".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "FromPort", &schema);
        assert!(
            type_str.contains("Int(-1..=65535)"),
            "FromPort should use override range, got: {}",
            type_str
        );
    }

    #[test]
    fn test_ref_fallback_arn_heuristic() {
        // A $ref property named "Arn" with no resolvable definition should use arn()
        let prop = CfnProperty {
            prop_type: None,
            description: None,
            enum_values: None,
            items: None,
            ref_path: Some("#/definitions/Arn".to_string()),
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::S3::Bucket".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "Arn", &schema);
        assert_eq!(type_str, "super::arn()");
    }

    #[test]
    fn test_transit_gateway_enum_overrides() {
        let overrides = known_enum_overrides();
        assert_eq!(
            overrides.get("AutoAcceptSharedAttachments"),
            Some(&vec!["enable", "disable"])
        );
        assert_eq!(
            overrides.get("DnsSupport"),
            Some(&vec!["enable", "disable"])
        );
        assert_eq!(
            overrides.get("VpnEcmpSupport"),
            Some(&vec!["enable", "disable"])
        );
    }

    #[test]
    fn test_number_with_range_produces_custom_type() {
        // "number" type should also support range constraints
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("number".to_string())),
            description: Some("Port number.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: Some(0),
            maximum: Some(65535),
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::SecurityGroupIngress".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let (type_str, _) = cfn_type_to_carina_type_with_enum(&prop, "FromPort", &schema);
        assert!(
            type_str.contains("Int(0..=65535)"),
            "Number with range should include range in type name, got: {}",
            type_str
        );
    }

    #[test]
    fn test_get_resource_id_type_vpc_id() {
        assert_eq!(get_resource_id_type("VpcId"), "super::vpc_id()");
    }

    #[test]
    fn test_get_resource_id_type_subnet_id() {
        assert_eq!(get_resource_id_type("SubnetId"), "super::subnet_id()");
    }

    #[test]
    fn test_get_resource_id_type_security_group_id() {
        assert_eq!(
            get_resource_id_type("SecurityGroupId"),
            "super::security_group_id()"
        );
        assert_eq!(
            get_resource_id_type("DestinationSecurityGroupId"),
            "super::security_group_id()"
        );
        assert_eq!(
            get_resource_id_type("SourceSecurityGroupId"),
            "super::security_group_id()"
        );
        assert_eq!(
            get_resource_id_type("GroupId"),
            "super::security_group_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_egress_only_internet_gateway_id() {
        assert_eq!(
            get_resource_id_type("EgressOnlyInternetGatewayId"),
            "super::egress_only_internet_gateway_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_internet_gateway_id() {
        assert_eq!(
            get_resource_id_type("InternetGatewayId"),
            "super::internet_gateway_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_route_table_id() {
        assert_eq!(
            get_resource_id_type("RouteTableId"),
            "super::route_table_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_nat_gateway_id() {
        assert_eq!(
            get_resource_id_type("NatGatewayId"),
            "super::nat_gateway_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_vpc_peering_connection_id() {
        assert_eq!(
            get_resource_id_type("VpcPeeringConnectionId"),
            "super::vpc_peering_connection_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_transit_gateway_id() {
        assert_eq!(
            get_resource_id_type("TransitGatewayId"),
            "super::transit_gateway_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_vpn_gateway_id() {
        assert_eq!(
            get_resource_id_type("VpnGatewayId"),
            "super::vpn_gateway_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_vpc_endpoint_id() {
        assert_eq!(
            get_resource_id_type("VpcEndpointId"),
            "super::vpc_endpoint_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_non_vpc_endpoint_id() {
        // Regression test for #244: ServiceEndpointId should NOT match VPC Endpoint ID
        // Previously, due to operator precedence, anything ending with "endpointid" matched
        assert_eq!(
            get_resource_id_type("ServiceEndpointId"),
            "super::aws_resource_id()"
        );
    }

    #[test]
    fn test_get_resource_id_type_fallback() {
        assert_eq!(
            get_resource_id_type("SomeUnknownId"),
            "super::aws_resource_id()"
        );
    }

    #[test]
    fn test_get_resource_id_display_name_vpc_id() {
        assert_eq!(get_resource_id_display_name("VpcId"), "VpcId");
    }

    #[test]
    fn test_get_resource_id_display_name_subnet_id() {
        assert_eq!(get_resource_id_display_name("SubnetId"), "SubnetId");
    }

    #[test]
    fn test_get_resource_id_display_name_security_group_id() {
        assert_eq!(
            get_resource_id_display_name("SecurityGroupId"),
            "SecurityGroupId"
        );
        assert_eq!(
            get_resource_id_display_name("DestinationSecurityGroupId"),
            "SecurityGroupId"
        );
        assert_eq!(get_resource_id_display_name("GroupId"), "SecurityGroupId");
    }

    #[test]
    fn test_get_resource_id_display_name_egress_only_internet_gateway_id() {
        assert_eq!(
            get_resource_id_display_name("EgressOnlyInternetGatewayId"),
            "EgressOnlyInternetGatewayId"
        );
    }

    #[test]
    fn test_get_resource_id_display_name_internet_gateway_id() {
        assert_eq!(
            get_resource_id_display_name("InternetGatewayId"),
            "InternetGatewayId"
        );
    }

    #[test]
    fn test_get_resource_id_display_name_route_table_id() {
        assert_eq!(get_resource_id_display_name("RouteTableId"), "RouteTableId");
    }

    #[test]
    fn test_get_resource_id_display_name_nat_gateway_id() {
        assert_eq!(get_resource_id_display_name("NatGatewayId"), "NatGatewayId");
    }

    #[test]
    fn test_get_resource_id_display_name_vpc_peering_connection_id() {
        assert_eq!(
            get_resource_id_display_name("VpcPeeringConnectionId"),
            "VpcPeeringConnectionId"
        );
    }

    #[test]
    fn test_get_resource_id_display_name_transit_gateway_id() {
        assert_eq!(
            get_resource_id_display_name("TransitGatewayId"),
            "TransitGatewayId"
        );
    }

    #[test]
    fn test_get_resource_id_display_name_vpn_gateway_id() {
        assert_eq!(get_resource_id_display_name("VpnGatewayId"), "VpnGatewayId");
    }

    #[test]
    fn test_get_resource_id_display_name_vpc_endpoint_id() {
        assert_eq!(
            get_resource_id_display_name("VpcEndpointId"),
            "VpcEndpointId"
        );
    }

    #[test]
    fn test_get_resource_id_display_name_non_vpc_endpoint_id() {
        // Regression test for #244: ServiceEndpointId should NOT match VPC Endpoint ID
        assert_eq!(
            get_resource_id_display_name("ServiceEndpointId"),
            "AwsResourceId"
        );
    }

    #[test]
    fn test_get_resource_id_display_name_fallback() {
        assert_eq!(
            get_resource_id_display_name("SomeUnknownId"),
            "AwsResourceId"
        );
    }

    #[test]
    fn test_classify_resource_id() {
        assert_eq!(classify_resource_id("VpcId"), ResourceIdKind::VpcId);
        assert_eq!(classify_resource_id("SubnetId"), ResourceIdKind::SubnetId);
        assert_eq!(
            classify_resource_id("SecurityGroupId"),
            ResourceIdKind::SecurityGroupId
        );
        assert_eq!(
            classify_resource_id("GroupId"),
            ResourceIdKind::SecurityGroupId
        );
        assert_eq!(
            classify_resource_id("EgressOnlyInternetGatewayId"),
            ResourceIdKind::EgressOnlyInternetGatewayId
        );
        assert_eq!(
            classify_resource_id("InternetGatewayId"),
            ResourceIdKind::InternetGatewayId
        );
        assert_eq!(
            classify_resource_id("RouteTableId"),
            ResourceIdKind::RouteTableId
        );
        assert_eq!(
            classify_resource_id("NatGatewayId"),
            ResourceIdKind::NatGatewayId
        );
        assert_eq!(
            classify_resource_id("VpcPeeringConnectionId"),
            ResourceIdKind::VpcPeeringConnectionId
        );
        assert_eq!(
            classify_resource_id("TransitGatewayId"),
            ResourceIdKind::TransitGatewayId
        );
        assert_eq!(
            classify_resource_id("VpnGatewayId"),
            ResourceIdKind::VpnGatewayId
        );
        assert_eq!(
            classify_resource_id("VpcEndpointId"),
            ResourceIdKind::VpcEndpointId
        );
        assert_eq!(
            classify_resource_id("SomeUnknownId"),
            ResourceIdKind::Generic
        );
        // Regression: ServiceEndpointId should NOT match VpcEndpointId
        assert_eq!(
            classify_resource_id("ServiceEndpointId"),
            ResourceIdKind::Generic
        );
    }

    #[test]
    fn test_classify_resource_id_type_and_display_name_consistency() {
        // Verify that get_resource_id_type and get_resource_id_display_name
        // agree on classification for all test inputs
        let test_inputs = [
            "VpcId",
            "SubnetId",
            "SecurityGroupId",
            "DestinationSecurityGroupId",
            "GroupId",
            "EgressOnlyInternetGatewayId",
            "InternetGatewayId",
            "RouteTableId",
            "NatGatewayId",
            "VpcPeeringConnectionId",
            "TransitGatewayId",
            "VpnGatewayId",
            "VpcEndpointId",
            "ServiceEndpointId",
            "SomeUnknownId",
        ];

        for input in &test_inputs {
            let kind = classify_resource_id(input);
            let is_generic = kind == ResourceIdKind::Generic;
            let type_is_generic = get_resource_id_type(input) == "super::aws_resource_id()";
            let display_is_generic = get_resource_id_display_name(input) == "AwsResourceId";
            assert_eq!(
                is_generic, type_is_generic,
                "Mismatch for {input}: classify says generic={is_generic}, type says generic={type_is_generic}"
            );
            assert_eq!(
                is_generic, display_is_generic,
                "Mismatch for {input}: classify says generic={is_generic}, display says generic={display_is_generic}"
            );
        }
    }
}
