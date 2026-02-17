//! AWS Cloud Control type definitions and validators
//!
//! This module contains stable utility code for AWS-specific types,
//! validators, and enum normalization. These functions are NOT generated
//! from CloudFormation schemas — they are hand-written and imported by
//! the generated `mod.rs`.

use carina_core::resource::Value;
use carina_core::schema::{AttributeType, ResourceSchema};
use carina_core::utils::{extract_enum_value, validate_enum_namespace};

/// AWS Cloud Control schema configuration
///
/// Combines the generated ResourceSchema with AWS-specific metadata
/// that was previously in ResourceConfig.
pub struct AwsccSchemaConfig {
    /// AWS CloudFormation type name (e.g., "AWS::EC2::VPC")
    pub aws_type_name: &'static str,
    /// Resource type name used in DSL (e.g., "ec2_vpc")
    pub resource_type_name: &'static str,
    /// Whether this resource type uses tags
    pub has_tags: bool,
    /// The resource schema with attribute definitions
    pub schema: ResourceSchema,
}

/// Tags type for AWS resources (Terraform-style map)
pub fn tags_type() -> AttributeType {
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
pub fn canonicalize_enum_value(raw: &str, valid_values: &[&str]) -> String {
    find_matching_enum_value(raw, valid_values)
        .unwrap_or(raw)
        .to_string()
}

/// Validate a namespaced enum value.
/// Returns Ok(()) if valid, Err with message if invalid.
pub fn validate_namespaced_enum(
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
            Err(format!(
                "Invalid value '{}', expected one of: {}",
                s,
                valid_values.join(", ")
            ))
        }
    } else {
        Err("Expected string".to_string())
    }
}

/// IPAM Pool ID type (e.g., "ipam-pool-0123456789abcdef0")
/// Validates format: ipam-pool-{hex} where hex is 8+ hex digits
pub fn ipam_pool_id() -> AttributeType {
    AttributeType::Custom {
        name: "IpamPoolId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_ipam_pool_id(s)
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

pub fn validate_ipam_pool_id(id: &str) -> Result<(), String> {
    let Some(hex_part) = id.strip_prefix("ipam-pool-") else {
        return Err(format!(
            "Invalid IPAM Pool ID '{}': expected format 'ipam-pool-{{hex}}'",
            id
        ));
    };
    if hex_part.len() < 8 {
        return Err(format!(
            "Invalid IPAM Pool ID '{}': hex part must be at least 8 characters",
            id
        ));
    }
    if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid IPAM Pool ID '{}': hex part must contain only hex digits",
            id
        ));
    }
    Ok(())
}

/// ARN type (e.g., "arn:aws:s3:::my-bucket")
pub fn arn() -> AttributeType {
    AttributeType::Custom {
        name: "Arn".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_arn(s)
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
        return Err(format!("Invalid ARN '{}': must start with 'arn:'", arn));
    }
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() < 6 {
        return Err(format!(
            "Invalid ARN '{}': must have at least 6 colon-separated parts (arn:partition:service:region:account:resource)",
            arn
        ));
    }
    Ok(())
}

/// AWS resource ID type (e.g., "vpc-1a2b3c4d", "subnet-0123456789abcdef0")
/// Validates format: {prefix}-{hex} where hex is 8+ hex digits
pub fn aws_resource_id() -> AttributeType {
    AttributeType::Custom {
        name: "AwsResourceId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_aws_resource_id(s)
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

pub fn validate_aws_resource_id(id: &str) -> Result<(), String> {
    let Some(dash_pos) = id.find('-') else {
        return Err(format!(
            "Invalid resource ID '{}': expected format 'prefix-hexdigits'",
            id
        ));
    };

    let prefix = &id[..dash_pos];
    let hex_part = &id[dash_pos + 1..];

    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(format!(
            "Invalid resource ID '{}': prefix must be lowercase alphanumeric",
            id
        ));
    }

    if hex_part.len() < 8 {
        return Err(format!(
            "Invalid resource ID '{}': ID part must be at least 8 characters after prefix",
            id
        ));
    }

    if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid resource ID '{}': ID part must contain only hex digits",
            id
        ));
    }

    Ok(())
}

/// Validate a resource ID with a specific prefix
fn validate_prefixed_resource_id(id: &str, expected_prefix: &str) -> Result<(), String> {
    let expected_format = format!("{}-xxxxxxxx", expected_prefix);
    if !id.starts_with(&format!("{}-", expected_prefix)) {
        return Err(format!(
            "Invalid resource ID '{}': expected format '{}'",
            id, expected_format
        ));
    }
    // Reuse existing validation for the rest
    validate_aws_resource_id(id)
}

/// VPC ID type (e.g., "vpc-1a2b3c4d")
pub fn vpc_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpcId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "vpc")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Subnet ID type (e.g., "subnet-0123456789abcdef0")
pub fn subnet_id() -> AttributeType {
    AttributeType::Custom {
        name: "SubnetId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "subnet")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Security Group ID type (e.g., "sg-12345678")
pub fn security_group_id() -> AttributeType {
    AttributeType::Custom {
        name: "SecurityGroupId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "sg")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Internet Gateway ID type (e.g., "igw-12345678")
pub fn internet_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "InternetGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "igw")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Route Table ID type (e.g., "rtb-abcdef12")
pub fn route_table_id() -> AttributeType {
    AttributeType::Custom {
        name: "RouteTableId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "rtb")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// NAT Gateway ID type (e.g., "nat-0123456789abcdef0")
pub fn nat_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "NatGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "nat")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// VPC Peering Connection ID type (e.g., "pcx-12345678")
pub fn vpc_peering_connection_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpcPeeringConnectionId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "pcx")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Transit Gateway ID type (e.g., "tgw-0123456789abcdef0")
pub fn transit_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "TransitGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "tgw")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// VPN Gateway ID type (e.g., "vgw-12345678")
pub fn vpn_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpnGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "vgw")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Egress Only Internet Gateway ID type (e.g., "eigw-12345678")
pub fn egress_only_internet_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "EgressOnlyInternetGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "eigw")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// VPC Endpoint ID type (e.g., "vpce-0123456789abcdef0")
pub fn vpc_endpoint_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpcEndpointId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "vpce")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// Availability Zone type (e.g., "us-east-1a", "ap-northeast-1c")
/// Validates format: region + single letter zone identifier
pub fn availability_zone() -> AttributeType {
    AttributeType::Custom {
        name: "AvailabilityZone".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_enum_namespace(s, "AvailabilityZone", "awscc")?;
                let extracted = extract_enum_value(s);
                let normalized = extracted.replace('_', "-");
                validate_availability_zone(&normalized)
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: Some("awscc".to_string()),
        to_dsl: Some(|s: &str| s.replace('-', "_")),
    }
}

pub fn validate_availability_zone(az: &str) -> Result<(), String> {
    // Must end with a single lowercase letter (zone identifier)
    let zone_letter = az.chars().last();
    if !zone_letter.is_some_and(|c| c.is_ascii_lowercase()) {
        return Err(format!(
            "Invalid availability zone '{}': must end with a zone letter (a-z)",
            az
        ));
    }

    // Region part is everything except the last character
    let region = &az[..az.len() - 1];

    // Region must match pattern: lowercase-lowercase-digit
    // e.g., "us-east-1", "ap-northeast-1", "eu-west-2"
    let parts: Vec<&str> = region.split('-').collect();
    if parts.len() < 3 {
        return Err(format!(
            "Invalid availability zone '{}': expected format like 'us-east-1a'",
            az
        ));
    }

    // Last part of region must be a number
    let last = parts.last().unwrap();
    if last.parse::<u8>().is_err() {
        return Err(format!(
            "Invalid availability zone '{}': region must end with a number",
            az
        ));
    }

    // All other parts must be lowercase alphabetic
    for part in &parts[..parts.len() - 1] {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_lowercase()) {
            return Err(format!(
                "Invalid availability zone '{}': expected format like 'us-east-1a'",
                az
            ));
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
            "Expected {} ARN, got service '{}'",
            expected_service, parts[2]
        ));
    }
    if let Some(prefix) = resource_prefix
        && !parts[5].starts_with(prefix)
    {
        return Err(format!(
            "Expected resource starting with '{}', got '{}'",
            prefix, parts[5]
        ));
    }
    Ok(())
}

/// IAM Role ARN type (e.g., "arn:aws:iam::123456789012:role/MyRole")
pub fn iam_role_arn() -> AttributeType {
    AttributeType::Custom {
        name: "IamRoleArn".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_service_arn(s, "iam", Some("role/"))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// IAM Policy ARN type (e.g., "arn:aws:iam::123456789012:policy/MyPolicy")
pub fn iam_policy_arn() -> AttributeType {
    AttributeType::Custom {
        name: "IamPolicyArn".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_service_arn(s, "iam", Some("policy/"))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

/// KMS Key ARN type (e.g., "arn:aws:kms:us-east-1:123456789012:key/1234abcd-12ab-34cd-56ef-1234567890ab")
pub fn kms_key_arn() -> AttributeType {
    AttributeType::Custom {
        name: "KmsKeyArn".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_service_arn(s, "kms", Some("key/"))
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
pub fn kms_key_id() -> AttributeType {
    AttributeType::Custom {
        name: "KmsKeyId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_kms_key_id(s)
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
                "Invalid KMS ARN resource '{}': expected 'key/...' or 'alias/...'",
                resource
            ));
        }
        return Ok(());
    }
    // Accept alias format: alias/<name>
    if value.starts_with("alias/") {
        if value.len() <= "alias/".len() {
            return Err("Invalid KMS alias: missing alias name after 'alias/'".to_string());
        }
        return Ok(());
    }
    // Accept bare key ID (UUID format: 8-4-4-4-12 hex digits)
    if is_uuid(value) {
        return Ok(());
    }
    Err(format!(
        "Invalid KMS key identifier '{}': expected a key ARN, alias ARN, alias name (alias/...), or key ID (UUID format)",
        value
    ))
}

/// IAM Policy Document type
/// Validates the structure of IAM policy documents (trust policies, inline policies, etc.)
pub fn iam_policy_document() -> AttributeType {
    AttributeType::Custom {
        name: "IamPolicyDocument".to_string(),
        base: Box::new(AttributeType::Map(Box::new(AttributeType::String))),
        validate: |value| validate_iam_policy_document(value),
        namespace: None,
        to_dsl: None,
    }
}

pub fn validate_iam_policy_document(value: &Value) -> Result<(), String> {
    let Value::Map(map) = value else {
        return Err("Expected a map for IAM policy document".to_string());
    };

    // Check Version if present
    if let Some(Value::String(v)) = map.get("version")
        && v != "2012-10-17"
        && v != "2008-10-17"
    {
        return Err(format!(
            "Invalid policy document Version '{}', expected '2012-10-17' or '2008-10-17'",
            v
        ));
    }

    // Check Statement if present
    if let Some(statement) = map.get("statement") {
        let Value::List(statements) = statement else {
            return Err("Policy document 'statement' must be a list".to_string());
        };

        for (i, stmt) in statements.iter().enumerate() {
            let Value::Map(stmt_map) = stmt else {
                return Err(format!("Statement[{}] must be a map", i));
            };

            // Validate Effect if present
            if let Some(Value::String(e)) = stmt_map.get("effect")
                && e != "Allow"
                && e != "Deny"
            {
                return Err(format!(
                    "Statement[{}] has invalid Effect '{}', expected 'Allow' or 'Deny'",
                    i, e
                ));
            }

            // Validate Sid if present (must be a string)
            if let Some(sid) = stmt_map.get("sid")
                && !matches!(sid, Value::String(_))
            {
                return Err(format!("Statement[{}] 'sid' must be a string", i));
            }

            // Validate Action / NotAction if present (must be string or list of strings)
            for key in &["action", "not_action"] {
                if let Some(action) = stmt_map.get(*key) {
                    match action {
                        Value::String(_) => {}
                        Value::List(items) => {
                            for (j, item) in items.iter().enumerate() {
                                if !matches!(item, Value::String(_)) {
                                    return Err(format!(
                                        "Statement[{}] '{}[{}]' must be a string",
                                        i, key, j
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(format!(
                                "Statement[{}] '{}' must be a string or list of strings",
                                i, key
                            ));
                        }
                    }
                }
            }

            // Validate Resource / NotResource if present (must be string or list of strings)
            for key in &["resource", "not_resource"] {
                if let Some(resource) = stmt_map.get(*key) {
                    match resource {
                        Value::String(_) => {}
                        Value::List(items) => {
                            for (j, item) in items.iter().enumerate() {
                                if !matches!(item, Value::String(_)) {
                                    return Err(format!(
                                        "Statement[{}] '{}[{}]' must be a string",
                                        i, key, j
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(format!(
                                "Statement[{}] '{}' must be a string or list of strings",
                                i, key
                            ));
                        }
                    }
                }
            }

            // Validate Principal / NotPrincipal if present (must be string "*" or a map)
            for key in &["principal", "not_principal"] {
                if let Some(principal) = stmt_map.get(*key) {
                    match principal {
                        Value::String(_) => {}
                        Value::Map(_) => {}
                        _ => {
                            return Err(format!(
                                "Statement[{}] '{}' must be a string or a map",
                                i, key
                            ));
                        }
                    }
                }
            }

            // Validate Condition if present (must be a map)
            if let Some(condition) = stmt_map.get("condition")
                && !matches!(condition, Value::Map(_))
            {
                return Err(format!("Statement[{}] 'condition' must be a map", i));
            }
        }
    }

    Ok(())
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
            t.validate(&Value::ResourceRef("role".to_string(), "arn".to_string()))
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
            t.validate(&Value::ResourceRef(
                "my_vpc".to_string(),
                "vpc_id".to_string()
            ))
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
    fn validate_iam_policy_document_valid() {
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
        assert!(validate_iam_policy_document(&doc).is_ok());
    }

    #[test]
    fn validate_iam_policy_document_invalid_version() {
        let doc = Value::Map(
            vec![(
                "version".to_string(),
                Value::String("2023-01-01".to_string()),
            )]
            .into_iter()
            .collect(),
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("2012-10-17"));
    }

    #[test]
    fn validate_iam_policy_document_invalid_effect() {
        let doc = Value::Map(
            vec![(
                "statement".to_string(),
                Value::List(vec![Value::Map(
                    vec![("effect".to_string(), Value::String("Maybe".to_string()))]
                        .into_iter()
                        .collect(),
                )]),
            )]
            .into_iter()
            .collect(),
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Maybe"));
    }

    #[test]
    fn validate_iam_policy_document_not_a_map() {
        let result = validate_iam_policy_document(&Value::String("not a map".to_string()));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Expected a map"));
    }

    #[test]
    fn validate_iam_policy_document_type_with_resource_ref() {
        let t = iam_policy_document();
        // ResourceRef should be accepted (via Custom type handling in schema.rs)
        assert!(
            t.validate(&Value::ResourceRef(
                "role".to_string(),
                "policy".to_string()
            ))
            .is_ok()
        );
    }

    #[test]
    fn validate_iam_policy_document_valid_with_all_fields() {
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
                            (
                                "sid".to_string(),
                                Value::String("AllowS3Access".to_string()),
                            ),
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
                                Value::List(vec![
                                    Value::String("s3:GetObject".to_string()),
                                    Value::String("s3:PutObject".to_string()),
                                ]),
                            ),
                            ("resource".to_string(), Value::String("*".to_string())),
                            (
                                "condition".to_string(),
                                Value::Map(
                                    vec![(
                                        "string_equals".to_string(),
                                        Value::Map(
                                            vec![(
                                                "aws:RequestedRegion".to_string(),
                                                Value::String("us-east-1".to_string()),
                                            )]
                                            .into_iter()
                                            .collect(),
                                        ),
                                    )]
                                    .into_iter()
                                    .collect(),
                                ),
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
        assert!(validate_iam_policy_document(&doc).is_ok());
    }

    #[test]
    fn validate_iam_policy_document_invalid_principal() {
        let doc = Value::Map(
            vec![(
                "statement".to_string(),
                Value::List(vec![Value::Map(
                    vec![
                        ("effect".to_string(), Value::String("Allow".to_string())),
                        (
                            "principal".to_string(),
                            Value::List(vec![Value::String("not-valid".to_string())]),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                )]),
            )]
            .into_iter()
            .collect(),
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("principal"));
    }

    #[test]
    fn validate_iam_policy_document_invalid_action() {
        let doc = Value::Map(
            vec![(
                "statement".to_string(),
                Value::List(vec![Value::Map(
                    vec![
                        ("effect".to_string(), Value::String("Allow".to_string())),
                        ("action".to_string(), Value::Int(42)),
                    ]
                    .into_iter()
                    .collect(),
                )]),
            )]
            .into_iter()
            .collect(),
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("action"));
    }

    #[test]
    fn validate_iam_policy_document_invalid_condition() {
        let doc = Value::Map(
            vec![(
                "statement".to_string(),
                Value::List(vec![Value::Map(
                    vec![
                        ("effect".to_string(), Value::String("Allow".to_string())),
                        (
                            "condition".to_string(),
                            Value::String("not-a-map".to_string()),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                )]),
            )]
            .into_iter()
            .collect(),
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("condition"));
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
            t.validate(&Value::ResourceRef("role".to_string(), "arn".to_string()))
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
            t.validate(&Value::ResourceRef("policy".to_string(), "arn".to_string()))
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
            t.validate(&Value::ResourceRef("key".to_string(), "arn".to_string()))
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
            t.validate(&Value::ResourceRef("key".to_string(), "arn".to_string()))
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
            get_enum_valid_values("ec2_ipam", "tier"),
            Some(["free", "advanced"].as_slice())
        );
        assert_eq!(
            get_enum_valid_values("ec2_ipam_pool", "address_family"),
            Some(["IPv4", "IPv6"].as_slice())
        );
        assert_eq!(
            get_enum_valid_values("ec2_vpc", "instance_tenancy"),
            Some(["default", "dedicated", "host"].as_slice())
        );
    }

    #[test]
    fn auto_generated_get_enum_valid_values_transit_gateway() {
        use crate::schemas::generated::get_enum_valid_values;
        assert_eq!(
            get_enum_valid_values("ec2_transit_gateway", "auto_accept_shared_attachments"),
            Some(["enable", "disable"].as_slice())
        );
        assert_eq!(
            get_enum_valid_values("ec2_transit_gateway", "dns_support"),
            Some(["enable", "disable"].as_slice())
        );
        assert_eq!(
            get_enum_valid_values("ec2_transit_gateway", "vpn_ecmp_support"),
            Some(["enable", "disable"].as_slice())
        );
    }

    #[test]
    fn auto_generated_get_enum_valid_values_unknown() {
        use crate::schemas::generated::get_enum_valid_values;
        assert_eq!(get_enum_valid_values("ec2_vpc", "cidr_block"), None);
        assert_eq!(get_enum_valid_values("unknown", "unknown"), None);
    }

    #[test]
    fn validate_namespaced_enum_plain_value() {
        let result = validate_namespaced_enum(
            &Value::String("default".to_string()),
            "InstanceTenancy",
            "awscc.ec2_vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_2part_namespaced() {
        let result = validate_namespaced_enum(
            &Value::String("InstanceTenancy.default".to_string()),
            "InstanceTenancy",
            "awscc.ec2_vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_full_namespaced() {
        let result = validate_namespaced_enum(
            &Value::String("awscc.ec2_vpc.InstanceTenancy.default".to_string()),
            "InstanceTenancy",
            "awscc.ec2_vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_invalid_value() {
        let result = validate_namespaced_enum(
            &Value::String("invalid".to_string()),
            "InstanceTenancy",
            "awscc.ec2_vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid value"));
    }

    #[test]
    fn validate_namespaced_enum_underscore_to_hyphen() {
        let result = validate_namespaced_enum(
            &Value::String("cloud_watch_logs".to_string()),
            "LogDestinationType",
            "awscc.ec2_flow_log",
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
            "awscc.ec2_ipam_pool",
            &["IPv4", "IPv6"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_case_insensitive_with_namespace() {
        // Namespaced form with case-insensitive value
        let result = validate_namespaced_enum(
            &Value::String("awscc.ec2_ipam_pool.AddressFamily.ipv4".to_string()),
            "AddressFamily",
            "awscc.ec2_ipam_pool",
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
            "awscc.ec2_flow_log",
            &["cloud-watch-logs", "s3", "kinesis-data-firehose"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_namespaced_enum_invalid_namespace() {
        let result = validate_namespaced_enum(
            &Value::String("wrong.ec2_vpc.InstanceTenancy.default".to_string()),
            "InstanceTenancy",
            "awscc.ec2_vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_err());
    }

    #[test]
    fn validate_namespaced_enum_non_string() {
        let result = validate_namespaced_enum(
            &Value::Int(42),
            "InstanceTenancy",
            "awscc.ec2_vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Expected string");
    }
}
