//! Auto-generated AWS Cloud Control resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-schemas.sh

use std::collections::HashMap;
use std::sync::LazyLock;

// Re-export all types and validators from awscc_types so that
// generated schema files can use `super::` to access them.
pub use super::awscc_types::*;

pub mod ec2;
pub mod iam;
pub mod logs;
pub mod s3;

/// Cached schema configs, initialized once on first access.
static SCHEMA_CONFIGS: LazyLock<Vec<AwsccSchemaConfig>> = LazyLock::new(build_configs);

/// Index from resource_type_name (e.g., "ec2.vpc") to position in SCHEMA_CONFIGS.
static SCHEMA_CONFIG_INDEX: LazyLock<HashMap<&'static str, usize>> = LazyLock::new(|| {
    SCHEMA_CONFIGS
        .iter()
        .enumerate()
        .map(|(i, c)| (c.resource_type_name, i))
        .collect()
});

/// Cached enum valid values: resource_type -> (attr_name -> valid values slice).
static ENUM_VALID_VALUES: LazyLock<
    HashMap<&'static str, HashMap<&'static str, &'static [&'static str]>>,
> = LazyLock::new(|| {
    #[allow(clippy::type_complexity)]
    let modules: &[(&str, &[(&str, &[&str])])] = &[
        ec2::vpc::enum_valid_values(),
        ec2::subnet::enum_valid_values(),
        ec2::internet_gateway::enum_valid_values(),
        ec2::route_table::enum_valid_values(),
        ec2::route::enum_valid_values(),
        ec2::subnet_route_table_association::enum_valid_values(),
        ec2::eip::enum_valid_values(),
        ec2::nat_gateway::enum_valid_values(),
        ec2::security_group::enum_valid_values(),
        ec2::security_group_ingress::enum_valid_values(),
        ec2::security_group_egress::enum_valid_values(),
        ec2::vpc_endpoint::enum_valid_values(),
        ec2::vpc_gateway_attachment::enum_valid_values(),
        ec2::flow_log::enum_valid_values(),
        ec2::ipam::enum_valid_values(),
        ec2::ipam_pool::enum_valid_values(),
        ec2::vpn_gateway::enum_valid_values(),
        ec2::transit_gateway::enum_valid_values(),
        ec2::vpc_peering_connection::enum_valid_values(),
        ec2::egress_only_internet_gateway::enum_valid_values(),
        ec2::transit_gateway_attachment::enum_valid_values(),
        s3::bucket::enum_valid_values(),
        iam::role::enum_valid_values(),
        logs::log_group::enum_valid_values(),
    ];
    let mut map: HashMap<&str, HashMap<&str, &[&str]>> = HashMap::new();
    for (rt, attrs) in modules {
        let attr_map = map.entry(rt).or_default();
        for (attr, values) in *attrs {
            attr_map.insert(attr, values);
        }
    }
    map
});

/// Function signature for enum alias reverse lookups.
type EnumAliasReverseFn = fn(&str, &str) -> Option<&'static str>;

/// Enum alias reverse dispatch table: resource_type -> dispatch function.
static ENUM_ALIAS_DISPATCH: LazyLock<HashMap<&'static str, EnumAliasReverseFn>> =
    LazyLock::new(|| {
        let entries: Vec<(&str, EnumAliasReverseFn)> = vec![
            ("ec2.vpc", ec2::vpc::enum_alias_reverse),
            ("ec2.subnet", ec2::subnet::enum_alias_reverse),
            (
                "ec2.internet_gateway",
                ec2::internet_gateway::enum_alias_reverse,
            ),
            ("ec2.route_table", ec2::route_table::enum_alias_reverse),
            ("ec2.route", ec2::route::enum_alias_reverse),
            (
                "ec2.subnet_route_table_association",
                ec2::subnet_route_table_association::enum_alias_reverse,
            ),
            ("ec2.eip", ec2::eip::enum_alias_reverse),
            ("ec2.nat_gateway", ec2::nat_gateway::enum_alias_reverse),
            (
                "ec2.security_group",
                ec2::security_group::enum_alias_reverse,
            ),
            (
                "ec2.security_group_ingress",
                ec2::security_group_ingress::enum_alias_reverse,
            ),
            (
                "ec2.security_group_egress",
                ec2::security_group_egress::enum_alias_reverse,
            ),
            ("ec2.vpc_endpoint", ec2::vpc_endpoint::enum_alias_reverse),
            (
                "ec2.vpc_gateway_attachment",
                ec2::vpc_gateway_attachment::enum_alias_reverse,
            ),
            ("ec2.flow_log", ec2::flow_log::enum_alias_reverse),
            ("ec2.ipam", ec2::ipam::enum_alias_reverse),
            ("ec2.ipam_pool", ec2::ipam_pool::enum_alias_reverse),
            ("ec2.vpn_gateway", ec2::vpn_gateway::enum_alias_reverse),
            (
                "ec2.transit_gateway",
                ec2::transit_gateway::enum_alias_reverse,
            ),
            (
                "ec2.vpc_peering_connection",
                ec2::vpc_peering_connection::enum_alias_reverse,
            ),
            (
                "ec2.egress_only_internet_gateway",
                ec2::egress_only_internet_gateway::enum_alias_reverse,
            ),
            (
                "ec2.transit_gateway_attachment",
                ec2::transit_gateway_attachment::enum_alias_reverse,
            ),
            ("s3.bucket", s3::bucket::enum_alias_reverse),
            ("iam.role", iam::role::enum_alias_reverse),
            ("logs.log_group", logs::log_group::enum_alias_reverse),
        ];
        entries.into_iter().collect()
    });

/// Build all schema configs (called once by LazyLock).
fn build_configs() -> Vec<AwsccSchemaConfig> {
    vec![
        ec2::vpc::ec2_vpc_config(),
        ec2::subnet::ec2_subnet_config(),
        ec2::internet_gateway::ec2_internet_gateway_config(),
        ec2::route_table::ec2_route_table_config(),
        ec2::route::ec2_route_config(),
        ec2::subnet_route_table_association::ec2_subnet_route_table_association_config(),
        ec2::eip::ec2_eip_config(),
        ec2::nat_gateway::ec2_nat_gateway_config(),
        ec2::security_group::ec2_security_group_config(),
        ec2::security_group_ingress::ec2_security_group_ingress_config(),
        ec2::security_group_egress::ec2_security_group_egress_config(),
        ec2::vpc_endpoint::ec2_vpc_endpoint_config(),
        ec2::vpc_gateway_attachment::ec2_vpc_gateway_attachment_config(),
        ec2::flow_log::ec2_flow_log_config(),
        ec2::ipam::ec2_ipam_config(),
        ec2::ipam_pool::ec2_ipam_pool_config(),
        ec2::vpn_gateway::ec2_vpn_gateway_config(),
        ec2::transit_gateway::ec2_transit_gateway_config(),
        ec2::vpc_peering_connection::ec2_vpc_peering_connection_config(),
        ec2::egress_only_internet_gateway::ec2_egress_only_internet_gateway_config(),
        ec2::transit_gateway_attachment::ec2_transit_gateway_attachment_config(),
        s3::bucket::s3_bucket_config(),
        iam::role::iam_role_config(),
        logs::log_group::logs_log_group_config(),
    ]
}

/// Returns a reference to the cached schema configs slice.
pub fn configs() -> &'static [AwsccSchemaConfig] {
    &SCHEMA_CONFIGS
}

/// Look up a schema config by resource_type_name (e.g., "ec2.vpc"). O(1).
pub fn get_config_by_type(resource_type: &str) -> Option<&'static AwsccSchemaConfig> {
    SCHEMA_CONFIG_INDEX
        .get(resource_type)
        .map(|&i| &SCHEMA_CONFIGS[i])
}

/// Get valid enum values for a given resource type and attribute name. O(1).
/// Used during read-back to normalize AWS-returned values to canonical DSL form.
///
/// Auto-generated from schema enum constants.
pub fn get_enum_valid_values(
    resource_type: &str,
    attr_name: &str,
) -> Option<&'static [&'static str]> {
    ENUM_VALID_VALUES
        .get(resource_type)
        .and_then(|attrs| attrs.get(attr_name))
        .copied()
}

/// Maps DSL alias values back to canonical AWS values. O(1) dispatch.
/// Dispatches to per-module enum_alias_reverse() functions.
pub fn get_enum_alias_reverse(
    resource_type: &str,
    attr_name: &str,
    value: &str,
) -> Option<&'static str> {
    ENUM_ALIAS_DISPATCH
        .get(resource_type)
        .and_then(|f| f(attr_name, value))
}
