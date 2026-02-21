//! Resource definitions for Smithy-based codegen.
//!
//! Each `ResourceDef` describes how to map AWS API operations to a Carina resource schema.
//! These definitions are consumed by the `smithy-codegen` binary.

/// Defines how to map an AWS API resource to a Carina schema.
pub struct ResourceDef {
    /// Carina resource name (e.g., "ec2_vpc")
    pub name: &'static str,
    /// Smithy service namespace (e.g., "com.amazonaws.ec2")
    pub service_namespace: &'static str,
    /// Create operation short name (e.g., "CreateVpc")
    pub create_op: &'static str,
    /// Smithy structure name representing the read state (e.g., "Vpc")
    pub read_structure: &'static str,
    /// Delete operation short name (e.g., "DeleteVpc")
    pub delete_op: &'static str,
    /// Operations that modify existing resources
    pub update_ops: Vec<UpdateOp>,
    /// Primary identifier field name (e.g., "VpcId")
    pub identifier: &'static str,
    /// Whether this resource supports tags
    pub has_tags: bool,
    /// Type overrides: (field_name, type_code)
    pub type_overrides: Vec<(&'static str, &'static str)>,
    /// Fields to exclude from the schema
    pub exclude_fields: Vec<&'static str>,
    /// Fields to force as create-only even if they appear in update ops
    pub create_only_overrides: Vec<&'static str>,
    /// Enum aliases: (attr_snake_name, dsl_alias, canonical_value)
    pub enum_aliases: Vec<(&'static str, &'static str, &'static str)>,
    /// to_dsl overrides: (attr_snake_name, closure_code)
    pub to_dsl_overrides: Vec<(&'static str, &'static str)>,
    /// Required field overrides: fields that should be marked required
    /// even if not marked with smithy.api#required in the create input
    pub required_overrides: Vec<&'static str>,
    /// Extra read-only fields to include from the read structure
    /// that wouldn't normally be included (e.g., fields with different names)
    pub extra_read_only: Vec<&'static str>,
    /// Fields to force as read-only even if they appear in create input
    pub read_only_overrides: Vec<&'static str>,
}

/// An update operation and the fields it can modify.
pub struct UpdateOp {
    /// Operation short name (e.g., "ModifyVpcAttribute")
    pub operation: &'static str,
    /// Fields this operation can update
    pub fields: Vec<&'static str>,
}

/// Returns EC2 resource definitions.
pub fn ec2_resources() -> Vec<ResourceDef> {
    vec![
        // ec2_vpc
        ResourceDef {
            name: "ec2_vpc",
            service_namespace: "com.amazonaws.ec2",
            create_op: "CreateVpc",
            read_structure: "Vpc",
            delete_op: "DeleteVpc",
            update_ops: vec![UpdateOp {
                operation: "ModifyVpcAttribute",
                fields: vec!["EnableDnsHostnames", "EnableDnsSupport"],
            }],
            identifier: "VpcId",
            has_tags: true,
            type_overrides: vec![("CidrBlock", "types::ipv4_cidr()")],
            exclude_fields: vec![
                "DryRun",
                "TagSpecifications",
                "AmazonProvidedIpv6CidrBlock",
                "Ipv6Pool",
                "Ipv6CidrBlock",
                "Ipv6IpamPoolId",
                "Ipv6CidrBlockNetworkBorderGroup",
                "Ipv6NetmaskLength",
                "VpcEncryptionControl",
            ],
            create_only_overrides: vec![],
            enum_aliases: vec![],
            to_dsl_overrides: vec![],
            required_overrides: vec![],
            extra_read_only: vec![],
            read_only_overrides: vec![],
        },
        // ec2_subnet
        ResourceDef {
            name: "ec2_subnet",
            service_namespace: "com.amazonaws.ec2",
            create_op: "CreateSubnet",
            read_structure: "Subnet",
            delete_op: "DeleteSubnet",
            update_ops: vec![UpdateOp {
                operation: "ModifySubnetAttribute",
                fields: vec![
                    "AssignIpv6AddressOnCreation",
                    "MapPublicIpOnLaunch",
                    "EnableDns64",
                    "EnableLniAtDeviceIndex",
                    "PrivateDnsNameOptionsOnLaunch",
                ],
            }],
            identifier: "SubnetId",
            has_tags: true,
            type_overrides: vec![],
            exclude_fields: vec!["DryRun", "TagSpecifications"],
            create_only_overrides: vec![],
            enum_aliases: vec![],
            to_dsl_overrides: vec![],
            required_overrides: vec![],
            extra_read_only: vec![],
            read_only_overrides: vec![],
        },
        // ec2_internet_gateway
        ResourceDef {
            name: "ec2_internet_gateway",
            service_namespace: "com.amazonaws.ec2",
            create_op: "CreateInternetGateway",
            read_structure: "InternetGateway",
            delete_op: "DeleteInternetGateway",
            update_ops: vec![],
            identifier: "InternetGatewayId",
            has_tags: true,
            type_overrides: vec![],
            exclude_fields: vec!["DryRun", "TagSpecifications"],
            create_only_overrides: vec![],
            enum_aliases: vec![],
            to_dsl_overrides: vec![],
            required_overrides: vec![],
            extra_read_only: vec![],
            read_only_overrides: vec![],
        },
        // ec2_route_table
        ResourceDef {
            name: "ec2_route_table",
            service_namespace: "com.amazonaws.ec2",
            create_op: "CreateRouteTable",
            read_structure: "RouteTable",
            delete_op: "DeleteRouteTable",
            update_ops: vec![],
            identifier: "RouteTableId",
            has_tags: true,
            type_overrides: vec![],
            exclude_fields: vec!["DryRun", "TagSpecifications", "ClientToken"],
            create_only_overrides: vec![],
            enum_aliases: vec![],
            to_dsl_overrides: vec![],
            required_overrides: vec![],
            extra_read_only: vec![],
            read_only_overrides: vec![],
        },
        // ec2_route
        ResourceDef {
            name: "ec2_route",
            service_namespace: "com.amazonaws.ec2",
            create_op: "CreateRoute",
            read_structure: "Route",
            delete_op: "DeleteRoute",
            update_ops: vec![UpdateOp {
                operation: "ReplaceRoute",
                fields: vec![
                    "GatewayId",
                    "InstanceId",
                    "NatGatewayId",
                    "TransitGatewayId",
                    "LocalGatewayId",
                    "CarrierGatewayId",
                    "NetworkInterfaceId",
                    "VpcPeeringConnectionId",
                    "EgressOnlyInternetGatewayId",
                    "VpcEndpointId",
                    "CoreNetworkArn",
                ],
            }],
            identifier: "RouteTableId",
            has_tags: false,
            type_overrides: vec![],
            exclude_fields: vec!["DryRun", "OdbNetworkArn", "LocalTarget"],
            create_only_overrides: vec![],
            enum_aliases: vec![],
            to_dsl_overrides: vec![],
            required_overrides: vec![],
            extra_read_only: vec![],
            read_only_overrides: vec![],
        },
        // ec2_security_group
        ResourceDef {
            name: "ec2_security_group",
            service_namespace: "com.amazonaws.ec2",
            create_op: "CreateSecurityGroup",
            read_structure: "SecurityGroup",
            delete_op: "DeleteSecurityGroup",
            update_ops: vec![],
            identifier: "GroupId",
            has_tags: true,
            type_overrides: vec![],
            exclude_fields: vec!["DryRun", "TagSpecifications"],
            create_only_overrides: vec![],
            enum_aliases: vec![],
            to_dsl_overrides: vec![],
            required_overrides: vec![],
            extra_read_only: vec![],
            read_only_overrides: vec![],
        },
        // ec2_security_group_ingress
        ResourceDef {
            name: "ec2_security_group_ingress",
            service_namespace: "com.amazonaws.ec2",
            create_op: "AuthorizeSecurityGroupIngress",
            read_structure: "SecurityGroupRule",
            delete_op: "RevokeSecurityGroupIngress",
            update_ops: vec![],
            identifier: "SecurityGroupRuleId",
            has_tags: false,
            type_overrides: vec![],
            exclude_fields: vec![
                "DryRun",
                "TagSpecifications",
                "IpPermissions",
                "SecurityGroupRuleIds",
            ],
            create_only_overrides: vec![],
            enum_aliases: vec![("ip_protocol", "all", "-1")],
            to_dsl_overrides: vec![(
                "ip_protocol",
                r#"Some(|s: &str| match s { "-1" => "all".to_string(), _ => s.replace('-', "_") })"#,
            )],
            required_overrides: vec!["IpProtocol"],
            extra_read_only: vec![],
            read_only_overrides: vec![],
        },
        // ec2_security_group_egress
        ResourceDef {
            name: "ec2_security_group_egress",
            service_namespace: "com.amazonaws.ec2",
            create_op: "AuthorizeSecurityGroupEgress",
            read_structure: "SecurityGroupRule",
            delete_op: "RevokeSecurityGroupEgress",
            update_ops: vec![],
            identifier: "SecurityGroupRuleId",
            has_tags: false,
            type_overrides: vec![],
            exclude_fields: vec![
                "DryRun",
                "TagSpecifications",
                "IpPermissions",
                "SecurityGroupRuleIds",
            ],
            create_only_overrides: vec![],
            enum_aliases: vec![("ip_protocol", "all", "-1")],
            to_dsl_overrides: vec![(
                "ip_protocol",
                r#"Some(|s: &str| match s { "-1" => "all".to_string(), _ => s.replace('-', "_") })"#,
            )],
            required_overrides: vec!["IpProtocol", "GroupId"],
            extra_read_only: vec![],
            read_only_overrides: vec![],
        },
    ]
}
