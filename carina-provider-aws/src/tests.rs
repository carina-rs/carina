//! Tests for generated provider methods and integration patterns

use std::collections::HashMap;

use carina_core::resource::Value;

use crate::AwsProvider;

// --- extract_ec2_vpc_attributes tests ---

#[test]
fn test_extract_ec2_vpc_attributes() {
    let vpc = aws_sdk_ec2::types::Vpc::builder()
        .vpc_id("vpc-12345678")
        .cidr_block("10.0.0.0/16")
        .instance_tenancy(aws_sdk_ec2::types::Tenancy::Default)
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_vpc_attributes(&vpc, &mut attributes);
    assert_eq!(identifier, Some("vpc-12345678".to_string()));
    assert_eq!(
        attributes.get("vpc_id"),
        Some(&Value::String("vpc-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("cidr_block"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
    assert_eq!(
        attributes.get("instance_tenancy"),
        Some(&Value::String("default".to_string()))
    );
}

#[test]
fn test_extract_ec2_vpc_attributes_minimal() {
    let vpc = aws_sdk_ec2::types::Vpc::builder().build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_vpc_attributes(&vpc, &mut attributes);
    assert_eq!(identifier, None);
    assert!(attributes.is_empty());
}

// --- extract_ec2_subnet_attributes tests ---

#[test]
fn test_extract_ec2_subnet_attributes() {
    let subnet = aws_sdk_ec2::types::Subnet::builder()
        .subnet_id("subnet-12345678")
        .vpc_id("vpc-12345678")
        .cidr_block("10.0.1.0/24")
        .availability_zone("ap-northeast-1a")
        .map_public_ip_on_launch(false)
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_subnet_attributes(&subnet, &mut attributes);
    assert_eq!(identifier, Some("subnet-12345678".to_string()));
    assert_eq!(
        attributes.get("subnet_id"),
        Some(&Value::String("subnet-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("vpc_id"),
        Some(&Value::String("vpc-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("cidr_block"),
        Some(&Value::String("10.0.1.0/24".to_string()))
    );
    assert_eq!(
        attributes.get("availability_zone"),
        Some(&Value::String("ap-northeast-1a".to_string()))
    );
    assert_eq!(
        attributes.get("map_public_ip_on_launch"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn test_extract_ec2_subnet_attributes_minimal() {
    let subnet = aws_sdk_ec2::types::Subnet::builder().build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_subnet_attributes(&subnet, &mut attributes);
    assert_eq!(identifier, None);
}

#[test]
fn test_extract_ec2_subnet_attributes_with_private_dns_name_options() {
    use aws_sdk_ec2::types::{HostnameType, PrivateDnsNameOptionsOnLaunch};

    let dns_options = PrivateDnsNameOptionsOnLaunch::builder()
        .hostname_type(HostnameType::IpName)
        .enable_resource_name_dns_a_record(true)
        .enable_resource_name_dns_aaaa_record(false)
        .build();

    let subnet = aws_sdk_ec2::types::Subnet::builder()
        .subnet_id("subnet-12345678")
        .vpc_id("vpc-12345678")
        .cidr_block("10.0.1.0/24")
        .private_dns_name_options_on_launch(dns_options)
        .build();

    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_subnet_attributes(&subnet, &mut attributes);
    assert_eq!(identifier, Some("subnet-12345678".to_string()));

    // Verify the struct is extracted as a Value::Map
    let dns_value = attributes
        .get("private_dns_name_options_on_launch")
        .expect("private_dns_name_options_on_launch should be present");

    if let Value::Map(fields) = dns_value {
        assert_eq!(
            fields.get("hostname_type"),
            Some(&Value::String("ip-name".to_string()))
        );
        assert_eq!(
            fields.get("enable_resource_name_dns_a_record"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            fields.get("enable_resource_name_dns_aaaa_record"),
            Some(&Value::Bool(false))
        );
    } else {
        panic!(
            "Expected Value::Map for private_dns_name_options_on_launch, got {:?}",
            dns_value
        );
    }
}

// --- extract_ec2_internet_gateway_attributes tests ---

#[test]
fn test_extract_ec2_internet_gateway_attributes() {
    let igw = aws_sdk_ec2::types::InternetGateway::builder()
        .internet_gateway_id("igw-12345678")
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_internet_gateway_attributes(&igw, &mut attributes);
    assert_eq!(identifier, Some("igw-12345678".to_string()));
    assert_eq!(
        attributes.get("internet_gateway_id"),
        Some(&Value::String("igw-12345678".to_string()))
    );
}

#[test]
fn test_extract_ec2_internet_gateway_attributes_minimal() {
    let igw = aws_sdk_ec2::types::InternetGateway::builder().build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_internet_gateway_attributes(&igw, &mut attributes);
    assert_eq!(identifier, None);
    assert!(attributes.is_empty());
}

// --- extract_ec2_route_table_attributes tests ---

#[test]
fn test_extract_ec2_route_table_attributes() {
    let rt = aws_sdk_ec2::types::RouteTable::builder()
        .route_table_id("rtb-12345678")
        .vpc_id("vpc-12345678")
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_route_table_attributes(&rt, &mut attributes);
    assert_eq!(identifier, Some("rtb-12345678".to_string()));
    assert_eq!(
        attributes.get("route_table_id"),
        Some(&Value::String("rtb-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("vpc_id"),
        Some(&Value::String("vpc-12345678".to_string()))
    );
}

#[test]
fn test_extract_ec2_route_table_attributes_minimal() {
    let rt = aws_sdk_ec2::types::RouteTable::builder().build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_route_table_attributes(&rt, &mut attributes);
    assert_eq!(identifier, None);
}

// --- extract_ec2_route_attributes tests ---

#[test]
fn test_extract_ec2_route_attributes() {
    let route = aws_sdk_ec2::types::Route::builder()
        .destination_cidr_block("0.0.0.0/0")
        .gateway_id("igw-12345678")
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_route_attributes(&route, &mut attributes);
    // Route extraction returns None (no single identifier)
    assert_eq!(identifier, None);
    assert_eq!(
        attributes.get("destination_cidr_block"),
        Some(&Value::String("0.0.0.0/0".to_string()))
    );
    assert_eq!(
        attributes.get("gateway_id"),
        Some(&Value::String("igw-12345678".to_string()))
    );
}

#[test]
fn test_extract_ec2_route_attributes_with_nat_gateway() {
    let route = aws_sdk_ec2::types::Route::builder()
        .destination_cidr_block("10.0.0.0/8")
        .nat_gateway_id("nat-12345678")
        .build();
    let mut attributes = HashMap::new();
    AwsProvider::extract_ec2_route_attributes(&route, &mut attributes);
    assert_eq!(
        attributes.get("destination_cidr_block"),
        Some(&Value::String("10.0.0.0/8".to_string()))
    );
    assert_eq!(
        attributes.get("nat_gateway_id"),
        Some(&Value::String("nat-12345678".to_string()))
    );
}

#[test]
fn test_extract_ec2_route_attributes_ignores_unsupported() {
    // transit_gateway_id is not in the schema, so it should not be extracted
    let route = aws_sdk_ec2::types::Route::builder()
        .destination_cidr_block("172.16.0.0/12")
        .transit_gateway_id("tgw-12345678")
        .build();
    let mut attributes = HashMap::new();
    AwsProvider::extract_ec2_route_attributes(&route, &mut attributes);
    assert_eq!(
        attributes.get("destination_cidr_block"),
        Some(&Value::String("172.16.0.0/12".to_string()))
    );
    assert_eq!(attributes.get("transit_gateway_id"), None);
}

// --- extract_ec2_security_group_attributes tests ---

#[test]
fn test_extract_ec2_security_group_attributes() {
    let sg = aws_sdk_ec2::types::SecurityGroup::builder()
        .group_id("sg-12345678")
        .group_name("test-sg")
        .description("Test security group")
        .vpc_id("vpc-12345678")
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_security_group_attributes(&sg, &mut attributes);
    assert_eq!(identifier, Some("sg-12345678".to_string()));
    assert_eq!(
        attributes.get("group_id"),
        Some(&Value::String("sg-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("group_name"),
        Some(&Value::String("test-sg".to_string()))
    );
    assert_eq!(
        attributes.get("description"),
        Some(&Value::String("Test security group".to_string()))
    );
    assert_eq!(
        attributes.get("vpc_id"),
        Some(&Value::String("vpc-12345678".to_string()))
    );
}

#[test]
fn test_extract_ec2_security_group_attributes_minimal() {
    let sg = aws_sdk_ec2::types::SecurityGroup::builder().build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_security_group_attributes(&sg, &mut attributes);
    assert_eq!(identifier, None);
}

// --- extract_ec2_security_group_ingress_attributes tests ---

#[test]
fn test_extract_ec2_security_group_ingress_attributes() {
    let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
        .security_group_rule_id("sgr-12345678")
        .group_id("sg-12345678")
        .ip_protocol("tcp")
        .from_port(443)
        .to_port(443)
        .description("HTTPS")
        .build();
    let mut attributes = HashMap::new();
    let identifier =
        AwsProvider::extract_ec2_security_group_ingress_attributes(&rule, &mut attributes);
    assert_eq!(identifier, Some("sgr-12345678".to_string()));
    assert_eq!(
        attributes.get("security_group_rule_id"),
        Some(&Value::String("sgr-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("group_id"),
        Some(&Value::String("sg-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("ip_protocol"),
        Some(&Value::String("tcp".to_string()))
    );
    assert_eq!(attributes.get("from_port"), Some(&Value::Int(443)));
    assert_eq!(attributes.get("to_port"), Some(&Value::Int(443)));
    assert_eq!(
        attributes.get("description"),
        Some(&Value::String("HTTPS".to_string()))
    );
}

#[test]
fn test_extract_ec2_security_group_ingress_attributes_with_prefix_list() {
    let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
        .security_group_rule_id("sgr-99999999")
        .group_id("sg-12345678")
        .ip_protocol("tcp")
        .from_port(80)
        .to_port(80)
        .prefix_list_id("pl-12345678")
        .build();
    let mut attributes = HashMap::new();
    AwsProvider::extract_ec2_security_group_ingress_attributes(&rule, &mut attributes);
    assert_eq!(
        attributes.get("source_prefix_list_id"),
        Some(&Value::String("pl-12345678".to_string()))
    );
}

// --- extract_ec2_security_group_egress_attributes tests ---

#[test]
fn test_extract_ec2_security_group_egress_attributes() {
    let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
        .security_group_rule_id("sgr-87654321")
        .group_id("sg-12345678")
        .ip_protocol("-1")
        .from_port(0)
        .to_port(0)
        .build();
    let mut attributes = HashMap::new();
    let identifier =
        AwsProvider::extract_ec2_security_group_egress_attributes(&rule, &mut attributes);
    assert_eq!(identifier, Some("sgr-87654321".to_string()));
    assert_eq!(
        attributes.get("group_id"),
        Some(&Value::String("sg-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("ip_protocol"),
        Some(&Value::String("-1".to_string()))
    );
    assert_eq!(attributes.get("from_port"), Some(&Value::Int(0)));
    assert_eq!(attributes.get("to_port"), Some(&Value::Int(0)));
}

#[test]
fn test_extract_ec2_security_group_egress_attributes_with_prefix_list() {
    let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
        .security_group_rule_id("sgr-11111111")
        .group_id("sg-12345678")
        .ip_protocol("tcp")
        .from_port(443)
        .to_port(443)
        .prefix_list_id("pl-87654321")
        .build();
    let mut attributes = HashMap::new();
    AwsProvider::extract_ec2_security_group_egress_attributes(&rule, &mut attributes);
    assert_eq!(
        attributes.get("destination_prefix_list_id"),
        Some(&Value::String("pl-87654321".to_string()))
    );
}

#[test]
fn test_extract_ec2_security_group_egress_attributes_with_ipv6() {
    let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
        .security_group_rule_id("sgr-22222222")
        .group_id("sg-12345678")
        .ip_protocol("-1")
        .from_port(0)
        .to_port(0)
        .cidr_ipv6("::/0")
        .build();
    let mut attributes = HashMap::new();
    AwsProvider::extract_ec2_security_group_egress_attributes(&rule, &mut attributes);
    assert_eq!(
        attributes.get("cidr_ipv6"),
        Some(&Value::String("::/0".to_string()))
    );
}

// --- EC2 route table route extraction from describe response ---

#[test]
fn test_route_table_routes_extraction() {
    // Simulates the route extraction logic in read_ec2_route_table
    let route1 = aws_sdk_ec2::types::Route::builder()
        .destination_cidr_block("10.0.0.0/16")
        .gateway_id("local")
        .build();
    let route2 = aws_sdk_ec2::types::Route::builder()
        .destination_cidr_block("0.0.0.0/0")
        .gateway_id("igw-12345678")
        .build();

    let rt = aws_sdk_ec2::types::RouteTable::builder()
        .route_table_id("rtb-12345678")
        .vpc_id("vpc-12345678")
        .routes(route1)
        .routes(route2)
        .build();

    // Replicate route extraction logic from read_ec2_route_table
    let mut routes_list = Vec::new();
    for route in rt.routes() {
        let mut route_map = HashMap::new();
        if let Some(dest) = route.destination_cidr_block() {
            route_map.insert("destination".to_string(), Value::String(dest.to_string()));
        }
        if let Some(gw) = route.gateway_id() {
            route_map.insert("gateway_id".to_string(), Value::String(gw.to_string()));
        }
        if !route_map.is_empty() {
            routes_list.push(Value::Map(route_map));
        }
    }

    assert_eq!(routes_list.len(), 2);
    if let Value::Map(ref map) = routes_list[0] {
        assert_eq!(
            map.get("destination"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
        assert_eq!(
            map.get("gateway_id"),
            Some(&Value::String("local".to_string()))
        );
    }
    if let Value::Map(ref map) = routes_list[1] {
        assert_eq!(
            map.get("destination"),
            Some(&Value::String("0.0.0.0/0".to_string()))
        );
        assert_eq!(
            map.get("gateway_id"),
            Some(&Value::String("igw-12345678".to_string()))
        );
    }
}

#[test]
fn test_route_table_routes_extraction_empty() {
    let rt = aws_sdk_ec2::types::RouteTable::builder()
        .route_table_id("rtb-12345678")
        .build();
    assert!(rt.routes().is_empty());
}

// --- Internet Gateway attachment extraction ---

#[test]
fn test_internet_gateway_attachment_extraction() {
    // Simulates the vpc_id extraction from IGW attachments
    let attachment = aws_sdk_ec2::types::InternetGatewayAttachment::builder()
        .vpc_id("vpc-12345678")
        .state(aws_sdk_ec2::types::AttachmentStatus::from("available"))
        .build();
    let igw = aws_sdk_ec2::types::InternetGateway::builder()
        .internet_gateway_id("igw-12345678")
        .attachments(attachment)
        .build();

    // Replicate logic from read_ec2_internet_gateway
    let mut attributes = HashMap::new();
    if let Some(att) = igw.attachments().first()
        && let Some(vpc_id) = att.vpc_id()
    {
        attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
    }

    assert_eq!(
        attributes.get("vpc_id"),
        Some(&Value::String("vpc-12345678".to_string()))
    );
}

#[test]
fn test_internet_gateway_no_attachment() {
    let igw = aws_sdk_ec2::types::InternetGateway::builder()
        .internet_gateway_id("igw-12345678")
        .build();

    let mut attributes = HashMap::new();
    if let Some(att) = igw.attachments().first()
        && let Some(vpc_id) = att.vpc_id()
    {
        attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
    }

    assert!(!attributes.contains_key("vpc_id"));
}

// --- extract_ec2_subnet_attributes with map_public_ip_on_launch true ---

#[test]
fn test_extract_ec2_subnet_attributes_map_public_ip_true() {
    let subnet = aws_sdk_ec2::types::Subnet::builder()
        .subnet_id("subnet-12345678")
        .vpc_id("vpc-12345678")
        .cidr_block("10.0.1.0/24")
        .availability_zone("ap-northeast-1a")
        .map_public_ip_on_launch(true)
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_subnet_attributes(&subnet, &mut attributes);
    assert_eq!(identifier, Some("subnet-12345678".to_string()));
    assert_eq!(
        attributes.get("map_public_ip_on_launch"),
        Some(&Value::Bool(true))
    );
}

// --- Subnet availability zone DSL format conversion ---

#[test]
fn test_subnet_availability_zone_dsl_format() {
    // Simulates the AZ conversion in read_ec2_subnet
    let az = "ap-northeast-1a";
    let az_dsl = format!("aws.AvailabilityZone.{}", az.replace('-', "_"));
    assert_eq!(az_dsl, "aws.AvailabilityZone.ap_northeast_1a");
}

#[test]
fn test_subnet_availability_zone_dsl_format_us_east() {
    let az = "us-east-1b";
    let az_dsl = format!("aws.AvailabilityZone.{}", az.replace('-', "_"));
    assert_eq!(az_dsl, "aws.AvailabilityZone.us_east_1b");
}

// --- Subnet DNS hostname_type enum conversion ---

#[test]
fn test_subnet_hostname_type_dsl_to_aws_sdk() {
    use aws_sdk_ec2::types::HostnameType;
    use carina_core::utils::convert_enum_value;

    // DSL uses underscores: aws.ec2.subnet.HostnameType.ip_name
    // convert_enum_value for 5-part identifiers converts underscores to hyphens
    let dsl_value = "aws.ec2.subnet.HostnameType.ip_name";
    let converted = convert_enum_value(dsl_value);
    assert_eq!(converted, "ip-name");
    let hostname_type = HostnameType::from(converted.as_str());
    assert_eq!(hostname_type, HostnameType::IpName);

    let dsl_value2 = "aws.ec2.subnet.HostnameType.resource_name";
    let converted2 = convert_enum_value(dsl_value2);
    assert_eq!(converted2, "resource-name");
    let hostname_type2 = HostnameType::from(converted2.as_str());
    assert_eq!(hostname_type2, HostnameType::ResourceName);
}

// --- Subnet modify_subnet_attributes: DNS options must be separate API calls ---
// The AWS ModifySubnetAttribute API only allows modifying one attribute at a time.
// See: https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_ModifySubnetAttribute.html
// "You can only modify one attribute at a time."
// This test verifies that private_dns_name_options_on_launch fields are parsed
// correctly for separate API calls.

#[test]
fn test_subnet_dns_options_fields_parsed_separately() {
    use carina_core::utils::convert_enum_value;

    // Simulate the attributes map that would be passed to modify_subnet_attributes
    let mut fields = HashMap::new();
    fields.insert(
        "hostname_type".to_string(),
        Value::String("aws.ec2.subnet.HostnameType.ip_name".to_string()),
    );
    fields.insert(
        "enable_resource_name_dns_a_record".to_string(),
        Value::Bool(true),
    );
    fields.insert(
        "enable_resource_name_dns_aaaa_record".to_string(),
        Value::Bool(false),
    );

    // Each field should be independently extractable for separate API calls
    if let Some(Value::String(ht)) = fields.get("hostname_type") {
        let hostname_val = convert_enum_value(ht);
        assert_eq!(hostname_val, "ip-name");
    } else {
        panic!("hostname_type should be present and a String");
    }

    if let Some(Value::Bool(v)) = fields.get("enable_resource_name_dns_a_record") {
        assert!(*v);
    } else {
        panic!("enable_resource_name_dns_a_record should be present and a Bool");
    }

    if let Some(Value::Bool(v)) = fields.get("enable_resource_name_dns_aaaa_record") {
        assert!(!(*v));
    } else {
        panic!("enable_resource_name_dns_aaaa_record should be present and a Bool");
    }
}

// --- extract_ec2_eip_attributes tests ---

#[test]
fn test_extract_ec2_eip_attributes() {
    let addr = aws_sdk_ec2::types::Address::builder()
        .allocation_id("eipalloc-12345678")
        .domain(aws_sdk_ec2::types::DomainType::Vpc)
        .public_ip("203.0.113.1")
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_eip_attributes(&addr, &mut attributes);
    assert_eq!(identifier, Some("eipalloc-12345678".to_string()));
    assert_eq!(
        attributes.get("allocation_id"),
        Some(&Value::String("eipalloc-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("domain"),
        Some(&Value::String("vpc".to_string()))
    );
    assert_eq!(
        attributes.get("public_ip"),
        Some(&Value::String("203.0.113.1".to_string()))
    );
}

#[test]
fn test_extract_ec2_eip_attributes_minimal() {
    let addr = aws_sdk_ec2::types::Address::builder().build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_eip_attributes(&addr, &mut attributes);
    assert_eq!(identifier, None);
    assert!(attributes.is_empty());
}

// --- extract_ec2_nat_gateway_attributes tests ---

#[test]
fn test_extract_ec2_nat_gateway_attributes() {
    let nat_addr = aws_sdk_ec2::types::NatGatewayAddress::builder()
        .allocation_id("eipalloc-12345678")
        .build();
    let ngw = aws_sdk_ec2::types::NatGateway::builder()
        .nat_gateway_id("nat-12345678")
        .subnet_id("subnet-12345678")
        .connectivity_type(aws_sdk_ec2::types::ConnectivityType::Public)
        .nat_gateway_addresses(nat_addr)
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_nat_gateway_attributes(&ngw, &mut attributes);
    assert_eq!(identifier, Some("nat-12345678".to_string()));
    assert_eq!(
        attributes.get("nat_gateway_id"),
        Some(&Value::String("nat-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("subnet_id"),
        Some(&Value::String("subnet-12345678".to_string()))
    );
    assert_eq!(
        attributes.get("connectivity_type"),
        Some(&Value::String("public".to_string()))
    );
    assert_eq!(
        attributes.get("allocation_id"),
        Some(&Value::String("eipalloc-12345678".to_string()))
    );
}

#[test]
fn test_extract_ec2_nat_gateway_attributes_minimal() {
    let ngw = aws_sdk_ec2::types::NatGateway::builder().build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_nat_gateway_attributes(&ngw, &mut attributes);
    assert_eq!(identifier, None);
}

#[test]
fn test_extract_ec2_nat_gateway_attributes_private() {
    let ngw = aws_sdk_ec2::types::NatGateway::builder()
        .nat_gateway_id("nat-87654321")
        .subnet_id("subnet-87654321")
        .connectivity_type(aws_sdk_ec2::types::ConnectivityType::Private)
        .build();
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_ec2_nat_gateway_attributes(&ngw, &mut attributes);
    assert_eq!(identifier, Some("nat-87654321".to_string()));
    assert_eq!(
        attributes.get("connectivity_type"),
        Some(&Value::String("private".to_string()))
    );
    // Private NAT gateways don't have allocation_id
    assert_eq!(attributes.get("allocation_id"), None);
}

// --- extract_iam_role_attributes tests ---

#[test]
fn test_extract_iam_role_attributes() {
    let role = aws_sdk_iam::types::Role::builder()
        .role_name("test-role")
        .role_id("AROAEXAMPLE12345")
        .arn("arn:aws:iam::123456789012:role/test-role")
        .path("/")
        .assume_role_policy_document(
            "%7B%22Version%22%3A%222012-10-17%22%2C%22Statement%22%3A%5B%7B%22Effect%22%3A%22Allow%22%2C%22Principal%22%3A%7B%22Service%22%3A%22ec2.amazonaws.com%22%7D%2C%22Action%22%3A%22sts%3AAssumeRole%22%7D%5D%7D",
        )
        .description("Test role")
        .max_session_duration(7200)
        .create_date(aws_sdk_iam::primitives::DateTime::from_secs(0))
        .build()
        .expect("failed to build Role");
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_iam_role_attributes(&role, &mut attributes);
    assert_eq!(identifier, Some("test-role".to_string()));
    assert_eq!(
        attributes.get("role_name"),
        Some(&Value::String("test-role".to_string()))
    );
    assert_eq!(
        attributes.get("role_id"),
        Some(&Value::String("AROAEXAMPLE12345".to_string()))
    );
    assert_eq!(
        attributes.get("arn"),
        Some(&Value::String(
            "arn:aws:iam::123456789012:role/test-role".to_string()
        ))
    );
    assert_eq!(
        attributes.get("path"),
        Some(&Value::String("/".to_string()))
    );
    assert_eq!(
        attributes.get("description"),
        Some(&Value::String("Test role".to_string()))
    );
    assert_eq!(
        attributes.get("max_session_duration"),
        Some(&Value::Int(7200))
    );
    // Verify that the assume_role_policy_document is converted to a Map with snake_case keys
    let policy_doc = attributes
        .get("assume_role_policy_document")
        .expect("assume_role_policy_document should be present");
    if let Value::Map(map) = policy_doc {
        assert!(map.contains_key("version"), "should have 'version' key");
        assert!(map.contains_key("statement"), "should have 'statement' key");
        if let Some(Value::String(v)) = map.get("version") {
            assert_eq!(v, "2012-10-17");
        } else {
            panic!("Expected version to be String");
        }
    } else {
        panic!("Expected Map, got {:?}", policy_doc);
    }
}

#[test]
fn test_extract_iam_role_attributes_minimal() {
    let role = aws_sdk_iam::types::Role::builder()
        .role_name("minimal-role")
        .role_id("AROAMINIMAL")
        .arn("arn:aws:iam::123456789012:role/minimal-role")
        .path("/")
        .create_date(aws_sdk_iam::primitives::DateTime::from_secs(0))
        .build()
        .expect("failed to build Role");
    let mut attributes = HashMap::new();
    let identifier = AwsProvider::extract_iam_role_attributes(&role, &mut attributes);
    assert_eq!(identifier, Some("minimal-role".to_string()));
    assert_eq!(attributes.get("description"), None);
    assert_eq!(attributes.get("max_session_duration"), None);
}
