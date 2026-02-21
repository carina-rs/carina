use carina_smithy::*;
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/smithy");
    path.push(format!("{name}.json"));
    path
}

fn load_model(name: &str) -> SmithyModel {
    let path = fixture_path(name);
    if !path.exists() {
        panic!(
            "Model file not found: {}\nRun scripts/download-smithy-models.sh first",
            path.display()
        );
    }
    let file = std::fs::File::open(&path).expect("Failed to open model file");
    let reader = std::io::BufReader::new(file);
    parse_reader(reader).expect("Failed to parse model")
}

// ── EC2 Tests ──

#[test]
fn test_ec2_parse() {
    let model = load_model("ec2");
    assert_eq!(model.smithy, "2.0");
    assert!(model.shapes.len() > 4000); // EC2 has ~4,715 shapes
}

#[test]
fn test_ec2_find_service() {
    let model = load_model("ec2");
    let (id, service) = model.find_service().expect("No service found");
    assert_eq!(id, "com.amazonaws.ec2#AmazonEC2");
    assert_eq!(service.version, "2016-11-15");
    assert!(service.operations.len() > 700);
}

#[test]
fn test_ec2_create_vpc_operation() {
    let model = load_model("ec2");
    let op = model
        .get_operation("com.amazonaws.ec2#CreateVpc")
        .expect("CreateVpc not found");

    assert_eq!(
        op.input.as_ref().unwrap().target,
        "com.amazonaws.ec2#CreateVpcRequest"
    );
    assert_eq!(
        op.output.as_ref().unwrap().target,
        "com.amazonaws.ec2#CreateVpcResult"
    );
    assert!(op.traits.contains_key("smithy.api#documentation"));
}

#[test]
fn test_ec2_create_vpc_input() {
    let model = load_model("ec2");
    let input = model
        .operation_input("com.amazonaws.ec2#CreateVpc")
        .expect("No input");

    // CreateVpcRequest has members including CidrBlock, InstanceTenancy, TagSpecifications
    assert!(input.members.contains_key("CidrBlock"));
    assert!(input.members.contains_key("InstanceTenancy"));
    assert!(input.members.contains_key("TagSpecifications"));
    assert!(input.members.contains_key("DryRun"));
    assert!(SmithyModel::is_input(input));
}

#[test]
fn test_ec2_create_vpc_output() {
    let model = load_model("ec2");
    let output = model
        .operation_output("com.amazonaws.ec2#CreateVpc")
        .expect("No output");

    assert!(output.members.contains_key("Vpc"));
    assert!(SmithyModel::is_output(output));
}

#[test]
fn test_ec2_vpc_structure() {
    let model = load_model("ec2");
    let vpc = model
        .get_structure("com.amazonaws.ec2#Vpc")
        .expect("Vpc not found");

    // Check key fields
    assert!(vpc.members.contains_key("VpcId"));
    assert!(vpc.members.contains_key("CidrBlock"));
    assert!(vpc.members.contains_key("State"));
    assert!(vpc.members.contains_key("InstanceTenancy"));
    assert!(vpc.members.contains_key("Tags"));
    assert!(vpc.members.contains_key("OwnerId"));
    assert!(vpc.members.contains_key("DhcpOptionsId"));
    assert!(vpc.members.contains_key("IsDefault"));

    // Verify type references
    assert_eq!(vpc.members["State"].target, "com.amazonaws.ec2#VpcState");
    assert_eq!(
        vpc.members["InstanceTenancy"].target,
        "com.amazonaws.ec2#Tenancy"
    );
}

#[test]
fn test_ec2_delete_vpc_unit_output() {
    let model = load_model("ec2");
    // DeleteVpc returns smithy.api#Unit
    assert!(
        model
            .operation_output("com.amazonaws.ec2#DeleteVpc")
            .is_none()
    );
}

#[test]
fn test_ec2_delete_vpc_required_field() {
    let model = load_model("ec2");
    let input = model
        .operation_input("com.amazonaws.ec2#DeleteVpc")
        .expect("No input");

    assert!(SmithyModel::is_required(&input.members["VpcId"]));
    assert!(!SmithyModel::is_required(&input.members["DryRun"]));
}

#[test]
fn test_ec2_tenancy_enum() {
    let model = load_model("ec2");
    let values = model
        .enum_values("com.amazonaws.ec2#Tenancy")
        .expect("Tenancy enum not found");

    let value_strings: Vec<&str> = values.iter().map(|(_, v)| v.as_str()).collect();
    assert!(value_strings.contains(&"default"));
    assert!(value_strings.contains(&"dedicated"));
    assert!(value_strings.contains(&"host"));
}

#[test]
fn test_ec2_vpc_state_enum() {
    let model = load_model("ec2");
    let values = model
        .enum_values("com.amazonaws.ec2#VpcState")
        .expect("VpcState enum not found");

    let value_strings: Vec<&str> = values.iter().map(|(_, v)| v.as_str()).collect();
    assert!(value_strings.contains(&"pending"));
    assert!(value_strings.contains(&"available"));
}

#[test]
fn test_ec2_internet_gateway() {
    let model = load_model("ec2");

    // Operation exists
    let op = model
        .get_operation("com.amazonaws.ec2#CreateInternetGateway")
        .expect("CreateInternetGateway not found");
    assert!(op.input.is_some());
    assert!(op.output.is_some());

    // InternetGateway structure
    let igw = model
        .get_structure("com.amazonaws.ec2#InternetGateway")
        .expect("InternetGateway not found");
    assert!(igw.members.contains_key("InternetGatewayId"));
    assert!(igw.members.contains_key("Tags"));
    assert!(igw.members.contains_key("Attachments"));
    assert!(igw.members.contains_key("OwnerId"));
}

#[test]
fn test_ec2_describe_vpcs_paginated() {
    let model = load_model("ec2");
    let op = model
        .get_operation("com.amazonaws.ec2#DescribeVpcs")
        .expect("DescribeVpcs not found");

    // Should have pagination trait
    assert!(op.traits.contains_key("smithy.api#paginated"));
}

#[test]
fn test_ec2_shape_kind_resolution() {
    let model = load_model("ec2");

    assert_eq!(
        model.shape_kind("com.amazonaws.ec2#AmazonEC2"),
        Some(ShapeKind::Service)
    );
    assert_eq!(
        model.shape_kind("com.amazonaws.ec2#CreateVpc"),
        Some(ShapeKind::Operation)
    );
    assert_eq!(
        model.shape_kind("com.amazonaws.ec2#Vpc"),
        Some(ShapeKind::Structure)
    );
    assert_eq!(
        model.shape_kind("com.amazonaws.ec2#Tenancy"),
        Some(ShapeKind::Enum)
    );
    assert_eq!(
        model.shape_kind("com.amazonaws.ec2#Boolean"),
        Some(ShapeKind::Boolean)
    );
    assert_eq!(
        model.shape_kind("com.amazonaws.ec2#String"),
        Some(ShapeKind::String)
    );
    assert_eq!(
        model.shape_kind("com.amazonaws.ec2#Integer"),
        Some(ShapeKind::Integer)
    );
}

// ── S3 Tests ──

#[test]
fn test_s3_parse() {
    let model = load_model("s3");
    assert_eq!(model.smithy, "2.0");
    assert!(model.shapes.len() > 700);
}

#[test]
fn test_s3_find_service() {
    let model = load_model("s3");
    let (id, _service) = model.find_service().expect("No service found");
    assert_eq!(id, "com.amazonaws.s3#AmazonS3");
}

#[test]
fn test_s3_create_bucket() {
    let model = load_model("s3");
    let op = model
        .get_operation("com.amazonaws.s3#CreateBucket")
        .expect("CreateBucket not found");
    assert!(op.input.is_some());
    assert!(op.output.is_some());
}

#[test]
fn test_s3_has_union_shapes() {
    let model = load_model("s3");
    // S3 has union shapes (e.g., AnalyticsFilter, MetricsFilter)
    let mut union_count = 0;
    for shape in model.shapes.values() {
        if let Shape::Union(_) = shape {
            union_count += 1;
        }
    }
    assert!(union_count > 0, "S3 should have union shapes");
}

#[test]
fn test_s3_has_map_shapes() {
    let model = load_model("s3");
    // S3 has map shapes (e.g., Metadata)
    let metadata = model.get_shape("com.amazonaws.s3#Metadata");
    assert!(metadata.is_some());
    if let Some(Shape::Map(map)) = metadata {
        assert_eq!(map.key.target, "com.amazonaws.s3#MetadataKey");
        assert_eq!(map.value.target, "com.amazonaws.s3#MetadataValue");
    } else {
        panic!("Metadata should be a map shape");
    }
}

#[test]
fn test_s3_has_timestamp_shapes() {
    let model = load_model("s3");
    let mut timestamp_count = 0;
    for shape in model.shapes.values() {
        if let Shape::Timestamp(_) = shape {
            timestamp_count += 1;
        }
    }
    assert!(timestamp_count > 0, "S3 should have timestamp shapes");
}
