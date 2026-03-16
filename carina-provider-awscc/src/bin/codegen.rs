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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{self, Read};
use std::sync::LazyLock;

/// Unified type override for resource-scoped property overrides.
/// Allows overriding string type, enum values, integer range, or integer enum
/// for a specific (resource_type, property_name) pair.
#[derive(Debug, Clone, PartialEq)]
enum TypeOverride {
    /// Override to a specific string type (e.g., "super::iam_role_arn()")
    StringType(&'static str),
    /// Override to an enum with specific values
    Enum(Vec<&'static str>),
    /// Override to an integer range (min, max)
    IntRange(i64, i64),
    /// Override to an integer enum with specific allowed values
    IntEnum(Vec<i64>),
}

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

    /// Print DSL resource name (service.resource) for the given type and exit
    /// e.g., AWS::EC2::SecurityGroupEgress -> ec2.security_group_egress
    #[arg(long)]
    print_dsl_resource_name: bool,

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

#[derive(Debug, Clone, Default, Deserialize)]
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
    /// Whether additional properties are allowed (for object types)
    #[serde(default)]
    additional_properties: Option<bool>,
    /// Const value (single fixed value for this property)
    #[serde(rename = "const")]
    const_value: Option<serde_json::Value>,
    /// Default value for this property
    #[serde(rename = "default")]
    default_value: Option<serde_json::Value>,
    /// Regex pattern constraint (for string types)
    #[serde(default)]
    pattern: Option<String>,
    /// Minimum number of items (for array types)
    #[serde(default)]
    min_items: Option<i64>,
    /// Maximum number of items (for array types)
    #[serde(default)]
    max_items: Option<i64>,
    /// Minimum length constraint (for string types)
    #[serde(default, rename = "minLength")]
    min_length: Option<u64>,
    /// Maximum length constraint (for string types)
    #[serde(default, rename = "maxLength")]
    max_length: Option<u64>,
    /// Format constraint (e.g., "int64", "uri", "double")
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CfnOneOfVariant {
    properties: Option<BTreeMap<String, CfnProperty>>,
    #[serde(default)]
    required: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CfnDefinition {
    #[serde(rename = "type")]
    def_type: Option<String>,
    properties: Option<BTreeMap<String, CfnProperty>>,
    #[serde(default)]
    required: Vec<String>,
    /// oneOf variants (union types)
    #[serde(default, rename = "oneOf")]
    one_of: Vec<CfnOneOfVariant>,
    /// Array item schema (for array-typed definitions)
    items: Option<Box<CfnProperty>>,
    /// Enum values (for enum-only definitions)
    #[serde(rename = "enum")]
    enum_values: Option<Vec<EnumValue>>,
    /// Regex pattern constraint (for string-typed definitions)
    #[serde(default)]
    pattern: Option<String>,
    /// Minimum string length constraint (for string-typed definitions)
    #[serde(default, rename = "minLength")]
    min_length: Option<u64>,
    /// Maximum string length constraint (for string-typed definitions)
    #[serde(default, rename = "maxLength")]
    max_length: Option<u64>,
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
/// Used for Rust module names, file names, and function names.
fn full_resource_name_from_type(type_name: &str) -> Result<String> {
    let parts: Vec<&str> = type_name.split("::").collect();
    if parts.len() != 3 {
        anyhow::bail!("Invalid type name format: {}", type_name);
    }
    let service = parts[1].to_lowercase();
    let resource = parts[2].to_snake_case();
    Ok(format!("{}_{}", service, resource))
}

/// Compute DSL resource name (service.resource) from CloudFormation type name
/// e.g., "AWS::EC2::SecurityGroupEgress" -> "ec2.security_group_egress"
/// Used for DSL-facing strings (resource_type_name, namespace, display).
fn dsl_resource_name_from_type(type_name: &str) -> Result<String> {
    let parts: Vec<&str> = type_name.split("::").collect();
    if parts.len() != 3 {
        anyhow::bail!("Invalid type name format: {}", type_name);
    }
    let service = parts[1].to_lowercase();
    let resource = parts[2].to_snake_case();
    Ok(format!("{}.{}", service, resource))
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

    if args.print_dsl_resource_name {
        println!("{}", dsl_resource_name_from_type(&args.type_name)?);
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
fn list_element_type_display(items: &CfnProperty, prop_name: &str, resource_type: &str) -> String {
    match items.prop_type.as_ref().and_then(|t| t.as_str()) {
        Some("string") => {
            let element_type = infer_string_type_display(prop_name, resource_type);
            format!("`List<{}>`", element_type)
        }
        Some("integer") => "`List<Int>`".to_string(),
        Some("number") => "`List<Float>`".to_string(),
        Some("boolean") => "`List<Bool>`".to_string(),
        _ => "`List<String>`".to_string(),
    }
}

/// Infer a display type name for a string property based on its name.
/// Used by both scalar `type_display_string()` and `list_element_type_display()`.
/// Handles both singular (e.g., "SubnetId") and plural (e.g., "SubnetIds") property names.
fn infer_string_type_display(prop_name: &str, resource_type: &str) -> String {
    // Check resource-specific overrides first (StringType only for display)
    if let Some(TypeOverride::StringType(override_type)) =
        resource_type_overrides().get(&(resource_type, prop_name))
    {
        return override_type_to_display_name(override_type).to_string();
    }
    // Check known string type overrides
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
    } else if prop_lower == "availabilityzoneid" || prop_lower == "availabilityzoneids" {
        "AvailabilityZoneId".to_string()
    } else if prop_lower.ends_with("region") || prop_lower == "regionname" {
        "Region".to_string()
    } else if prop_lower.ends_with("arn")
        || prop_lower.ends_with("arns")
        || prop_lower.contains("_arn")
    {
        "Arn".to_string()
    } else if is_ipam_pool_id_property(singular_name) {
        "IpamPoolId".to_string()
    } else if is_aws_resource_id_property(singular_name) {
        get_resource_id_display_name(singular_name).to_string()
    } else if prop_lower.ends_with("ownerid") || prop_lower.ends_with("accountid") {
        "AwsAccountId".to_string()
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
        "super::gateway_id()" => "GatewayId",
        "super::network_acl_id()" => "NetworkAclId",
        "super::aws_account_id()" => "AwsAccountId",
        "super::instance_id()" => "InstanceId",
        "super::network_interface_id()" => "NetworkInterfaceId",
        "super::allocation_id()" => "AllocationId",
        "super::prefix_list_id()" => "PrefixListId",
        "super::carrier_gateway_id()" => "CarrierGatewayId",
        "super::local_gateway_id()" => "LocalGatewayId",
        "super::egress_only_internet_gateway_id()" => "EgressOnlyInternetGatewayId",
        "super::transit_gateway_id()" => "TransitGatewayId",
        "super::vpc_peering_connection_id()" => "VpcPeeringConnectionId",
        "super::vpc_endpoint_id()" => "VpcEndpointId",
        "super::transit_gateway_attachment_id()" => "TransitGatewayAttachmentId",
        "super::flow_log_id()" => "FlowLogId",
        "super::subnet_route_table_association_id()" => "SubnetRouteTableAssociationId",
        "super::ipam_id()" => "IpamId",
        "super::iam_role_id()" => "IamRoleId",
        "super::awscc_region()" => "Region",
        "types::ipv4_address()" => "Ipv4Address",
        "super::arn()" => "Arn",
        "super::ipam_pool_id()" => "IpamPoolId",
        "super::vpc_cidr_block_association_id()" => "VpcCidrBlockAssociationId",
        "super::tgw_route_table_id()" => "TgwRouteTableId",
        "types::cidr()" => "Cidr",
        "AttributeType::String" => "String",
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
        let enum_link = format!(
            "[Enum ({})](#{}-{})",
            enums[prop_name].type_name,
            prop_name.to_snake_case(),
            enums[prop_name].type_name.to_lowercase()
        );
        // Check if property is an array type — if so, wrap as List<Enum>
        let is_array = prop
            .prop_type
            .as_ref()
            .and_then(|t| t.as_str())
            .map(|t| t == "array")
            .unwrap_or(false);
        let is_ref_array = prop
            .ref_path
            .as_ref()
            .and_then(|ref_path| resolve_ref(schema, ref_path))
            .map(|def| def.def_type.as_deref() == Some("array"))
            .unwrap_or(false);
        if is_array || is_ref_array {
            format!("List\\<{}\\>", enum_link)
        } else {
            enum_link
        }
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
        } else if let Some(def_name) = ref_def_name(ref_path)
            && resolve_ref(schema, ref_path)
                .map(|d| !d.one_of.is_empty())
                .unwrap_or(false)
        {
            format!("[Struct({})](#{})", def_name, def_name.to_lowercase())
        } else {
            // Apply name-based heuristics for unresolvable $ref
            infer_string_type_display(prop_name, &schema.type_name)
        }
    } else {
        match prop.prop_type.as_ref().and_then(|t| t.as_str()) {
            Some("string") => {
                if prop_name.ends_with("PolicyDocument") {
                    "IamPolicyDocument".to_string()
                } else {
                    let base = infer_string_type_display(prop_name, &schema.type_name);
                    if base != "String" {
                        return base;
                    }
                    let effective_min = prop.min_length.filter(|&m| m > 0);
                    let has_length = effective_min.is_some() || prop.max_length.is_some();
                    // Check for numeric string pattern
                    if let Some(ref pattern) = prop.pattern
                        && is_numeric_string_pattern(pattern)
                    {
                        return if has_length {
                            let range = string_length_display(effective_min, prop.max_length);
                            format!("NumericString(len: {})", range)
                        } else {
                            "NumericString".to_string()
                        };
                    }
                    // Check for pattern + length combination
                    if prop.pattern.is_some() && has_length {
                        let range = string_length_display(effective_min, prop.max_length);
                        return format!("String(pattern, len: {})", range);
                    }
                    // Check for string format constraint
                    if let Some(ref fmt) = prop.format {
                        return format!("String({})", fmt);
                    }
                    // Append length constraint if present and type is plain String
                    if has_length && prop.enum_values.is_none() {
                        let range = string_length_display(effective_min, prop.max_length);
                        format!("String(len: {})", range)
                    } else {
                        base
                    }
                }
            }
            Some("boolean") => "Bool".to_string(),
            Some("integer") => {
                // Check resource-scoped overrides first
                let res_override =
                    resource_type_overrides().get(&(schema.type_name.as_str(), prop_name));
                if let Some(TypeOverride::IntEnum(values)) = res_override {
                    let values_str = values
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return format!("IntEnum([{}])", values_str);
                }
                if let Some(TypeOverride::IntRange(min, max)) = res_override {
                    return format!("Int({})", range_display_string(Some(*min), Some(*max)));
                }
                // Check for integer enum values from schema
                if let Some(enum_values) = &prop.enum_values {
                    let all_ints = enum_values.iter().all(|v| matches!(v, EnumValue::Int(_)));
                    if all_ints && !enum_values.is_empty() {
                        let values_str = enum_values
                            .iter()
                            .map(|v| v.to_string_value())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return format!("IntEnum([{}])", values_str);
                    }
                }
                let range: Option<(Option<i64>, Option<i64>)> =
                    if prop.minimum.is_some() || prop.maximum.is_some() {
                        Some((prop.minimum, prop.maximum))
                    } else {
                        known_int_range_overrides()
                            .get(prop_name)
                            .map(|&(min, max)| (Some(min), Some(max)))
                    };
                if let Some((min, max)) = range {
                    let range_str = range_display_string(min, max);
                    if let Some(ref fmt) = prop.format {
                        format!("Int({}, {})", range_str, fmt)
                    } else {
                        format!("Int({})", range_str)
                    }
                } else if let Some(ref fmt) = prop.format {
                    format!("Int({})", fmt)
                } else {
                    "Int".to_string()
                }
            }
            Some("number") => {
                // Check resource-scoped overrides first
                let res_override =
                    resource_type_overrides().get(&(schema.type_name.as_str(), prop_name));
                if let Some(TypeOverride::IntRange(min, max)) = res_override {
                    return format!("Float({})", range_display_string(Some(*min), Some(*max)));
                }
                let range: Option<(Option<i64>, Option<i64>)> =
                    if prop.minimum.is_some() || prop.maximum.is_some() {
                        Some((prop.minimum, prop.maximum))
                    } else {
                        known_int_range_overrides()
                            .get(prop_name)
                            .map(|&(min, max)| (Some(min), Some(max)))
                    };
                if let Some((min, max)) = range {
                    let range_str = range_display_string(min, max);
                    if let Some(ref fmt) = prop.format {
                        format!("Float({}, {})", range_str, fmt)
                    } else {
                        format!("Float({})", range_str)
                    }
                } else if let Some(ref fmt) = prop.format {
                    format!("Float({})", fmt)
                } else {
                    "Float".to_string()
                }
            }
            Some("array") => {
                let base_display = if let Some(items) = &prop.items {
                    if let Some(ref_path) = &items.ref_path {
                        if !ref_path.contains("/Tag") {
                            if let Some(def_name) = ref_def_name(ref_path)
                                && resolve_ref(schema, ref_path)
                                    .and_then(|d| d.properties.as_ref())
                                    .map(|p| !p.is_empty())
                                    .unwrap_or(false)
                            {
                                format!("[List\\<{}\\>](#{})", def_name, def_name.to_lowercase())
                            } else if enums.contains_key(prop_name) {
                                format!(
                                    "List\\<[Enum ({})](#{}-{})\\>",
                                    enums[prop_name].type_name,
                                    prop_name.to_snake_case(),
                                    enums[prop_name].type_name.to_lowercase()
                                )
                            } else {
                                "`List<String>`".to_string()
                            }
                        } else {
                            "`List<Map>`".to_string()
                        }
                    } else {
                        list_element_type_display(items, prop_name, &schema.type_name)
                    }
                } else {
                    "`List<String>`".to_string()
                };
                // Append item count constraints if present
                if prop.min_items.is_some() || prop.max_items.is_some() {
                    let constraint = range_display_string(prop.min_items, prop.max_items);
                    format!("{} (items: {})", base_display, constraint)
                } else {
                    base_display
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

    let dsl_resource = dsl_resource_name_from_type(type_name)?;
    let namespace = format!("awscc.{}", dsl_resource);

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
        let (_, enum_info) =
            cfn_type_to_carina_type_with_enum(prop, prop_name, schema, &namespace, &enums);
        if let Some(info) = enum_info {
            enums.insert(prop_name.clone(), info);
        }
        // Collect struct definitions from $ref
        collect_struct_defs(prop, prop_name, schema, &mut struct_defs);
    }

    // Scan struct definition fields for enum info (const values, $ref to enum-only definitions)
    for (def_name, def_info) in &struct_defs {
        for (field_name, field_prop) in &def_info.properties {
            let (_, enum_info) = cfn_type_to_carina_type_with_enum(
                field_prop, field_name, schema, &namespace, &enums,
            );
            if let Some(info) = enum_info {
                let composite_key = format!("{}.{}", def_name, field_name);
                enums.insert(composite_key, info);
            }
        }
    }

    disambiguate_enum_type_names(&mut enums);

    // Title
    md.push_str(&format!("# awscc.{}\n\n", dsl_resource));
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
        if let Some(default_val) = &prop.default_value
            && let Some(display) = json_default_to_markdown(default_val)
        {
            md.push_str(&format!("- **Default:** `{}`\n", display));
        }
        md.push('\n');

        if let Some(d) = &prop.description {
            let desc = collapse_whitespace(&d.replace('\n', " "));
            md.push_str(&format!("{}\n\n", desc));
        }
    }

    // Enum values section
    if !enums.is_empty() {
        let aliases = known_enum_aliases();
        md.push_str("## Enum Values\n\n");
        let mut rendered_enums: HashSet<(String, Vec<String>)> = HashSet::new();
        for (prop_name, enum_info) in &enums {
            // Skip duplicate enum types with identical values
            // (e.g., Ingress.IpProtocol and Egress.IpProtocol share the same type and values)
            let key = (enum_info.type_name.clone(), enum_info.values.clone());
            if !rendered_enums.insert(key) {
                continue;
            }
            // For composite keys "DefName.FieldName", use just "FieldName" for display
            let field_name = prop_name
                .split('.')
                .next_back()
                .unwrap_or(prop_name.as_str());
            let attr_name = field_name.to_snake_case();
            let has_hyphens = enum_info.values.iter().any(|v| v.contains('-'));
            let prop_aliases = aliases.get(field_name);
            md.push_str(&format!("### {} ({})\n\n", attr_name, enum_info.type_name));
            md.push_str("| Value | DSL Identifier |\n");
            md.push_str("|-------|----------------|\n");
            for value in &enum_info.values {
                // Check if this value has an alias
                let dsl_value = if let Some(alias_list) = prop_aliases {
                    if let Some((_, alias)) = alias_list.iter().find(|(c, _)| c == value) {
                        alias.to_string()
                    } else if has_hyphens {
                        value.replace('-', "_")
                    } else {
                        value.clone()
                    }
                } else if has_hyphens {
                    value.replace('-', "_")
                } else {
                    value.clone()
                };
                let dsl_id = format!("{}.{}.{}", namespace, enum_info.type_name, dsl_value);
                md.push_str(&format!("| `{}` | `{}` |\n", value, dsl_id));
            }
            md.push('\n');
            let empty = String::new();
            let first_value = enum_info.values.first().unwrap_or(&empty);
            let first_dsl = if let Some(alias_list) = prop_aliases {
                if let Some((_, alias)) = alias_list.iter().find(|(c, _)| c == first_value) {
                    alias.to_string()
                } else if has_hyphens {
                    first_value.replace('-', "_")
                } else {
                    first_value.clone()
                }
            } else if has_hyphens {
                first_value.replace('-', "_")
            } else {
                first_value.clone()
            };
            md.push_str(&format!(
                "Shorthand formats: `{}` or `{}.{}`\n\n",
                first_dsl, enum_info.type_name, first_dsl,
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
                let composite_key = format!("{}.{}", def_info.def_name, field_name);
                let field_type_display: String = if enums.contains_key(&composite_key) {
                    let enum_info = &enums[&composite_key];
                    let enum_link = format!(
                        "[Enum ({})](#{}-{})",
                        enum_info.type_name,
                        snake_name,
                        enum_info.type_name.to_lowercase()
                    );
                    let is_array = field_prop
                        .prop_type
                        .as_ref()
                        .and_then(|t| t.as_str())
                        .map(|t| t == "array")
                        .unwrap_or(false);
                    if is_array {
                        format!("List\\<{}\\>", enum_link)
                    } else {
                        enum_link
                    }
                } else if overrides.contains_key(field_name.as_str()) {
                    "Enum".to_string()
                } else {
                    type_display_string(field_name, field_prop, schema, &enums)
                };
                let desc = collapse_whitespace(
                    &field_prop
                        .description
                        .as_deref()
                        .unwrap_or("")
                        .replace('\n', " "),
                );
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
    {
        if let Some(props) = &def.properties
            && !props.is_empty()
        {
            if !struct_defs.contains_key(def_name) {
                struct_defs.insert(
                    def_name.to_string(),
                    StructDefInfo {
                        def_name: def_name.to_string(),
                        properties: props.clone(),
                        required: def.required.clone(),
                    },
                );
                // Recursively collect nested struct defs
                for (field_name, field_prop) in props {
                    collect_struct_defs(field_prop, field_name, schema, struct_defs);
                }
            }
        } else if !def.one_of.is_empty() {
            // Merge oneOf variant properties into a single struct
            let mut merged_props = BTreeMap::new();
            for variant in &def.one_of {
                if let Some(props) = &variant.properties {
                    for (k, v) in props {
                        merged_props.insert(k.clone(), v.clone());
                    }
                }
            }
            if !merged_props.is_empty() && !struct_defs.contains_key(def_name) {
                struct_defs.insert(
                    def_name.to_string(),
                    StructDefInfo {
                        def_name: def_name.to_string(),
                        properties: merged_props.clone(),
                        required: vec![], // oneOf variants are mutually exclusive
                    },
                );
                // Recursively collect struct defs from merged properties
                for (field_name, field_prop) in &merged_props {
                    collect_struct_defs(field_prop, field_name, schema, struct_defs);
                }
            }
        }
    }
    // Handle array items with $ref
    if let Some(items) = &prop.items
        && let Some(ref_path) = &items.ref_path
        && !ref_path.contains("/Tag")
        && let Some(def_name) = ref_def_name(ref_path)
        && let Some(def) = resolve_ref(schema, ref_path)
        && let Some(props) = &def.properties
        && !props.is_empty()
        && !struct_defs.contains_key(def_name)
    {
        struct_defs.insert(
            def_name.to_string(),
            StructDefInfo {
                def_name: def_name.to_string(),
                properties: props.clone(),
                required: def.required.clone(),
            },
        );
        // Recursively collect nested struct defs
        for (field_name, field_prop) in props {
            collect_struct_defs(field_prop, field_name, schema, struct_defs);
        }
    }
    // Handle inline object with properties
    if let Some(type_val) = &prop.prop_type
        && type_val.as_str() == Some("object")
        && let Some(props) = &prop.properties
        && !props.is_empty()
        && !struct_defs.contains_key(prop_name)
    {
        struct_defs.insert(
            prop_name.to_string(),
            StructDefInfo {
                def_name: prop_name.to_string(),
                properties: props.clone(),
                required: prop.required.clone(),
            },
        );
        // Recursively collect nested struct defs
        for (field_name, field_prop) in props {
            collect_struct_defs(field_prop, field_name, schema, struct_defs);
        }
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
    let dsl_resource = dsl_resource_name_from_type(type_name)?;
    // Namespace for enums: awscc.ec2.vpc
    let namespace = format!("awscc.{}", dsl_resource);

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
    let mut ranged_ints: BTreeMap<String, (Option<i64>, Option<i64>)> = BTreeMap::new();
    let mut ranged_floats: BTreeMap<String, (Option<i64>, Option<i64>)> = BTreeMap::new();
    let mut int_enums: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut ranged_lists: BTreeMap<String, (Option<i64>, Option<i64>)> = BTreeMap::new();
    let mut patterns: BTreeMap<String, String> = BTreeMap::new();
    let mut ranged_strings: BTreeMap<String, (Option<u64>, Option<u64>)> = BTreeMap::new();
    // Combined pattern + length constraints: (pattern, min_length, max_length) set
    let mut pattern_with_lengths: BTreeSet<(String, Option<u64>, Option<u64>)> = BTreeSet::new();
    // Patterns that are used standalone (without length constraints)
    let mut patterns_used_standalone: HashSet<String> = HashSet::new();

    for (prop_name, prop) in &schema.properties {
        let (attr_type, enum_info) =
            cfn_type_to_carina_type_with_enum(prop, prop_name, schema, &namespace, &enums);
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
        // Check resource-scoped overrides for enum, int range, and int enum
        let resource_override = resource_type_overrides().get(&(type_name, prop_name.as_str()));
        if let Some(TypeOverride::Enum(values)) = resource_override {
            enums.insert(
                prop_name.clone(),
                EnumInfo {
                    type_name: prop_name.clone(),
                    values: values.iter().map(|v| v.to_string()).collect(),
                },
            );
        }
        // Collect ranged integer/number properties (including one-sided ranges)
        match prop.prop_type.as_ref().and_then(|t| t.as_str()) {
            Some("integer") => {
                if let Some(TypeOverride::IntRange(min, max)) = resource_override {
                    ranged_ints.insert(prop_name.clone(), (Some(*min), Some(*max)));
                } else if let Some(TypeOverride::IntEnum(values)) = resource_override {
                    int_enums.insert(prop_name.clone(), values.clone());
                } else if prop.minimum.is_some() || prop.maximum.is_some() {
                    ranged_ints.insert(prop_name.clone(), (prop.minimum, prop.maximum));
                } else if let Some(&(min, max)) =
                    known_int_range_overrides().get(prop_name.as_str())
                {
                    ranged_ints.insert(prop_name.clone(), (Some(min), Some(max)));
                }
                // Collect integer enum values (only if no resource-scoped override)
                if resource_override.is_none()
                    && let Some(enum_values) = &prop.enum_values
                {
                    let values: Vec<i64> = enum_values
                        .iter()
                        .filter_map(|v| match v {
                            EnumValue::Int(i) => Some(*i),
                            _ => None,
                        })
                        .collect();
                    if !values.is_empty()
                        && enum_values.iter().all(|v| matches!(v, EnumValue::Int(_)))
                    {
                        int_enums.insert(prop_name.clone(), values);
                    }
                }
            }
            Some("number") => {
                if let Some(TypeOverride::IntRange(min, max)) = resource_override {
                    ranged_floats.insert(prop_name.clone(), (Some(*min), Some(*max)));
                } else if prop.minimum.is_some() || prop.maximum.is_some() {
                    ranged_floats.insert(prop_name.clone(), (prop.minimum, prop.maximum));
                } else if let Some(&(min, max)) =
                    known_int_range_overrides().get(prop_name.as_str())
                {
                    ranged_floats.insert(prop_name.clone(), (Some(min), Some(max)));
                }
            }
            Some("array") => {
                if prop.min_items.is_some() || prop.max_items.is_some() {
                    ranged_lists.insert(prop_name.clone(), (prop.min_items, prop.max_items));
                }
            }
            Some("string") => {
                // Collect string length constraints (only for plain strings without
                // enum values or type overrides that would take precedence)
                // Treat minLength=0 as no constraint since usize is always >= 0
                let effective_min = prop.min_length.filter(|&m| m > 0);
                if effective_min.is_some() || prop.max_length.is_some() {
                    let has_desc_enum = prop
                        .description
                        .as_ref()
                        .and_then(|d| extract_enum_from_description(d))
                        .is_some();
                    if prop.enum_values.is_none()
                        && !enums.contains_key(prop_name)
                        && !has_desc_enum
                        && infer_string_type(prop_name, &schema.type_name).is_none()
                        && !prop_name.ends_with("PolicyDocument")
                        && prop_name != "Tags"
                        && prop.pattern.is_none()
                    {
                        ranged_strings.insert(prop_name.clone(), (effective_min, prop.max_length));
                    }
                }
            }
            _ => {}
        }
        // Collect pattern constraints for string properties
        if attr_type.contains("validate_") && attr_type.contains("_pattern") {
            // Pattern can come from the property directly or from a $ref definition
            let pattern = prop.pattern.clone().or_else(|| {
                prop.ref_path
                    .as_ref()
                    .and_then(|ref_path| resolve_ref(schema, ref_path))
                    .and_then(|def| def.pattern.clone())
            });
            if let Some(pattern) = pattern {
                // Check if this pattern also has length constraints
                let effective_min = prop.min_length.filter(|&m| m > 0);
                let has_length = effective_min.is_some() || prop.max_length.is_some();
                if has_length {
                    pattern_with_lengths.insert((pattern.clone(), effective_min, prop.max_length));
                } else {
                    patterns_used_standalone.insert(pattern.clone());
                }
                patterns.insert(prop_name.clone(), pattern);
            }
        }
    }

    // Also scan definitions for struct field integer/number properties
    let int_overrides = known_int_range_overrides();
    if let Some(definitions) = &schema.definitions {
        for def in definitions.values() {
            if let Some(props) = &def.properties {
                for (field_name, field_prop) in props {
                    let prop_type = field_prop.prop_type.as_ref().and_then(|t| t.as_str());
                    if matches!(prop_type, Some("integer") | Some("number")) {
                        // Collect ranges from definitions (including one-sided ranges)
                        if field_prop.minimum.is_some() || field_prop.maximum.is_some() {
                            if prop_type == Some("number") {
                                if !ranged_floats.contains_key(field_name) {
                                    ranged_floats.insert(
                                        field_name.clone(),
                                        (field_prop.minimum, field_prop.maximum),
                                    );
                                }
                            } else if !ranged_ints.contains_key(field_name) {
                                ranged_ints.insert(
                                    field_name.clone(),
                                    (field_prop.minimum, field_prop.maximum),
                                );
                            }
                        } else if int_overrides.contains_key(field_name.as_str()) {
                            // Fall back to known overrides
                            let (min, max) = int_overrides[field_name.as_str()];
                            if prop_type == Some("number") {
                                if !ranged_floats.contains_key(field_name) {
                                    ranged_floats
                                        .insert(field_name.clone(), (Some(min), Some(max)));
                                }
                            } else if !ranged_ints.contains_key(field_name) {
                                ranged_ints.insert(field_name.clone(), (Some(min), Some(max)));
                            }
                        }
                    }
                    // Collect array item count constraints from definitions
                    if prop_type == Some("array")
                        && (field_prop.min_items.is_some() || field_prop.max_items.is_some())
                        && !ranged_lists.contains_key(field_name)
                    {
                        ranged_lists.insert(
                            field_name.clone(),
                            (field_prop.min_items, field_prop.max_items),
                        );
                    }
                    // Also collect integer enums from definitions
                    if prop_type == Some("integer")
                        && let Some(enum_values) = &field_prop.enum_values
                    {
                        let values: Vec<i64> = enum_values
                            .iter()
                            .filter_map(|v| match v {
                                EnumValue::Int(i) => Some(*i),
                                _ => None,
                            })
                            .collect();
                        if !values.is_empty()
                            && enum_values.iter().all(|v| matches!(v, EnumValue::Int(_)))
                            && !int_enums.contains_key(field_name)
                        {
                            int_enums.insert(field_name.clone(), values);
                        }
                    }
                    // Also collect pattern constraints from definitions
                    if prop_type == Some("string")
                        && field_prop.pattern.is_some()
                        && !patterns.contains_key(field_name)
                    {
                        // Check if this field would actually use a pattern type
                        // (skip if it has a known type override)
                        let (field_type, _) = cfn_type_to_carina_type_with_enum(
                            field_prop, field_name, schema, &namespace, &enums,
                        );
                        if field_type.contains("validate_") && field_type.contains("_pattern") {
                            let pattern = field_prop.pattern.clone().unwrap();
                            // Check if this pattern also has length constraints
                            let effective_min = field_prop.min_length.filter(|&m| m > 0);
                            let has_length =
                                effective_min.is_some() || field_prop.max_length.is_some();
                            if has_length {
                                pattern_with_lengths.insert((
                                    pattern.clone(),
                                    effective_min,
                                    field_prop.max_length,
                                ));
                            } else {
                                patterns_used_standalone.insert(pattern.clone());
                            }
                            patterns.insert(field_name.clone(), pattern);
                        }
                    }
                    // Collect pattern constraints from $ref to string definitions with patterns
                    if let Some(ref_path) = &field_prop.ref_path
                        && let Some(ref_def) = resolve_ref(schema, ref_path)
                        && ref_def.def_type.as_deref() == Some("string")
                        && let Some(pattern) = &ref_def.pattern
                        && !patterns.contains_key(field_name)
                    {
                        let (field_type, _) = cfn_type_to_carina_type_with_enum(
                            field_prop, field_name, schema, &namespace, &enums,
                        );
                        if field_type.contains("validate_") && field_type.contains("_pattern") {
                            // Check length constraints from the $ref definition
                            let effective_min = ref_def.min_length.filter(|&m| m > 0);
                            let has_length =
                                effective_min.is_some() || ref_def.max_length.is_some();
                            if has_length {
                                pattern_with_lengths.insert((
                                    pattern.clone(),
                                    effective_min,
                                    ref_def.max_length,
                                ));
                            } else {
                                patterns_used_standalone.insert(pattern.clone());
                            }
                            patterns.insert(field_name.clone(), pattern.clone());
                        }
                    }
                }
            }
        }
    }

    // Also scan definitions for struct field string length constraints
    // (including array items with string length constraints)
    // Skip Tag definitions since Tags are handled specially by tags_type()
    if let Some(definitions) = &schema.definitions {
        for (def_name, def) in definitions {
            if def_name.contains("Tag") {
                continue;
            }
            if let Some(props) = &def.properties {
                for (field_name, field_prop) in props {
                    collect_string_length_constraints(field_name, field_prop, &mut ranged_strings);
                }
            }
        }
    }
    // Also scan top-level array items for string length constraints
    for (prop_name, prop) in &schema.properties {
        if let Some(items) = &prop.items {
            collect_string_length_constraints(prop_name, items, &mut ranged_strings);
        }
    }

    // Also scan definitions for struct field enum properties
    // Use composite key "DefName.FieldName" to avoid conflicts when different
    // definitions have fields with the same name but different enum values
    // (e.g., IntelligentTieringConfiguration.Status vs VersioningConfiguration.Status)
    if let Some(definitions) = &schema.definitions {
        // Build set of existing snake_case names from top-level properties
        // (e.g., "SSEAlgorithm" and "SseAlgorithm" both become "sse_algorithm")
        let existing_snake: HashSet<String> = enums.keys().map(|k| k.to_snake_case()).collect();
        for (def_name, def) in definitions {
            if let Some(props) = &def.properties {
                for (field_name, field_prop) in props {
                    let snake = field_name.to_snake_case();
                    // Skip if a top-level property with the same snake_case name exists
                    if existing_snake.contains(&snake) {
                        continue;
                    }
                    let (_, field_enum_info) = cfn_type_to_carina_type_with_enum(
                        field_prop, field_name, schema, &namespace, &enums,
                    );
                    if let Some(info) = field_enum_info {
                        let composite_key = format!("{}.{}", def_name, field_name);
                        enums.insert(composite_key, info);
                    }
                }
            }
        }
    }

    disambiguate_enum_type_names(&mut enums);

    let has_enums = !enums.is_empty();
    let has_ranged_ints = !ranged_ints.is_empty();
    let has_ranged_floats = !ranged_floats.is_empty();
    let has_int_enums = !int_enums.is_empty();
    let has_ranged_lists = !ranged_lists.is_empty();
    let has_patterns = !patterns.is_empty();
    let has_ranged_strings = !ranged_strings.is_empty();
    let has_defaults = schema.properties.values().any(|p| {
        p.default_value
            .as_ref()
            .is_some_and(|v| json_default_to_value_code(v).is_some())
    });

    // Enums use AttributeType::Custom with AttributeType::String base
    if has_enums {
        needs_attribute_type = true;
    }

    // Ranged ints/floats/lists/strings use AttributeType::Custom with AttributeType::Int/Float/List/String base
    if has_ranged_ints || has_ranged_floats || has_ranged_lists || has_ranged_strings {
        needs_attribute_type = true;
    }

    // Int enums use AttributeType::Custom with AttributeType::Int base
    if has_int_enums {
        needs_attribute_type = true;
    }

    // Patterns use AttributeType::Custom with AttributeType::String base
    if has_patterns {
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

    if has_ranged_ints
        || has_ranged_floats
        || has_int_enums
        || has_ranged_lists
        || has_ranged_strings
        || has_defaults
        || has_patterns
    {
        code.push_str("use carina_core::resource::Value;\n");
    }
    if has_patterns {
        code.push_str("use regex::Regex;\n");
    }
    if needs_tags_type {
        code.push_str("use super::tags_type;\n");
    }
    code.push('\n');

    // Generate enum constants.
    // Constants are always emitted (referenced by enum_valid_values()).
    let aliases = known_enum_aliases();
    for (prop_name, enum_info) in &enums {
        // For composite keys like "DefName.FieldName", extract just "FieldName" for alias lookup
        let field_name = prop_name
            .split('.')
            .next_back()
            .unwrap_or(prop_name.as_str());
        let const_name = format!("VALID_{}", prop_name.to_snake_case().to_uppercase());

        // Generate constant (including alias values) - always emitted
        let mut all_values: Vec<String> = enum_info
            .values
            .iter()
            .map(|v| format!("\"{}\"", v))
            .collect();
        if let Some(prop_aliases) = aliases.get(field_name) {
            for (_, alias) in prop_aliases {
                all_values.push(format!("\"{}\"", alias));
            }
        }
        let values_str = all_values.join(", ");
        code.push_str(&format!(
            "const {}: &[&str] = &[{}];\n\n",
            const_name, values_str
        ));
    }

    // Generate range validation functions for integer properties
    for (prop_name, (min, max)) in &ranged_ints {
        let fn_name = format!("validate_{}_range", prop_name.to_snake_case());
        let (condition, range_display) = int_range_condition_and_display(*min, *max);
        code.push_str(&format!(
            r#"fn {}(value: &Value) -> Result<(), String> {{
    if let Value::Int(n) = value {{
        if {} {{
            Err(format!("Value {{}} is out of range {}", n))
        }} else {{
            Ok(())
        }}
    }} else {{
        Err("Expected integer".to_string())
    }}
}}

"#,
            fn_name, condition, range_display
        ));
    }

    // Generate range validation functions for float (number) properties
    for (prop_name, (min, max)) in &ranged_floats {
        let fn_name = format!("validate_{}_range", prop_name.to_snake_case());
        let (condition, range_display) = float_range_condition_and_display(*min, *max);
        code.push_str(&format!(
            r#"fn {}(value: &Value) -> Result<(), String> {{
    let n = match value {{
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => return Err("Expected number".to_string()),
    }};
    if {} {{
        Err(format!("Value {{}} is out of range {}", n))
    }} else {{
        Ok(())
    }}
}}

"#,
            fn_name, condition, range_display
        ));
    }

    // Generate validation functions for integer enum properties
    for (prop_name, values) in &int_enums {
        let fn_name = format!("validate_{}_int_enum", prop_name.to_snake_case());
        let const_name = format!("VALID_{}_VALUES", prop_name.to_snake_case().to_uppercase());
        let values_str = values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        code.push_str(&format!(
            r#"const {}: &[i64] = &[{}];

fn {}(value: &Value) -> Result<(), String> {{
    if let Value::Int(n) = value {{
        if {}.contains(n) {{
            Ok(())
        }} else {{
            Err(format!("Value {{}} is not a valid value", n))
        }}
    }} else {{
        Err("Expected integer".to_string())
    }}
}}

"#,
            const_name, values_str, fn_name, const_name
        ));
    }

    // Generate pattern validation functions for string properties (deduplicated)
    // Multiple properties with the same pattern share one validation function.
    // Skip patterns that are only used with combined length constraints
    // (those get combined validation functions generated separately).
    {
        let mut generated_patterns: HashSet<String> = HashSet::new();
        for pattern in patterns.values() {
            if generated_patterns.contains(pattern) {
                continue;
            }
            // Skip if this pattern is only used with combined length constraints
            if pattern_with_lengths.iter().any(|(p, _, _)| p == pattern)
                && !patterns_used_standalone.contains(pattern)
            {
                generated_patterns.insert(pattern.clone());
                continue;
            }
            generated_patterns.insert(pattern.clone());
            let fn_name = pattern_fn_name(pattern);
            // Convert PCRE-specific constructs to Rust regex equivalents
            let rust_pattern = pattern.replace("\\Z", "\\z");
            // Escape for Rust string literal (double the backslashes, escape quotes)
            let escaped_for_rust = rust_pattern.replace('\\', "\\\\").replace('"', "\\\"");
            // For the error message, also escape { and } for the inner format!() macro
            let escaped_for_msg = escaped_for_rust.replace('{', "{{").replace('}', "}}");
            code.push_str("fn ");
            code.push_str(&fn_name);
            code.push_str("(value: &Value) -> Result<(), String> {\n");
            code.push_str("    if let Value::String(s) = value {\n");
            code.push_str(
                "        static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {\n",
            );
            code.push_str(&format!(
                "            Regex::new(\"{}\").expect(\"invalid pattern regex\")\n",
                escaped_for_rust
            ));
            code.push_str("        });\n");
            code.push_str("        if RE.is_match(s) {\n");
            code.push_str("            Ok(())\n");
            code.push_str("        } else {\n");
            code.push_str(&format!(
                "            Err(format!(\"Value '{{}}' does not match pattern {}\", s))\n",
                escaped_for_msg
            ));
            code.push_str("        }\n");
            code.push_str("    } else {\n");
            code.push_str("        Err(\"Expected string\".to_string())\n");
            code.push_str("    }\n");
            code.push_str("}\n\n");
        }
    }

    // Generate validation functions for array item count constraints (deduplicated)
    // Multiple properties with the same min/max items share one validation function.
    {
        let mut generated_items: HashSet<(Option<i64>, Option<i64>)> = HashSet::new();
        for (min, max) in ranged_lists.values() {
            let key = (*min, *max);
            if generated_items.contains(&key) {
                continue;
            }
            generated_items.insert(key);
            let fn_name = list_items_fn_name(*min, *max);
            let (condition, range_display) = list_items_condition_and_display(*min, *max);
            code.push_str(&format!(
                r#"fn {}(value: &Value) -> Result<(), String> {{
    if let Value::List(items) = value {{
        let len = items.len();
        if {} {{
            Err(format!("List has {{}} items, expected {}", len))
        }} else {{
            Ok(())
        }}
    }} else {{
        Err("Expected list".to_string())
    }}
}}

"#,
                fn_name, condition, range_display
            ));
        }
    }

    // Generate length validation functions for string properties (deduplicated)
    // Multiple properties with the same min/max length share one validation function.
    {
        let mut generated_lengths: HashSet<(Option<u64>, Option<u64>)> = HashSet::new();
        for (min, max) in ranged_strings.values() {
            let key = (*min, *max);
            if generated_lengths.contains(&key) {
                continue;
            }
            generated_lengths.insert(key);
            let fn_name = string_length_fn_name(*min, *max);
            let (condition, range_display) = string_length_condition_and_display(*min, *max);
            code.push_str(&format!(
                r#"fn {}(value: &Value) -> Result<(), String> {{
    if let Value::String(s) = value {{
        let len = s.chars().count();
        if {} {{
            Err(format!("String length {{}} is out of range {}", len))
        }} else {{
            Ok(())
        }}
    }} else {{
        Ok(())
    }}
}}

"#,
                fn_name, condition, range_display
            ));
        }
    }

    // Generate combined pattern + length validation functions (deduplicated)
    {
        let mut generated_combined: HashSet<String> = HashSet::new();
        for (pattern, min, max) in &pattern_with_lengths {
            let fn_name = pattern_and_length_fn_name(pattern, *min, *max);
            if generated_combined.contains(&fn_name) {
                continue;
            }
            generated_combined.insert(fn_name.clone());
            let rust_pattern = pattern.replace("\\Z", "\\z");
            let escaped_for_rust = rust_pattern.replace('\\', "\\\\").replace('"', "\\\"");
            let escaped_for_msg = escaped_for_rust.replace('{', "{{").replace('}', "}}");
            let (len_condition, range_display) = string_length_condition_and_display(*min, *max);
            code.push_str(&format!(
                r#"fn {fn_name}(value: &Value) -> Result<(), String> {{
    if let Value::String(s) = value {{
        static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {{
            Regex::new("{escaped_for_rust}").expect("invalid pattern regex")
        }});
        if !RE.is_match(s) {{
            return Err(format!("Value '{{}}' does not match pattern {escaped_for_msg}", s));
        }}
        let len = s.chars().count();
        if {len_condition} {{
            return Err(format!("String length {{}} is out of range {range_display}", len));
        }}
        Ok(())
    }} else {{
        Err("Expected string".to_string())
    }}
}}

"#
            ));
        }
    }

    // Generate config function
    let config_fn_name = format!("{}_config", full_resource);
    // Use awscc.service.resource format (e.g., awscc.ec2.vpc)
    let schema_name = format!("awscc.{}", dsl_resource);

    code.push_str(&format!(
        r#"/// Returns the schema config for {} ({})
pub fn {}() -> AwsccSchemaConfig {{
    AwsccSchemaConfig {{
        aws_type_name: "{}",
        resource_type_name: "{}",
        has_tags: {},
        schema: ResourceSchema::new("{}")
"#,
        full_resource, type_name, config_fn_name, type_name, dsl_resource, has_tags, schema_name
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

    // Collect all attribute names for block_name collision detection
    let all_attr_names: HashSet<String> = schema
        .properties
        .keys()
        .map(|k| k.to_snake_case())
        .collect();

    // Generate attributes for each property
    for (prop_name, prop) in &schema.properties {
        let attr_name = prop_name.to_snake_case();
        let is_required = required.contains(prop_name) && !read_only.contains(prop_name);
        let is_read_only = read_only.contains(prop_name);
        let is_create_only = create_only.contains(prop_name);

        let attr_type = if let Some(enum_info) = enums.get(prop_name) {
            // Use shared schema enum type for constrained strings.
            // Build to_dsl closure: handles aliases and hyphen-to-underscore conversion
            let prop_aliases = aliases.get(prop_name.as_str());
            let has_hyphens = enum_info.values.iter().any(|v| v.contains('-'));
            let to_dsl_code = if let Some(alias_list) = prop_aliases {
                // Generate a closure that maps canonical values to aliases,
                // with fallback to hyphen-to-underscore if needed
                let mut match_arms: Vec<String> = alias_list
                    .iter()
                    .map(|(canonical, alias)| {
                        format!("\"{}\" => \"{}\".to_string()", canonical, alias)
                    })
                    .collect();
                let fallback = if has_hyphens {
                    "_ => s.replace('-', \"_\")"
                } else {
                    "_ => s.to_string()"
                };
                match_arms.push(fallback.to_string());
                format!("Some(|s: &str| match s {{ {} }})", match_arms.join(", "))
            } else if has_hyphens {
                "Some(|s: &str| s.replace('-', \"_\"))".to_string()
            } else {
                "None".to_string()
            };
            let values_str = enum_info
                .values
                .iter()
                .map(|v| format!("\"{}\".to_string()", v))
                .collect::<Vec<_>>()
                .join(", ");
            let enum_type = format!(
                r#"AttributeType::StringEnum {{
                name: "{}".to_string(),
                values: vec![{}],
                namespace: Some("{}".to_string()),
                to_dsl: {},
            }}"#,
                enum_info.type_name, values_str, namespace, to_dsl_code
            );
            // Wrap in List if the property is an array type
            let is_array = prop
                .prop_type
                .as_ref()
                .and_then(|t| t.as_str())
                .map(|t| t == "array")
                .unwrap_or(false);
            // Also check if the property is a $ref to an array-typed definition
            let is_ref_array = prop
                .ref_path
                .as_ref()
                .and_then(|ref_path| resolve_ref(schema, ref_path))
                .map(|def| def.def_type.as_deref() == Some("array"))
                .unwrap_or(false);
            if is_array || is_ref_array {
                format!("AttributeType::List(Box::new({}))", enum_type)
            } else {
                enum_type
            }
        } else {
            let (attr_type, _) =
                cfn_type_to_carina_type_with_enum(prop, prop_name, schema, &namespace, &enums);
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
            let escaped = collapse_whitespace(&desc.replace('"', "\\\"").replace('\n', " "));
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

        // Add default value if defined in CloudFormation schema
        if let Some(default_val) = &prop.default_value
            && let Some(default_code) = json_default_to_value_code(default_val)
        {
            attr_code.push_str(&format!(
                "\n                .with_default({})",
                default_code
            ));
        }

        // Add block_name for List(Struct) attributes with a natural singular form
        if attr_type.starts_with("AttributeType::List(Box::new(AttributeType::Struct")
            && let Some(singular) = compute_block_name(&attr_name)
            && !all_attr_names.contains(&singular)
        {
            attr_code.push_str(&format!(
                "\n                .with_block_name(\"{}\")",
                singular
            ));
        }

        attr_code.push_str(",\n        )\n");
        code.push_str(&attr_code);
    }

    // Determine name_attribute from primaryIdentifier:
    // If the primary identifier points to a single user-settable (non-read-only) string property,
    // it's the name attribute used for unique name generation during create-before-destroy.
    if let Some(primary_ids) = &schema.primary_identifier
        && primary_ids.len() == 1
    {
        let prop_path = primary_ids[0].trim_start_matches("/properties/");
        if !read_only.contains(prop_path)
            && let Some(prop) = schema.properties.get(prop_path)
        {
            let is_string = prop
                .prop_type
                .as_ref()
                .and_then(|t| t.as_str())
                .map(|t| t == "string")
                .unwrap_or(false);
            if is_string {
                let attr_name = prop_path.to_snake_case();
                code.push_str(&format!(
                    "        .with_name_attribute(\"{}\")\n",
                    attr_name
                ));
            }
        }
    }

    // Resource types where CloudControl API rejects updates despite the schema
    // having an update handler. These must be replaced instead of updated.
    const FORCE_REPLACE_TYPES: &[&str] = &["AWS::EC2::InternetGateway", "AWS::EC2::IPAM"];
    if FORCE_REPLACE_TYPES.contains(&type_name) {
        code.push_str("        .force_replace()\n");
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
            format!("    (\"{}\", &[])\n", dsl_resource)
        } else {
            let entries: Vec<String> = enums
                .keys()
                .map(|prop_name| {
                    // For composite keys "DefName.FieldName", use just "FieldName" for attribute name
                    let field_name = prop_name
                        .split('.')
                        .next_back()
                        .unwrap_or(prop_name.as_str());
                    let attr_name = field_name.to_snake_case();
                    // Constant name uses the full composite key for uniqueness
                    let const_name =
                        format!("VALID_{}", prop_name.to_snake_case().to_uppercase());
                    format!("        (\"{}\", {}),", attr_name, const_name)
                })
                .collect();
            format!(
                "    (\"{}\", &[\n{}\n    ])\n",
                dsl_resource,
                entries.join("\n")
            )
        }
    ));

    // Generate enum_alias_reverse() function that maps DSL alias -> canonical AWS value
    {
        let mut match_arms: Vec<String> = Vec::new();
        let mut seen_arms: HashSet<String> = HashSet::new();
        for prop_name in enums.keys() {
            // For composite keys "DefName.FieldName", use just "FieldName" for alias lookup
            let field_name = prop_name
                .split('.')
                .next_back()
                .unwrap_or(prop_name.as_str());
            if let Some(prop_aliases) = aliases.get(field_name) {
                let attr_name = field_name.to_snake_case();
                for (canonical, alias) in prop_aliases {
                    let arm = format!(
                        "        (\"{}\", \"{}\") => Some(\"{}\")",
                        attr_name, alias, canonical
                    );
                    if seen_arms.insert(arm.clone()) {
                        match_arms.push(arm);
                    }
                }
            }
        }
        code.push_str(
            "\n/// Maps DSL alias values back to canonical AWS values for this module.\n\
             /// e.g., (\"ip_protocol\", \"all\") -> Some(\"-1\")\n\
             pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {\n",
        );
        if match_arms.is_empty() {
            code.push_str("    let _ = (attr_name, value);\n    None\n");
        } else {
            match_arms.push("        _ => None".to_string());
            code.push_str(&format!(
                "    match (attr_name, value) {{\n{}\n    }}\n",
                match_arms.join(",\n")
            ));
        }
        code.push_str("}\n");
    }

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

/// Disambiguate enum type_names that collide (same type_name, different values).
/// For struct field enums with composite keys "DefName.FieldName", prefixes the
/// type_name with the parent struct name (e.g., "Status" -> "VersioningConfigurationStatus").
/// Enums with the same type_name and identical values are left unchanged (they deduplicate later).
fn disambiguate_enum_type_names(enums: &mut BTreeMap<String, EnumInfo>) {
    let mut type_name_groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, info) in enums.iter() {
        type_name_groups
            .entry(info.type_name.clone())
            .or_default()
            .push(key.clone());
    }
    for keys in type_name_groups.values() {
        let unique_value_sets: HashSet<Vec<String>> = keys
            .iter()
            .filter_map(|k| enums.get(k))
            .map(|info| info.values.clone())
            .collect();
        if unique_value_sets.len() <= 1 {
            continue; // No collision - all have identical values
        }
        // There's a collision: disambiguate by prefixing parent struct name
        for key in keys {
            if let Some(dot_pos) = key.find('.') {
                let parent_struct = &key[..dot_pos];
                if let Some(info) = enums.get_mut(key) {
                    info.type_name = format!("{}{}", parent_struct, info.type_name);
                }
            }
            // Top-level properties (no dot) keep their original type_name
        }
    }
}

/// Deduplicate enum values while preserving order.
/// Performs case-insensitive deduplication, preferring uppercase variants
/// (e.g., "GLACIER" over "Glacier") to normalize inconsistent CloudFormation schemas.
fn deduplicate_enum_values(values: Vec<String>) -> Option<Vec<String>> {
    let mut seen_lower: HashMap<String, usize> = HashMap::new();
    let mut unique: Vec<String> = Vec::new();
    for v in values {
        let lower = v.to_lowercase();
        if let Some(&idx) = seen_lower.get(&lower) {
            // Prefer the uppercase variant (e.g., "GLACIER" over "Glacier")
            if v.chars().all(|c| !c.is_alphabetic() || c.is_uppercase()) {
                unique[idx] = v;
            }
        } else {
            seen_lower.insert(lower, unique.len());
            unique.push(v);
        }
    }
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
    namespace: &str,
    enums: &BTreeMap<String, EnumInfo>,
) -> String {
    let required_set: HashSet<&str> = required.iter().map(|s| s.as_str()).collect();
    let aliases = known_enum_aliases();

    // Collect all field names (snake_case) to check block name conflicts
    let all_field_names: HashSet<String> = properties.keys().map(|k| k.to_snake_case()).collect();

    let fields: Vec<String> = properties
        .iter()
        .map(|(field_name, field_prop)| {
            let snake_name = field_name.to_snake_case();
            let (field_type, enum_info) =
                cfn_type_to_carina_type_with_enum(field_prop, field_name, schema, namespace, enums);
            // If enum detected in struct field and it's in the enums map, use shared string enum type.
            // Try composite key "DefName.FieldName" first (for definition-scoped enums),
            // then fall back to plain "FieldName" (for top-level property enums).
            // Check if the original field type was a List (array)
            let is_list_field = field_type.contains("AttributeType::List");
            let field_type = if let Some(local_enum_info) = enum_info {
                let composite_key = format!("{}.{}", def_name, field_name);
                let enum_type = if let Some(enum_info) =
                    enums.get(&composite_key).or_else(|| enums.get(field_name))
                {
                    let prop_aliases = aliases.get(field_name.as_str());
                    let has_hyphens = enum_info.values.iter().any(|v| v.contains('-'));
                    let to_dsl_code = if let Some(alias_list) = prop_aliases {
                        let mut match_arms: Vec<String> = alias_list
                            .iter()
                            .map(|(canonical, alias)| {
                                format!("\"{}\" => \"{}\".to_string()", canonical, alias)
                            })
                            .collect();
                        let fallback = if has_hyphens {
                            "_ => s.replace('-', \"_\")"
                        } else {
                            "_ => s.to_string()"
                        };
                        match_arms.push(fallback.to_string());
                        format!("Some(|s: &str| match s {{ {} }})", match_arms.join(", "))
                    } else if has_hyphens {
                        "Some(|s: &str| s.replace('-', \"_\"))".to_string()
                    } else {
                        "None".to_string()
                    };
                    let values_str = enum_info
                        .values
                        .iter()
                        .map(|v| format!("\"{}\".to_string()", v))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        r#"AttributeType::StringEnum {{
                name: "{}".to_string(),
                values: vec![{}],
                namespace: Some("{}".to_string()),
                to_dsl: {},
            }}"#,
                        enum_info.type_name, values_str, namespace, to_dsl_code
                    )
                } else {
                    // Fallback: emit StringEnum with namespace even when not in the
                    // pre-scanned enums map (e.g., nested struct fields that were
                    // skipped during scanning due to snake_case name conflicts).
                    let prop_aliases = aliases.get(field_name.as_str());
                    let has_hyphens = local_enum_info.values.iter().any(|v| v.contains('-'));
                    let to_dsl_code = if let Some(alias_list) = prop_aliases {
                        let mut match_arms: Vec<String> = alias_list
                            .iter()
                            .map(|(canonical, alias)| {
                                format!("\"{}\" => \"{}\".to_string()", canonical, alias)
                            })
                            .collect();
                        let fallback = if has_hyphens {
                            "_ => s.replace('-', \"_\")"
                        } else {
                            "_ => s.to_string()"
                        };
                        match_arms.push(fallback.to_string());
                        format!("Some(|s: &str| match s {{ {} }})", match_arms.join(", "))
                    } else if has_hyphens {
                        "Some(|s: &str| s.replace('-', \"_\"))".to_string()
                    } else {
                        "None".to_string()
                    };
                    let values_str = local_enum_info
                        .values
                        .iter()
                        .map(|v| format!("\"{}\".to_string()", v))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        r#"AttributeType::StringEnum {{
                name: "{}".to_string(),
                values: vec![{}],
                namespace: Some("{}".to_string()),
                to_dsl: {},
            }}"#,
                        local_enum_info.type_name, values_str, namespace, to_dsl_code
                    )
                };
                // Wrap in List if the original field type was a List
                if is_list_field {
                    format!("AttributeType::List(Box::new({}))", enum_type)
                } else {
                    enum_type
                }
            } else {
                field_type
            };
            let is_required = required_set.contains(field_name.as_str());

            let mut field_code = format!("StructField::new(\"{}\", {})", snake_name, field_type);
            if is_required {
                field_code.push_str(".required()");
            }
            if let Some(desc) = &field_prop.description {
                let escaped = collapse_whitespace(&desc.replace('"', "\\\"").replace('\n', " "));
                let truncated = if escaped.len() > 150 {
                    format!("{}...", &escaped[..150])
                } else {
                    escaped
                };
                field_code.push_str(&format!(".with_description(\"{}\")", truncated));
            }
            field_code.push_str(&format!(".with_provider_name(\"{}\")", field_name));

            // Add block_name for List(Struct) fields with a natural singular form
            if field_type.starts_with("AttributeType::List(Box::new(AttributeType::Struct")
                && let Some(singular) = compute_block_name(&snake_name)
                && !all_field_names.contains(&singular)
            {
                field_code.push_str(&format!(".with_block_name(\"{}\")", singular));
            }

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
        m.insert("EncryptionSupportState", vec!["disable", "enable"]);
        m
    });
    &OVERRIDES
}

/// Known enum aliases for properties where an AWS canonical value should have
/// a human-readable alias in DSL. Maps PropertyName -> [(canonical_aws_value, dsl_alias)].
/// e.g., IpProtocol: "-1" -> "all"
fn known_enum_aliases() -> &'static HashMap<&'static str, Vec<(&'static str, &'static str)>> {
    static ALIASES: LazyLock<HashMap<&'static str, Vec<(&'static str, &'static str)>>> =
        LazyLock::new(|| {
            let mut m = HashMap::new();
            m.insert("IpProtocol", vec![("-1", "all")]);
            m
        });
    &ALIASES
}

/// Generate condition string and display string for integer range validation.
fn int_range_condition_and_display(min: Option<i64>, max: Option<i64>) -> (String, String) {
    match (min, max) {
        (Some(min), Some(max)) => (
            format!("*n < {} || *n > {}", min, max),
            format!("{}..={}", min, max),
        ),
        (Some(min), None) => (format!("*n < {}", min), format!("{}..", min)),
        (None, Some(max)) => (format!("*n > {}", max), format!("..={}", max)),
        (None, None) => unreachable!("at least one bound must be present"),
    }
}

/// Generate condition string and display string for float range validation.
/// Generate condition string and display string for list item count validation.
fn list_items_condition_and_display(min: Option<i64>, max: Option<i64>) -> (String, String) {
    match (min, max) {
        (Some(min), Some(max)) => (
            format!("!({}..={}).contains(&len)", min, max),
            format!("{}..={}", min, max),
        ),
        (Some(min), None) => (format!("len < {}", min), format!("{}..", min)),
        (None, Some(max)) => (format!("len > {}", max), format!("..={}", max)),
        (None, None) => unreachable!("at least one bound must be present"),
    }
}

fn float_range_condition_and_display(min: Option<i64>, max: Option<i64>) -> (String, String) {
    match (min, max) {
        (Some(min), Some(max)) => (
            format!("n < {}.0 || n > {}.0", min, max),
            format!("{}..={}", min, max),
        ),
        (Some(min), None) => (format!("n < {}.0", min), format!("{}..", min)),
        (None, Some(max)) => (format!("n > {}.0", max), format!("..={}", max)),
        (None, None) => unreachable!("at least one bound must be present"),
    }
}

/// Format a range display string for type names.
fn range_display_string(min: Option<i64>, max: Option<i64>) -> String {
    match (min, max) {
        (Some(min), Some(max)) => format!("{}..={}", min, max),
        (Some(min), None) => format!("{}..", min),
        (None, Some(max)) => format!("..={}", max),
        (None, None) => unreachable!("at least one bound must be present"),
    }
}

/// Check if a regex pattern represents a numeric string (digits only).
/// Matches patterns like "[0-9]+", "^[0-9]+$", "^\d+$", etc.
fn is_numeric_string_pattern(pattern: &str) -> bool {
    matches!(
        pattern,
        "[0-9]+" | "^[0-9]+$" | "\\d+" | "^\\d+$" | "[0-9]*" | "^[0-9]*$"
    )
}

/// Recursively collect string length constraints from a property and its array items.
/// Treats minLength=0 as no constraint since usize is always >= 0.
fn collect_string_length_constraints(
    prop_name: &str,
    prop: &CfnProperty,
    ranged_strings: &mut BTreeMap<String, (Option<u64>, Option<u64>)>,
) {
    let prop_type = prop.prop_type.as_ref().and_then(|t| t.as_str());
    let effective_min = prop.min_length.filter(|&m| m > 0);
    if prop_type == Some("string")
        && (effective_min.is_some() || prop.max_length.is_some())
        && prop.enum_values.is_none()
        && prop.pattern.is_none()
        && !ranged_strings.contains_key(prop_name)
    {
        ranged_strings.insert(prop_name.to_string(), (effective_min, prop.max_length));
    }
    // Recurse into array items
    if prop_type == Some("array")
        && let Some(items) = &prop.items
    {
        collect_string_length_constraints(prop_name, items, ranged_strings);
    }
}

/// Format a range display string for string length type names.
fn string_length_display(min: Option<u64>, max: Option<u64>) -> String {
    match (min, max) {
        (Some(min), Some(max)) => format!("{}..={}", min, max),
        (Some(min), None) => format!("{}..", min),
        (None, Some(max)) => format!("..={}", max),
        (None, None) => unreachable!("at least one bound must be present"),
    }
}

/// Generate a constraint-based function name for string length validation.
/// E.g., `validate_string_length_max_1024` or `validate_string_length_1_512`.
fn string_length_fn_name(min: Option<u64>, max: Option<u64>) -> String {
    let effective_min = min.filter(|&m| m > 0);
    match (effective_min, max) {
        (Some(min), Some(max)) => format!("validate_string_length_{min}_{max}"),
        (Some(min), None) => format!("validate_string_length_min_{min}"),
        (None, Some(max)) => format!("validate_string_length_max_{max}"),
        (None, None) => unreachable!("at least one bound must be present"),
    }
}

/// Generate a constraint-based function name for list items validation.
/// E.g., `validate_list_items_1_100` or `validate_list_items_max_10`.
fn list_items_fn_name(min: Option<i64>, max: Option<i64>) -> String {
    match (min, max) {
        (Some(min), Some(max)) => format!("validate_list_items_{min}_{max}"),
        (Some(min), None) => format!("validate_list_items_min_{min}"),
        (None, Some(max)) => format!("validate_list_items_max_{max}"),
        (None, None) => unreachable!("at least one bound must be present"),
    }
}

/// Generate a constraint-based function name for pattern validation.
/// Uses a hash of the pattern to create a unique but deterministic name.
fn pattern_fn_name(pattern: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    pattern.hash(&mut hasher);
    let hash = hasher.finish();
    format!("validate_string_pattern_{:016x}", hash)
}

/// Generate a constraint-based function name for combined pattern + length validation.
/// Uses a hash of the pattern combined with length bounds.
fn pattern_and_length_fn_name(pattern: &str, min: Option<u64>, max: Option<u64>) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    pattern.hash(&mut hasher);
    let hash = hasher.finish();
    let effective_min = min.filter(|&m| m > 0);
    match (effective_min, max) {
        (Some(min_val), Some(max_val)) => {
            format!(
                "validate_string_pattern_{:016x}_len_{min_val}_{max_val}",
                hash
            )
        }
        (Some(min_val), None) => {
            format!("validate_string_pattern_{:016x}_len_min_{min_val}", hash)
        }
        (None, Some(max_val)) => {
            format!("validate_string_pattern_{:016x}_len_max_{max_val}", hash)
        }
        (None, None) => pattern_fn_name(pattern),
    }
}

/// Generate condition string and display string for string length validation.
/// Treats min=0 as no minimum since `usize` is always >= 0.
fn string_length_condition_and_display(min: Option<u64>, max: Option<u64>) -> (String, String) {
    let effective_min = min.filter(|&m| m > 0);
    match (effective_min, max) {
        (Some(min), Some(max)) => (
            format!("!({}..={}).contains(&len)", min, max),
            format!("{}..={}", min, max),
        ),
        (Some(min), None) => (format!("len < {}", min), format!("{}..", min)),
        (None, Some(max)) => (format!("len > {}", max), format!("..={}", max)),
        (None, None) => unreachable!("at least one bound must be present"),
    }
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
        m.insert("DefaultNetworkAcl", "super::network_acl_id()");
        m.insert("DeliverCrossAccountRole", "super::iam_role_arn()");
        m.insert("DeliverLogsPermissionArn", "super::iam_role_arn()");
        m.insert("PeerRoleArn", "super::iam_role_arn()");
        m.insert("PermissionsBoundary", "super::iam_policy_arn()");
        m.insert("ManagedPolicyArns", "super::iam_policy_arn()");
        m.insert("KmsKeyId", "super::kms_key_arn()");
        m.insert("KMSMasterKeyID", "super::kms_key_id()");
        m.insert("ReplicaKmsKeyID", "super::kms_key_id()");
        m.insert("KmsKeyArn", "super::kms_key_arn()");
        m.insert("IpamId", "super::ipam_id()");
        m.insert("Locale", "super::awscc_region()");
        m.insert("BucketAccountId", "super::aws_account_id()");
        m
    });
    &OVERRIDES
}

/// Unified resource-specific property type overrides.
/// Maps (CloudFormation type name, property name) to a TypeOverride.
/// Use this when a property needs resource-specific type treatment that differs
/// from global overrides or pattern-based inference.
///
/// Two-layer system: global overrides (known_*_overrides) apply by property name,
/// resource-specific overrides here take precedence.
fn resource_type_overrides() -> &'static HashMap<(&'static str, &'static str), TypeOverride> {
    static OVERRIDES: LazyLock<HashMap<(&'static str, &'static str), TypeOverride>> =
        LazyLock::new(|| {
            let mut m = HashMap::new();

            // === StringType overrides ===

            // IAM Role's Arn is always an IAM Role ARN
            m.insert(
                ("AWS::IAM::Role", "Arn"),
                TypeOverride::StringType("super::iam_role_arn()"),
            );
            // IAM Role's RoleId uses AROA prefix pattern
            m.insert(
                ("AWS::IAM::Role", "RoleId"),
                TypeOverride::StringType("super::iam_role_id()"),
            );
            // EC2 Route's GatewayId accepts both igw-* and vgw-*
            m.insert(
                ("AWS::EC2::Route", "GatewayId"),
                TypeOverride::StringType("super::gateway_id()"),
            );
            // Generic "Id" attributes on resources where the specific ID type is known
            m.insert(
                ("AWS::EC2::EgressOnlyInternetGateway", "Id"),
                TypeOverride::StringType("super::egress_only_internet_gateway_id()"),
            );
            m.insert(
                ("AWS::EC2::TransitGateway", "Id"),
                TypeOverride::StringType("super::transit_gateway_id()"),
            );
            m.insert(
                ("AWS::EC2::VPCPeeringConnection", "Id"),
                TypeOverride::StringType("super::vpc_peering_connection_id()"),
            );
            m.insert(
                ("AWS::EC2::VPCEndpoint", "Id"),
                TypeOverride::StringType("super::vpc_endpoint_id()"),
            );
            m.insert(
                ("AWS::EC2::SecurityGroup", "Id"),
                TypeOverride::StringType("super::security_group_id()"),
            );
            m.insert(
                ("AWS::EC2::TransitGatewayAttachment", "Id"),
                TypeOverride::StringType("super::transit_gateway_attachment_id()"),
            );
            m.insert(
                ("AWS::EC2::FlowLog", "Id"),
                TypeOverride::StringType("super::flow_log_id()"),
            );
            m.insert(
                ("AWS::EC2::SubnetRouteTableAssociation", "Id"),
                TypeOverride::StringType("super::subnet_route_table_association_id()"),
            );
            // EIP Address and TransferAddress are IPv4 addresses
            m.insert(
                ("AWS::EC2::EIP", "Address"),
                TypeOverride::StringType("types::ipv4_address()"),
            );
            m.insert(
                ("AWS::EC2::EIP", "TransferAddress"),
                TypeOverride::StringType("types::ipv4_address()"),
            );
            // FlowLog LogDestination is an ARN (S3 bucket, CloudWatch log group, or Firehose)
            m.insert(
                ("AWS::EC2::FlowLog", "LogDestination"),
                TypeOverride::StringType("super::arn()"),
            );
            // S3 Bucket notification ARNs
            m.insert(
                ("AWS::S3::Bucket", "Function"),
                TypeOverride::StringType("super::arn()"),
            );
            m.insert(
                ("AWS::S3::Bucket", "Queue"),
                TypeOverride::StringType("super::arn()"),
            );
            m.insert(
                ("AWS::S3::Bucket", "Topic"),
                TypeOverride::StringType("super::arn()"),
            );
            // S3 Bucket replication role is an IAM Role ARN
            m.insert(
                ("AWS::S3::Bucket", "Role"),
                TypeOverride::StringType("super::iam_role_arn()"),
            );
            // S3 Bucket replication destination account
            m.insert(
                ("AWS::S3::Bucket", "Account"),
                TypeOverride::StringType("super::aws_account_id()"),
            );
            // VPC CidrBlockAssociations are association IDs (vpc-cidr-assoc-xxx), not CIDRs
            m.insert(
                ("AWS::EC2::VPC", "CidrBlockAssociations"),
                TypeOverride::StringType("super::vpc_cidr_block_association_id()"),
            );
            // Transit Gateway route table IDs use tgw-rtb- prefix, not rtb-
            m.insert(
                ("AWS::EC2::TransitGateway", "AssociationDefaultRouteTableId"),
                TypeOverride::StringType("super::tgw_route_table_id()"),
            );
            m.insert(
                ("AWS::EC2::TransitGateway", "PropagationDefaultRouteTableId"),
                TypeOverride::StringType("super::tgw_route_table_id()"),
            );
            // Transit Gateway CIDR blocks support both IPv4 and IPv6
            m.insert(
                ("AWS::EC2::TransitGateway", "TransitGatewayCidrBlocks"),
                TypeOverride::StringType("types::cidr()"),
            );
            // IPAM Pool provisioned CIDRs support both IPv4 and IPv6
            m.insert(
                ("AWS::EC2::IPAMPool", "Cidr"),
                TypeOverride::StringType("types::cidr()"),
            );
            // S3 ReplaceKeyPrefixWith is a free-form string, not an enum
            m.insert(
                ("AWS::S3::Bucket", "ReplaceKeyPrefixWith"),
                TypeOverride::StringType("AttributeType::String"),
            );

            // === Enum overrides ===

            // VPN Gateway Type only accepts "ipsec.1"
            m.insert(
                ("AWS::EC2::VPNGateway", "Type"),
                TypeOverride::Enum(vec!["ipsec.1"]),
            );

            // === IntRange overrides ===

            // VPN Gateway and Transit Gateway ASN range
            m.insert(
                ("AWS::EC2::VPNGateway", "AmazonSideAsn"),
                TypeOverride::IntRange(1, 4294967294),
            );
            m.insert(
                ("AWS::EC2::TransitGateway", "AmazonSideAsn"),
                TypeOverride::IntRange(1, 4294967294),
            );

            // === IntEnum overrides ===

            // FlowLog max aggregation interval: 60 or 600 seconds
            m.insert(
                ("AWS::EC2::FlowLog", "MaxAggregationInterval"),
                TypeOverride::IntEnum(vec![60, 600]),
            );
            // CloudWatch Logs retention: specific allowed day counts
            m.insert(
                ("AWS::Logs::LogGroup", "RetentionInDays"),
                TypeOverride::IntEnum(vec![
                    1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827,
                    2192, 2557, 2922, 3288, 3653,
                ]),
            );

            m
        });
    &OVERRIDES
}

/// Infer the Carina type string for a property based on its name.
/// Checks resource-specific overrides, known string type overrides, ARN patterns,
/// and resource ID patterns.
/// Returns None if no heuristic matches (caller should default to String).
fn infer_string_type(prop_name: &str, resource_type: &str) -> Option<String> {
    // Check resource-specific overrides first (StringType only for string inference)
    if let Some(TypeOverride::StringType(override_type)) =
        resource_type_overrides().get(&(resource_type, prop_name))
    {
        return Some(override_type.to_string());
    }
    // Check known string type overrides
    if let Some(&override_type) = known_string_type_overrides().get(prop_name) {
        return Some(override_type.to_string());
    }

    // Normalize plural forms for type inference
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
    if let Some(&override_type) = known_string_type_overrides().get(singular_name) {
        return Some(override_type.to_string());
    }

    let prop_lower = singular_name.to_lowercase();

    // CIDR types - differentiate IPv4 vs IPv6 based on property name
    if prop_lower.contains("cidr") {
        if prop_lower.contains("ipv6") {
            return Some("types::ipv6_cidr()".to_string());
        }
        return Some("types::ipv4_cidr()".to_string());
    }

    // IP address types (not CIDR) - e.g., PrivateIpAddress, PublicIp
    if (prop_lower.contains("ipaddress")
        || prop_lower.ends_with("ip")
        || prop_lower.contains("ipaddresses"))
        && !prop_lower.contains("count")
        && !prop_lower.contains("type")
    {
        if prop_lower.contains("ipv6") {
            return Some("types::ipv6_address()".to_string());
        }
        return Some("types::ipv4_address()".to_string());
    }

    // Availability zone (but not AvailabilityZoneId which uses AZ ID format like "use1-az1")
    if prop_lower == "availabilityzone" || prop_lower == "availabilityzones" {
        return Some("super::availability_zone()".to_string());
    }

    // Availability zone ID (e.g., "use1-az1", "usw2-az2")
    if prop_lower == "availabilityzoneid" || prop_lower == "availabilityzoneids" {
        return Some("super::availability_zone_id()".to_string());
    }

    // Region types (e.g., PeerRegion, ServiceRegion, RegionName, ResourceRegion)
    if prop_lower.ends_with("region") || prop_lower == "regionname" {
        return Some("super::awscc_region()".to_string());
    }

    // Check ARN pattern
    if prop_lower.ends_with("arn") || prop_lower.ends_with("arns") || prop_lower.contains("_arn") {
        return Some("super::arn()".to_string());
    }

    // IPAM Pool IDs
    if is_ipam_pool_id_property(singular_name) {
        return Some("super::ipam_pool_id()".to_string());
    }

    // Check resource ID pattern
    if is_aws_resource_id_property(singular_name) {
        return Some(get_resource_id_type(singular_name).to_string());
    }

    // AWS Account ID (owner IDs and account IDs are 12-digit account IDs)
    if prop_lower.ends_with("ownerid") || prop_lower.ends_with("accountid") {
        return Some("super::aws_account_id()".to_string());
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
    InstanceId,
    NetworkInterfaceId,
    AllocationId,
    PrefixListId,
    CarrierGatewayId,
    LocalGatewayId,
    NetworkAclId,
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
    // Instance IDs (e.g., InstanceId)
    if lower.ends_with("instanceid") {
        return ResourceIdKind::InstanceId;
    }
    // Network Interface IDs (e.g., NetworkInterfaceId, EniId)
    if lower.ends_with("networkinterfaceid") || lower.ends_with("eniid") {
        return ResourceIdKind::NetworkInterfaceId;
    }
    // Allocation IDs (e.g., AllocationId)
    if lower.ends_with("allocationid") {
        return ResourceIdKind::AllocationId;
    }
    // Prefix List IDs (e.g., PrefixListId, DestinationPrefixListId)
    if lower.ends_with("prefixlistid") {
        return ResourceIdKind::PrefixListId;
    }
    // Carrier Gateway IDs (e.g., CarrierGatewayId)
    if lower.contains("carriergateway") && lower.ends_with("id") {
        return ResourceIdKind::CarrierGatewayId;
    }
    // Local Gateway IDs (e.g., LocalGatewayId)
    if lower.contains("localgateway") && lower.ends_with("id") {
        return ResourceIdKind::LocalGatewayId;
    }
    // Network ACL IDs (e.g., NetworkAclId)
    if lower.contains("networkacl") && lower.ends_with("id") {
        return ResourceIdKind::NetworkAclId;
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
        ResourceIdKind::InstanceId => "super::instance_id()",
        ResourceIdKind::NetworkInterfaceId => "super::network_interface_id()",
        ResourceIdKind::AllocationId => "super::allocation_id()",
        ResourceIdKind::PrefixListId => "super::prefix_list_id()",
        ResourceIdKind::CarrierGatewayId => "super::carrier_gateway_id()",
        ResourceIdKind::LocalGatewayId => "super::local_gateway_id()",
        ResourceIdKind::NetworkAclId => "super::network_acl_id()",
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
        ResourceIdKind::InstanceId => "InstanceId",
        ResourceIdKind::NetworkInterfaceId => "NetworkInterfaceId",
        ResourceIdKind::AllocationId => "AllocationId",
        ResourceIdKind::PrefixListId => "PrefixListId",
        ResourceIdKind::CarrierGatewayId => "CarrierGatewayId",
        ResourceIdKind::LocalGatewayId => "LocalGatewayId",
        ResourceIdKind::NetworkAclId => "NetworkAclId",
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
    namespace: &str,
    enums: &BTreeMap<String, EnumInfo>,
) -> (String, Option<EnumInfo>) {
    // Tags property is special - it's a Map in Carina (Terraform-style)
    if prop_name == "Tags" {
        return ("tags_type()".to_string(), None);
    }

    // Handle const values - generate single-value enum
    if let Some(const_val) = &prop.const_value {
        let value_str = match const_val {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let type_name = prop_name.to_pascal_case();
        let enum_info = EnumInfo {
            type_name,
            values: vec![value_str],
        };
        return ("/* enum */".to_string(), Some(enum_info));
    }

    // Handle $ref
    if let Some(ref_path) = &prop.ref_path {
        if ref_path.contains("/Tag") {
            return ("tags_type()".to_string(), None);
        }
        // Try to resolve the $ref to a definition and generate Struct type
        if let Some(def) = resolve_ref(schema, ref_path) {
            if let Some(props) = &def.properties
                && !props.is_empty()
            {
                let def_name = ref_def_name(ref_path).unwrap_or(prop_name);
                return (
                    generate_struct_type(def_name, props, &def.required, schema, namespace, enums),
                    None,
                );
            }
            // Handle oneOf: merge all variant properties into a single struct
            if !def.one_of.is_empty() {
                let mut merged_props = BTreeMap::new();
                // oneOf variants are mutually exclusive, so no field is required
                for variant in &def.one_of {
                    if let Some(props) = &variant.properties {
                        for (k, v) in props {
                            merged_props.insert(k.clone(), v.clone());
                        }
                    }
                }
                if !merged_props.is_empty() {
                    let def_name = ref_def_name(ref_path).unwrap_or(prop_name);
                    return (
                        generate_struct_type(
                            def_name,
                            &merged_props,
                            &[], // no required fields - variants are mutually exclusive
                            schema,
                            namespace,
                            enums,
                        ),
                        None,
                    );
                }
            }
            // Handle enum-only $ref definitions (no properties, just enum values)
            if let Some(enum_values) = &def.enum_values
                && !enum_values.is_empty()
            {
                let def_name = ref_def_name(ref_path).unwrap_or(prop_name);
                let type_name = def_name.to_pascal_case();
                let string_values: Vec<String> =
                    enum_values.iter().map(|v| v.to_string_value()).collect();
                let deduped =
                    deduplicate_enum_values(string_values.clone()).unwrap_or(string_values);
                let enum_info = EnumInfo {
                    type_name,
                    values: deduped,
                };
                return ("/* enum */".to_string(), Some(enum_info));
            }
            // Handle array-typed $ref definitions (e.g., BlockedEncryptionTypeList)
            if def.def_type.as_deref() == Some("array")
                && let Some(items) = &def.items
            {
                let (item_type, item_enum) =
                    cfn_type_to_carina_type_with_enum(items, prop_name, schema, namespace, enums);
                // Propagate item_enum so callers can register the enum type.
                let effective_item_type = if item_enum.is_some() {
                    "AttributeType::String".to_string()
                } else {
                    item_type
                };
                return (
                    format!("AttributeType::List(Box::new({}))", effective_item_type),
                    item_enum,
                );
            }
            // Handle string-typed $ref definitions with pattern constraints
            if def.def_type.as_deref() == Some("string")
                && let Some(pattern) = &def.pattern
            {
                // Check if name-based heuristics would override the type
                if infer_string_type(prop_name, &schema.type_name).is_none() {
                    let effective_min = def.min_length.filter(|&m| m > 0);
                    let has_length = effective_min.is_some() || def.max_length.is_some();
                    let validate_fn = if has_length {
                        pattern_and_length_fn_name(pattern, effective_min, def.max_length)
                    } else {
                        pattern_fn_name(pattern)
                    };
                    let name = if has_length {
                        let range = string_length_display(effective_min, def.max_length);
                        format!("String(pattern, len: {})", range)
                    } else {
                        "String(pattern)".to_string()
                    };
                    return (
                        format!(
                            r#"AttributeType::Custom {{
                name: "{}".to_string(),
                base: Box::new(AttributeType::String),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                            name, validate_fn
                        ),
                        None,
                    );
                }
            }
        }
        // Apply name-based heuristics for unresolvable $ref
        if let Some(inferred) = infer_string_type(prop_name, &schema.type_name) {
            return (inferred, None);
        }
        return ("AttributeType::String".to_string(), None);
    }

    // Handle explicit enum
    if let Some(enum_values) = &prop.enum_values {
        // If all enum values are integers, generate IntEnum Custom type
        let all_ints = enum_values.iter().all(|v| matches!(v, EnumValue::Int(_)));
        if all_ints && !enum_values.is_empty() {
            let values: Vec<i64> = enum_values
                .iter()
                .filter_map(|v| match v {
                    EnumValue::Int(i) => Some(*i),
                    _ => None,
                })
                .collect();
            let values_str = values
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let validate_fn = format!("validate_{}_int_enum", prop_name.to_snake_case());
            return (
                format!(
                    r#"AttributeType::Custom {{
                name: "IntEnum([{}])".to_string(),
                base: Box::new(AttributeType::Int),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                    values_str, validate_fn
                ),
                None,
            );
        }

        let type_name = prop_name.to_pascal_case();
        let string_values: Vec<String> = enum_values.iter().map(|v| v.to_string_value()).collect();
        // Deduplicate case-insensitive duplicates (e.g., "GLACIER" and "Glacier")
        let deduped = deduplicate_enum_values(string_values.clone()).unwrap_or(string_values);
        let enum_info = EnumInfo {
            type_name,
            values: deduped,
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
            // Check known string type overrides first (includes CIDR, IP, AZ,
            // ARN, resource IDs, IPAM Pool IDs, and owner IDs)
            if let Some(inferred) = infer_string_type(prop_name, &schema.type_name) {
                return (inferred, None);
            }

            // Check if this is a policy document field (CFN sometimes types these as "string")
            if prop_name.ends_with("PolicyDocument") {
                return ("super::iam_policy_document()".to_string(), None);
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

            // Check for string length constraints
            let effective_min = prop.min_length.filter(|&m| m > 0);
            let has_length = effective_min.is_some() || prop.max_length.is_some();

            // Check for numeric string pattern (e.g., "[0-9]+")
            if let Some(ref pattern) = prop.pattern
                && is_numeric_string_pattern(pattern)
            {
                let validate_fn = if has_length {
                    pattern_and_length_fn_name(pattern, effective_min, prop.max_length)
                } else {
                    pattern_fn_name(pattern)
                };
                let name = if has_length {
                    let range = string_length_display(effective_min, prop.max_length);
                    format!("NumericString(len: {})", range)
                } else {
                    "NumericString".to_string()
                };
                return (
                    format!(
                        r#"AttributeType::Custom {{
                name: "{}".to_string(),
                base: Box::new(AttributeType::String),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        name, validate_fn
                    ),
                    None,
                );
            }

            // Check for regex pattern constraint
            if let Some(pattern) = &prop.pattern {
                let validate_fn = if has_length {
                    pattern_and_length_fn_name(pattern, effective_min, prop.max_length)
                } else {
                    pattern_fn_name(pattern)
                };
                let name = if has_length {
                    let range = string_length_display(effective_min, prop.max_length);
                    format!("String(pattern, len: {})", range)
                } else {
                    "String(pattern)".to_string()
                };
                return (
                    format!(
                        r#"AttributeType::Custom {{
                name: "{}".to_string(),
                base: Box::new(AttributeType::String),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        name, validate_fn
                    ),
                    None,
                );
            }

            // Check for string format constraint (e.g., "uri", "date-time")
            if let Some(ref fmt) = prop.format {
                return (
                    format!(
                        r#"AttributeType::Custom {{
                name: "String({})".to_string(),
                base: Box::new(AttributeType::String),
                validate: |_| Ok(()),
                namespace: None,
                to_dsl: None,
            }}"#,
                        fmt
                    ),
                    None,
                );
            }

            // Check for string length constraints (minLength/maxLength)
            if has_length {
                let validate_fn = string_length_fn_name(effective_min, prop.max_length);
                let display = string_length_display(effective_min, prop.max_length);
                return (
                    format!(
                        r#"AttributeType::Custom {{
                name: "String(len: {})".to_string(),
                base: Box::new(AttributeType::String),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        display, validate_fn
                    ),
                    None,
                );
            }

            ("AttributeType::String".to_string(), None)
        }
        Some("boolean") => ("AttributeType::Bool".to_string(), None),
        Some("integer") => {
            // Check resource-scoped overrides first
            let res_override =
                resource_type_overrides().get(&(schema.type_name.as_str(), prop_name));
            if let Some(TypeOverride::IntRange(min, max)) = res_override {
                let validate_fn = format!("validate_{}_range", prop_name.to_snake_case());
                let display = range_display_string(Some(*min), Some(*max));
                return (
                    format!(
                        r#"AttributeType::Custom {{
                name: "Int({})".to_string(),
                base: Box::new(AttributeType::Int),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        display, validate_fn
                    ),
                    None,
                );
            }
            if let Some(TypeOverride::IntEnum(values)) = res_override {
                let validate_fn = format!("validate_{}_int_enum", prop_name.to_snake_case());
                let values_str = values
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return (
                    format!(
                        r#"AttributeType::Custom {{
                name: "IntEnum([{}])".to_string(),
                base: Box::new(AttributeType::Int),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        values_str, validate_fn
                    ),
                    None,
                );
            }
            // Use CF min/max if available (including one-sided), otherwise check known overrides
            let range: Option<(Option<i64>, Option<i64>)> =
                if prop.minimum.is_some() || prop.maximum.is_some() {
                    Some((prop.minimum, prop.maximum))
                } else {
                    known_int_range_overrides()
                        .get(prop_name)
                        .map(|&(min, max)| (Some(min), Some(max)))
                };
            if let Some((min, max)) = range {
                // Generate a ranged int type with validation
                let validate_fn = format!("validate_{}_range", prop_name.to_snake_case());
                let range_str = range_display_string(min, max);
                let display = if let Some(ref fmt) = prop.format {
                    format!("{}, {}", range_str, fmt)
                } else {
                    range_str
                };
                (
                    format!(
                        r#"AttributeType::Custom {{
                name: "Int({})".to_string(),
                base: Box::new(AttributeType::Int),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        display, validate_fn
                    ),
                    None,
                )
            } else if let Some(ref fmt) = prop.format {
                // Format-only integer (e.g., int64) - informational, no range validation
                (
                    format!(
                        r#"AttributeType::Custom {{
                name: "Int({})".to_string(),
                base: Box::new(AttributeType::Int),
                validate: |_| Ok(()),
                namespace: None,
                to_dsl: None,
            }}"#,
                        fmt
                    ),
                    None,
                )
            } else {
                ("AttributeType::Int".to_string(), None)
            }
        }
        Some("number") => {
            // Check resource-scoped overrides first
            let res_override =
                resource_type_overrides().get(&(schema.type_name.as_str(), prop_name));
            if let Some(TypeOverride::IntRange(min, max)) = res_override {
                let validate_fn = format!("validate_{}_range", prop_name.to_snake_case());
                let display = range_display_string(Some(*min), Some(*max));
                return (
                    format!(
                        r#"AttributeType::Custom {{
                name: "Float({})".to_string(),
                base: Box::new(AttributeType::Float),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        display, validate_fn
                    ),
                    None,
                );
            }
            // Use CF min/max if available (including one-sided), otherwise check known overrides
            let range: Option<(Option<i64>, Option<i64>)> =
                if prop.minimum.is_some() || prop.maximum.is_some() {
                    Some((prop.minimum, prop.maximum))
                } else {
                    known_int_range_overrides()
                        .get(prop_name)
                        .map(|&(min, max)| (Some(min), Some(max)))
                };
            if let Some((min, max)) = range {
                // Generate a ranged float type with validation
                let validate_fn = format!("validate_{}_range", prop_name.to_snake_case());
                let range_str = range_display_string(min, max);
                let display = if let Some(ref fmt) = prop.format {
                    format!("{}, {}", range_str, fmt)
                } else {
                    range_str
                };
                (
                    format!(
                        r#"AttributeType::Custom {{
                name: "Float({})".to_string(),
                base: Box::new(AttributeType::Float),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        display, validate_fn
                    ),
                    None,
                )
            } else if let Some(ref fmt) = prop.format {
                // Format-only float (e.g., double) - informational, no range validation
                (
                    format!(
                        r#"AttributeType::Custom {{
                name: "Float({})".to_string(),
                base: Box::new(AttributeType::Float),
                validate: |_| Ok(()),
                namespace: None,
                to_dsl: None,
            }}"#,
                        fmt
                    ),
                    None,
                )
            } else {
                ("AttributeType::Float".to_string(), None)
            }
        }
        Some("array") => {
            let (list_type, item_enum) = if let Some(items) = &prop.items {
                // Check if items has a $ref to a definition
                if let Some(ref_path) = &items.ref_path
                    && !ref_path.contains("/Tag")
                    && let Some(def) = resolve_ref(schema, ref_path)
                    && let Some(props) = &def.properties
                    && !props.is_empty()
                {
                    let def_name = ref_def_name(ref_path).unwrap_or(prop_name);
                    let struct_type = generate_struct_type(
                        def_name,
                        props,
                        &def.required,
                        schema,
                        namespace,
                        enums,
                    );
                    (
                        format!("AttributeType::List(Box::new({}))", struct_type),
                        None,
                    )
                } else {
                    let (item_type, item_enum) = cfn_type_to_carina_type_with_enum(
                        items, prop_name, schema, namespace, enums,
                    );
                    // If array items have enum values, propagate the enum info so the
                    // caller can register it. The item type uses String as a placeholder;
                    // the actual enum type will be substituted when the enum is registered.
                    let effective_item_type = if item_enum.is_some() {
                        "AttributeType::String".to_string()
                    } else {
                        item_type
                    };
                    (
                        format!("AttributeType::List(Box::new({}))", effective_item_type),
                        item_enum,
                    )
                }
            } else {
                (
                    "AttributeType::List(Box::new(AttributeType::String))".to_string(),
                    None,
                )
            };
            // Wrap in Custom type if minItems/maxItems constraints exist
            if prop.min_items.is_some() || prop.max_items.is_some() {
                let validate_fn = list_items_fn_name(prop.min_items, prop.max_items);
                let display = range_display_string(prop.min_items, prop.max_items);
                (
                    format!(
                        r#"AttributeType::Custom {{
                name: "List({})".to_string(),
                base: Box::new({}),
                validate: {},
                namespace: None,
                to_dsl: None,
            }}"#,
                        display, list_type, validate_fn
                    ),
                    item_enum,
                )
            } else {
                (list_type, item_enum)
            }
        }
        Some("object") => {
            // Check if this object has inline properties -> Struct
            if let Some(props) = &prop.properties
                && !props.is_empty()
            {
                return (
                    generate_struct_type(
                        prop_name,
                        props,
                        &prop.required,
                        schema,
                        namespace,
                        enums,
                    ),
                    None,
                );
            }
            // Check if this is an IAM policy document
            if prop_name.ends_with("PolicyDocument") {
                return ("super::iam_policy_document()".to_string(), None);
            }
            // Empty object with no or empty properties and additionalProperties: false
            // -> empty Struct (e.g., SimplePrefix)
            if prop.additional_properties == Some(false) {
                let struct_name = prop_name.to_pascal_case();
                return (
                    format!(
                        r#"AttributeType::Struct {{
                    name: "{}".to_string(),
                    fields: vec![],
                }}"#,
                        struct_name
                    ),
                    None,
                );
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

/// Convert a JSON default value to a Rust `Value::...` expression string.
/// Returns `None` for unsupported types (arrays, objects, null).
fn json_default_to_value_code(val: &serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("Value::String(\"{}\".to_string())", escaped))
        }
        serde_json::Value::Bool(b) => Some(format!("Value::Bool({})", b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(format!("Value::Int({})", i))
            } else {
                n.as_f64().map(|f| format!("Value::Float({:.1})", f))
            }
        }
        _ => None,
    }
}

/// Convert a JSON default value to a display string for markdown documentation.
/// Returns `None` for unsupported types (arrays, objects, null).
fn json_default_to_markdown(val: &serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => Some(format!("\"{}\"", s)),
        serde_json::Value::Bool(b) => Some(format!("{}", b)),
        serde_json::Value::Number(n) => Some(format!("{}", n)),
        _ => None,
    }
}

/// Compute singular block name from a plural snake_case attribute name.
///
/// Returns `Some(singular)` if the singular differs from the original,
/// or `None` if the name is already singular or doesn't match known patterns.
fn compute_block_name(name: &str) -> Option<String> {
    let singular = if let Some(stem) = name.strip_suffix("ies") {
        // policies -> policy, entries -> entry
        format!("{}y", stem)
    } else if let Some(stem) = name.strip_suffix("sses") {
        // accesses -> access
        format!("{}ss", stem)
    } else if let Some(stem) = name.strip_suffix("xes") {
        // boxes -> box
        format!("{}x", stem)
    } else if let Some(stem) = name.strip_suffix("ses") {
        // buses -> bus
        format!("{}s", stem)
    } else if let Some(stem) = name.strip_suffix('s') {
        if name.ends_with("ss") || name.ends_with("us") {
            // "access" ends in "ss", "status" ends in "us" -> skip
            return None;
        }
        // regions -> region, tags -> tag, sizes -> size
        stem.to_string()
    } else {
        return None;
    };

    if singular == name || singular.is_empty() {
        None
    } else {
        Some(singular)
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

fn collapse_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(c);
            prev_space = false;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collapse_whitespace() {
        assert_eq!(collapse_whitespace("no  extra"), "no extra");
        assert_eq!(collapse_whitespace("many      spaces"), "many spaces");
        assert_eq!(collapse_whitespace("a  b  c"), "a b c");
        assert_eq!(collapse_whitespace("already fine"), "already fine");
        assert_eq!(collapse_whitespace("            12 spaces"), " 12 spaces");
        assert_eq!(collapse_whitespace(""), "");
    }

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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (_, enum_info) =
            cfn_type_to_carina_type_with_enum(&prop, "IpProtocol", &schema, "", &BTreeMap::new());
        assert!(
            enum_info.is_some(),
            "IpProtocol should produce EnumInfo via overrides"
        );
        let info = enum_info.unwrap();
        assert_eq!(info.type_name, "IpProtocol");
        assert_eq!(info.values, vec!["tcp", "udp", "icmp", "icmpv6", "-1"]);
    }

    #[test]
    fn test_to_dsl_generated_for_hyphenated_enum_values() {
        // EnumInfo with hyphenated values should trigger to_dsl generation
        let with_hyphens = EnumInfo {
            type_name: "LogDestinationType".to_string(),
            values: vec![
                "cloud-watch-logs".to_string(),
                "s3".to_string(),
                "kinesis-data-firehose".to_string(),
            ],
        };
        assert!(
            with_hyphens.values.iter().any(|v| v.contains('-')),
            "Enum with hyphenated values should be detected"
        );

        let without_hyphens = EnumInfo {
            type_name: "InstanceTenancy".to_string(),
            values: vec![
                "default".to_string(),
                "dedicated".to_string(),
                "host".to_string(),
            ],
        };
        assert!(
            !without_hyphens.values.iter().any(|v| v.contains('-')),
            "Enum without hyphenated values should not be detected"
        );
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) =
            cfn_type_to_carina_type_with_enum(&prop, "CidrIp", &schema, "", &BTreeMap::new());
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) =
            cfn_type_to_carina_type_with_enum(&prop, "CidrIpv6", &schema, "", &BTreeMap::new());
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "PrivateIpAddress",
            &schema,
            "",
            &BTreeMap::new(),
        );
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) =
            cfn_type_to_carina_type_with_enum(&prop, "PublicIp", &schema, "", &BTreeMap::new());
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "SecondaryPrivateIpAddressCount",
            &schema,
            "",
            &BTreeMap::new(),
        );
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "AvailabilityZone",
            &schema,
            "",
            &BTreeMap::new(),
        );
        assert_eq!(type_str, "super::availability_zone()");

        // AvailabilityZoneId should use super::availability_zone_id()
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "AvailabilityZoneId",
            &schema,
            "",
            &BTreeMap::new(),
        );
        assert_eq!(type_str, "super::availability_zone_id()");
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "GenericProp", ""),
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "GenericProp", ""),
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "GenericProp", ""),
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "GenericProp", ""),
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
        };
        assert_eq!(
            list_element_type_display(&prop, "SubnetIds", ""),
            "`List<SubnetId>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "SecurityGroupIds", ""),
            "`List<SecurityGroupId>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "RouteTableIds", ""),
            "`List<RouteTableId>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "NetworkInterfaceIds", ""),
            "`List<NetworkInterfaceId>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "VpcEndpointIds", ""),
            "`List<VpcEndpointId>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "RoleArns", ""),
            "`List<Arn>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "CidrBlocks", ""),
            "`List<Ipv4Cidr>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "Ipv6CidrBlocks", ""),
            "`List<Ipv6Cidr>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "Names", ""),
            "`List<String>`"
        );
        assert_eq!(
            list_element_type_display(&prop, "SubnetId", ""),
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
                additional_properties: None,
                const_value: None,
                default_value: None,
                pattern: None,
                min_items: None,
                max_items: None,
                min_length: None,
                max_length: None,
                format: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
                additional_properties: None,
                const_value: None,
                default_value: None,
                pattern: None,
                min_items: None,
                max_items: None,
                min_length: None,
                max_length: None,
                format: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
                additional_properties: None,
                const_value: None,
                default_value: None,
                pattern: None,
                min_items: None,
                max_items: None,
                min_length: None,
                max_length: None,
                format: None,
            };
            let result = list_element_type_display(&prop, "GenericProp", "");
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
                additional_properties: None,
                const_value: None,
                default_value: None,
                pattern: None,
                min_items: None,
                max_items: None,
                min_length: None,
                max_length: None,
                format: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
                additional_properties: None,
                const_value: None,
                default_value: None,
                pattern: None,
                min_items: None,
                max_items: None,
                min_length: None,
                max_length: None,
                format: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
                additional_properties: None,
                const_value: None,
                default_value: None,
                pattern: None,
                min_items: None,
                max_items: None,
                min_length: None,
                max_length: None,
                format: None,
            })),
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "Ipv4NetmaskLength",
            &schema,
            "",
            &BTreeMap::new(),
        );
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
            cfn_type_to_carina_type_with_enum(&prop, "SomeCount", &schema, "", &BTreeMap::new());
        assert_eq!(type_str, "AttributeType::Int");
    }

    #[test]
    fn test_integer_with_only_minimum_produces_custom_type() {
        // Only minimum set - should produce Custom type with one-sided range
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
            cfn_type_to_carina_type_with_enum(&prop, "SomeCount", &schema, "", &BTreeMap::new());
        assert!(
            type_str.contains("AttributeType::Custom"),
            "Integer with only minimum should produce Custom type, got: {}",
            type_str
        );
        assert!(
            type_str.contains("Int(0..)"),
            "Custom type name should show one-sided range, got: {}",
            type_str
        );
    }

    #[test]
    fn test_integer_with_only_maximum_produces_custom_type() {
        // Only maximum set - should produce Custom type with one-sided range
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
            maximum: Some(100),
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
            cfn_type_to_carina_type_with_enum(&prop, "SomeCount", &schema, "", &BTreeMap::new());
        assert!(
            type_str.contains("AttributeType::Custom"),
            "Integer with only maximum should produce Custom type, got: {}",
            type_str
        );
        assert!(
            type_str.contains("Int(..=100)"),
            "Custom type name should show one-sided range, got: {}",
            type_str
        );
    }

    #[test]
    fn test_integer_enum_produces_custom_type() {
        // Integer enum values should produce Custom IntEnum type
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("Retention in days.".to_string()),
            enum_values: Some(vec![
                EnumValue::Int(1),
                EnumValue::Int(3),
                EnumValue::Int(5),
                EnumValue::Int(7),
                EnumValue::Int(14),
            ]),
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::Logs::LogGroup".to_string(),
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "RetentionInDays",
            &schema,
            "",
            &BTreeMap::new(),
        );
        assert!(
            type_str.contains("AttributeType::Custom"),
            "Integer enum should produce Custom type, got: {}",
            type_str
        );
        assert!(
            type_str.contains("IntEnum([1, 3, 5, 7, 14])"),
            "Custom type name should list enum values, got: {}",
            type_str
        );
        assert!(
            type_str.contains("validate_retention_in_days_int_enum"),
            "Should reference int enum validation function, got: {}",
            type_str
        );
    }

    #[test]
    fn test_type_display_string_one_sided_range() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("Count.".to_string()),
            enum_values: None,
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: Some(1),
            maximum: None,
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let enums = BTreeMap::new();
        let result = type_display_string("SecondaryPrivateIpAddressCount", &prop, &schema, &enums);
        assert_eq!(result, "Int(1..)");
    }

    #[test]
    fn test_type_display_string_int_enum() {
        // Use a property/resource pair that has no resource-scoped override
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("Priority values.".to_string()),
            enum_values: Some(vec![
                EnumValue::Int(1),
                EnumValue::Int(3),
                EnumValue::Int(5),
            ]),
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::SomeService::Resource".to_string(),
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
        let result = type_display_string("Priority", &prop, &schema, &enums);
        assert_eq!(result, "IntEnum([1, 3, 5])");
    }

    #[test]
    fn test_type_display_string_int_enum_resource_override() {
        // RetentionInDays on AWS::Logs::LogGroup has a resource-scoped IntEnum override
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("Retention in days.".to_string()),
            enum_values: Some(vec![
                EnumValue::Int(1),
                EnumValue::Int(3),
                EnumValue::Int(5),
            ]),
            items: None,
            ref_path: None,
            insertion_order: None,
            properties: None,
            required: vec![],
            minimum: None,
            maximum: None,
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
        };
        let schema = CfnSchema {
            type_name: "AWS::Logs::LogGroup".to_string(),
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
        // Resource-scoped override takes precedence over schema enum values
        let result = type_display_string("RetentionInDays", &prop, &schema, &enums);
        assert_eq!(
            result,
            "IntEnum([1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922, 3288, 3653])"
        );
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "DefaultSecurityGroup",
            &schema,
            "",
            &BTreeMap::new(),
        );
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) =
            cfn_type_to_carina_type_with_enum(&prop, "FromPort", &schema, "", &BTreeMap::new());
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) =
            cfn_type_to_carina_type_with_enum(&prop, "Arn", &schema, "", &BTreeMap::new());
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
    fn test_number_with_range_produces_custom_float_type() {
        // "number" type should produce Float with range constraints
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
            additional_properties: None,
            const_value: None,
            default_value: None,
            pattern: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            format: None,
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
        let (type_str, _) =
            cfn_type_to_carina_type_with_enum(&prop, "FromPort", &schema, "", &BTreeMap::new());
        assert!(
            type_str.contains("Float(0..=65535)"),
            "Number with range should produce Float type, got: {}",
            type_str
        );
        assert!(
            type_str.contains("AttributeType::Float"),
            "Number type should use AttributeType::Float base, got: {}",
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
        // New resource ID types
        assert_eq!(
            classify_resource_id("InstanceId"),
            ResourceIdKind::InstanceId
        );
        assert_eq!(
            classify_resource_id("NetworkInterfaceId"),
            ResourceIdKind::NetworkInterfaceId
        );
        assert_eq!(
            classify_resource_id("EniId"),
            ResourceIdKind::NetworkInterfaceId
        );
        assert_eq!(
            classify_resource_id("AllocationId"),
            ResourceIdKind::AllocationId
        );
        assert_eq!(
            classify_resource_id("PrefixListId"),
            ResourceIdKind::PrefixListId
        );
        assert_eq!(
            classify_resource_id("DestinationPrefixListId"),
            ResourceIdKind::PrefixListId
        );
        assert_eq!(
            classify_resource_id("CarrierGatewayId"),
            ResourceIdKind::CarrierGatewayId
        );
        assert_eq!(
            classify_resource_id("LocalGatewayId"),
            ResourceIdKind::LocalGatewayId
        );
        assert_eq!(
            classify_resource_id("NetworkAclId"),
            ResourceIdKind::NetworkAclId
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
            "InstanceId",
            "NetworkInterfaceId",
            "EniId",
            "AllocationId",
            "PrefixListId",
            "CarrierGatewayId",
            "LocalGatewayId",
            "NetworkAclId",
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

    #[test]
    fn test_resource_type_overrides_string_type() {
        // IAM Role's Arn should use iam_role_arn, not generic arn
        assert_eq!(
            infer_string_type("Arn", "AWS::IAM::Role"),
            Some("super::iam_role_arn()".to_string())
        );
        // Other resources' Arn should use generic arn
        assert_eq!(
            infer_string_type("Arn", "AWS::S3::Bucket"),
            Some("super::arn()".to_string())
        );
        // Non-overridden properties are unaffected
        assert_eq!(
            infer_string_type("VpcId", "AWS::IAM::Role"),
            Some("super::vpc_id()".to_string())
        );
        // EC2 Route's GatewayId should use gateway_id (union), not generic aws_resource_id
        assert_eq!(
            infer_string_type("GatewayId", "AWS::EC2::Route"),
            Some("super::gateway_id()".to_string())
        );
        // Other resources' GatewayId should use generic resource ID type
        assert_eq!(
            infer_string_type("GatewayId", "AWS::EC2::VPNGatewayRoutePropagation"),
            Some("super::aws_resource_id()".to_string())
        );
    }

    #[test]
    fn test_resource_type_overrides_string_type_display() {
        // IAM Role's Arn should display as IamRoleArn
        assert_eq!(
            infer_string_type_display("Arn", "AWS::IAM::Role"),
            "IamRoleArn"
        );
        // Other resources' Arn should display as generic Arn
        assert_eq!(infer_string_type_display("Arn", "AWS::S3::Bucket"), "Arn");
        // EC2 Route's GatewayId should display as GatewayId
        assert_eq!(
            infer_string_type_display("GatewayId", "AWS::EC2::Route"),
            "GatewayId"
        );
        // Other resources' GatewayId should use generic type
        assert_eq!(
            infer_string_type_display("GatewayId", "AWS::EC2::VPNGatewayRoutePropagation"),
            "AwsResourceId"
        );
    }

    #[test]
    fn test_resource_type_overrides_enum() {
        let overrides = resource_type_overrides();
        assert_eq!(
            overrides.get(&("AWS::EC2::VPNGateway", "Type")),
            Some(&TypeOverride::Enum(vec!["ipsec.1"]))
        );
    }

    #[test]
    fn test_resource_type_overrides_int_range() {
        let overrides = resource_type_overrides();
        assert_eq!(
            overrides.get(&("AWS::EC2::VPNGateway", "AmazonSideAsn")),
            Some(&TypeOverride::IntRange(1, 4294967294))
        );
        assert_eq!(
            overrides.get(&("AWS::EC2::TransitGateway", "AmazonSideAsn")),
            Some(&TypeOverride::IntRange(1, 4294967294))
        );
    }

    #[test]
    fn test_resource_type_overrides_int_enum() {
        let overrides = resource_type_overrides();
        assert_eq!(
            overrides.get(&("AWS::EC2::FlowLog", "MaxAggregationInterval")),
            Some(&TypeOverride::IntEnum(vec![60, 600]))
        );
        assert_eq!(
            overrides.get(&("AWS::Logs::LogGroup", "RetentionInDays")),
            Some(&TypeOverride::IntEnum(vec![
                1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192,
                2557, 2922, 3288, 3653
            ]))
        );
    }

    #[test]
    fn test_infer_string_type_cidr() {
        assert_eq!(
            infer_string_type("CidrBlock", ""),
            Some("types::ipv4_cidr()".to_string())
        );
        assert_eq!(
            infer_string_type("CidrIp", ""),
            Some("types::ipv4_cidr()".to_string())
        );
        assert_eq!(
            infer_string_type("Ipv6CidrBlock", ""),
            Some("types::ipv6_cidr()".to_string())
        );
        assert_eq!(
            infer_string_type("DestinationCidrBlock", ""),
            Some("types::ipv4_cidr()".to_string())
        );
    }

    #[test]
    fn test_infer_string_type_ip_address() {
        assert_eq!(
            infer_string_type("PrivateIpAddress", ""),
            Some("types::ipv4_address()".to_string())
        );
        assert_eq!(
            infer_string_type("PublicIp", ""),
            Some("types::ipv4_address()".to_string())
        );
        assert_eq!(
            infer_string_type("Ipv6IpAddress", ""),
            Some("types::ipv6_address()".to_string())
        );
    }

    #[test]
    fn test_infer_string_type_availability_zone() {
        assert_eq!(
            infer_string_type("AvailabilityZone", ""),
            Some("super::availability_zone()".to_string())
        );
        // AvailabilityZoneId should use availability_zone_id()
        assert_eq!(
            infer_string_type("AvailabilityZoneId", ""),
            Some("super::availability_zone_id()".to_string())
        );
    }

    #[test]
    fn test_infer_string_type_owner_id() {
        assert_eq!(
            infer_string_type("SourceSecurityGroupOwnerId", ""),
            Some("super::aws_account_id()".to_string())
        );
        assert_eq!(
            infer_string_type("PeerOwnerId", ""),
            Some("super::aws_account_id()".to_string())
        );
    }

    #[test]
    fn test_infer_string_type_new_resource_ids() {
        assert_eq!(
            infer_string_type("InstanceId", ""),
            Some("super::instance_id()".to_string())
        );
        assert_eq!(
            infer_string_type("NetworkInterfaceId", ""),
            Some("super::network_interface_id()".to_string())
        );
        assert_eq!(
            infer_string_type("EniId", ""),
            Some("super::network_interface_id()".to_string())
        );
        assert_eq!(
            infer_string_type("AllocationId", ""),
            Some("super::allocation_id()".to_string())
        );
        assert_eq!(
            infer_string_type("PrefixListId", ""),
            Some("super::prefix_list_id()".to_string())
        );
        assert_eq!(
            infer_string_type("DestinationPrefixListId", ""),
            Some("super::prefix_list_id()".to_string())
        );
        assert_eq!(
            infer_string_type("CarrierGatewayId", ""),
            Some("super::carrier_gateway_id()".to_string())
        );
        assert_eq!(
            infer_string_type("LocalGatewayId", ""),
            Some("super::local_gateway_id()".to_string())
        );
    }

    #[test]
    fn test_infer_string_type_ipam_pool_id() {
        assert_eq!(
            infer_string_type("IpamPoolId", ""),
            Some("super::ipam_pool_id()".to_string())
        );
        assert_eq!(
            infer_string_type("Ipv4IpamPoolId", ""),
            Some("super::ipam_pool_id()".to_string())
        );
    }

    #[test]
    fn test_infer_string_type_default_network_acl() {
        assert_eq!(
            infer_string_type("DefaultNetworkAcl", ""),
            Some("super::network_acl_id()".to_string())
        );
    }

    #[test]
    fn test_known_enum_aliases() {
        let aliases = known_enum_aliases();

        // IpProtocol should have "-1" -> "all" alias
        let ip_protocol = aliases.get("IpProtocol");
        assert!(ip_protocol.is_some(), "IpProtocol should have aliases");
        assert_eq!(ip_protocol.unwrap(), &vec![("-1", "all")]);
    }

    #[test]
    fn test_known_enum_aliases_included_in_valid_values() {
        // Verify that known_enum_overrides + known_enum_aliases produces correct VALID_* content
        let overrides = known_enum_overrides();
        let aliases = known_enum_aliases();

        // IpProtocol should have "all" in the combined list
        let ip_protocol_values = overrides.get("IpProtocol").unwrap();
        assert!(ip_protocol_values.contains(&"-1"), "Should have -1");

        let ip_protocol_aliases = aliases.get("IpProtocol").unwrap();
        let alias_values: Vec<&str> = ip_protocol_aliases.iter().map(|(_, a)| *a).collect();
        assert!(alias_values.contains(&"all"), "Should have 'all' alias");
    }

    #[test]
    fn test_compute_block_name_simple_plural() {
        assert_eq!(compute_block_name("regions"), Some("region".to_string()));
        assert_eq!(compute_block_name("tags"), Some("tag".to_string()));
        assert_eq!(
            compute_block_name("operating_regions"),
            Some("operating_region".to_string())
        );
    }

    #[test]
    fn test_compute_block_name_ies_suffix() {
        assert_eq!(compute_block_name("policies"), Some("policy".to_string()));
        assert_eq!(compute_block_name("entries"), Some("entry".to_string()));
    }

    #[test]
    fn test_compute_block_name_ses_xes_suffix() {
        assert_eq!(compute_block_name("buses"), Some("bus".to_string()));
        assert_eq!(compute_block_name("boxes"), Some("box".to_string()));
    }

    #[test]
    fn test_compute_block_name_sses_suffix() {
        assert_eq!(compute_block_name("accesses"), Some("access".to_string()));
        // "addresses" -> "address" (sses rule takes priority)
        assert_eq!(compute_block_name("addresses"), Some("address".to_string()));
    }

    #[test]
    fn test_compute_block_name_no_change() {
        // Already singular or doesn't match patterns
        assert_eq!(compute_block_name("name"), None);
        assert_eq!(compute_block_name("status"), None);
        assert_eq!(compute_block_name("access"), None);
    }

    #[test]
    fn test_generate_schema_code_uses_string_enum_for_enum_like_strings() {
        let mut properties = BTreeMap::new();
        properties.insert(
            "AddressFamily".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("The address family. Either IPv4 or IPv6.".to_string()),
                enum_values: Some(vec![
                    EnumValue::Str("IPv4".to_string()),
                    EnumValue::Str("IPv6".to_string()),
                ]),
                items: None,
                ref_path: None,
                insertion_order: None,
                properties: None,
                required: vec![],
                minimum: None,
                maximum: None,
                additional_properties: None,
                const_value: None,
                default_value: None,
                pattern: None,
                min_items: None,
                max_items: None,
                min_length: None,
                max_length: None,
                format: None,
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::EC2::IPAMPool".to_string(),
            description: None,
            properties,
            required: vec!["AddressFamily".to_string()],
            read_only_properties: vec![],
            create_only_properties: vec!["AddressFamily".to_string()],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::EC2::IPAMPool").unwrap();

        assert!(
            generated.contains("AttributeType::StringEnum {"),
            "enum-like strings should be emitted as StringEnum: {generated}"
        );
        assert!(
            !generated.contains("validate_address_family"),
            "StringEnum generation should not fall back to per-attribute validators: {generated}"
        );
        assert!(
            !generated.contains(".with_completions("),
            "enum completions should come from schema type metadata: {generated}"
        );
    }

    #[test]
    fn test_struct_field_enum_emits_string_enum_not_enum() {
        // Simulate a resource with a struct property whose field has an enum.
        // The struct comes from a $ref definition. If the definition's enum field
        // was not picked up during pre-scanning (e.g., due to snake_case conflict),
        // the fallback should still emit StringEnum, not Enum.
        let mut def_props = BTreeMap::new();
        def_props.insert(
            "Mode".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("The mode setting".to_string()),
                enum_values: Some(vec![
                    EnumValue::Str("off".to_string()),
                    EnumValue::Str("block-bidirectional".to_string()),
                    EnumValue::Str("block-ingress".to_string()),
                ]),
                items: None,
                ref_path: None,
                insertion_order: None,
                properties: None,
                required: vec![],
                minimum: None,
                maximum: None,
                additional_properties: None,
                const_value: None,
                default_value: None,
                pattern: None,
                min_items: None,
                max_items: None,
                min_length: None,
                max_length: None,
                format: None,
            },
        );

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "BlockSettings".to_string(),
            CfnDefinition {
                def_type: None,
                properties: Some(def_props),
                required: vec![],
                one_of: vec![],
                pattern: None,
                items: None,
                enum_values: None,
                min_length: None,
                max_length: None,
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "BlockSettings".to_string(),
            CfnProperty {
                prop_type: None,
                description: Some("Block settings".to_string()),
                enum_values: None,
                items: None,
                ref_path: Some("#/definitions/BlockSettings".to_string()),
                insertion_order: None,
                properties: None,
                required: vec![],
                minimum: None,
                maximum: None,
                additional_properties: None,
                const_value: None,
                default_value: None,
                pattern: None,
                min_items: None,
                max_items: None,
                min_length: None,
                max_length: None,
                format: None,
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::EC2::TestResource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::EC2::TestResource").unwrap();

        assert!(
            generated.contains("AttributeType::StringEnum {"),
            "struct field enums should be emitted as StringEnum: {generated}"
        );
        // Should have namespace
        assert!(
            generated.contains("namespace: Some("),
            "StringEnum should include namespace: {generated}"
        );
        // Should handle hyphens in values with to_dsl
        assert!(
            generated.contains("s.replace('-', \"_\")"),
            "hyphenated enum values should generate to_dsl: {generated}"
        );
    }

    #[test]
    fn test_const_value_generates_single_value_enum() {
        let prop = CfnProperty {
            const_value: Some(serde_json::Value::String("V_1".to_string())),
            ..Default::default()
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
        let enums = BTreeMap::new();
        let (_, enum_info) = cfn_type_to_carina_type_with_enum(
            &prop,
            "OutputSchemaVersion",
            &schema,
            "awscc.s3.bucket",
            &enums,
        );
        assert!(enum_info.is_some(), "const value should produce enum info");
        let info = enum_info.unwrap();
        assert_eq!(info.values, vec!["V_1"]);
        assert_eq!(info.type_name, "OutputSchemaVersion");
    }

    #[test]
    fn test_array_items_enum_propagated() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            items: Some(Box::new(CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                enum_values: Some(vec![
                    EnumValue::Str("GET".to_string()),
                    EnumValue::Str("PUT".to_string()),
                    EnumValue::Str("HEAD".to_string()),
                    EnumValue::Str("POST".to_string()),
                    EnumValue::Str("DELETE".to_string()),
                ]),
                ..Default::default()
            })),
            ..Default::default()
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
        let enums = BTreeMap::new();
        let (type_str, enum_info) = cfn_type_to_carina_type_with_enum(
            &prop,
            "AllowedMethods",
            &schema,
            "awscc.s3.bucket",
            &enums,
        );
        assert!(
            type_str.contains("List"),
            "array type should produce List: {type_str}"
        );
        assert!(
            enum_info.is_some(),
            "array item enum should be propagated back"
        );
        let info = enum_info.unwrap();
        assert_eq!(info.values.len(), 5);
        assert!(info.values.contains(&"GET".to_string()));
    }

    #[test]
    fn test_ref_enum_only_definition_resolved() {
        let mut definitions = BTreeMap::new();
        definitions.insert(
            "HttpMethod".to_string(),
            CfnDefinition {
                def_type: Some("string".to_string()),
                properties: None,
                required: vec![],
                one_of: vec![],
                pattern: None,
                items: None,
                enum_values: Some(vec![
                    EnumValue::Str("GET".to_string()),
                    EnumValue::Str("POST".to_string()),
                ]),
                min_length: None,
                max_length: None,
            },
        );
        let prop = CfnProperty {
            ref_path: Some("#/definitions/HttpMethod".to_string()),
            ..Default::default()
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
            definitions: Some(definitions),
            tagging: None,
        };
        let enums = BTreeMap::new();
        let (_, enum_info) = cfn_type_to_carina_type_with_enum(
            &prop,
            "HttpMethod",
            &schema,
            "awscc.s3.bucket",
            &enums,
        );
        assert!(
            enum_info.is_some(),
            "enum-only $ref definition should produce enum info"
        );
        let info = enum_info.unwrap();
        assert_eq!(info.values, vec!["GET", "POST"]);
        assert_eq!(info.type_name, "HttpMethod");
    }

    #[test]
    fn test_ref_array_with_enum_items_definition() {
        // Test a $ref to an array-typed definition where items have enum values
        // (e.g., BlockedEncryptionTypeList)
        let mut definitions = BTreeMap::new();
        definitions.insert(
            "BlockedEncryptionTypeList".to_string(),
            CfnDefinition {
                def_type: Some("array".to_string()),
                properties: None,
                required: vec![],
                one_of: vec![],
                pattern: None,
                items: Some(Box::new(CfnProperty {
                    prop_type: Some(TypeValue::Single("string".to_string())),
                    enum_values: Some(vec![
                        EnumValue::Str("NONE".to_string()),
                        EnumValue::Str("SSE-C".to_string()),
                    ]),
                    ..Default::default()
                })),
                enum_values: None,
                min_length: None,
                max_length: None,
            },
        );
        let prop = CfnProperty {
            ref_path: Some("#/definitions/BlockedEncryptionTypeList".to_string()),
            ..Default::default()
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
            definitions: Some(definitions),
            tagging: None,
        };
        let enums = BTreeMap::new();
        let (type_str, enum_info) = cfn_type_to_carina_type_with_enum(
            &prop,
            "BlockedEncryptionTypes",
            &schema,
            "awscc.s3.bucket",
            &enums,
        );
        assert!(
            type_str.contains("List"),
            "should produce List type: {type_str}"
        );
        assert!(
            enum_info.is_some(),
            "array item enum should be propagated from $ref definition"
        );
        let info = enum_info.unwrap();
        assert_eq!(info.values.len(), 2);
        assert!(info.values.contains(&"NONE".to_string()));
        assert!(info.values.contains(&"SSE-C".to_string()));
    }

    #[test]
    fn test_list_enum_generates_list_of_string_enum_in_schema_code() {
        // Ensure that when an array property has enum items, the generated
        // schema code wraps the StringEnum in List
        let mut properties = BTreeMap::new();
        properties.insert(
            "AllowedMethods".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("array".to_string())),
                items: Some(Box::new(CfnProperty {
                    prop_type: Some(TypeValue::Single("string".to_string())),
                    enum_values: Some(vec![
                        EnumValue::Str("GET".to_string()),
                        EnumValue::Str("PUT".to_string()),
                    ]),
                    ..Default::default()
                })),
                ..Default::default()
            },
        );
        let schema = CfnSchema {
            type_name: "AWS::S3::Bucket".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };
        let generated = generate_schema_code(&schema, "AWS::S3::Bucket").unwrap();
        assert!(
            generated.contains("AttributeType::List(Box::new(AttributeType::StringEnum"),
            "array with enum items should generate List(StringEnum): {generated}"
        );
    }

    #[test]
    fn test_struct_field_const_value_shows_enum_in_markdown() {
        let mut def_props = BTreeMap::new();
        def_props.insert(
            "ObjectLockEnabled".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                const_value: Some(serde_json::Value::String("Enabled".to_string())),
                description: Some("Object lock enabled".to_string()),
                ..Default::default()
            },
        );

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "ObjectLockConfiguration".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(def_props),
                required: vec![],
                enum_values: None,
                items: None,
                one_of: vec![],
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "ObjectLockConfiguration".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/ObjectLockConfiguration".to_string()),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::S3::Bucket".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let md = generate_markdown(&schema, "AWS::S3::Bucket").unwrap();
        assert!(
            md.contains("Enum (ObjectLockEnabled)"),
            "Struct field with const_value should display as Enum in markdown, got:\n{}",
            md.lines()
                .filter(|l| l.contains("object_lock_enabled"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn test_struct_field_ref_enum_shows_enum_in_markdown() {
        // Struct field with $ref to an enum-only definition
        let mut def_props = BTreeMap::new();
        def_props.insert(
            "Mode".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/RetentionMode".to_string()),
                ..Default::default()
            },
        );

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "DefaultRetention".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(def_props),
                required: vec![],
                enum_values: None,
                items: None,
                one_of: vec![],
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );
        definitions.insert(
            "RetentionMode".to_string(),
            CfnDefinition {
                def_type: Some("string".to_string()),
                properties: None,
                required: vec![],
                enum_values: Some(vec![
                    EnumValue::Str("COMPLIANCE".to_string()),
                    EnumValue::Str("GOVERNANCE".to_string()),
                ]),
                items: None,
                one_of: vec![],
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "DefaultRetention".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/DefaultRetention".to_string()),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::S3::Bucket".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let md = generate_markdown(&schema, "AWS::S3::Bucket").unwrap();
        assert!(
            md.contains("Enum (RetentionMode)"),
            "Struct field with $ref to enum-only definition should display as Enum, got:\n{}",
            md.lines()
                .filter(|l| l.contains("mode"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn test_shared_enum_type_across_structs_is_deduplicated_in_markdown() {
        // Two structs (Ingress, Egress) sharing the same enum field (IpProtocol)
        // should produce only one enum section in markdown
        let mut ingress_props = BTreeMap::new();
        ingress_props.insert(
            "IpProtocol".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/IpProtocol".to_string()),
                ..Default::default()
            },
        );

        let mut egress_props = BTreeMap::new();
        egress_props.insert(
            "IpProtocol".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/IpProtocol".to_string()),
                ..Default::default()
            },
        );

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "Ingress".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(ingress_props),
                required: vec![],
                enum_values: None,
                items: None,
                one_of: vec![],
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );
        definitions.insert(
            "Egress".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(egress_props),
                required: vec![],
                enum_values: None,
                items: None,
                one_of: vec![],
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );
        definitions.insert(
            "IpProtocol".to_string(),
            CfnDefinition {
                def_type: Some("string".to_string()),
                properties: None,
                required: vec![],
                enum_values: Some(vec![
                    EnumValue::Str("tcp".to_string()),
                    EnumValue::Str("udp".to_string()),
                ]),
                items: None,
                one_of: vec![],
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "SecurityGroupIngress".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("array".to_string())),
                items: Some(Box::new(CfnProperty {
                    ref_path: Some("#/definitions/Ingress".to_string()),
                    ..Default::default()
                })),
                ..Default::default()
            },
        );
        properties.insert(
            "SecurityGroupEgress".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("array".to_string())),
                items: Some(Box::new(CfnProperty {
                    ref_path: Some("#/definitions/Egress".to_string()),
                    ..Default::default()
                })),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::EC2::SecurityGroup".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let md = generate_markdown(&schema, "AWS::EC2::SecurityGroup").unwrap();
        let ip_protocol_count = md.matches("### ip_protocol (IpProtocol)").count();
        assert_eq!(
            ip_protocol_count,
            1,
            "Expected exactly 1 IpProtocol enum section, found {}.\nEnum sections:\n{}",
            ip_protocol_count,
            md.lines()
                .filter(|l| l.contains("IpProtocol"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn test_same_type_name_different_values_not_deduplicated() {
        // Two structs with a "Status" field but different enum values
        // should produce two separate enum sections
        let mut config_a_props = BTreeMap::new();
        config_a_props.insert(
            "Status".to_string(),
            CfnProperty {
                enum_values: Some(vec![
                    EnumValue::Str("Disabled".to_string()),
                    EnumValue::Str("Enabled".to_string()),
                ]),
                ..Default::default()
            },
        );

        let mut config_b_props = BTreeMap::new();
        config_b_props.insert(
            "Status".to_string(),
            CfnProperty {
                enum_values: Some(vec![
                    EnumValue::Str("Enabled".to_string()),
                    EnumValue::Str("Suspended".to_string()),
                ]),
                ..Default::default()
            },
        );

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "ConfigA".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(config_a_props),
                required: vec![],
                enum_values: None,
                items: None,
                one_of: vec![],
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );
        definitions.insert(
            "ConfigB".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(config_b_props),
                required: vec![],
                enum_values: None,
                items: None,
                one_of: vec![],
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "ConfigA".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/ConfigA".to_string()),
                ..Default::default()
            },
        );
        properties.insert(
            "ConfigB".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/ConfigB".to_string()),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let md = generate_markdown(&schema, "AWS::Test::Resource").unwrap();

        // With disambiguation, the two Status enums should have different type names
        assert!(
            md.contains("### status (ConfigAStatus)"),
            "Expected ConfigAStatus heading.\nEnum Values section:\n{}",
            md.lines()
                .filter(|l| l.contains("Status") || l.contains("status"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            md.contains("### status (ConfigBStatus)"),
            "Expected ConfigBStatus heading.\nEnum Values section:\n{}",
            md.lines()
                .filter(|l| l.contains("Status") || l.contains("status"))
                .collect::<Vec<_>>()
                .join("\n")
        );

        // DSL identifiers should also be disambiguated
        assert!(
            md.contains("awscc.test.resource.ConfigAStatus.Disabled"),
            "Expected disambiguated DSL identifier for ConfigA"
        );
        assert!(
            md.contains("awscc.test.resource.ConfigBStatus.Suspended"),
            "Expected disambiguated DSL identifier for ConfigB"
        );

        // Old ambiguous format should NOT appear
        assert!(
            !md.contains("### status (Status)"),
            "Should not have ambiguous '### status (Status)' heading"
        );
    }

    #[test]
    fn test_inline_enum_struct_fields_display_as_links_not_plain_enum() {
        // Struct fields with inline enum_values should display as linked
        // "[Enum (TypeName)](#...)" in the struct table, not plain "Enum".
        let mut config_props = BTreeMap::new();
        config_props.insert(
            "Status".to_string(),
            CfnProperty {
                enum_values: Some(vec![
                    EnumValue::Str("Enabled".to_string()),
                    EnumValue::Str("Disabled".to_string()),
                ]),
                description: Some("The status field".to_string()),
                ..Default::default()
            },
        );

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "MyConfig".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(config_props),
                required: vec![],
                enum_values: None,
                items: None,
                one_of: vec![],
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "MyConfig".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/MyConfig".to_string()),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let md = generate_markdown(&schema, "AWS::Test::Resource").unwrap();

        // The struct field table should contain a link, not plain "Enum"
        assert!(
            md.contains("[Enum (Status)]"),
            "Expected linked enum type in struct field table, got plain 'Enum'.\nStruct lines:\n{}",
            md.lines()
                .filter(|l| l.contains("status") || l.contains("Enum"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            !md.contains("| Enum |"),
            "Should not have plain 'Enum' in struct field table.\nStruct lines:\n{}",
            md.lines()
                .filter(|l| l.contains("status") || l.contains("Enum"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn test_generate_schema_code_emits_default_string_value() {
        let mut properties = BTreeMap::new();
        properties.insert(
            "Path".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("The path to the role.".to_string()),
                default_value: Some(serde_json::Value::String("/".to_string())),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::IAM::Role".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::IAM::Role").unwrap();

        assert!(
            generated.contains(r#".with_default(Value::String("/".to_string()))"#),
            "Should emit .with_default() for string default value: {generated}"
        );
        assert!(
            generated.contains("use carina_core::resource::Value;"),
            "Should import Value when defaults are present: {generated}"
        );
    }

    #[test]
    fn test_generate_schema_code_emits_default_bool_value() {
        let mut properties = BTreeMap::new();
        properties.insert(
            "DeletionProtectionEnabled".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("boolean".to_string())),
                description: Some("Whether deletion protection is enabled.".to_string()),
                default_value: Some(serde_json::Value::Bool(false)),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Logs::LogGroup".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Logs::LogGroup").unwrap();

        assert!(
            generated.contains(".with_default(Value::Bool(false))"),
            "Should emit .with_default() for boolean default value: {generated}"
        );
    }

    #[test]
    fn test_generate_schema_code_emits_default_int_value() {
        let mut properties = BTreeMap::new();
        properties.insert(
            "MaxRetries".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("integer".to_string())),
                description: Some("Maximum retry count.".to_string()),
                default_value: Some(serde_json::json!(3)),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Test::Resource").unwrap();

        assert!(
            generated.contains(".with_default(Value::Int(3))"),
            "Should emit .with_default() for integer default value: {generated}"
        );
    }

    #[test]
    fn test_generate_markdown_shows_default_values() {
        let mut properties = BTreeMap::new();
        properties.insert(
            "Path".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("The path.".to_string()),
                default_value: Some(serde_json::Value::String("/".to_string())),
                ..Default::default()
            },
        );
        properties.insert(
            "Enabled".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("boolean".to_string())),
                description: Some("Whether enabled.".to_string()),
                default_value: Some(serde_json::Value::Bool(true)),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let md = generate_markdown(&schema, "AWS::Test::Resource").unwrap();

        assert!(
            md.contains("**Default:** `\"/\"`"),
            "Should show string default in markdown: {md}"
        );
        assert!(
            md.contains("**Default:** `true`"),
            "Should show boolean default in markdown: {md}"
        );
    }

    #[test]
    fn test_json_default_to_value_code() {
        assert_eq!(
            json_default_to_value_code(&serde_json::Value::String("hello".to_string())),
            Some("Value::String(\"hello\".to_string())".to_string())
        );
        assert_eq!(
            json_default_to_value_code(&serde_json::Value::Bool(true)),
            Some("Value::Bool(true)".to_string())
        );
        assert_eq!(
            json_default_to_value_code(&serde_json::json!(42)),
            Some("Value::Int(42)".to_string())
        );
        assert_eq!(json_default_to_value_code(&serde_json::Value::Null), None,);
        assert_eq!(json_default_to_value_code(&serde_json::json!([])), None,);
    }

    #[test]
    fn test_string_with_pattern_produces_custom_type() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("The name of the log group.".to_string()),
            pattern: Some(r"^[.\-_/#A-Za-z0-9]{1,512}\Z".to_string()),
            ..Default::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::Logs::LogGroup".to_string(),
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "LogGroupName",
            &schema,
            "awscc.logs.log_group",
            &BTreeMap::new(),
        );
        assert!(
            type_str.contains("AttributeType::Custom"),
            "String with pattern should produce Custom type, got: {}",
            type_str
        );
        assert!(
            type_str.contains("validate_string_pattern_"),
            "Should reference pattern validation function, got: {}",
            type_str
        );
    }

    #[test]
    fn test_string_with_pattern_but_known_type_skips_pattern() {
        // KmsKeyId has a pattern but is already recognized as a KMS ARN type
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("The ARN of the KMS key.".to_string()),
            pattern: Some(r"^arn:[a-z0-9-]+:kms:[a-z0-9-]+:\d{12}:(key|alias)/.+\Z".to_string()),
            ..Default::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::Logs::LogGroup".to_string(),
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "KmsKeyId",
            &schema,
            "awscc.logs.log_group",
            &BTreeMap::new(),
        );
        // KmsKeyId should be resolved to a specific ARN type, not pattern-validated
        assert!(
            !type_str.contains("validate_string_pattern_"),
            "Known type (KmsKeyId) should not get pattern validation, got: {}",
            type_str
        );
    }

    #[test]
    fn test_pattern_validation_generated_in_schema_code() {
        let mut properties = BTreeMap::new();
        properties.insert(
            "LogGroupName".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("The name of the log group.".to_string()),
                pattern: Some(r"^[.\-_/#A-Za-z0-9]{1,512}\Z".to_string()),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Logs::LogGroup".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let code = generate_schema_code(&schema, "AWS::Logs::LogGroup").unwrap();
        assert!(
            code.contains("fn validate_string_pattern_"),
            "Generated code should contain pattern validation function:\n{}",
            code
        );
        assert!(
            code.contains("Regex::new"),
            "Pattern validation should use Regex:\n{}",
            code
        );
    }

    #[test]
    fn test_array_with_min_max_items_generates_custom_list_type() {
        // Array with minItems/maxItems should generate a Custom type wrapping List
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            items: Some(Box::new(CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                ..Default::default()
            })),
            min_items: Some(1),
            max_items: Some(10),
            ..Default::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPCEndpoint".to_string(),
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "PrivateDnsSpecifiedDomains",
            &schema,
            "awscc.ec2.vpc_endpoint",
            &enums,
        );
        assert!(
            type_str.contains("Custom"),
            "array with minItems/maxItems should produce Custom type: {type_str}"
        );
        assert!(
            type_str.contains("List"),
            "Custom type should wrap a List: {type_str}"
        );
        assert!(
            type_str.contains("validate_list_items_1_10"),
            "should reference items validation function: {type_str}"
        );
    }

    #[test]
    fn test_array_with_only_min_items_generates_custom_list_type() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            items: Some(Box::new(CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                ..Default::default()
            })),
            min_items: Some(1),
            max_items: None,
            ..Default::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPCEndpoint".to_string(),
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "SomeArray",
            &schema,
            "awscc.ec2.vpc_endpoint",
            &enums,
        );
        assert!(
            type_str.contains("Custom"),
            "array with only minItems should produce Custom type: {type_str}"
        );
        assert!(
            type_str.contains("1.."),
            "should show min bound in type name: {type_str}"
        );
    }

    #[test]
    fn test_array_without_min_max_items_produces_plain_list() {
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("array".to_string())),
            items: Some(Box::new(CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                ..Default::default()
            })),
            min_items: None,
            max_items: None,
            ..Default::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::EC2::VPCEndpoint".to_string(),
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "SomeArray",
            &schema,
            "awscc.ec2.vpc_endpoint",
            &enums,
        );
        assert!(
            !type_str.contains("Custom"),
            "array without minItems/maxItems should not produce Custom type: {type_str}"
        );
        assert!(
            type_str.contains("List"),
            "should produce plain List type: {type_str}"
        );
    }

    #[test]
    fn test_generate_schema_code_emits_list_items_validation() {
        // Test that generate_schema_code emits validation functions for array properties
        // with minItems/maxItems constraints (via struct field in a definition)
        let mut dns_props = BTreeMap::new();
        dns_props.insert(
            "PrivateDnsSpecifiedDomains".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("array".to_string())),
                items: Some(Box::new(CfnProperty {
                    prop_type: Some(TypeValue::Single("string".to_string())),
                    ..Default::default()
                })),
                min_items: Some(1),
                max_items: Some(10),
                ..Default::default()
            },
        );

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "DnsOptionsSpecification".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(dns_props),
                required: vec![],
                one_of: vec![],
                pattern: None,
                items: None,
                enum_values: None,
                min_length: None,
                max_length: None,
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "DnsOptions".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/DnsOptionsSpecification".to_string()),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::EC2::VPCEndpoint".to_string(),
            description: Some("Test".to_string()),
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::EC2::VPCEndpoint").unwrap();
        assert!(
            generated.contains("validate_list_items_1_10"),
            "should generate items validation function: {generated}"
        );
        assert!(
            generated.contains("Value::List(items)"),
            "validation function should check Value::List: {generated}"
        );
        assert!(
            generated.contains("1..=10"),
            "should show range 1..=10 in validation: {generated}"
        );
    }

    #[test]
    fn test_generate_schema_code_string_min_max_length() {
        let mut properties = BTreeMap::new();
        properties.insert(
            "LogGroupName".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("The name of the log group.".to_string()),
                min_length: Some(1),
                max_length: Some(512),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Logs::LogGroup".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Logs::LogGroup").unwrap();

        // Should generate a length validation function
        assert!(
            generated.contains("validate_string_length_1_512"),
            "Should generate length validation function: {generated}"
        );

        // Should use AttributeType::Custom wrapping String
        assert!(
            generated.contains("base: Box::new(AttributeType::String)"),
            "Should wrap String in Custom type with length constraint: {generated}"
        );

        // Should display length range
        assert!(
            generated.contains("1..=512"),
            "Should display length range: {generated}"
        );
    }

    #[test]
    fn test_generate_schema_code_string_min_length_only() {
        let mut properties = BTreeMap::new();
        properties.insert(
            "ResourceName".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("Name of the resource.".to_string()),
                min_length: Some(1),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Test::Resource").unwrap();

        assert!(
            generated.contains("validate_string_length_min_1"),
            "Should generate length validation function for min-only: {generated}"
        );
        assert!(
            generated.contains("1.."),
            "Should display open-ended range for min-only: {generated}"
        );
    }

    #[test]
    fn test_generate_schema_code_string_max_length_only() {
        let mut properties = BTreeMap::new();
        properties.insert(
            "Description".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("A description.".to_string()),
                max_length: Some(256),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Test::Resource").unwrap();

        assert!(
            generated.contains("validate_string_length_max_256"),
            "Should generate length validation function for max-only: {generated}"
        );
        assert!(
            generated.contains("..=256"),
            "Should display open-ended range for max-only: {generated}"
        );
    }

    #[test]
    fn test_ref_to_definition_with_pattern_produces_custom_type() {
        // When a property uses $ref to a definition that is a simple string with a pattern,
        // the pattern should be propagated to the referencing property.
        let prop = CfnProperty {
            ref_path: Some("#/definitions/iso8601UTC".to_string()),
            description: Some("Indicates when objects are deleted.".to_string()),
            ..Default::default()
        };

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "iso8601UTC".to_string(),
            CfnDefinition {
                def_type: Some("string".to_string()),
                pattern: Some(
                    r"^(\d{4})-(0[0-9]|1[0-2])-([0-2]\d|3[01])T([01]\d|2[0-4]):([0-5]\d):([0-6]\d)((\.\d{3})?)Z$".to_string(),
                ),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::S3::Bucket".to_string(),
            description: None,
            properties: BTreeMap::new(),
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "ExpirationDate",
            &schema,
            "awscc.s3.bucket",
            &BTreeMap::new(),
        );
        assert!(
            type_str.contains("AttributeType::Custom"),
            "Ref to string definition with pattern should produce Custom type, got: {}",
            type_str
        );
        assert!(
            type_str.contains("validate_string_pattern_"),
            "Should reference hash-based pattern validation function, got: {}",
            type_str
        );
    }

    #[test]
    fn test_ref_pattern_collected_in_generate_schema_code() {
        // When a struct field uses $ref to a definition with a pattern,
        // the pattern validation function should be generated in the schema code.
        let mut rule_props = BTreeMap::new();
        rule_props.insert(
            "ExpirationDate".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/iso8601UTC".to_string()),
                description: Some("Indicates when objects are deleted.".to_string()),
                ..Default::default()
            },
        );
        rule_props.insert(
            "Status".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                enum_values: Some(vec![
                    EnumValue::Str("Enabled".to_string()),
                    EnumValue::Str("Disabled".to_string()),
                ]),
                ..Default::default()
            },
        );

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "iso8601UTC".to_string(),
            CfnDefinition {
                def_type: Some("string".to_string()),
                pattern: Some(
                    r"^(\d{4})-(0[0-9]|1[0-2])-([0-2]\d|3[01])T([01]\d|2[0-4]):([0-5]\d):([0-6]\d)((\.\d{3})?)Z$".to_string(),
                ),
                ..Default::default()
            },
        );
        definitions.insert(
            "Rule".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(rule_props),
                required: vec!["Status".to_string()],
                ..Default::default()
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "LifecycleConfiguration".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("object".to_string())),
                properties: Some({
                    let mut lc_props = BTreeMap::new();
                    lc_props.insert(
                        "Rules".to_string(),
                        CfnProperty {
                            prop_type: Some(TypeValue::Single("array".to_string())),
                            items: Some(Box::new(CfnProperty {
                                ref_path: Some("#/definitions/Rule".to_string()),
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                    );
                    lc_props
                }),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::S3::Bucket".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::S3::Bucket").unwrap();
        assert!(
            generated.contains("fn validate_string_pattern_"),
            "Should generate hash-based pattern validation function for $ref definition pattern: {generated}"
        );
    }

    #[test]
    fn test_top_level_ref_pattern_collected_in_generate_schema_code() {
        // When a top-level property uses $ref to a definition with a pattern,
        // the pattern validation function should be generated.
        let mut definitions = BTreeMap::new();
        definitions.insert(
            "iso8601UTC".to_string(),
            CfnDefinition {
                def_type: Some("string".to_string()),
                pattern: Some(r"^(\d{4})-(0[0-9]|1[0-2])$".to_string()),
                ..Default::default()
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "CreatedDate".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/iso8601UTC".to_string()),
                description: Some("When the resource was created.".to_string()),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Test::Resource").unwrap();
        assert!(
            generated.contains("fn validate_string_pattern_"),
            "Should generate hash-based pattern validation function for top-level $ref pattern: {generated}"
        );
    }

    #[test]
    fn test_dedup_string_length_validation_functions() {
        // Two properties with identical maxLength constraints should share one validation function
        let mut properties = BTreeMap::new();
        properties.insert(
            "Name".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("Name field".to_string()),
                max_length: Some(1024),
                ..Default::default()
            },
        );
        properties.insert(
            "Prefix".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("Prefix field".to_string()),
                max_length: Some(1024),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Test::Resource").unwrap();

        // Should NOT have per-property validation functions
        assert!(
            !generated.contains("fn validate_name_length"),
            "Should not generate per-property length function: {generated}"
        );
        assert!(
            !generated.contains("fn validate_prefix_length"),
            "Should not generate per-property length function: {generated}"
        );

        // Should have a single shared validation function named by constraints
        let fn_count = generated.matches("fn validate_string_length_").count();
        assert_eq!(
            fn_count, 1,
            "Should generate exactly one shared length validation function, got {fn_count}: {generated}"
        );

        // Both properties should reference the same validation function
        let ref_count = generated.matches("validate_string_length_max_1024").count();
        assert!(
            ref_count >= 3,
            "Both properties should reference the shared function (1 def + 2 refs), got {ref_count}: {generated}"
        );
    }

    #[test]
    fn test_string_length_validation_uses_char_count() {
        // String length validation should use s.chars().count() (character count)
        // instead of s.len() (byte count) to correctly handle multi-byte characters.
        // e.g., "テスト" is 3 characters but 9 bytes in UTF-8.
        let mut properties = BTreeMap::new();
        properties.insert(
            "DisplayName".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                description: Some("Display name.".to_string()),
                max_length: Some(10),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Test::Resource").unwrap();

        // Should use chars().count() for character-based length, not byte-based len()
        assert!(
            generated.contains("s.chars().count()"),
            "String length validation should use s.chars().count() instead of s.len(): {generated}"
        );
        assert!(
            !generated.contains("let len = s.len()"),
            "String length validation should NOT use s.len() for string properties: {generated}"
        );
    }

    #[test]
    fn test_dedup_pattern_validation_functions() {
        // Two properties with identical pattern constraints should share one validation function
        let mut properties = BTreeMap::new();
        properties.insert(
            "ObjectSizeGreaterThan".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                pattern: Some("[0-9]+".to_string()),
                ..Default::default()
            },
        );
        properties.insert(
            "ObjectSizeLessThan".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("string".to_string())),
                pattern: Some("[0-9]+".to_string()),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: None,
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Test::Resource").unwrap();

        // Should NOT have per-property pattern validation functions
        assert!(
            !generated.contains("fn validate_object_size_greater_than_pattern"),
            "Should not generate per-property pattern function: {generated}"
        );
        assert!(
            !generated.contains("fn validate_object_size_less_than_pattern"),
            "Should not generate per-property pattern function: {generated}"
        );

        // Should have exactly one shared pattern validation function
        let fn_count = generated.matches("fn validate_string_pattern_").count();
        assert_eq!(
            fn_count, 1,
            "Should generate exactly one shared pattern validation function, got {fn_count}: {generated}"
        );
    }

    #[test]
    fn test_dedup_list_items_validation_functions() {
        // Two array properties with identical minItems/maxItems should share one validation function
        let mut def_props = BTreeMap::new();
        def_props.insert(
            "AllowedOrigins".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("array".to_string())),
                items: Some(Box::new(CfnProperty {
                    prop_type: Some(TypeValue::Single("string".to_string())),
                    ..Default::default()
                })),
                min_items: Some(1),
                max_items: Some(100),
                ..Default::default()
            },
        );
        def_props.insert(
            "AllowedHeaders".to_string(),
            CfnProperty {
                prop_type: Some(TypeValue::Single("array".to_string())),
                items: Some(Box::new(CfnProperty {
                    prop_type: Some(TypeValue::Single("string".to_string())),
                    ..Default::default()
                })),
                min_items: Some(1),
                max_items: Some(100),
                ..Default::default()
            },
        );

        let mut definitions = BTreeMap::new();
        definitions.insert(
            "CorsRule".to_string(),
            CfnDefinition {
                def_type: Some("object".to_string()),
                properties: Some(def_props),
                required: vec![],
                one_of: vec![],
                items: None,
                enum_values: None,
                pattern: None,
                min_length: None,
                max_length: None,
            },
        );

        let mut properties = BTreeMap::new();
        properties.insert(
            "CorsRules".to_string(),
            CfnProperty {
                ref_path: Some("#/definitions/CorsRule".to_string()),
                ..Default::default()
            },
        );

        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
            description: None,
            properties,
            required: vec![],
            read_only_properties: vec![],
            create_only_properties: vec![],
            write_only_properties: vec![],
            primary_identifier: None,
            definitions: Some(definitions),
            tagging: None,
        };

        let generated = generate_schema_code(&schema, "AWS::Test::Resource").unwrap();

        // Should NOT have per-property items validation functions
        assert!(
            !generated.contains("fn validate_allowed_origins_items"),
            "Should not generate per-property items function: {generated}"
        );
        assert!(
            !generated.contains("fn validate_allowed_headers_items"),
            "Should not generate per-property items function: {generated}"
        );

        // Should have exactly one shared items validation function
        let fn_count = generated.matches("fn validate_list_items_").count();
        assert_eq!(
            fn_count, 1,
            "Should generate exactly one shared items validation function, got {fn_count}: {generated}"
        );
    }

    #[test]
    fn test_integer_format_int64_generates_int_with_format() {
        // "type": "integer", "format": "int64" should include int64 in type name
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("A 64-bit integer value".to_string()),
            format: Some("int64".to_string()),
            ..CfnProperty::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
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
            cfn_type_to_carina_type_with_enum(&prop, "SomeValue", &schema, "", &BTreeMap::new());
        assert!(
            type_str.contains("int64"),
            "int64 format should appear in type: {type_str}"
        );
    }

    #[test]
    fn test_integer_format_int64_display() {
        // Markdown display for int64 should show Int(int64)
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("A 64-bit integer value".to_string()),
            format: Some("int64".to_string()),
            ..CfnProperty::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
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
        let display = type_display_string("SomeValue", &prop, &schema, &BTreeMap::new());
        assert!(
            display.contains("int64"),
            "int64 format should appear in display: {display}"
        );
    }

    #[test]
    fn test_string_format_uri_type() {
        // "type": "string", "format": "uri" should produce String(uri) type
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("A URL".to_string()),
            format: Some("uri".to_string()),
            ..CfnProperty::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
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
            cfn_type_to_carina_type_with_enum(&prop, "Url", &schema, "", &BTreeMap::new());
        assert!(
            type_str.contains("uri"),
            "uri format should appear in type: {type_str}"
        );
    }

    #[test]
    fn test_string_format_uri_display() {
        // "type": "string", "format": "uri" should show String(uri) in display
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("A URL".to_string()),
            format: Some("uri".to_string()),
            ..CfnProperty::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
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
        let display = type_display_string("Url", &prop, &schema, &BTreeMap::new());
        assert!(
            display.contains("uri"),
            "uri format should appear in display: {display}"
        );
    }

    #[test]
    fn test_numeric_string_pattern_generates_numeric_string_type() {
        // "type": "string", "pattern": "[0-9]+" should be treated as numeric string
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("Object size in bytes".to_string()),
            pattern: Some("[0-9]+".to_string()),
            max_length: Some(20),
            ..CfnProperty::default()
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "ObjectSizeGreaterThan",
            &schema,
            "",
            &BTreeMap::new(),
        );
        assert!(
            type_str.contains("NumericString"),
            "Numeric string pattern should produce NumericString type: {type_str}"
        );
    }

    #[test]
    fn test_numeric_string_pattern_display() {
        // Markdown display for numeric string pattern
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("Object size in bytes".to_string()),
            pattern: Some("[0-9]+".to_string()),
            max_length: Some(20),
            ..CfnProperty::default()
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
        let display =
            type_display_string("ObjectSizeGreaterThan", &prop, &schema, &BTreeMap::new());
        assert!(
            display.contains("NumericString"),
            "Numeric string should show NumericString in display: {display}"
        );
    }

    #[test]
    fn test_number_format_double_display() {
        // "type": "number", "format": "double" should show Float(double) in display
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("number".to_string())),
            description: Some("A floating point value".to_string()),
            format: Some("double".to_string()),
            ..CfnProperty::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
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
        let display = type_display_string("Value", &prop, &schema, &BTreeMap::new());
        assert!(
            display.contains("double"),
            "double format should appear in display: {display}"
        );
    }

    #[test]
    fn test_integer_format_int64_with_range_combines() {
        // "type": "integer", "format": "int64", "minimum": 0, "maximum": 100
        // should show both the format and range
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("integer".to_string())),
            description: Some("A bounded int64 value".to_string()),
            format: Some("int64".to_string()),
            minimum: Some(0),
            maximum: Some(100),
            ..CfnProperty::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
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
            cfn_type_to_carina_type_with_enum(&prop, "BoundedValue", &schema, "", &BTreeMap::new());
        // Should have range validation (0..=100) and int64 format info
        assert!(
            type_str.contains("0..=100"),
            "Should have range constraint: {type_str}"
        );
        assert!(
            type_str.contains("int64"),
            "Should have int64 format info: {type_str}"
        );
    }

    #[test]
    fn test_numeric_string_pattern_with_max_length_combines_constraints() {
        // "type": "string", "pattern": "[0-9]+", "maxLength": 20
        // should produce a validate function that checks BOTH pattern and length
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("Object size in bytes".to_string()),
            pattern: Some("[0-9]+".to_string()),
            max_length: Some(20),
            ..CfnProperty::default()
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
        let (type_str, _) = cfn_type_to_carina_type_with_enum(
            &prop,
            "ObjectSizeGreaterThan",
            &schema,
            "",
            &BTreeMap::new(),
        );
        // Should mention both constraints
        assert!(
            type_str.contains("NumericString"),
            "Should have NumericString type: {type_str}"
        );
        assert!(
            type_str.contains("20"),
            "Should have max length constraint 20: {type_str}"
        );
    }

    #[test]
    fn test_string_pattern_with_length_constraints_combines() {
        // "type": "string", "pattern": "^[a-z]+$", "minLength": 1, "maxLength": 64
        // should produce a validate function that checks BOTH pattern and length
        let prop = CfnProperty {
            prop_type: Some(TypeValue::Single("string".to_string())),
            description: Some("A constrained string".to_string()),
            pattern: Some("^[a-z]+$".to_string()),
            min_length: Some(1),
            max_length: Some(64),
            ..CfnProperty::default()
        };
        let schema = CfnSchema {
            type_name: "AWS::Test::Resource".to_string(),
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
            cfn_type_to_carina_type_with_enum(&prop, "SomeName", &schema, "", &BTreeMap::new());
        // Should mention both constraints
        assert!(
            type_str.contains("pattern"),
            "Should have pattern constraint: {type_str}"
        );
        assert!(
            type_str.contains("len:"),
            "Should have length constraint: {type_str}"
        );
    }

    #[test]
    fn test_disambiguate_prefixes_colliding_struct_field_enums() {
        // When multiple struct field enums share the same type_name but have different
        // values, they should be disambiguated by prefixing the parent struct name.
        // This ensures that e.g. VersioningConfiguration.Status (Enabled/Suspended) and
        // IntelligentTieringConfiguration.Status (Enabled/Disabled) get distinct type names.
        // See: https://github.com/carina-rs/carina/issues/640
        let mut enums = BTreeMap::new();

        enums.insert(
            "VersioningConfiguration.Status".to_string(),
            EnumInfo {
                type_name: "Status".to_string(),
                values: vec!["Enabled".to_string(), "Suspended".to_string()],
            },
        );
        enums.insert(
            "IntelligentTieringConfiguration.Status".to_string(),
            EnumInfo {
                type_name: "Status".to_string(),
                values: vec!["Disabled".to_string(), "Enabled".to_string()],
            },
        );

        disambiguate_enum_type_names(&mut enums);

        let versioning = enums.get("VersioningConfiguration.Status").unwrap();
        assert_eq!(
            versioning.type_name, "VersioningConfigurationStatus",
            "Colliding struct field enum should be prefixed with parent struct name"
        );
        let tiering = enums.get("IntelligentTieringConfiguration.Status").unwrap();
        assert_eq!(
            tiering.type_name, "IntelligentTieringConfigurationStatus",
            "Colliding struct field enum should be prefixed with parent struct name"
        );
    }

    #[test]
    fn test_format_field_parsed_from_schema() {
        // Verify that the format field is correctly parsed from JSON
        let json = r#"{
            "type": "integer",
            "format": "int64",
            "description": "Test"
        }"#;
        let prop: CfnProperty = serde_json::from_str(json).unwrap();
        assert_eq!(prop.format, Some("int64".to_string()));
    }
}
