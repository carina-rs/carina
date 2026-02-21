//! Auto-generated AWS provider resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   ./carina-provider-aws/scripts/generate-schemas-smithy.sh

use carina_core::schema::ResourceSchema;

// Re-export all types and validators from types so that
// generated schema files can use `super::` to access them.
pub use super::types::*;

pub mod ec2_internet_gateway;
pub mod ec2_route;
pub mod ec2_route_table;
pub mod ec2_security_group;
pub mod ec2_security_group_egress;
pub mod ec2_security_group_ingress;
pub mod ec2_subnet;
pub mod ec2_vpc;
pub mod s3_bucket;

/// Returns all generated schema configs
pub fn configs() -> Vec<AwsSchemaConfig> {
    vec![
        ec2_internet_gateway::ec2_internet_gateway_config(),
        ec2_route::ec2_route_config(),
        ec2_route_table::ec2_route_table_config(),
        ec2_security_group::ec2_security_group_config(),
        ec2_security_group_egress::ec2_security_group_egress_config(),
        ec2_security_group_ingress::ec2_security_group_ingress_config(),
        ec2_subnet::ec2_subnet_config(),
        ec2_vpc::ec2_vpc_config(),
        s3_bucket::s3_bucket_config(),
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
        ec2_internet_gateway::enum_valid_values(),
        ec2_route::enum_valid_values(),
        ec2_route_table::enum_valid_values(),
        ec2_security_group::enum_valid_values(),
        ec2_security_group_egress::enum_valid_values(),
        ec2_security_group_ingress::enum_valid_values(),
        ec2_subnet::enum_valid_values(),
        ec2_vpc::enum_valid_values(),
        s3_bucket::enum_valid_values(),
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
    if resource_type == "ec2_internet_gateway" {
        return ec2_internet_gateway::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2_route" {
        return ec2_route::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2_route_table" {
        return ec2_route_table::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2_security_group" {
        return ec2_security_group::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2_security_group_egress" {
        return ec2_security_group_egress::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2_security_group_ingress" {
        return ec2_security_group_ingress::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2_subnet" {
        return ec2_subnet::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "ec2_vpc" {
        return ec2_vpc::enum_alias_reverse(attr_name, value);
    }
    if resource_type == "s3_bucket" {
        return s3_bucket::enum_alias_reverse(attr_name, value);
    }
    None
}
