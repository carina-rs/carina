//! Auto-generated AWS Cloud Control resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   aws-vault exec <profile> -- ./scripts/generate-awscc-schemas.sh

use carina_core::schema::{AttributeType, ResourceSchema};

/// Tags type for AWS resources (Terraform-style map)
pub fn tags_type() -> AttributeType {
    AttributeType::Map(Box::new(AttributeType::String))
}

pub mod eip;
pub mod internet_gateway;
pub mod nat_gateway;
pub mod route;
pub mod route_table;
pub mod route_table_association;
pub mod security_group;
pub mod subnet;
pub mod vpc;
pub mod vpc_endpoint;
pub mod vpc_gateway_attachment;

/// Returns all generated schemas
pub fn schemas() -> Vec<ResourceSchema> {
    vec![
        vpc::vpc_schema(),
        subnet::subnet_schema(),
        internet_gateway::internet_gateway_schema(),
        route_table::route_table_schema(),
        route::route_schema(),
        route_table_association::subnet_route_table_association_schema(),
        eip::eip_schema(),
        nat_gateway::nat_gateway_schema(),
        security_group::security_group_schema(),
        vpc_endpoint::vpc_endpoint_schema(),
        vpc_gateway_attachment::vpc_gateway_attachment_schema(),
    ]
}
