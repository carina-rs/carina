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
