use super::*;

#[test]
fn block_name_not_flagged_as_unknown() {
    let engine = test_engine();
    // Use operating_region (singular block_name) instead of operating_regions
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

awscc.ec2.ipam {
name = "test-ipam"
operating_region {
    region_name = "ap-northeast-1"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown attribute 'operating_region'"));
    assert!(
        unknown.is_none(),
        "block_name 'operating_region' should not be flagged as unknown. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// Create a DiagnosticEngine with a schema that has deeply nested structs for testing.
#[test]
fn unknown_field_in_nested_struct_block() {
    let engine = test_engine_with_nested_structs();
    let doc = create_document(
        r#"let r = test.nested.resource {
outer {
    inner {
        unknown_nested = "bad"
    }
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown field 'unknown_nested'"));
    assert!(
        unknown.is_some(),
        "Should warn about unknown field in nested struct block. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn type_mismatch_in_nested_struct_field() {
    let engine = test_engine_with_nested_structs();
    let doc = create_document(
        r#"let r = test.nested.resource {
outer {
    inner {
        leaf_int = "not_a_number"
    }
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("Int"));
    assert!(
        mismatch.is_some(),
        "Should warn about type mismatch in nested struct field. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn valid_nested_struct_no_diagnostics() {
    let engine = test_engine_with_nested_structs();
    let doc = create_document(
        r#"let r = test.nested.resource {
outer {
    inner {
        leaf_field = "valid"
        leaf_int = 42
    }
    outer_field = "also valid"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    // Filter to only struct-related diagnostics (ignore unknown attribute warnings from test schema)
    let struct_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Unknown field") || d.message.contains("Type mismatch"))
        .collect();
    assert!(
        struct_diags.is_empty(),
        "Valid nested struct should have no field diagnostics. Got: {:?}",
        struct_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn string_enum_invalid_value_top_level() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
cidr_block = "10.0.0.0/16"
instance_tenancy = awscc.ec2.vpc.InstanceTenancy.invalid_value
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let enum_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("invalid_value"));
    assert!(
        enum_diag.is_some(),
        "Should warn about invalid StringEnum value. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn string_enum_valid_value_top_level() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
cidr_block = "10.0.0.0/16"
instance_tenancy = awscc.ec2.vpc.InstanceTenancy.default
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let enum_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("instance_tenancy") && d.message.contains("invalid"));
    assert!(
        enum_diag.is_none(),
        "Should NOT warn about valid StringEnum value. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn string_enum_invalid_value_in_struct_field() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = awscc.ec2.security_group.IpProtocol.invalid_proto
    from_port = 80
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let enum_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("invalid_proto"));
    assert!(
        enum_diag.is_some(),
        "Should warn about invalid StringEnum value in struct field. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn string_enum_valid_value_in_struct_field() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = awscc.ec2.security_group.IpProtocol.tcp
    from_port = 80
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let enum_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("ip_protocol") && d.message.contains("invalid"));
    assert!(
        enum_diag.is_none(),
        "Should NOT warn about valid StringEnum value in struct field. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn custom_type_validation_in_struct_field() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = awscc.ec2.security_group.IpProtocol.tcp
    from_port = 99999
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let port_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("from_port") || d.message.contains("99999"));
    assert!(
        port_diag.is_some(),
        "Should warn about out-of-range port in struct field. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn list_item_type_validation() {
    // Use a test engine with a List(StringEnum) schema to test list item validation
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let list_enum = AttributeType::List(Box::new(AttributeType::StringEnum {
        name: "Protocol".to_string(),
        values: vec!["tcp".to_string(), "udp".to_string()],
        namespace: None,
        to_dsl: None,
    }));

    let schema = ResourceSchema::new("test.list.resource")
        .attribute(AttributeSchema::new("protocols", list_enum));

    let mut schemas = HashMap::new();
    schemas.insert("test.list.resource".to_string(), schema);

    let engine = DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    );

    let doc = create_document(
        r#"let r = test.list.resource {
protocols = ["tcp", "invalid_protocol"]
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let item_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("invalid_protocol"));
    assert!(
        item_diag.is_some(),
        "Should warn about invalid item in List(StringEnum). Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn block_name_mixed_syntax_error() {
    let engine = test_engine();
    // Use both operating_region and operating_regions - should error
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

awscc.ec2.ipam {
name = "test-ipam"
operating_region {
    region_name = "ap-northeast-1"
}
operating_regions = [{
    region_name = "us-east-1"
}]
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let mixed_error = diagnostics.iter().find(|d| {
        d.message.contains("operating_region")
            && d.message.contains("operating_regions")
            && d.message.contains("same attribute")
    });
    assert!(
        mixed_error.is_some(),
        "Should error on mixed block_name and canonical name. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn union_static_value_validated() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::new("name", AttributeType::String).required())
        .attribute(AttributeSchema::new(
            "mode",
            AttributeType::Union(vec![
                AttributeType::StringEnum {
                    name: "Mode".to_string(),
                    values: vec!["active".to_string(), "passive".to_string()],
                    namespace: None,
                    to_dsl: None,
                },
                AttributeType::Int,
            ]),
        ));

    let mut schemas = HashMap::new();
    schemas.insert("test.test.resource".to_string(), schema);

    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"provider test {
region = "ap-northeast-1"
}

test.test.resource {
name = "my-resource"
mode = "invalid_mode"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_error = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch"));
    assert!(
        type_error.is_some(),
        "Should warn about invalid Union value. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn union_valid_static_value_no_warning() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::new("name", AttributeType::String).required())
        .attribute(AttributeSchema::new(
            "mode",
            AttributeType::Union(vec![
                AttributeType::StringEnum {
                    name: "Mode".to_string(),
                    values: vec!["active".to_string(), "passive".to_string()],
                    namespace: None,
                    to_dsl: None,
                },
                AttributeType::Int,
            ]),
        ));

    let mut schemas = HashMap::new();
    schemas.insert("test.test.resource".to_string(), schema);

    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"provider test {
region = "ap-northeast-1"
}

test.test.resource {
name = "my-resource"
mode = "active"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_error = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch"));
    assert!(
        type_error.is_none(),
        "Should NOT warn about valid Union value. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn union_valid_int_value_no_warning() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::new("name", AttributeType::String).required())
        .attribute(AttributeSchema::new(
            "mode",
            AttributeType::Union(vec![
                AttributeType::StringEnum {
                    name: "Mode".to_string(),
                    values: vec!["active".to_string(), "passive".to_string()],
                    namespace: None,
                    to_dsl: None,
                },
                AttributeType::Int,
            ]),
        ));

    let mut schemas = HashMap::new();
    schemas.insert("test.test.resource".to_string(), schema);

    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"provider test {
region = "ap-northeast-1"
}

test.test.resource {
name = "my-resource"
mode = 42
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_error = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch"));
    assert!(
        type_error.is_none(),
        "Should NOT warn when Int value matches Union member. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}
