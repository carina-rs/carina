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
#[ignore = "requires provider schemas"]
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
#[ignore = "requires provider schemas"]
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
#[ignore = "requires provider schemas"]
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

    let list_enum = AttributeType::list(AttributeType::StringEnum {
        name: "Protocol".to_string(),
        values: vec!["tcp".to_string(), "udp".to_string()],
        namespace: None,
        to_dsl: None,
    });

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
#[ignore = "requires provider schemas"]
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

#[test]
fn attributes_block_undefined_binding_reference() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    sg_id: string = nonexistent.id
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined") && d.message.contains("nonexistent"));
    assert!(
        undefined_diag.is_some(),
        "Should warn about undefined binding in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_valid_binding_reference() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
}

attributes {
    sg_id: string = sg.group_id
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined") && d.message.contains("sg"));
    assert!(
        undefined_diag.is_none(),
        "Should NOT warn about defined binding in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_type_mismatch_bool_to_string() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    flag: string = true
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("expected string, got bool"));
    assert!(
        type_diag.is_some(),
        "Should warn about type mismatch in attributes block (bool assigned to string). Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_valid_types_no_warning() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
}

attributes {
    sg_id: string = sg.group_id
    name: string = "hello"
    enabled: bool = true
    count: int = 42
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("expected") && d.message.contains("got"));
    assert!(
        type_diag.is_none(),
        "Should NOT warn about valid types in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn nested_block_name_not_flagged_as_unknown() {
    // When a nested struct field uses block_name (e.g., "transition" for "transitions"),
    // validate_struct_value should not flag it as an unknown field.
    let engine = test_engine_with_block_name_nested();
    let doc = create_document(
        r#"let r = test.block.resource {
config = {
    transition {
        days = 30
        storage_class = "GLACIER"
    }
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown field 'transition'"));
    assert!(
        unknown.is_none(),
        "block_name 'transition' should not be flagged as unknown in nested struct. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn find_let_binding_position_with_multibyte_leading_whitespace() {
    // Regression test for issue #724: find_let_binding_position uses byte offset
    // as character column. When multi-byte whitespace (e.g., full-width space U+3000)
    // appears before "let", the byte offset differs from the character offset.
    let engine = test_engine();

    // U+3000 (ideographic space) is 3 bytes in UTF-8 but 1 character.
    // Rust's str::trim() strips it as Unicode whitespace.
    // Line: "\u{3000}let my_var = awscc.ec2.vpc { }"
    // "let " starts at byte 3, but character offset 1.
    // name_col should be char 1 + 4 = 5 (correct)
    // Bug produces byte 3 + 4 = 7 (wrong)
    let text = "\u{3000}let my_var = awscc.ec2.vpc { }";
    let result = engine.find_let_binding_position(text, "my_var");
    assert_eq!(
        result,
        Some((0, 5)),
        "Column should be character offset (5), not byte offset (7)"
    );
}

#[test]
fn attributes_block_detection_with_brace_on_same_line() {
    // Regression test: ensure output block detection works correctly
    // after removing the redundant `|| trimmed == "attributes {"` condition.
    // The simplified condition `starts_with("output ") && contains('{')` must
    // still detect `attributes {` (the only valid output block syntax).
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    flag: string = true
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("expected string, got bool"));
    assert!(
        type_diag.is_some(),
        "Output block detection should work with simplified condition. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn undefined_reference_detected_for_non_id_name_properties() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

awscc.ec2.vpc {
name = "test-vpc"
cidr_block = "10.0.0.0/16"
}

awscc.ec2.subnet {
name = "test-subnet"
vpc_id = nonexistent_vpc.vpc_id
cidr_block = "10.0.1.0/24"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined") && d.message.contains("nonexistent_vpc"));
    assert!(
        undefined_diag.is_some(),
        "Should warn about undefined reference 'nonexistent_vpc.vpc_id'. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn defined_reference_not_flagged_for_non_id_name_properties() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let main_vpc = awscc.ec2.vpc {
name = "test-vpc"
cidr_block = "10.0.0.0/16"
}

awscc.ec2.subnet {
name = "test-subnet"
vpc_id = main_vpc.vpc_id
cidr_block = "10.0.1.0/24"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined") && d.message.contains("main_vpc"));
    assert!(
        undefined_diag.is_none(),
        "Should NOT warn about defined binding 'main_vpc'. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn provider_in_module_emits_error() {
    let engine = test_engine();
    let doc = create_document(
        r#"arguments {
    vpc_cidr: string
}

provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.vpc {
    cidr_block = args.vpc_cidr
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let provider_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("provider blocks are not allowed inside modules")
    });
    assert!(
        provider_diag.is_some(),
        "Should error about provider block in module. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diag = provider_diag.unwrap();
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn provider_without_module_markers_no_error() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let provider_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("provider blocks are not allowed inside modules")
    });
    assert!(
        provider_diag.is_none(),
        "Should NOT error about provider in non-module file. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn unknown_function_call_produces_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.vpc {
    cidr_block = not_a_function("hello")
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let func_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown function 'not_a_function'"));
    assert!(
        func_diag.is_some(),
        "Should report unknown function 'not_a_function'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    let diag = func_diag.unwrap();
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn known_function_call_no_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.vpc {
    cidr_block = join("-", ["a", "b"])
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let func_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown function"));
    assert!(
        func_diag.is_none(),
        "Known function 'join' should not produce unknown function diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn resource_ref_typo_suggests_similar_attribute() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}

let igw = awscc.ec2.internet_gateway {
}

let rt = awscc.ec2.route_table {
  vpc_id = "vpc-123"
}

awscc.ec2.route {
  route_table_id         = rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = igw.internet_gateway_idd
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let typo_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("Unknown attribute 'internet_gateway_idd'")
            && d.message.contains("Did you mean 'internet_gateway_id'?")
    });
    assert!(
        typo_diag.is_some(),
        "Should warn about typo in resource ref attribute with suggestion. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn resource_ref_valid_attribute_no_warning() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}

let igw = awscc.ec2.internet_gateway {
}

awscc.ec2.vpc_gateway_attachment {
  internet_gateway_id = igw.internet_gateway_id
  vpc_id              = "vpc-123"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let ref_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown attribute") && d.message.contains("igw"));
    assert!(
        ref_diag.is_none(),
        "Valid attribute reference should not produce warning. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn pipe_preferred_direct_call_produces_info_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let name = join("-", parts)
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let pipe_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Consider using pipe form for 'join'"));
    assert!(
        pipe_diag.is_some(),
        "Direct call to pipe-preferred function should produce diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diag = pipe_diag.unwrap();
    assert_eq!(
        diag.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::INFORMATION),
        "Pipe-preferred diagnostic should be info-level, not warning"
    );
}

#[test]
fn validate_module_arg_type_ipv4_address_invalid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv4_address".to_string());
    let value = Value::String("not-an-ip".to_string());
    let result = engine.validate_module_arg_type(&type_expr, &value);
    assert!(
        result.is_some(),
        "Should return error for invalid ipv4_address"
    );
}

#[test]
fn validate_module_arg_type_ipv4_address_valid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv4_address".to_string());
    let value = Value::String("192.168.1.1".to_string());
    let result = engine.validate_module_arg_type(&type_expr, &value);
    assert!(
        result.is_none(),
        "Should not return error for valid ipv4_address. Got: {:?}",
        result
    );
}

#[test]
fn validate_module_arg_type_ipv6_cidr_invalid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv6_cidr".to_string());
    let value = Value::String("not-a-cidr".to_string());
    let result = engine.validate_module_arg_type(&type_expr, &value);
    assert!(
        result.is_some(),
        "Should return error for invalid ipv6_cidr"
    );
}

#[test]
fn validate_module_arg_type_ipv6_cidr_valid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv6_cidr".to_string());
    let value = Value::String("2001:db8::/32".to_string());
    let result = engine.validate_module_arg_type(&type_expr, &value);
    assert!(
        result.is_none(),
        "Should not return error for valid ipv6_cidr. Got: {:?}",
        result
    );
}

#[test]
fn validate_module_arg_type_ipv6_address_invalid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv6_address".to_string());
    let value = Value::String("not-an-ipv6".to_string());
    let result = engine.validate_module_arg_type(&type_expr, &value);
    assert!(
        result.is_some(),
        "Should return error for invalid ipv6_address"
    );
}

#[test]
fn validate_module_arg_type_ipv6_address_valid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv6_address".to_string());
    let value = Value::String("2001:db8::1".to_string());
    let result = engine.validate_module_arg_type(&type_expr, &value);
    assert!(
        result.is_none(),
        "Should not return error for valid ipv6_address. Got: {:?}",
        result
    );
}

#[test]
fn validate_module_arg_type_list_ipv4_address_invalid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::List(Box::new(
        carina_core::parser::TypeExpr::Simple("ipv4_address".to_string()),
    ));
    let value = Value::List(vec![
        Value::String("192.168.1.1".to_string()),
        Value::String("bad-ip".to_string()),
    ]);
    let result = engine.validate_module_arg_type(&type_expr, &value);
    assert!(
        result.is_some(),
        "Should return error for invalid ipv4_address in list"
    );
    assert!(
        result.as_ref().unwrap().contains("Element 1"),
        "Error should reference element index. Got: {:?}",
        result
    );
}

#[test]
fn validate_module_arg_type_list_ipv6_cidr_valid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::List(Box::new(
        carina_core::parser::TypeExpr::Simple("ipv6_cidr".to_string()),
    ));
    let value = Value::List(vec![
        Value::String("2001:db8::/32".to_string()),
        Value::String("::/0".to_string()),
    ]);
    let result = engine.validate_module_arg_type(&type_expr, &value);
    assert!(
        result.is_none(),
        "Should not return error for valid ipv6_cidr list. Got: {:?}",
        result
    );
}

#[test]
fn pipe_preferred_pipe_form_no_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let name = parts |> join("-")
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let pipe_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Consider using pipe form for 'join'"));
    assert!(
        pipe_diag.is_none(),
        "Pipe form should not produce pipe-preferred diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_ipv4_address_invalid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    ip: ipv4_address = "not-an-ip"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("ipv4_address") || d.message.contains("IPv4"));
    assert!(
        type_diag.is_some(),
        "Should warn about invalid ipv4_address in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_ipv4_address_valid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    ip: ipv4_address = "192.168.1.1"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("ipv4_address") || d.message.contains("IPv4"));
    assert!(
        type_diag.is_none(),
        "Should NOT warn about valid ipv4_address in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_cidr_invalid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    network: ipv4_cidr = "not-a-cidr"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("CIDR") || d.message.contains("cidr"));
    assert!(
        type_diag.is_some(),
        "Should warn about invalid cidr in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_ipv6_address_invalid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    addr: ipv6_address = "not-ipv6"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("ipv6") || d.message.contains("IPv6"));
    assert!(
        type_diag.is_some(),
        "Should warn about invalid ipv6_address in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn resource_ref_type_check_helper_regression() {
    // Regression test for refactoring: all three ResourceRef type-checking paths
    // (Union, StringEnum, Custom) must produce consistent "Type mismatch" diagnostics.
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    fn dummy_validate(v: &carina_core::resource::Value) -> Result<(), String> {
        match v {
            carina_core::resource::Value::String(s) if s.starts_with("test.") => Ok(()),
            _ => Err("invalid custom value".to_string()),
        }
    }

    // Source resource: has a String attribute "name" and a Custom "my_id"
    let source_schema = ResourceSchema::new("test.source")
        .attribute(AttributeSchema::new("name", AttributeType::String))
        .attribute(AttributeSchema::new(
            "my_id",
            AttributeType::Custom {
                name: "MyId".to_string(),
                base: Box::new(AttributeType::String),
                validate: dummy_validate,
                namespace: Some("test".to_string()),
                to_dsl: None,
            },
        ));

    // Target resource: has Union, StringEnum, and Custom attributes
    let target_schema = ResourceSchema::new("test.target")
        .attribute(AttributeSchema::new(
            "union_attr",
            AttributeType::Union(vec![AttributeType::Int, AttributeType::Bool]),
        ))
        .attribute(AttributeSchema::new(
            "enum_attr",
            AttributeType::StringEnum {
                name: "Status".to_string(),
                values: vec!["active".to_string(), "inactive".to_string()],
                namespace: None,
                to_dsl: None,
            },
        ))
        .attribute(AttributeSchema::new(
            "custom_attr",
            AttributeType::Custom {
                name: "MyId".to_string(),
                base: Box::new(AttributeType::String),
                validate: dummy_validate,
                namespace: Some("test".to_string()),
                to_dsl: None,
            },
        ));

    let mut schemas = HashMap::new();
    schemas.insert("test.source".to_string(), source_schema);
    schemas.insert("test.target".to_string(), target_schema);
    let engine = custom_engine(schemas);

    // Case 1: Union attr with incompatible ResourceRef (MyId != Int|Bool) -> mismatch
    let doc = create_document(
        r#"let src = test.source {
name = "hello"
}

test.target {
union_attr = src.my_id
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let union_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("MyId"));
    assert!(
        union_mismatch.is_some(),
        "Union attr should warn about type mismatch for incompatible ResourceRef. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    // Case 2: StringEnum attr with incompatible ResourceRef (MyId != Status) -> mismatch
    let doc = create_document(
        r#"let src = test.source {
name = "hello"
}

test.target {
enum_attr = src.my_id
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let enum_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("MyId"));
    assert!(
        enum_mismatch.is_some(),
        "StringEnum attr should warn about type mismatch for incompatible ResourceRef. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    // Case 3: Custom attr with compatible ResourceRef (MyId == MyId) -> no mismatch
    let doc = create_document(
        r#"let src = test.source {
name = "hello"
}

test.target {
custom_attr = src.my_id
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let custom_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("custom_attr"));
    assert!(
        custom_mismatch.is_none(),
        "Custom attr should NOT warn when ResourceRef type matches. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    // Case 4: Union attr with String ResourceRef -> no mismatch (String is always compatible)
    let doc = create_document(
        r#"let src = test.source {
name = "hello"
}

test.target {
union_attr = src.name
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let string_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("union_attr"));
    assert!(
        string_mismatch.is_none(),
        "Union attr should NOT warn when ResourceRef is String type. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_ipv6_cidr_invalid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    net6: ipv6_cidr = "not-a-cidr"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("IPv6") || d.message.contains("ipv6"));
    assert!(
        type_diag.is_some(),
        "Should warn about invalid ipv6_cidr in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn resource_validation_failed_with_attribute_points_to_attribute_line() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeError};
    use std::collections::HashMap;

    let schema = ResourceSchema::new("mock.test.resource")
        .attribute(AttributeSchema::new("name", AttributeType::String).required())
        .attribute(AttributeSchema::new(
            "tags",
            AttributeType::Map(Box::new(AttributeType::String)),
        ))
        .with_validator(|attrs| {
            if let Some(carina_core::resource::Value::Map(map)) = attrs.get("tags") {
                let has_key = map.keys().any(|k| k.eq_ignore_ascii_case("key"));
                let has_value = map.keys().any(|k| k.eq_ignore_ascii_case("value"));
                if has_key && has_value {
                    return Err(vec![TypeError::ResourceValidationFailed {
                        message: "tags key/value error".to_string(),
                        attribute: Some("tags".to_string()),
                    }]);
                }
            }
            Ok(())
        });

    let mut schemas = HashMap::new();
    schemas.insert("mock.test.resource".to_string(), schema);
    let engine = custom_engine(schemas);

    let doc = create_document(
        "mock.test.resource {\n  name = 'test'\n  tags = {\n    key = 'Project'\n    value = 'carina'\n  }\n}",
    );

    let diagnostics = engine.analyze(&doc, None);
    let tags_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("tags key/value error"));
    assert!(
        tags_diag.is_some(),
        "Should have a tags validation error. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diag = tags_diag.unwrap();
    // The diagnostic should point to the "tags" line (line 2, 0-indexed),
    // not the resource declaration line (line 0).
    assert_eq!(
        diag.range.start.line, 2,
        "Diagnostic should point to the 'tags' attribute line (line 2), not the resource line. Got line {}",
        diag.range.start.line
    );
}

#[test]
fn warning_when_provider_loaded_but_schema_missing() {
    // Provider is loaded but doesn't have a schema for this resource type.
    // Should show WARNING (not ERROR), not "Unknown resource type".
    let engine = DiagnosticEngine::new(
        Arc::new(HashMap::new()),
        vec!["awscc".to_string()],
        Arc::new(vec![]),
    );
    let doc = create_document(
        r#"awscc.iam.role {
  role_name = 'test'
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown_type = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown resource type"));
    assert!(
        unknown_type.is_none(),
        "Loaded provider should not show 'Unknown resource type'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    let no_schema = diagnostics
        .iter()
        .find(|d| d.message.contains("No schema for"));
    assert!(
        no_schema.is_some(),
        "Should show 'No schema' warning. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(
        no_schema.unwrap().severity,
        Some(DiagnosticSeverity::WARNING)
    );
}

#[test]
fn error_when_provider_not_loaded_at_all() {
    // Provider completely unknown — not in provider_names, not in errors.
    let engine = DiagnosticEngine::new(Arc::new(HashMap::new()), vec![], Arc::new(vec![]));
    let doc = create_document(
        r#"awscc.iam.role {
  role_name = 'test'
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown_type = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown resource type"));
    assert!(
        unknown_type.is_some(),
        "Unknown provider should show 'Unknown resource type'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn no_undefined_resource_for_namespaced_enum_value() {
    // provider_names includes "awscc", so awscc.xxx.yyy.EnumType.VALUE
    // should NOT be flagged as "Undefined resource"
    let engine = DiagnosticEngine::new(
        Arc::new(HashMap::new()),
        vec!["awscc".to_string()],
        Arc::new(vec![]),
    );
    let doc = create_document(
        r#"awscc.organizations.organization {
  feature_set = awscc.organizations.organization.FeatureSet.ALL
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource"));
    assert!(
        undefined.is_none(),
        "Namespaced enum value should not be flagged as undefined resource. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}
