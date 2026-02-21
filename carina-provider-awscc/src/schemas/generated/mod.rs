//! Auto-generated AWS Cloud Control resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-schemas.sh

use carina_core::schema::ResourceSchema;

// Re-export all types and validators from awscc_types so that
// generated schema files can use `super::` to access them.
pub use super::awscc_types::*;

pub mod ec2_egress_only_internet_gateway;
pub mod ec2_eip;
pub mod ec2_flow_log;
pub mod ec2_internet_gateway;
pub mod ec2_ipam;
pub mod ec2_ipam_pool;
pub mod ec2_nat_gateway;
pub mod ec2_route;
pub mod ec2_route_table;
pub mod ec2_security_group;
pub mod ec2_security_group_egress;
pub mod ec2_security_group_ingress;
pub mod ec2_subnet;
pub mod ec2_subnet_route_table_association;
pub mod ec2_transit_gateway;
pub mod ec2_transit_gateway_attachment;
pub mod ec2_vpc;
pub mod ec2_vpc_endpoint;
pub mod ec2_vpc_gateway_attachment;
pub mod ec2_vpc_peering_connection;
pub mod ec2_vpn_gateway;
pub mod iam_role;
pub mod logs_log_group;
pub mod s3_bucket;

/// Returns all generated schema configs
pub fn configs() -> Vec<AwsccSchemaConfig> {
    vec![
        ec2_vpc::ec2_vpc_config(),
        ec2_subnet::ec2_subnet_config(),
        ec2_internet_gateway::ec2_internet_gateway_config(),
        ec2_route_table::ec2_route_table_config(),
        ec2_route::ec2_route_config(),
        ec2_subnet_route_table_association::ec2_subnet_route_table_association_config(),
        ec2_eip::ec2_eip_config(),
        ec2_nat_gateway::ec2_nat_gateway_config(),
        ec2_security_group::ec2_security_group_config(),
        ec2_security_group_ingress::ec2_security_group_ingress_config(),
        ec2_security_group_egress::ec2_security_group_egress_config(),
        ec2_vpc_endpoint::ec2_vpc_endpoint_config(),
        ec2_vpc_gateway_attachment::ec2_vpc_gateway_attachment_config(),
        ec2_flow_log::ec2_flow_log_config(),
        ec2_ipam::ec2_ipam_config(),
        ec2_ipam_pool::ec2_ipam_pool_config(),
        ec2_vpn_gateway::ec2_vpn_gateway_config(),
        ec2_transit_gateway::ec2_transit_gateway_config(),
        ec2_vpc_peering_connection::ec2_vpc_peering_connection_config(),
        ec2_egress_only_internet_gateway::ec2_egress_only_internet_gateway_config(),
        ec2_transit_gateway_attachment::ec2_transit_gateway_attachment_config(),
        s3_bucket::s3_bucket_config(),
        iam_role::iam_role_config(),
        logs_log_group::logs_log_group_config(),
    ]
}

/// Returns all generated schemas (for backward compatibility)
pub fn schemas() -> Vec<ResourceSchema> {
    configs().into_iter().map(|c| c.schema).collect()
}

/// Get valid enum values for a given resource type and attribute name.
/// Used during read-back to normalize AWS-returned values to canonical DSL form.
///
/// Auto-generated from schema enum constants.
#[allow(clippy::type_complexity)]
pub fn get_enum_valid_values(
    resource_type: &str,
    attr_name: &str,
) -> Option<&'static [&'static str]> {
    let modules: &[(&str, &[(&str, &[&str])])] = &[
        ec2_vpc::enum_valid_values(),
        ec2_subnet::enum_valid_values(),
        ec2_internet_gateway::enum_valid_values(),
        ec2_route_table::enum_valid_values(),
        ec2_route::enum_valid_values(),
        ec2_subnet_route_table_association::enum_valid_values(),
        ec2_eip::enum_valid_values(),
        ec2_nat_gateway::enum_valid_values(),
        ec2_security_group::enum_valid_values(),
        ec2_security_group_ingress::enum_valid_values(),
        ec2_security_group_egress::enum_valid_values(),
        ec2_vpc_endpoint::enum_valid_values(),
        ec2_vpc_gateway_attachment::enum_valid_values(),
        ec2_flow_log::enum_valid_values(),
        ec2_ipam::enum_valid_values(),
        ec2_ipam_pool::enum_valid_values(),
        ec2_vpn_gateway::enum_valid_values(),
        ec2_transit_gateway::enum_valid_values(),
        ec2_vpc_peering_connection::enum_valid_values(),
        ec2_egress_only_internet_gateway::enum_valid_values(),
        ec2_transit_gateway_attachment::enum_valid_values(),
        s3_bucket::enum_valid_values(),
        iam_role::enum_valid_values(),
        logs_log_group::enum_valid_values(),
    ];
    for (rt, attrs) in modules {
        if *rt == resource_type {
            for (attr, values) in *attrs {
                if *attr == attr_name {
                    return Some(values);
                }
            }
            return None;
        }
    }
    None
}

/// Maps DSL alias values back to canonical AWS values.
/// Dispatches to per-module enum_alias_reverse() functions.
pub fn get_enum_alias_reverse(
    resource_type: &str,
    attr_name: &str,
    value: &str,
) -> Option<&'static str> {
    if resource_type == "ec2.vpc" {
        return ec2_vpc::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.subnet" {
        return ec2_subnet::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.internet_gateway" {
        return ec2_internet_gateway::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.route_table" {
        return ec2_route_table::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.route" {
        return ec2_route::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.subnet_route_table_association" {
        return ec2_subnet_route_table_association::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.eip" {
        return ec2_eip::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.nat_gateway" {
        return ec2_nat_gateway::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.security_group" {
        return ec2_security_group::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.security_group_ingress" {
        return ec2_security_group_ingress::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.security_group_egress" {
        return ec2_security_group_egress::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.vpc_endpoint" {
        return ec2_vpc_endpoint::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.vpc_gateway_attachment" {
        return ec2_vpc_gateway_attachment::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.flow_log" {
        return ec2_flow_log::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.ipam" {
        return ec2_ipam::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.ipam_pool" {
        return ec2_ipam_pool::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.vpn_gateway" {
        return ec2_vpn_gateway::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.transit_gateway" {
        return ec2_transit_gateway::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.vpc_peering_connection" {
        return ec2_vpc_peering_connection::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.egress_only_internet_gateway" {
        return ec2_egress_only_internet_gateway::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2.transit_gateway_attachment" {
        return ec2_transit_gateway_attachment::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "s3.bucket" {
        return s3_bucket::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "iam.role" {
        return iam_role::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "logs.log_group" {
        return logs_log_group::enum_alias_reverse(attr_name, value);
    }
    None
}
