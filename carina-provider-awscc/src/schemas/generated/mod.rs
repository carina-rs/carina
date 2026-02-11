//! Auto-generated AWS Cloud Control resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-schemas.sh

use carina_core::resource::Value;
use carina_core::schema::{AttributeType, ResourceSchema};

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

/// Normalize a namespaced enum value to its base value.
/// Handles formats like:
/// - "value" -> "value"
/// - "TypeName.value" -> "value"
/// - "awscc.resource.TypeName.value" -> "value"
pub fn normalize_namespaced_enum(s: &str) -> String {
    if s.contains('.') {
        let parts: Vec<&str> = s.split('.').collect();
        parts.last().map(|s| s.to_string()).unwrap_or_default()
    } else {
        s.to_string()
    }
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
        // Validate namespace format if it contains dots
        if s.contains('.') {
            let parts: Vec<&str> = s.split('.').collect();
            match parts.len() {
                // 2-part: TypeName.value
                2 => {
                    if parts[0] != type_name {
                        return Err(format!(
                            "Invalid format '{}', expected {}.value",
                            s, type_name
                        ));
                    }
                }
                // 4-part: awscc.resource.TypeName.value
                4 => {
                    let expected_namespace: Vec<&str> = namespace.split('.').collect();
                    if expected_namespace.len() != 2
                        || parts[0] != expected_namespace[0]
                        || parts[1] != expected_namespace[1]
                        || parts[2] != type_name
                    {
                        return Err(format!(
                            "Invalid format '{}', expected {}.{}.value",
                            s, namespace, type_name
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "Invalid format '{}', expected one of: value, {}.value, or {}.{}.value",
                        s, type_name, namespace, type_name
                    ));
                }
            }
        }

        let normalized = normalize_namespaced_enum(s);
        // Accept both underscore (DSL identifier) and hyphen (AWS value) forms
        // e.g., "cloud_watch_logs" matches "cloud-watch-logs"
        let hyphenated = normalized.replace('_', "-");
        if valid_values.contains(&normalized.as_str())
            || valid_values.contains(&hyphenated.as_str())
        {
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

/// Availability Zone type (e.g., "us-east-1a", "ap-northeast-1c")
/// Validates format: region + single letter zone identifier
pub fn availability_zone() -> AttributeType {
    AttributeType::Custom {
        name: "AvailabilityZone".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_availability_zone(s)
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
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

pub mod egress_only_internet_gateway;
pub mod eip;
pub mod flow_log;
pub mod internet_gateway;
pub mod ipam;
pub mod ipam_pool;
pub mod nat_gateway;
pub mod route;
pub mod route_table;
pub mod security_group;
pub mod security_group_egress;
pub mod security_group_ingress;
pub mod subnet;
pub mod subnet_route_table_association;
pub mod transit_gateway;
pub mod vpc;
pub mod vpc_endpoint;
pub mod vpc_gateway_attachment;
pub mod vpc_peering_connection;
pub mod vpn_gateway;

/// Returns all generated schema configs
pub fn configs() -> Vec<AwsccSchemaConfig> {
    vec![
        vpc::ec2_vpc_config(),
        subnet::ec2_subnet_config(),
        internet_gateway::ec2_internet_gateway_config(),
        route_table::ec2_route_table_config(),
        route::ec2_route_config(),
        subnet_route_table_association::ec2_subnet_route_table_association_config(),
        eip::ec2_eip_config(),
        nat_gateway::ec2_nat_gateway_config(),
        security_group::ec2_security_group_config(),
        security_group_ingress::ec2_security_group_ingress_config(),
        security_group_egress::ec2_security_group_egress_config(),
        vpc_endpoint::ec2_vpc_endpoint_config(),
        vpc_gateway_attachment::ec2_vpc_gateway_attachment_config(),
        flow_log::ec2_flow_log_config(),
        ipam::ec2_ipam_config(),
        ipam_pool::ec2_ipam_pool_config(),
        vpn_gateway::ec2_vpn_gateway_config(),
        transit_gateway::ec2_transit_gateway_config(),
        vpc_peering_connection::ec2_vpc_peering_connection_config(),
        egress_only_internet_gateway::ec2_egress_only_internet_gateway_config(),
    ]
}

/// Returns all generated schemas (for backward compatibility)
pub fn schemas() -> Vec<ResourceSchema> {
    configs().into_iter().map(|c| c.schema).collect()
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
        assert!(t.validate(&Value::String("us-east-1".to_string())).is_err());
        assert!(t.validate(&Value::String("invalid".to_string())).is_err());
        assert!(t.validate(&Value::Int(42)).is_err());
    }
}
