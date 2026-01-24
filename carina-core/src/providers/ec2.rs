//! EC2 resource schema definitions

use crate::resource::Value;
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

/// CIDR block type (with validation)
pub fn cidr_block() -> AttributeType {
    AttributeType::Custom {
        name: "CidrBlock".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                // Basic CIDR format validation: x.x.x.x/n
                let parts: Vec<&str> = s.split('/').collect();
                if parts.len() != 2 {
                    return Err("CIDR block must be in format x.x.x.x/n".to_string());
                }
                let ip_parts: Vec<&str> = parts[0].split('.').collect();
                if ip_parts.len() != 4 {
                    return Err("Invalid IP address in CIDR block".to_string());
                }
                for part in &ip_parts {
                    if part.parse::<u8>().is_err() {
                        return Err("Invalid IP address in CIDR block".to_string());
                    }
                }
                let prefix: u8 = parts[1]
                    .parse()
                    .map_err(|_| "Invalid prefix length in CIDR block".to_string())?;
                if prefix > 32 {
                    return Err("Prefix length must be between 0 and 32".to_string());
                }
                Ok(())
            } else {
                Err("Expected string".to_string())
            }
        },
    }
}

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
        "-1".to_string(), // All traffic
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
            AttributeSchema::new("region", types::aws_region())
                .required()
                .with_description("The AWS region for the VPC"),
        )
        .attribute(
            AttributeSchema::new("cidr_block", cidr_block())
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
            AttributeSchema::new("region", types::aws_region())
                .required()
                .with_description("The AWS region for the subnet"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("VPC ID to create the subnet in"),
        )
        .attribute(
            AttributeSchema::new("cidr_block", cidr_block())
                .required()
                .with_description("The IPv4 CIDR block for the subnet"),
        )
        .attribute(
            AttributeSchema::new("availability_zone", AttributeType::String)
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
                .required()
                .with_description("The AWS region for the Internet Gateway"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .with_description("VPC ID to attach the Internet Gateway to"),
        )
}

/// Route schema for route tables
fn route_schema() -> AttributeType {
    AttributeType::Map(Box::new(AttributeType::String))
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
            AttributeSchema::new("region", types::aws_region())
                .required()
                .with_description("The AWS region for the Route Table"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("VPC ID for the Route Table"),
        )
        .attribute(
            AttributeSchema::new("routes", AttributeType::List(Box::new(route_schema())))
                .with_description("List of routes"),
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
            AttributeSchema::new("region", types::aws_region())
                .required()
                .with_description("The AWS region for the Security Group"),
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
                .required()
                .with_description("The AWS region"),
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
            AttributeSchema::new("cidr", cidr_block()).with_description("CIDR block to allow"),
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
                .required()
                .with_description("The AWS region"),
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
            AttributeSchema::new("cidr", cidr_block()).with_description("CIDR block to allow"),
        )
}

/// Returns all EC2-related schemas
pub fn schemas() -> Vec<ResourceSchema> {
    vec![
        vpc_schema(),
        subnet_schema(),
        internet_gateway_schema(),
        route_table_schema(),
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
        let t = cidr_block();
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
        let t = cidr_block();
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
    fn vpc_missing_required() {
        let schema = vpc_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-vpc".to_string()));
        // missing region and cidr_block

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
            Value::String("ap-northeast-1a".to_string()),
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
