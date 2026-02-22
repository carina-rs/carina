//! AWS Cloud Control type definitions and validators
//!
//! This module contains stable utility code for AWS-specific types,
//! validators, and enum normalization. These functions are NOT generated
//! from CloudFormation schemas — they are hand-written and imported by
//! the generated `mod.rs`.

use carina_core::resource::Value;
use carina_core::schema::{AttributeType, ResourceSchema, StructField};
use carina_core::utils::{extract_enum_value, validate_enum_namespace};

/// AWS Cloud Control schema configuration
///
/// Combines the generated ResourceSchema with AWS-specific metadata
/// that was previously in ResourceConfig.
pub struct AwsccSchemaConfig {
    /// AWS CloudFormation type name (e.g., "AWS::EC2::VPC")
    pub aws_type_name: &'static str,
    /// Resource type name used in DSL (e.g., "ec2.vpc")
    pub resource_type_name: &'static str,
    /// Whether this resource type uses tags
    pub has_tags: bool,
    /// The resource schema with attribute definitions
    pub schema: ResourceSchema,
}

/// Tags type for AWS resources (Terraform-style map)
pub(crate) fn tags_type() -> AttributeType {
    AttributeType::Map(Box::new(AttributeType::String))
}

/// Check if `input` matches any of `valid_values` using enum matching rules:
/// exact match, case-insensitive, or underscore-to-hyphen (case-insensitive).
/// Returns the matched valid value if found.
fn find_matching_enum_value<'a>(input: &str, valid_values: &[&'a str]) -> Option<&'a str> {
    // Exact match
    if let Some(&v) = valid_values.iter().find(|&&v| v == input) {
        return Some(v);
    }
    // Case-insensitive match
    if let Some(&v) = valid_values
        .iter()
        .find(|&&v| v.eq_ignore_ascii_case(input))
    {
        return Some(v);
    }
    // Underscore-to-hyphen match (case-insensitive)
    let hyphenated = input.replace('_', "-");
    if let Some(&v) = valid_values
        .iter()
        .find(|&&v| v.eq_ignore_ascii_case(&hyphenated))
    {
        return Some(v);
    }
    None
}

/// Canonicalize an enum value by matching against valid values.
/// Handles exact match, case-insensitive match, and underscore-to-hyphen conversion.
pub(crate) fn canonicalize_enum_value(raw: &str, valid_values: &[&str]) -> String {
    find_matching_enum_value(raw, valid_values)
        .unwrap_or(raw)
        .to_string()
}

/// Validate a namespaced enum value.
/// Returns Ok(()) if valid, Err with bare reason string if invalid.
/// Callers are responsible for adding context (e.g., what value was provided).
pub(crate) fn validate_namespaced_enum(
    value: &Value,
    type_name: &str,
    namespace: &str,
    valid_values: &[&str],
) -> Result<(), String> {
    if let Value::String(s) = value {
        validate_enum_namespace(s, type_name, namespace)?;

        let normalized = extract_enum_value(s);
        if find_matching_enum_value(normalized, valid_values).is_some() {
            Ok(())
        } else {
            Err(format!("expected one of: {}", valid_values.join(", ")))
        }
    } else {
        Err("Expected string".to_string())
    }
}

/// IPAM Pool ID type (e.g., "ipam-pool-0123456789abcdef0")
/// Validates format: ipam-pool-{hex} where hex is 8+ hex digits
pub(crate) fn ipam_pool_id() -> AttributeType {
    AttributeType::Custom {
        name: "IpamPoolId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_ipam_pool_id(s)
                    .map_err(|reason| format!("Invalid IPAM Pool ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

fn validate_ipam_pool_id(id: &str) -> Result<(), String> {
    let Some(hex_part) = id.strip_prefix("ipam-pool-") else {
        return Err("expected format 'ipam-pool-{hex}'".to_string());
    };
    if hex_part.len() < 8 {
        return Err("hex part must be at least 8 characters".to_string());
    }
    if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("hex part must contain only hex digits".to_string());
    }
    Ok(())
}

/// ARN type (e.g., "arn:aws:s3:::my-bucket")
pub(crate) fn arn() -> AttributeType {
    AttributeType::Custom {
        name: "Arn".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_arn(s).map_err(|reason| format!("Invalid ARN '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

pub fn validate_arn(arn: &str) -> Result<(), String> {
    if !arn.starts_with("arn:") {
        return Err("must start with 'arn:'".to_string());
    }
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() < 6 {
        return Err(
            "must have at least 6 colon-separated parts (arn:partition:service:region:account:resource)".to_string()
        );
    }
    Ok(())
}

/// AWS resource ID type (e.g., "vpc-1a2b3c4d", "subnet-0123456789abcdef0")
/// Validates format: {prefix}-{hex} where hex is 8+ hex digits
pub(crate) fn aws_resource_id() -> AttributeType {
    AttributeType::Custom {
        name: "AwsResourceId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_aws_resource_id(s)
                    .map_err(|reason| format!("Invalid resource ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

fn validate_aws_resource_id(id: &str) -> Result<(), String> {
    let Some(dash_pos) = id.find('-') else {
        return Err("expected format 'prefix-hexdigits'".to_string());
    };

    let prefix = &id[..dash_pos];
    let hex_part = &id[dash_pos + 1..];

    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err("prefix must be lowercase alphanumeric".to_string());
    }

    if hex_part.len() < 8 {
        return Err("ID part must be at least 8 characters after prefix".to_string());
    }

    if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("ID part must contain only hex digits".to_string());
    }

    Ok(())
}

/// Validate a resource ID with a specific prefix
fn validate_prefixed_resource_id(id: &str, expected_prefix: &str) -> Result<(), String> {
    let expected_format = format!("{}-xxxxxxxx", expected_prefix);
    if !id.starts_with(&format!("{}-", expected_prefix)) {
        return Err(format!("expected format '{}'", expected_format));
    }
    // Reuse existing validation for the rest
    validate_aws_resource_id(id)
}

/// VPC ID type (e.g., "vpc-1a2b3c4d")
pub(crate) fn vpc_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpcId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "vpc")
                    .map_err(|reason| format!("Invalid VPC ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Subnet ID type (e.g., "subnet-0123456789abcdef0")
pub(crate) fn subnet_id() -> AttributeType {
    AttributeType::Custom {
        name: "SubnetId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "subnet")
                    .map_err(|reason| format!("Invalid Subnet ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Security Group ID type (e.g., "sg-12345678")
pub(crate) fn security_group_id() -> AttributeType {
    AttributeType::Custom {
        name: "SecurityGroupId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "sg")
                    .map_err(|reason| format!("Invalid Security Group ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Internet Gateway ID type (e.g., "igw-12345678")
pub(crate) fn internet_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "InternetGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "igw")
                    .map_err(|reason| format!("Invalid Internet Gateway ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Route Table ID type (e.g., "rtb-abcdef12")
pub(crate) fn route_table_id() -> AttributeType {
    AttributeType::Custom {
        name: "RouteTableId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "rtb")
                    .map_err(|reason| format!("Invalid Route Table ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// NAT Gateway ID type (e.g., "nat-0123456789abcdef0")
pub(crate) fn nat_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "NatGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "nat")
                    .map_err(|reason| format!("Invalid NAT Gateway ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// VPC Peering Connection ID type (e.g., "pcx-12345678")
pub(crate) fn vpc_peering_connection_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpcPeeringConnectionId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "pcx").map_err(|reason| {
                    format!("Invalid VPC Peering Connection ID '{}': {}", s, reason)
                })
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Transit Gateway ID type (e.g., "tgw-0123456789abcdef0")
pub(crate) fn transit_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "TransitGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "tgw")
                    .map_err(|reason| format!("Invalid Transit Gateway ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// VPN Gateway ID type (e.g., "vgw-12345678")
pub(crate) fn vpn_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpnGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "vgw")
                    .map_err(|reason| format!("Invalid VPN Gateway ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Gateway ID type — union of InternetGatewayId and VpnGatewayId.
/// Used for attributes like ec2_route.gateway_id that accept both igw-* and vgw-* IDs.
pub(crate) fn gateway_id() -> AttributeType {
    AttributeType::Union(vec![internet_gateway_id(), vpn_gateway_id()])
}

/// Egress Only Internet Gateway ID type (e.g., "eigw-12345678")
#[allow(dead_code)] // TODO: codegen should use this instead of internet_gateway_id() for eigw attributes
pub(crate) fn egress_only_internet_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "EgressOnlyInternetGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "eigw").map_err(|reason| {
                    format!(
                        "Invalid Egress Only Internet Gateway ID '{}': {}",
                        s, reason
                    )
                })
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// VPC Endpoint ID type (e.g., "vpce-0123456789abcdef0")
pub(crate) fn vpc_endpoint_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpcEndpointId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "vpce")
                    .map_err(|reason| format!("Invalid VPC Endpoint ID '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Valid AWS regions (in AWS format with hyphens)
const VALID_REGIONS: &[&str] = &[
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-northeast-3",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-south-1",
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-central-1",
    "eu-north-1",
    "ca-central-1",
    "sa-east-1",
];

/// AWSCC region type with custom validation
/// Accepts:
/// - DSL format: awscc.Region.ap_northeast_1
/// - AWS string format: "ap-northeast-1"
/// - Shorthand: ap_northeast_1
pub fn awscc_region() -> AttributeType {
    AttributeType::Custom {
        name: "Region".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_enum_namespace(s, "Region", "awscc")
                    .map_err(|reason| format!("Invalid region '{}': {}", s, reason))?;
                let normalized = extract_enum_value(s).replace('_', "-");
                if VALID_REGIONS.contains(&normalized.as_str()) {
                    Ok(())
                } else {
                    Err(format!(
                        "Invalid region '{}', expected one of: {} or DSL format like awscc.Region.ap_northeast_1",
                        s,
                        VALID_REGIONS.join(", ")
                    ))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: Some("awscc".to_string()),
        to_dsl: None,
    }
}

/// Availability Zone type (e.g., "us-east-1a", "ap-northeast-1c")
/// Validates format: region + single letter zone identifier
pub(crate) fn availability_zone() -> AttributeType {
    AttributeType::Custom {
        name: "AvailabilityZone".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_enum_namespace(s, "AvailabilityZone", "awscc")
                    .map_err(|reason| format!("Invalid availability zone '{}': {}", s, reason))?;
                let extracted = extract_enum_value(s);
                let normalized = extracted.replace('_', "-");
                validate_availability_zone(&normalized)
                    .map_err(|reason| format!("Invalid availability zone '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: Some("awscc".to_string()),
        to_dsl: Some(|s: &str| s.replace('-', "_")),
    }
}

/// Validate availability zone format.
/// Returns the reason for failure (e.g., "must end with a zone letter (a-z)"),
/// without embedding the input value. Callers add context as needed.
fn validate_availability_zone(az: &str) -> Result<(), String> {
    // Must end with a single lowercase letter (zone identifier)
    let zone_letter = az.chars().last();
    if !zone_letter.is_some_and(|c| c.is_ascii_lowercase()) {
        return Err("must end with a zone letter (a-z)".to_string());
    }

    // Region part is everything except the last character
    let region = &az[..az.len() - 1];

    // Region must match pattern: lowercase-lowercase-digit
    // e.g., "us-east-1", "ap-northeast-1", "eu-west-2"
    let parts: Vec<&str> = region.split('-').collect();
    if parts.len() < 3 {
        return Err("expected format like 'us-east-1a'".to_string());
    }

    // Last part of region must be a number
    let last = parts.last().unwrap();
    if last.parse::<u8>().is_err() {
        return Err("region must end with a number".to_string());
    }

    // All other parts must be lowercase alphabetic
    for part in &parts[..parts.len() - 1] {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_lowercase()) {
            return Err("expected format like 'us-east-1a'".to_string());
        }
    }

    Ok(())
}

/// Validate an ARN for a specific AWS service and optional resource prefix
fn validate_service_arn(
    arn: &str,
    expected_service: &str,
    resource_prefix: Option<&str>,
) -> Result<(), String> {
    validate_arn(arn)?;
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts[2] != expected_service {
        return Err(format!(
            "expected {} service, got '{}'",
            expected_service, parts[2]
        ));
    }
    if let Some(prefix) = resource_prefix
        && !parts[5].starts_with(prefix)
    {
        return Err(format!(
            "expected resource starting with '{}', got '{}'",
            prefix, parts[5]
        ));
    }
    Ok(())
}

/// IAM Role ARN type (e.g., "arn:aws:iam::123456789012:role/MyRole")
pub(crate) fn iam_role_arn() -> AttributeType {
    AttributeType::Custom {
        name: "IamRoleArn".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_service_arn(s, "iam", Some("role/"))
                    .map_err(|reason| format!("Invalid IAM Role ARN '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// IAM Policy ARN type (e.g., "arn:aws:iam::123456789012:policy/MyPolicy")
pub(crate) fn iam_policy_arn() -> AttributeType {
    AttributeType::Custom {
        name: "IamPolicyArn".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_service_arn(s, "iam", Some("policy/"))
                    .map_err(|reason| format!("Invalid IAM Policy ARN '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// KMS Key ARN type (e.g., "arn:aws:kms:us-east-1:123456789012:key/1234abcd-12ab-34cd-56ef-1234567890ab")
pub(crate) fn kms_key_arn() -> AttributeType {
    AttributeType::Custom {
        name: "KmsKeyArn".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_service_arn(s, "kms", Some("key/"))
                    .map_err(|reason| format!("Invalid KMS Key ARN '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// KMS Key ID type - accepts multiple formats:
/// - Key ARN: "arn:aws:kms:us-east-1:123456789012:key/1234abcd-..."
/// - Key alias ARN: "arn:aws:kms:us-east-1:123456789012:alias/my-key"
/// - Key alias: "alias/my-key"
/// - Key ID: "1234abcd-12ab-34cd-56ef-1234567890ab"
pub(crate) fn kms_key_id() -> AttributeType {
    AttributeType::Custom {
        name: "KmsKeyId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_kms_key_id(s)
                    .map_err(|reason| format!("Invalid KMS key identifier '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Check if a string is a valid UUID (8-4-4-4-12 hex digits)
fn is_uuid(s: &str) -> bool {
    let expected_lens = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip(expected_lens.iter())
            .all(|(part, &len)| part.len() == len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

fn validate_kms_key_id(value: &str) -> Result<(), String> {
    // Accept KMS ARNs with key/ or alias/ resource prefix
    if value.starts_with("arn:") {
        validate_service_arn(value, "kms", None)?;
        let parts: Vec<&str> = value.splitn(6, ':').collect();
        let resource = parts[5];
        if !resource.starts_with("key/") && !resource.starts_with("alias/") {
            return Err(format!(
                "KMS ARN resource '{}' must start with 'key/' or 'alias/'",
                resource
            ));
        }
        return Ok(());
    }
    // Accept alias format: alias/<name>
    if value.starts_with("alias/") {
        if value.len() <= "alias/".len() {
            return Err("missing alias name after 'alias/'".to_string());
        }
        return Ok(());
    }
    // Accept bare key ID (UUID format: 8-4-4-4-12 hex digits)
    if is_uuid(value) {
        return Ok(());
    }
    Err(
        "expected a key ARN, alias ARN, alias name (alias/...), or key ID (UUID format)"
            .to_string(),
    )
}

/// IAM Policy Statement struct type
fn iam_policy_statement() -> AttributeType {
    // Union of String and List(String) for Action, Resource, etc.
    let string_or_list = AttributeType::Union(vec![
        AttributeType::String,
        AttributeType::List(Box::new(AttributeType::String)),
    ]);

    // Principal: Union of String (e.g., "*") and Map(Union(String, List(String)))
    let principal_type = AttributeType::Union(vec![
        AttributeType::String,
        AttributeType::Map(Box::new(string_or_list.clone())),
    ]);

    // Condition: Map(Map(Union(String, List(String))))
    let condition_type = AttributeType::Map(Box::new(AttributeType::Map(Box::new(
        string_or_list.clone(),
    ))));

    AttributeType::Struct {
        name: "IamPolicyStatement".to_string(),
        fields: vec![
            StructField::new("sid", AttributeType::String).with_provider_name("Sid"),
            StructField::new("effect", AttributeType::String)
                .required()
                .with_provider_name("Effect"),
            StructField::new("action", string_or_list.clone()).with_provider_name("Action"),
            StructField::new("not_action", string_or_list.clone()).with_provider_name("NotAction"),
            StructField::new("resource", string_or_list.clone()).with_provider_name("Resource"),
            StructField::new("not_resource", string_or_list.clone())
                .with_provider_name("NotResource"),
            StructField::new("principal", principal_type.clone()).with_provider_name("Principal"),
            StructField::new("not_principal", principal_type).with_provider_name("NotPrincipal"),
            StructField::new("condition", condition_type).with_provider_name("Condition"),
        ],
    }
}

/// IAM Policy Document type
/// Supports both block syntax and map syntax for policy documents.
pub(crate) fn iam_policy_document() -> AttributeType {
    AttributeType::Struct {
        name: "IamPolicyDocument".to_string(),
        fields: vec![
            StructField::new("version", AttributeType::String).with_provider_name("Version"),
            StructField::new("id", AttributeType::String).with_provider_name("Id"),
            StructField::new(
                "statement",
                AttributeType::List(Box::new(iam_policy_statement())),
            )
            .with_provider_name("Statement"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_arn_valid() {
        assert!(validate_arn("arn:aws:s3:::my-bucket").is_ok());
        assert!(validate_arn("arn:aws:iam::123456789012:role/MyRole").is_ok());
        assert!(validate_arn("arn:aws-cn:s3:::my-bucket").is_ok());
        assert!(validate_arn("arn:aws:ec2:us-east-1:123456789012:vpc/vpc-1234").is_ok());
    }

    #[test]
    fn validate_arn_invalid() {
        assert!(validate_arn("not-an-arn").is_err());
        assert!(validate_arn("arn:aws:s3").is_err());
        assert!(validate_arn("arn:aws").is_err());
        assert!(validate_arn("").is_err());
    }

    #[test]
    fn validate_arn_type_with_value() {
        let t = arn();
        assert!(
            t.validate(&Value::String("arn:aws:s3:::my-bucket".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("not-an-arn".to_string()))
                .is_err()
        );
        assert!(t.validate(&Value::Int(42)).is_err());
        // ResourceRef should be accepted
        assert!(
            t.validate(&Value::ResourceRef {
                binding_name: "role".to_string(),
                attribute_name: "arn".to_string(),
            })
            .is_ok()
        );
    }

    #[test]
    fn validate_aws_resource_id_valid() {
        assert!(validate_aws_resource_id("vpc-1a2b3c4d").is_ok());
        assert!(validate_aws_resource_id("subnet-0123456789abcdef0").is_ok());
        assert!(validate_aws_resource_id("sg-12345678").is_ok());
        assert!(validate_aws_resource_id("rtb-abcdef12").is_ok());
        assert!(validate_aws_resource_id("eipalloc-0123456789abcdef0").is_ok());
        assert!(validate_aws_resource_id("igw-12345678").is_ok());
    }

    #[test]
    fn validate_aws_resource_id_invalid() {
        assert!(validate_aws_resource_id("not-a-valid-id").is_err()); // hex part too short
        assert!(validate_aws_resource_id("vpc").is_err()); // no dash
        assert!(validate_aws_resource_id("vpc-short").is_err()); // hex part < 8
        assert!(validate_aws_resource_id("vpc-1234567").is_err()); // only 7 chars
        assert!(validate_aws_resource_id("VPC-12345678").is_err()); // uppercase prefix
    }

    #[test]
    fn validate_aws_resource_id_type_with_value() {
        let t = aws_resource_id();
        assert!(
            t.validate(&Value::String("vpc-1a2b3c4d".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("vpc".to_string())).is_err());
        assert!(t.validate(&Value::Int(42)).is_err());
        // ResourceRef should be accepted
        assert!(
            t.validate(&Value::ResourceRef {
                binding_name: "my_vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            })
            .is_ok()
        );
    }

    #[test]
    fn validate_availability_zone_valid() {
        assert!(validate_availability_zone("us-east-1a").is_ok());
        assert!(validate_availability_zone("ap-northeast-1c").is_ok());
        assert!(validate_availability_zone("eu-central-1b").is_ok());
        assert!(validate_availability_zone("me-south-1a").is_ok());
        assert!(validate_availability_zone("us-west-2d").is_ok());
    }

    #[test]
    fn validate_availability_zone_namespace_expanded() {
        let t = availability_zone();
        assert!(
            t.validate(&Value::String(
                "awscc.AvailabilityZone.ap_northeast_1a".to_string()
            ))
            .is_ok()
        );
        assert!(
            t.validate(&Value::String(
                "awscc.AvailabilityZone.us_east_1a".to_string()
            ))
            .is_ok()
        );
        assert!(
            t.validate(&Value::String(
                "awscc.AvailabilityZone.eu_central_1b".to_string()
            ))
            .is_ok()
        );
        assert!(
            t.validate(&Value::String("AvailabilityZone.us_west_2d".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_availability_zone_namespace_expanded_invalid() {
        let t = availability_zone();
        // No zone letter
        assert!(
            t.validate(&Value::String(
                "awscc.AvailabilityZone.us_east_1".to_string()
            ))
            .is_err()
        );
        // Wrong namespace prefix
        assert!(
            t.validate(&Value::String(
                "wrong.AvailabilityZone.us_east_1a".to_string()
            ))
            .is_err()
        );
    }

    #[test]
    fn validate_availability_zone_namespace_expanded_error_shows_original_input() {
        let t = availability_zone();
        // No zone letter - error should show original input, not normalized form
        let result = t.validate(&Value::String(
            "awscc.AvailabilityZone.us_east_1".to_string(),
        ));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("awscc.AvailabilityZone.us_east_1"),
            "Error should show original input, got: {}",
            err_msg
        );
        assert!(
            !err_msg.contains("'us-east-1'"),
            "Error should not show normalized form, got: {}",
            err_msg
        );
    }

    #[test]
    fn validate_availability_zone_underscored_error_shows_original_input() {
        let t = availability_zone();
        // Underscored form without namespace - error should show original, not normalized
        let result = t.validate(&Value::String("us_east_1".to_string()));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("us_east_1"),
            "Error should show original input, got: {}",
            err_msg
        );
        assert!(
            !err_msg.contains("'us-east-1'"),
            "Error should not show normalized form, got: {}",
            err_msg
        );
    }

    #[test]
    fn validate_availability_zone_invalid() {
        assert!(validate_availability_zone("us-east-1").is_err()); // no zone letter
        assert!(validate_availability_zone("US-EAST-1A").is_err()); // uppercase
        assert!(validate_availability_zone("us-east").is_err()); // no number
        assert!(validate_availability_zone("1a").is_err()); // too short
        assert!(validate_availability_zone("").is_err()); // empty
    }

    #[test]
    fn validate_availability_zone_type_with_value() {
        let t = availability_zone();
        assert!(t.validate(&Value::String("us-east-1a".to_string())).is_ok());
        assert!(
            t.validate(&Value::String(
                "awscc.AvailabilityZone.us_east_1a".to_string()
            ))
            .is_ok()
        );
        // Underscored form without namespace (consistent with other enum types
        // accepting underscore-to-hyphen conversion via find_matching_enum_value)
        assert!(
            t.validate(&Value::String("ap_northeast_1a".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("us-east-1".to_string())).is_err());
        assert!(t.validate(&Value::String("invalid".to_string())).is_err());
        assert!(t.validate(&Value::Int(42)).is_err());
    }

    #[test]
    fn validate_vpc_id_valid() {
        let t = vpc_id();
        assert!(
            t.validate(&Value::String("vpc-1a2b3c4d".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("vpc-0123456789abcdef0".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_vpc_id_invalid() {
        let t = vpc_id();
        assert!(
            t.validate(&Value::String("subnet-12345678".to_string()))
                .is_err()
        );
        assert!(t.validate(&Value::String("vpc-short".to_string())).is_err());
        assert!(t.validate(&Value::String("vpc".to_string())).is_err());
    }

    #[test]
    fn validate_subnet_id_valid() {
        let t = subnet_id();
        assert!(
            t.validate(&Value::String("subnet-0123456789abcdef0".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("subnet-12345678".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_subnet_id_invalid() {
        let t = subnet_id();
        assert!(
            t.validate(&Value::String("vpc-12345678".to_string()))
                .is_err()
        );
        assert!(
            t.validate(&Value::String("subnet-short".to_string()))
                .is_err()
        );
    }

    #[test]
    fn validate_security_group_id_valid() {
        let t = security_group_id();
        assert!(
            t.validate(&Value::String("sg-12345678".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("sg-0123456789abcdef0".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_security_group_id_invalid() {
        let t = security_group_id();
        assert!(
            t.validate(&Value::String("vpc-12345678".to_string()))
                .is_err()
        );
        assert!(t.validate(&Value::String("sg-short".to_string())).is_err());
    }

    #[test]
    fn validate_internet_gateway_id_valid() {
        let t = internet_gateway_id();
        assert!(
            t.validate(&Value::String("igw-12345678".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("igw-0123456789abcdef0".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_route_table_id_valid() {
        let t = route_table_id();
        assert!(
            t.validate(&Value::String("rtb-abcdef12".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("rtb-0123456789abcdef0".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_nat_gateway_id_valid() {
        let t = nat_gateway_id();
        assert!(
            t.validate(&Value::String("nat-0123456789abcdef0".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("nat-12345678".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_vpc_peering_connection_id_valid() {
        let t = vpc_peering_connection_id();
        assert!(
            t.validate(&Value::String("pcx-12345678".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("pcx-0123456789abcdef0".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_transit_gateway_id_valid() {
        let t = transit_gateway_id();
        assert!(
            t.validate(&Value::String("tgw-0123456789abcdef0".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("tgw-12345678".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_vpn_gateway_id_valid() {
        let t = vpn_gateway_id();
        assert!(
            t.validate(&Value::String("vgw-12345678".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("vgw-0123456789abcdef0".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_egress_only_internet_gateway_id_valid() {
        let t = egress_only_internet_gateway_id();
        assert!(
            t.validate(&Value::String("eigw-12345678".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("eigw-0123456789abcdef0".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_gateway_id_union() {
        let t = gateway_id();
        // InternetGatewayId (igw-*) should be accepted
        assert!(
            t.validate(&Value::String("igw-12345678".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("igw-0123456789abcdef0".to_string()))
                .is_ok()
        );
        // VpnGatewayId (vgw-*) should be accepted
        assert!(
            t.validate(&Value::String("vgw-12345678".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("vgw-0123456789abcdef0".to_string()))
                .is_ok()
        );
        // Other prefixes should be rejected
        assert!(
            t.validate(&Value::String("vpc-12345678".to_string()))
                .is_err()
        );
        assert!(
            t.validate(&Value::String("nat-12345678".to_string()))
                .is_err()
        );
        // ResourceRef should be accepted
        assert!(
            t.validate(&Value::ResourceRef {
                binding_name: "igw".to_string(),
                attribute_name: "internet_gateway_id".to_string(),
            })
            .is_ok()
        );
        // type_name should show both members
        assert_eq!(t.type_name(), "InternetGatewayId | VpnGatewayId");
    }

    #[test]
    fn validate_vpc_endpoint_id_valid() {
        let t = vpc_endpoint_id();
        assert!(
            t.validate(&Value::String("vpce-0123456789abcdef0".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("vpce-12345678".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn iam_policy_document_is_struct_type() {
        let t = iam_policy_document();
        match &t {
            AttributeType::Struct { name, fields } => {
                assert_eq!(name, "IamPolicyDocument");
                // Should have version, id, statement fields
                let field_names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                assert!(field_names.contains(&"version"));
                assert!(field_names.contains(&"id"));
                assert!(field_names.contains(&"statement"));
            }
            _ => panic!("Expected Struct type, got: {:?}", t),
        }
    }

    #[test]
    fn iam_policy_document_validates_map_syntax() {
        let t = iam_policy_document();
        // Map syntax (old style): assume_role_policy_document = { version = "...", statement = [...] }
        let doc = Value::Map(
            vec![
                (
                    "version".to_string(),
                    Value::String("2012-10-17".to_string()),
                ),
                (
                    "statement".to_string(),
                    Value::List(vec![Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            (
                                "principal".to_string(),
                                Value::Map(
                                    vec![(
                                        "service".to_string(),
                                        Value::String("ec2.amazonaws.com".to_string()),
                                    )]
                                    .into_iter()
                                    .collect(),
                                ),
                            ),
                            (
                                "action".to_string(),
                                Value::String("sts:AssumeRole".to_string()),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                    )]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        assert!(t.validate(&doc).is_ok());
    }

    #[test]
    fn iam_policy_document_validates_block_syntax() {
        let t = iam_policy_document();
        // Block syntax produces: List([Map({ version, statement: List([Map(...)]) })])
        let doc = Value::List(vec![Value::Map(
            vec![
                (
                    "version".to_string(),
                    Value::String("2012-10-17".to_string()),
                ),
                (
                    "statement".to_string(),
                    Value::List(vec![Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            (
                                "action".to_string(),
                                Value::String("sts:AssumeRole".to_string()),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                    )]),
                ),
            ]
            .into_iter()
            .collect(),
        )]);
        assert!(t.validate(&doc).is_ok());
    }

    #[test]
    fn iam_policy_document_type_with_resource_ref() {
        let t = iam_policy_document();
        // ResourceRef should be accepted (via Struct type handling in schema.rs)
        assert!(
            t.validate(&Value::ResourceRef {
                binding_name: "role".to_string(),
                attribute_name: "policy".to_string(),
            })
            .is_ok()
        );
    }

    #[test]
    fn validate_iam_role_arn_valid() {
        let t = iam_role_arn();
        assert!(
            t.validate(&Value::String(
                "arn:aws:iam::123456789012:role/MyRole".to_string()
            ))
            .is_ok()
        );
        assert!(
            t.validate(&Value::String(
                "arn:aws:iam::123456789012:role/path/to/MyRole".to_string()
            ))
            .is_ok()
        );
        // ResourceRef should be accepted
        assert!(
            t.validate(&Value::ResourceRef {
                binding_name: "role".to_string(),
                attribute_name: "arn".to_string(),
            })
            .is_ok()
        );
    }

    #[test]
    fn validate_iam_role_arn_invalid() {
        let t = iam_role_arn();
        // Wrong service
        assert!(
            t.validate(&Value::String("arn:aws:s3:::my-bucket".to_string()))
                .is_err()
        );
        // Wrong resource prefix
        assert!(
            t.validate(&Value::String(
                "arn:aws:iam::123456789012:policy/MyPolicy".to_string()
            ))
            .is_err()
        );
        // Not an ARN at all
        assert!(
            t.validate(&Value::String("not-an-arn".to_string()))
                .is_err()
        );
    }

    #[test]
    fn validate_iam_policy_arn_valid() {
        let t = iam_policy_arn();
        assert!(
            t.validate(&Value::String(
                "arn:aws:iam::123456789012:policy/MyPolicy".to_string()
            ))
            .is_ok()
        );
        assert!(
            t.validate(&Value::String(
                "arn:aws:iam::aws:policy/AdministratorAccess".to_string()
            ))
            .is_ok()
        );
        // ResourceRef should be accepted
        assert!(
            t.validate(&Value::ResourceRef {
                binding_name: "policy".to_string(),
                attribute_name: "arn".to_string(),
            })
            .is_ok()
        );
    }

    #[test]
    fn validate_iam_policy_arn_invalid() {
        let t = iam_policy_arn();
        // Wrong resource prefix
        assert!(
            t.validate(&Value::String(
                "arn:aws:iam::123456789012:role/MyRole".to_string()
            ))
            .is_err()
        );
        // Wrong service
        assert!(
            t.validate(&Value::String("arn:aws:s3:::my-bucket".to_string()))
                .is_err()
        );
    }

    #[test]
    fn validate_kms_key_arn_valid() {
        let t = kms_key_arn();
        assert!(
            t.validate(&Value::String(
                "arn:aws:kms:us-east-1:123456789012:key/1234abcd-12ab-34cd-56ef-1234567890ab"
                    .to_string()
            ))
            .is_ok()
        );
        // ResourceRef should be accepted
        assert!(
            t.validate(&Value::ResourceRef {
                binding_name: "key".to_string(),
                attribute_name: "arn".to_string(),
            })
            .is_ok()
        );
    }

    #[test]
    fn validate_kms_key_arn_invalid() {
        let t = kms_key_arn();
        // Wrong service
        assert!(
            t.validate(&Value::String(
                "arn:aws:iam::123456789012:role/MyRole".to_string()
            ))
            .is_err()
        );
        // Wrong resource prefix
        assert!(
            t.validate(&Value::String(
                "arn:aws:kms:us-east-1:123456789012:alias/my-key".to_string()
            ))
            .is_err()
        );
    }

    #[test]
    fn validate_kms_key_id_valid() {
        let t = kms_key_id();
        // Key ARN
        assert!(
            t.validate(&Value::String(
                "arn:aws:kms:us-east-1:123456789012:key/1234abcd-12ab-34cd-56ef-1234567890ab"
                    .to_string()
            ))
            .is_ok()
        );
        // Key alias ARN
        assert!(
            t.validate(&Value::String(
                "arn:aws:kms:us-east-1:123456789012:alias/my-key".to_string()
            ))
            .is_ok()
        );
        // Alias name
        assert!(
            t.validate(&Value::String("alias/my-key".to_string()))
                .is_ok()
        );
        // Bare key ID (UUID)
        assert!(
            t.validate(&Value::String(
                "1234abcd-12ab-34cd-56ef-1234567890ab".to_string()
            ))
            .is_ok()
        );
        // ResourceRef should be accepted
        assert!(
            t.validate(&Value::ResourceRef {
                binding_name: "key".to_string(),
                attribute_name: "arn".to_string(),
            })
            .is_ok()
        );
    }

    #[test]
    fn validate_kms_key_id_invalid() {
        let t = kms_key_id();
        // Wrong service ARN
        assert!(
            t.validate(&Value::String(
                "arn:aws:iam::123456789012:role/MyRole".to_string()
            ))
            .is_err()
        );
        // Not a valid format at all
        assert!(
            t.validate(&Value::String("not-a-valid-key".to_string()))
                .is_err()
        );
        // Empty alias name
        assert!(t.validate(&Value::String("alias/".to_string())).is_err());
        // KMS ARN with invalid resource prefix
        assert!(
            t.validate(&Value::String(
                "arn:aws:kms:us-east-1:123456789012:something/invalid".to_string()
            ))
            .is_err()
        );
    }

    #[test]
    fn validate_prefix_mismatch_error_messages() {
        let t = vpc_id();
        let result = t.validate(&Value::String("subnet-12345678".to_string()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("vpc-xxxxxxxx"));
        assert!(err_msg.contains("subnet-12345678"));
    }

    #[test]
    fn find_matching_enum_value_exact_match() {
        assert_eq!(
            find_matching_enum_value("IPv4", &["IPv4", "IPv6"]),
            Some("IPv4")
        );
    }

    #[test]
    fn find_matching_enum_value_case_insensitive() {
        assert_eq!(
            find_matching_enum_value("ipv4", &["IPv4", "IPv6"]),
            Some("IPv4")
        );
    }

    #[test]
    fn find_matching_enum_value_underscore_to_hyphen() {
        assert_eq!(
            find_matching_enum_value("cloud_watch_logs", &["cloud-watch-logs", "s3"]),
            Some("cloud-watch-logs")
        );
    }

    #[test]
    fn find_matching_enum_value_no_match() {
        assert_eq!(find_matching_enum_value("unknown", &["IPv4", "IPv6"]), None);
    }

    #[test]
    fn canonicalize_enum_value_exact_match() {
        assert_eq!(canonicalize_enum_value("IPv4", &["IPv4", "IPv6"]), "IPv4");
        assert_eq!(
            canonicalize_enum_value("advanced", &["free", "advanced"]),
            "advanced"
        );
    }

    #[test]
    fn canonicalize_enum_value_case_insensitive() {
        // AWS returns lowercase "ipv4" but schema expects "IPv4"
        assert_eq!(canonicalize_enum_value("ipv4", &["IPv4", "IPv6"]), "IPv4");
        assert_eq!(canonicalize_enum_value("ipv6", &["IPv4", "IPv6"]), "IPv6");
        // All-caps should also match
        assert_eq!(canonicalize_enum_value("IPV4", &["IPv4", "IPv6"]), "IPv4");
    }

    #[test]
    fn canonicalize_enum_value_no_match() {
        // Unknown value returned as-is
        assert_eq!(
            canonicalize_enum_value("unknown", &["IPv4", "IPv6"]),
            "unknown"
        );
    }

    #[test]
    fn canonicalize_enum_value_underscore_to_hyphen() {
        assert_eq!(
            canonicalize_enum_value("cloud_watch_logs", &["cloud-watch-logs", "s3"]),
            "cloud-watch-logs"
        );
    }

    #[test]
    fn auto_generated_get_enum_valid_values_known() {
        use crate::schemas::generated::get_enum_valid_values;
        assert_eq!(
            get_enum_valid_values("ec2.ipam", "tier"),
            Some(["free", "advanced"].as_slice())
        );
        assert_eq!(
            get_enum_valid_values("ec2.ipam_pool", "address_family"),
            Some(["IPv4", "IPv6"].as_slice())
        );
        assert_eq!(
            get_enum_valid_values("ec2.vpc", "instance_tenancy"),
            Some(["default", "dedicated", "host"].as_slice())
        );
    }

    #[test]
    fn auto_generated_get_enum_valid_values_transit_gateway() {
        use crate::schemas::generated::get_enum_valid_values;
        assert_eq!(
            get_enum_valid_values("ec2.transit_gateway", "auto_accept_shared_attachments"),
            Some(["enable", "disable"].as_slice())
        );
        assert_eq!(
            get_enum_valid_values("ec2.transit_gateway", "dns_support"),
            Some(["enable", "disable"].as_slice())
        );
        assert_eq!(
            get_enum_valid_values("ec2.transit_gateway", "vpn_ecmp_support"),
            Some(["enable", "disable"].as_slice())
        );
    }

    #[test]
    fn auto_generated_get_enum_valid_values_unknown() {
        use crate::schemas::generated::get_enum_valid_values;
        assert_eq!(get_enum_valid_values("ec2.vpc", "cidr_block"), None);
        assert_eq!(get_enum_valid_values("unknown", "unknown"), None);
    }

    #[test]
    fn validate_namespaced_enum_plain_value() {
        let result = validate_namespaced_enum(
            &Value::String("default".to_string()),
            "InstanceTenancy",
            "awscc.ec2.vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_2part_namespaced() {
        let result = validate_namespaced_enum(
            &Value::String("InstanceTenancy.default".to_string()),
            "InstanceTenancy",
            "awscc.ec2.vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_full_namespaced() {
        let result = validate_namespaced_enum(
            &Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
            "InstanceTenancy",
            "awscc.ec2.vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_invalid_value() {
        let result = validate_namespaced_enum(
            &Value::String("invalid".to_string()),
            "InstanceTenancy",
            "awscc.ec2.vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expected one of:"));
    }

    #[test]
    fn validate_namespaced_enum_underscore_to_hyphen() {
        let result = validate_namespaced_enum(
            &Value::String("cloud_watch_logs".to_string()),
            "LogDestinationType",
            "awscc.ec2.flow_log",
            &["cloud-watch-logs", "s3", "kinesis-data-firehose"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_case_insensitive() {
        // "ipv4" should match "IPv4" case-insensitively
        let result = validate_namespaced_enum(
            &Value::String("ipv4".to_string()),
            "AddressFamily",
            "awscc.ec2.ipam_pool",
            &["IPv4", "IPv6"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_case_insensitive_with_namespace() {
        // Namespaced form with case-insensitive value
        let result = validate_namespaced_enum(
            &Value::String("awscc.ec2.ipam_pool.AddressFamily.ipv4".to_string()),
            "AddressFamily",
            "awscc.ec2.ipam_pool",
            &["IPv4", "IPv6"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_case_insensitive_underscore_to_hyphen() {
        // "Cloud_Watch_Logs" -> hyphenated "Cloud-Watch-Logs" matches "cloud-watch-logs" case-insensitively
        let result = validate_namespaced_enum(
            &Value::String("Cloud_Watch_Logs".to_string()),
            "LogDestinationType",
            "awscc.ec2.flow_log",
            &["cloud-watch-logs", "s3", "kinesis-data-firehose"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_invalid_namespace() {
        let result = validate_namespaced_enum(
            &Value::String("wrong.ec2.vpc.InstanceTenancy.default".to_string()),
            "InstanceTenancy",
            "awscc.ec2.vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_err());
    }

    #[test]
    fn validate_namespaced_enum_non_string() {
        let result = validate_namespaced_enum(
            &Value::Int(42),
            "InstanceTenancy",
            "awscc.ec2.vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Expected string");
    }

    #[test]
    fn validate_ip_protocol_alias_all() {
        // "all" should be accepted as a valid IpProtocol value (alias for "-1")
        let valid_values = &["tcp", "udp", "icmp", "icmpv6", "-1", "all"];
        let result = validate_namespaced_enum(
            &Value::String("all".to_string()),
            "IpProtocol",
            "awscc.ec2.security_group_egress",
            valid_values,
        );
        assert!(result.is_ok(), "all should be accepted: {:?}", result);
    }

    #[test]
    fn validate_ip_protocol_canonical_minus_one() {
        // "-1" should still be accepted
        let valid_values = &["tcp", "udp", "icmp", "icmpv6", "-1", "all"];
        let result = validate_namespaced_enum(
            &Value::String("-1".to_string()),
            "IpProtocol",
            "awscc.ec2.security_group_egress",
            valid_values,
        );
        assert!(result.is_ok(), "-1 should still be accepted: {:?}", result);
    }

    #[test]
    fn validate_ip_protocol_namespaced_all() {
        // Full namespaced form: awscc.ec2.security_group_egress.IpProtocol.all
        let valid_values = &["tcp", "udp", "icmp", "icmpv6", "-1", "all"];
        let result = validate_namespaced_enum(
            &Value::String("awscc.ec2.security_group_egress.IpProtocol.all".to_string()),
            "IpProtocol",
            "awscc.ec2.security_group_egress",
            valid_values,
        );
        assert!(
            result.is_ok(),
            "Namespaced all should be accepted: {:?}",
            result
        );
    }

    #[test]
    fn auto_generated_get_enum_alias_reverse() {
        use crate::schemas::generated::get_enum_alias_reverse;
        // "all" maps to "-1" for ip_protocol on security_group_egress
        assert_eq!(
            get_enum_alias_reverse("ec2.security_group_egress", "ip_protocol", "all"),
            Some("-1")
        );
        // "all" maps to "-1" for ip_protocol on security_group_ingress
        assert_eq!(
            get_enum_alias_reverse("ec2.security_group_ingress", "ip_protocol", "all"),
            Some("-1")
        );
        // "tcp" has no alias mapping
        assert_eq!(
            get_enum_alias_reverse("ec2.security_group_egress", "ip_protocol", "tcp"),
            None
        );
        // Unknown resource has no alias mapping
        assert_eq!(
            get_enum_alias_reverse("ec2.vpc", "instance_tenancy", "default"),
            None
        );
    }

    #[test]
    fn auto_generated_ip_protocol_valid_values_include_all() {
        use crate::schemas::generated::get_enum_valid_values;
        // VALID_IP_PROTOCOL should include "all" as an alias
        let values = get_enum_valid_values("ec2.security_group_egress", "ip_protocol").unwrap();
        assert!(
            values.contains(&"all"),
            "VALID_IP_PROTOCOL should include 'all', got: {:?}",
            values
        );
        assert!(
            values.contains(&"-1"),
            "VALID_IP_PROTOCOL should still include '-1', got: {:?}",
            values
        );
    }
}
