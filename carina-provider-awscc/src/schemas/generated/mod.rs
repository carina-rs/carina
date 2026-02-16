//! Auto-generated AWS Cloud Control resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-schemas.sh

use carina_core::schema::ResourceSchema;

// Re-export all types and validators from awscc_types so that
// generated schema files can use `super::` to access them.
pub use super::awscc_types::*;

pub mod bucket;
pub mod egress_only_internet_gateway;
pub mod eip;
pub mod flow_log;
pub mod internet_gateway;
pub mod ipam;
pub mod ipam_pool;
pub mod log_group;
pub mod nat_gateway;
pub mod role;
pub mod route;
pub mod route_table;
pub mod security_group;
pub mod security_group_egress;
pub mod security_group_ingress;
pub mod subnet;
pub mod subnet_route_table_association;
pub mod transit_gateway;
pub mod transit_gateway_attachment;
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
        transit_gateway_attachment::ec2_transit_gateway_attachment_config(),
        bucket::s3_bucket_config(),
        role::iam_role_config(),
        log_group::logs_log_group_config(),
    ]
}

/// Returns all generated schemas (for backward compatibility)
pub fn schemas() -> Vec<ResourceSchema> {
    configs().into_iter().map(|c| c.schema).collect()
}
