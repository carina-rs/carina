//! VPC resource schema definitions

use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

/// Port number type (with validation)
pub fn port_number() -> AttributeType {
    AttributeType::Custom {
        name: "PortNumber".to_string(),
        base: Box::new(AttributeType::Int),
        validate: |value| {
            if let Value::Int(n) = value {
                if *n >= 0 && *n <= 65535 {
                    Ok(())
                } else {
                    Err("Port number must be between 0 and 65535".to_string())
                }
            } else {
                Err("Expected integer".to_string())
            }
        },
    }
}

/// Protocol type for security group rules
pub fn protocol() -> AttributeType {
    AttributeType::Enum(vec![
        "tcp".to_string(),
        "udp".to_string(),
        "icmp".to_string(),
        "all".to_string(), // All traffic (-1)
        // DSL format variants
        "Protocol.tcp".to_string(),
        "Protocol.udp".to_string(),
        "Protocol.icmp".to_string(),
        "Protocol.all".to_string(),
        "aws.Protocol.tcp".to_string(),
        "aws.Protocol.udp".to_string(),
        "aws.Protocol.icmp".to_string(),
        "aws.Protocol.all".to_string(),
    ])
}

/// Availability zone enum type
pub fn availability_zone() -> AttributeType {
    AttributeType::Enum(vec![
        // ap-northeast-1 (Tokyo)
        "ap_northeast_1a".to_string(),
        "ap_northeast_1c".to_string(),
        "ap_northeast_1d".to_string(),
        // ap-northeast-2 (Seoul)
        "ap_northeast_2a".to_string(),
        "ap_northeast_2b".to_string(),
        "ap_northeast_2c".to_string(),
        "ap_northeast_2d".to_string(),
        // ap-northeast-3 (Osaka)
        "ap_northeast_3a".to_string(),
        "ap_northeast_3b".to_string(),
        "ap_northeast_3c".to_string(),
        // ap-southeast-1 (Singapore)
        "ap_southeast_1a".to_string(),
        "ap_southeast_1b".to_string(),
        "ap_southeast_1c".to_string(),
        // ap-southeast-2 (Sydney)
        "ap_southeast_2a".to_string(),
        "ap_southeast_2b".to_string(),
        "ap_southeast_2c".to_string(),
        // ap-south-1 (Mumbai)
        "ap_south_1a".to_string(),
        "ap_south_1b".to_string(),
        "ap_south_1c".to_string(),
        // us-east-1 (N. Virginia)
        "us_east_1a".to_string(),
        "us_east_1b".to_string(),
        "us_east_1c".to_string(),
        "us_east_1d".to_string(),
        "us_east_1e".to_string(),
        "us_east_1f".to_string(),
        // us-east-2 (Ohio)
        "us_east_2a".to_string(),
        "us_east_2b".to_string(),
        "us_east_2c".to_string(),
        // us-west-1 (N. California)
        "us_west_1a".to_string(),
        "us_west_1b".to_string(),
        // us-west-2 (Oregon)
        "us_west_2a".to_string(),
        "us_west_2b".to_string(),
        "us_west_2c".to_string(),
        "us_west_2d".to_string(),
        // eu-west-1 (Ireland)
        "eu_west_1a".to_string(),
        "eu_west_1b".to_string(),
        "eu_west_1c".to_string(),
        // eu-west-2 (London)
        "eu_west_2a".to_string(),
        "eu_west_2b".to_string(),
        "eu_west_2c".to_string(),
        // eu-central-1 (Frankfurt)
        "eu_central_1a".to_string(),
        "eu_central_1b".to_string(),
        "eu_central_1c".to_string(),
    ])
}

/// Returns the schema for VPC
pub fn vpc_schema() -> ResourceSchema {
    ResourceSchema::new("vpc")
        .with_description("An AWS VPC (Virtual Private Cloud)")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("VPC ID (read-only, set after creation)"),
        )
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .required()
                .with_description("VPC name (Name tag)"),
        )
        .attribute(
            AttributeSchema::new("region", types::aws_region()).with_description(
                "The AWS region for the VPC (inherited from provider if not specified)",
            ),
        )
        .attribute(
            AttributeSchema::new("cidr_block", types::cidr())
                .required()
                .with_description("The IPv4 CIDR block for the VPC"),
        )
        .attribute(
            AttributeSchema::new("enable_dns_support", AttributeType::Bool)
                .with_description("Enable DNS resolution support"),
        )
        .attribute(
            AttributeSchema::new("enable_dns_hostnames", AttributeType::Bool)
                .with_description("Enable DNS hostnames"),
        )
}

/// Returns the schema for Subnet
pub fn subnet_schema() -> ResourceSchema {
    ResourceSchema::new("subnet")
        .with_description("An AWS VPC Subnet")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("Subnet ID (read-only, set after creation)"),
        )
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .required()
                .with_description("Subnet name (Name tag)"),
        )
        .attribute(
            AttributeSchema::new("region", types::aws_region()).with_description(
                "The AWS region for the subnet (inherited from provider if not specified)",
            ),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("VPC ID to create the subnet in"),
        )
        .attribute(
            AttributeSchema::new("cidr_block", types::cidr())
                .required()
                .with_description("The IPv4 CIDR block for the subnet"),
        )
        .attribute(
            AttributeSchema::new("availability_zone", availability_zone())
                .with_description("The availability zone for the subnet"),
        )
}

/// Returns the schema for Internet Gateway
pub fn internet_gateway_schema() -> ResourceSchema {
    ResourceSchema::new("internet_gateway")
        .with_description("An AWS Internet Gateway")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("Internet Gateway ID (read-only, set after creation)"),
        )
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .required()
                .with_description("Internet Gateway name (Name tag)"),
        )
        .attribute(
            AttributeSchema::new("region", types::aws_region())
                .with_description("The AWS region for the Internet Gateway (inherited from provider if not specified)"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .with_description("VPC ID to attach the Internet Gateway to"),
        )
}

/// Returns the schema for Route Table
pub fn route_table_schema() -> ResourceSchema {
    ResourceSchema::new("route_table")
        .with_description("An AWS VPC Route Table")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("Route Table ID (read-only, set after creation)"),
        )
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .required()
                .with_description("Route Table name (Name tag)"),
        )
        .attribute(
            AttributeSchema::new("region", types::aws_region()).with_description(
                "The AWS region for the Route Table (inherited from provider if not specified)",
            ),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("VPC ID for the Route Table"),
        )
}

/// Returns the schema for Route
pub fn route_schema() -> ResourceSchema {
    ResourceSchema::new("route")
        .with_description("A route in an AWS VPC Route Table")
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .required()
                .with_description("Route name (for identification)"),
        )
        .attribute(
            AttributeSchema::new("region", types::aws_region())
                .with_description("The AWS region (inherited from provider if not specified)"),
        )
        .attribute(
            AttributeSchema::new("route_table_id", AttributeType::String)
                .required()
                .with_description("Route Table ID"),
        )
        .attribute(
            AttributeSchema::new("destination_cidr_block", types::cidr())
                .required()
                .with_description("Destination CIDR block"),
        )
        .attribute(
            AttributeSchema::new("gateway_id", AttributeType::String)
                .with_description("Internet Gateway ID (for internet-bound traffic)"),
        )
        .attribute(
            AttributeSchema::new("nat_gateway_id", AttributeType::String)
                .with_description("NAT Gateway ID"),
        )
}

/// Returns the schema for Security Group
pub fn security_group_schema() -> ResourceSchema {
    ResourceSchema::new("security_group")
        .with_description("An AWS VPC Security Group")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("Security Group ID (read-only, set after creation)"),
        )
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .required()
                .with_description("Security Group name (Name tag)"),
        )
        .attribute(
            AttributeSchema::new("region", types::aws_region()).with_description(
                "The AWS region for the Security Group (inherited from provider if not specified)",
            ),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("VPC ID for the Security Group"),
        )
        .attribute(
            AttributeSchema::new("description", AttributeType::String)
                .with_description("Description of the Security Group"),
        )
}

/// Returns the schema for Security Group Ingress Rule
pub fn security_group_ingress_rule_schema() -> ResourceSchema {
    ResourceSchema::new("security_group.ingress_rule")
        .with_description("An inbound rule for an AWS VPC Security Group")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("Security Group Rule ID (read-only, set after creation)"),
        )
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .required()
                .with_description("Rule name (for identification)"),
        )
        .attribute(
            AttributeSchema::new("region", types::aws_region())
                .with_description("The AWS region (inherited from provider if not specified)"),
        )
        .attribute(
            AttributeSchema::new("security_group_id", AttributeType::String)
                .required()
                .with_description("Security Group ID"),
        )
        .attribute(
            AttributeSchema::new("protocol", protocol())
                .required()
                .with_description("Protocol (tcp, udp, icmp, or -1 for all)"),
        )
        .attribute(
            AttributeSchema::new("from_port", port_number())
                .required()
                .with_description("Start of port range"),
        )
        .attribute(
            AttributeSchema::new("to_port", port_number())
                .required()
                .with_description("End of port range"),
        )
        .attribute(
            AttributeSchema::new("cidr_blocks", AttributeType::List(Box::new(types::cidr())))
                .with_description("List of CIDR blocks to allow"),
        )
}

/// Returns the schema for Security Group Egress Rule
pub fn security_group_egress_rule_schema() -> ResourceSchema {
    ResourceSchema::new("security_group.egress_rule")
        .with_description("An outbound rule for an AWS VPC Security Group")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("Security Group Rule ID (read-only, set after creation)"),
        )
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .required()
                .with_description("Rule name (for identification)"),
        )
        .attribute(
            AttributeSchema::new("region", types::aws_region())
                .with_description("The AWS region (inherited from provider if not specified)"),
        )
        .attribute(
            AttributeSchema::new("security_group_id", AttributeType::String)
                .required()
                .with_description("Security Group ID"),
        )
        .attribute(
            AttributeSchema::new("protocol", protocol())
                .required()
                .with_description("Protocol (tcp, udp, icmp, or -1 for all)"),
        )
        .attribute(
            AttributeSchema::new("from_port", port_number())
                .required()
                .with_description("Start of port range"),
        )
        .attribute(
            AttributeSchema::new("to_port", port_number())
                .required()
                .with_description("End of port range"),
        )
        .attribute(
            AttributeSchema::new("cidr_blocks", AttributeType::List(Box::new(types::cidr())))
                .with_description("List of CIDR blocks to allow"),
        )
}

/// Returns all VPC-related schemas
pub fn schemas() -> Vec<ResourceSchema> {
    vec![
        vpc_schema(),
        subnet_schema(),
        internet_gateway_schema(),
        route_table_schema(),
        route_schema(),
        security_group_schema(),
        security_group_ingress_rule_schema(),
        security_group_egress_rule_schema(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn valid_cidr_block() {
        let t = types::cidr();
        assert!(
            t.validate(&Value::String("10.0.0.0/16".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("192.168.1.0/24".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("0.0.0.0/0".to_string())).is_ok());
    }

    #[test]
    fn invalid_cidr_block() {
        let t = types::cidr();
        assert!(t.validate(&Value::String("10.0.0.0".to_string())).is_err()); // missing prefix
        assert!(t.validate(&Value::String("10.0.0/16".to_string())).is_err()); // invalid IP
        assert!(
            t.validate(&Value::String("10.0.0.0/33".to_string()))
                .is_err()
        ); // prefix too large
    }

    #[test]
    fn valid_vpc() {
        let schema = vpc_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-vpc".to_string()));
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        attrs.insert("enable_dns_support".to_string(), Value::Bool(true));
        attrs.insert("enable_dns_hostnames".to_string(), Value::Bool(true));

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn vpc_missing_required_cidr_block() {
        let schema = vpc_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-vpc".to_string()));
        // missing cidr_block (region is optional, inherited from provider)

        let result = schema.validate(&attrs);
        assert!(result.is_err());
    }

    #[test]
    fn valid_subnet() {
        let schema = subnet_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-subnet".to_string()));
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );
        attrs.insert(
            "vpc_id".to_string(),
            Value::String("vpc-12345678".to_string()),
        );
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.1.0/24".to_string()),
        );
        attrs.insert(
            "availability_zone".to_string(),
            Value::String("aws.AvailabilityZone.ap_northeast_1a".to_string()),
        );

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn valid_internet_gateway() {
        let schema = internet_gateway_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-igw".to_string()));
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );
        attrs.insert(
            "vpc_id".to_string(),
            Value::String("vpc-12345678".to_string()),
        );

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn valid_route_table() {
        let schema = route_table_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-rt".to_string()));
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );
        attrs.insert(
            "vpc_id".to_string(),
            Value::String("vpc-12345678".to_string()),
        );

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn valid_security_group() {
        let schema = security_group_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-sg".to_string()));
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );
        attrs.insert(
            "vpc_id".to_string(),
            Value::String("vpc-12345678".to_string()),
        );
        attrs.insert(
            "description".to_string(),
            Value::String("My security group".to_string()),
        );

        assert!(schema.validate(&attrs).is_ok());
    }
}
