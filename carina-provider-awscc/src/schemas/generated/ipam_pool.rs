//! ipam_pool schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::IPAMPool
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

const VALID_ADDRESS_FAMILY: &[&str] = &["IPv4", "IPv6"];

fn validate_address_family(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "AddressFamily",
        "awscc.ec2_ipam_pool",
        VALID_ADDRESS_FAMILY,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid AddressFamily '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_AWS_SERVICE: &[&str] = &["ec2", "global-services"];

fn validate_aws_service(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "AwsService",
        "awscc.ec2_ipam_pool",
        VALID_AWS_SERVICE,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid AwsService '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_IPAM_SCOPE_TYPE: &[&str] = &["public", "private"];

fn validate_ipam_scope_type(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "IpamScopeType",
        "awscc.ec2_ipam_pool",
        VALID_IPAM_SCOPE_TYPE,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid IpamScopeType '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_PUBLIC_IP_SOURCE: &[&str] = &["byoip", "amazon"];

fn validate_public_ip_source(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "PublicIpSource",
        "awscc.ec2_ipam_pool",
        VALID_PUBLIC_IP_SOURCE,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid PublicIpSource '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_STATE: &[&str] = &[
    "create-in-progress",
    "create-complete",
    "modify-in-progress",
    "modify-complete",
    "delete-in-progress",
    "delete-complete",
];

fn validate_state(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(value, "State", "awscc.ec2_ipam_pool", VALID_STATE).map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid State '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

/// Returns the schema config for ec2_ipam_pool (AWS::EC2::IPAMPool)
pub fn ec2_ipam_pool_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::IPAMPool",
        resource_type_name: "ec2_ipam_pool",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_ipam_pool")
        .with_description("Resource Schema of AWS::EC2::IPAMPool Type")
        .attribute(
            AttributeSchema::new("address_family", AttributeType::Custom {
                name: "AddressFamily".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_address_family,
                namespace: Some("awscc.ec2_ipam_pool".to_string()),
                to_dsl: None,
            })
                .required()
                .create_only()
                .with_description("The address family of the address space in this pool. Either IPv4 or IPv6.")
                .with_provider_name("AddressFamily"),
        )
        .attribute(
            AttributeSchema::new("allocation_default_netmask_length", AttributeType::Int)
                .with_description("The default netmask length for allocations made from this pool. This value is used when the netmask length of an allocation isn't specified.")
                .with_provider_name("AllocationDefaultNetmaskLength"),
        )
        .attribute(
            AttributeSchema::new("allocation_max_netmask_length", AttributeType::Int)
                .with_description("The maximum allowed netmask length for allocations made from this pool.")
                .with_provider_name("AllocationMaxNetmaskLength"),
        )
        .attribute(
            AttributeSchema::new("allocation_min_netmask_length", AttributeType::Int)
                .with_description("The minimum allowed netmask length for allocations made from this pool.")
                .with_provider_name("AllocationMinNetmaskLength"),
        )
        .attribute(
            AttributeSchema::new("allocation_resource_tags", AttributeType::List(Box::new(tags_type())))
                .with_description("When specified, an allocation will not be allowed unless a resource has a matching set of tags.")
                .with_provider_name("AllocationResourceTags"),
        )
        .attribute(
            AttributeSchema::new("arn", super::arn())
                .with_description("The Amazon Resource Name (ARN) of the IPAM Pool. (read-only)")
                .with_provider_name("Arn"),
        )
        .attribute(
            AttributeSchema::new("auto_import", AttributeType::Bool)
                .with_description("Determines what to do if IPAM discovers resources that haven't been assigned an allocation. If set to true, an allocation will be made automatically.")
                .with_provider_name("AutoImport"),
        )
        .attribute(
            AttributeSchema::new("aws_service", AttributeType::Custom {
                name: "AwsService".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_aws_service,
                namespace: Some("awscc.ec2_ipam_pool".to_string()),
                to_dsl: None,
            })
                .create_only()
                .with_description("Limits which service in Amazon Web Services that the pool can be used in.")
                .with_provider_name("AwsService"),
        )
        .attribute(
            AttributeSchema::new("description", AttributeType::String)
                .with_provider_name("Description"),
        )
        .attribute(
            AttributeSchema::new("ipam_arn", super::arn())
                .with_description("The Amazon Resource Name (ARN) of the IPAM this pool is a part of. (read-only)")
                .with_provider_name("IpamArn"),
        )
        .attribute(
            AttributeSchema::new("ipam_pool_id", super::ipam_pool_id())
                .with_description("Id of the IPAM Pool. (read-only)")
                .with_provider_name("IpamPoolId"),
        )
        .attribute(
            AttributeSchema::new("ipam_scope_arn", super::arn())
                .with_description("The Amazon Resource Name (ARN) of the scope this pool is a part of. (read-only)")
                .with_provider_name("IpamScopeArn"),
        )
        .attribute(
            AttributeSchema::new("ipam_scope_id", AttributeType::String)
                .required()
                .create_only()
                .with_description("The Id of the scope this pool is a part of.")
                .with_provider_name("IpamScopeId"),
        )
        .attribute(
            AttributeSchema::new("ipam_scope_type", AttributeType::Custom {
                name: "IpamScopeType".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_ipam_scope_type,
                namespace: Some("awscc.ec2_ipam_pool".to_string()),
                to_dsl: None,
            })
                .with_description("Determines whether this scope contains publicly routable space or space for a private network (read-only)")
                .with_provider_name("IpamScopeType"),
        )
        .attribute(
            AttributeSchema::new("locale", AttributeType::String)
                .create_only()
                .with_description("The region of this pool. If not set, this will default to \"None\" which will disable non-custom allocations. If the locale has been specified for the...")
                .with_provider_name("Locale"),
        )
        .attribute(
            AttributeSchema::new("pool_depth", AttributeType::Int)
                .with_description("The depth of this pool in the source pool hierarchy. (read-only)")
                .with_provider_name("PoolDepth"),
        )
        .attribute(
            AttributeSchema::new("provisioned_cidrs", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "ProvisionedCidr".to_string(),
                    fields: vec![
                    StructField::new("cidr", AttributeType::String).required().with_provider_name("Cidr")
                    ],
                })))
                .with_description("A list of cidrs representing the address space available for allocation in this pool.")
                .with_provider_name("ProvisionedCidrs"),
        )
        .attribute(
            AttributeSchema::new("public_ip_source", AttributeType::Custom {
                name: "PublicIpSource".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_public_ip_source,
                namespace: Some("awscc.ec2_ipam_pool".to_string()),
                to_dsl: None,
            })
                .create_only()
                .with_description("The IP address source for pools in the public scope. Only used for provisioning IP address CIDRs to pools in the public scope. Default is `byoip`.")
                .with_provider_name("PublicIpSource"),
        )
        .attribute(
            AttributeSchema::new("publicly_advertisable", AttributeType::Bool)
                .create_only()
                .with_description("Determines whether or not address space from this pool is publicly advertised. Must be set if and only if the pool is IPv6.")
                .with_provider_name("PubliclyAdvertisable"),
        )
        .attribute(
            AttributeSchema::new("source_ipam_pool_id", super::ipam_pool_id())
                .create_only()
                .with_description("The Id of this pool's source. If set, all space provisioned in this pool must be free space provisioned in the parent pool.")
                .with_provider_name("SourceIpamPoolId"),
        )
        .attribute(
            AttributeSchema::new("source_resource", AttributeType::Struct {
                    name: "SourceResource".to_string(),
                    fields: vec![
                    StructField::new("resource_id", AttributeType::String).required().with_provider_name("ResourceId"),
                    StructField::new("resource_owner", AttributeType::String).required().with_provider_name("ResourceOwner"),
                    StructField::new("resource_region", AttributeType::String).required().with_provider_name("ResourceRegion"),
                    StructField::new("resource_type", AttributeType::String).required().with_provider_name("ResourceType")
                    ],
                })
                .create_only()
                .with_provider_name("SourceResource"),
        )
        .attribute(
            AttributeSchema::new("state", AttributeType::Custom {
                name: "State".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_state,
                namespace: Some("awscc.ec2_ipam_pool".to_string()),
                to_dsl: None,
            })
                .with_description("The state of this pool. This can be one of the following values: \"create-in-progress\", \"create-complete\", \"modify-in-progress\", \"modify-complet... (read-only)")
                .with_provider_name("State"),
        )
        .attribute(
            AttributeSchema::new("state_message", AttributeType::String)
                .with_description("An explanation of how the pool arrived at it current state. (read-only)")
                .with_provider_name("StateMessage"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("An array of key-value pairs to apply to this resource.")
                .with_provider_name("Tags"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "ec2_ipam_pool",
        &[
            ("address_family", VALID_ADDRESS_FAMILY),
            ("aws_service", VALID_AWS_SERVICE),
            ("ipam_scope_type", VALID_IPAM_SCOPE_TYPE),
            ("public_ip_source", VALID_PUBLIC_IP_SOURCE),
            ("state", VALID_STATE),
        ],
    )
}
