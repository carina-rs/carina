use super::*;
use crate::backend::document_end_position;
use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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
    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
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
    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
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
    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
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
    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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
            AttributeType::map(AttributeType::String),
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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

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
    // Message should point at the missing download, not a generic "Unknown
    // resource type" (which misleads the user into searching for typos in a
    // name that is actually correct — see issue #2005).
    let engine = DiagnosticEngine::new(Arc::new(HashMap::new()), vec![], Arc::new(vec![]));
    let doc = create_document(
        r#"awscc.iam.role {
  role_name = 'test'
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

    let not_downloaded = diagnostics.iter().find(|d| {
        d.message.contains("Provider 'awscc' is not downloaded")
            && d.message.contains("carina init")
    });
    assert!(
        not_downloaded.is_some(),
        "Provider-not-downloaded case should say so explicitly, not 'Unknown resource type'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(
        not_downloaded.unwrap().severity,
        Some(DiagnosticSeverity::ERROR)
    );

    // And the old generic message should no longer fire for this case.
    let generic_unknown = diagnostics
        .iter()
        .find(|d| d.message == "Unknown resource type: awscc.iam.role");
    assert!(
        generic_unknown.is_none(),
        "Should not emit generic 'Unknown resource type' when the provider itself is not downloaded. Got: {:?}",
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

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource"));
    assert!(
        undefined.is_none(),
        "Namespaced enum value should not be flagged as undefined resource. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn no_undefined_resource_for_declared_but_uninstalled_provider() {
    // Regression for #2019: when a provider is declared (`provider awscc { ... }`)
    // in the current document but hasn't been downloaded (installed provider_names
    // is empty), the provider-namespaced enum reference on its right-hand side
    // used to be flagged as `Undefined resource: 'awscc'. Define it with 'let
    // awscc = aws...'` — which is both wrong (awscc is a namespace, not a let
    // binding) and actively misleading (following the fix breaks valid DSL).
    let engine = DiagnosticEngine::new(
        Arc::new(HashMap::new()),
        vec![], // nothing installed — simulates missing .carina/
        Arc::new(vec![]),
    );
    let doc = create_document(
        r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource"));
    assert!(
        undefined.is_none(),
        "Declared-but-uninstalled provider name must not be flagged as 'Undefined resource'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn undefined_resource_still_fires_when_identifier_is_not_a_declared_provider() {
    // Regression guard for #2019: the fix must not swallow legitimate undefined-
    // binding diagnostics. When the root identifier is not a declared provider
    // and not a defined binding, the existing "Undefined resource" message
    // should still fire.
    let engine = DiagnosticEngine::new(Arc::new(HashMap::new()), vec![], Arc::new(vec![]));
    let doc = create_document(
        r#"provider awscc {
  region = totally_unknown.some_attr
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource: 'totally_unknown'"));
    assert!(
        undefined.is_some(),
        "A genuinely unknown identifier should still produce the 'Undefined resource' diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn map_key_validation_warns_on_invalid_key() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    // Build schema: statement.condition has StringEnum keys
    let condition_type = AttributeType::map_with_key(
        AttributeType::StringEnum {
            name: "ConditionOperator".to_string(),
            values: vec!["string_equals".to_string(), "string_like".to_string()],
            namespace: None,
            to_dsl: None,
        },
        AttributeType::map(AttributeType::String),
    );
    let statement_type = AttributeType::Struct {
        name: "Statement".to_string(),
        fields: vec![
            StructField::new("effect", AttributeType::String),
            StructField::new("condition", condition_type),
        ],
    };
    let schema = ResourceSchema::new("test.resource").attribute(AttributeSchema::new(
        "policy",
        AttributeType::Struct {
            name: "Policy".to_string(),
            fields: vec![StructField::new(
                "statement",
                AttributeType::list(statement_type),
            )],
        },
    ));

    let mut schemas = HashMap::new();
    schemas.insert("test.test.resource".to_string(), schema);

    let engine = DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    );

    // Invalid condition key "unknown_op"
    let doc = create_document(
        r#"test.test.resource {
  policy = {
    statement {
      effect = 'Allow'
      condition = {
        unknown_op = { 'key' = 'value' }
      }
    }
  }
}
"#,
    );
    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
    let has_key_error = diagnostics
        .iter()
        .any(|d| d.message.contains("Map key") || d.message.contains("unknown_op"));
    assert!(
        has_key_error,
        "Should warn about invalid map key 'unknown_op'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn string_based_custom_types_are_compatible() {
    // Regression test for #1822: when both the source and sink are String-based
    // Custom types (e.g., AwsAccountId → TargetId), the LSP should not flag a
    // type mismatch, matching the CLI's behavior (#1795).
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    fn validate_account_id(v: &carina_core::resource::Value) -> Result<(), String> {
        match v {
            carina_core::resource::Value::String(s) if s.len() == 12 => Ok(()),
            _ => Err("expected 12-digit account ID".to_string()),
        }
    }

    fn validate_target_id(v: &carina_core::resource::Value) -> Result<(), String> {
        match v {
            carina_core::resource::Value::String(_) => Ok(()),
            _ => Err("expected string".to_string()),
        }
    }

    // Source resource: has an AwsAccountId attribute
    let source_schema = ResourceSchema::new("sts.caller_identity").attribute(AttributeSchema::new(
        "account_id",
        AttributeType::Custom {
            name: "AwsAccountId".to_string(),
            base: Box::new(AttributeType::String),
            validate: validate_account_id,
            namespace: Some("aws".to_string()),
            to_dsl: None,
        },
    ));

    // Target resource: has a TargetId attribute (also String-based Custom)
    let target_schema = ResourceSchema::new("sso.assignment").attribute(AttributeSchema::new(
        "target_id",
        AttributeType::Custom {
            name: "TargetId".to_string(),
            base: Box::new(AttributeType::String),
            validate: validate_target_id,
            namespace: Some("awscc".to_string()),
            to_dsl: None,
        },
    ));

    let mut schemas = HashMap::new();
    schemas.insert("aws.sts.caller_identity".to_string(), source_schema);
    schemas.insert("awscc.sso.assignment".to_string(), target_schema);
    let engine = custom_engine(schemas);

    let doc = create_document(
        r#"let caller = read aws.sts.caller_identity {}

awscc.sso.assignment {
target_id = caller.account_id
}"#,
    );
    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
    let type_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch"));
    assert!(
        type_mismatch.is_none(),
        "String-based Custom types should be compatible (AwsAccountId → TargetId). Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_cross_file_ref_no_false_positive() {
    // Regression: when exports.crn references a binding from a sibling file,
    // single-file parsing leaves the reference as Value::String("binding.attr").
    // The custom type validator (e.g., aws_account_id: 12-digit check) must
    // skip these dot-notation strings to avoid false positives.
    use carina_core::resource::Value;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    use std::collections::HashMap;

    let aws_account_id_type = AttributeType::Custom {
        name: "AwsAccountId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |v| match v {
            Value::String(s) if s.len() == 12 && s.chars().all(|c| c.is_ascii_digit()) => Ok(()),
            Value::String(s) => Err(format!(
                "must be exactly 12 digits, got {} characters",
                s.len()
            )),
            _ => Err("expected string".to_string()),
        },
        namespace: None,
        to_dsl: None,
    };
    let schema = ResourceSchema::new("awscc.organizations.account")
        .attribute(AttributeSchema::new("account_id", aws_account_id_type));
    let schemas: HashMap<String, ResourceSchema> = vec![(schema.resource_type.clone(), schema)]
        .into_iter()
        .collect();
    let engine = custom_engine(schemas);

    // exports.crn parsed alone: "registry_prod.account_id" stays as String
    let doc = create_document(
        r#"exports {
  accounts: list(aws_account_id) = [
    registry_prod.account_id,
  ]
}"#,
    );

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

    let false_positive = diagnostics
        .iter()
        .find(|d| d.message.contains("12 digits") || d.message.contains("12 characters"));
    assert!(
        false_positive.is_none(),
        "Dot-notation cross-file ref should be skipped by type validator. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_type_warning_survives_formatter_round_trip() {
    use carina_core::formatter::{FormatConfig, format};

    let original = r#"exports {
  values: list(bool) = [
    "nope",
  ]
}"#;
    let formatted = format(original, &FormatConfig::default()).unwrap();

    let engine = test_engine();
    let before = engine.analyze(
        &create_document(original),
        None,
        &HashMap::new(),
        &HashSet::new(),
    );
    let after = engine.analyze(
        &create_document(&formatted),
        None,
        &HashMap::new(),
        &HashSet::new(),
    );

    let before_warning = before
        .iter()
        .find(|d| d.message.contains("expected bool, got string"))
        .map(|d| d.message.clone());
    let after_warning = after
        .iter()
        .find(|d| d.message.contains("expected bool, got string"))
        .map(|d| d.message.clone());

    assert_eq!(formatted, "exports {\n  values: list(bool) = ['nope']\n}\n");
    assert_eq!(before_warning, after_warning);
    assert!(
        after_warning.is_some(),
        "formatter round-trip should not suppress exports type warnings; diagnostics after formatting: {:?}",
        after.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_ref_type_warning_survives_formatter_round_trip() {
    use carina_core::formatter::{FormatConfig, format};
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    use std::collections::HashMap;

    let schema = ResourceSchema::new("test.sample.resource")
        .attribute(AttributeSchema::new("enabled", AttributeType::Bool));
    let schemas: HashMap<String, ResourceSchema> = vec![(schema.resource_type.clone(), schema)]
        .into_iter()
        .collect();
    let engine = custom_engine(schemas);

    let original = r#"let item = test.sample.resource {
  enabled = true
}

exports {
  values: list(string) = [
    item.enabled,
  ]
}"#;
    let formatted = format(original, &FormatConfig::default()).unwrap();

    let before = engine.analyze(
        &create_document(original),
        None,
        &HashMap::new(),
        &HashSet::new(),
    );
    let after = engine.analyze(
        &create_document(&formatted),
        None,
        &HashMap::new(),
        &HashSet::new(),
    );

    let before_warning = before
        .iter()
        .find(|d| d.message.contains("export 'values': type mismatch"))
        .map(|d| d.message.clone());
    let after_warning = after
        .iter()
        .find(|d| d.message.contains("export 'values': type mismatch"))
        .map(|d| d.message.clone());

    assert_eq!(
        formatted,
        "let item = test.sample.resource {\n  enabled = true\n}\n\nexports {\n  values: list(string) = [item.enabled]\n}\n"
    );
    assert_eq!(before_warning, after_warning);
    assert!(
        after_warning.is_some(),
        "formatter round-trip should not suppress exports ref type warnings; diagnostics after formatting: {:?}",
        after.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_ref_type_warning_survives_document_reparse_after_format_edit() {
    use carina_core::formatter::{FormatConfig, format};
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    use std::collections::HashMap;

    let schema = ResourceSchema::new("test.sample.resource")
        .attribute(AttributeSchema::new("enabled", AttributeType::Bool));
    let schemas: HashMap<String, ResourceSchema> = vec![(schema.resource_type.clone(), schema)]
        .into_iter()
        .collect();
    let engine = custom_engine(schemas);

    let original = r#"let item = test.sample.resource {
  enabled = true
}

exports {
  values: list(string) = [
    item.enabled,
  ]
}"#;
    let formatted = format(original, &FormatConfig::default()).unwrap();
    let mut doc = create_document(original);
    let (last_line, last_char) = document_end_position(original);

    doc.apply_change(TextDocumentContentChangeEvent {
        range: Some(Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: last_line,
                character: last_char,
            },
        }),
        range_length: None,
        text: formatted.clone(),
    });

    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
    let warning = diagnostics
        .iter()
        .find(|d| d.message.contains("export 'values': type mismatch"))
        .map(|d| d.message.clone());

    assert_eq!(doc.text(), formatted);
    assert!(
        doc.parse_error().is_none(),
        "formatted text should reparse cleanly, got: {:?}",
        doc.parse_error()
    );
    assert!(
        warning.is_some(),
        "format edit + reparse should preserve exports ref type warning; diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_type_warning_after_format_with_cross_file_ref() {
    // Simulate: file has cross-file ref, formatter collapses list,
    // user then changes type annotation to wrong type.
    // Warning should appear for the type mismatch.
    let engine = test_engine();

    // After format-on-save, list is on one line. User changes string→bool.
    let doc = create_document(
        r#"exports {
  accounts: list(bool) = [registry_prod.account_id, registry_dev.account_id]
}"#,
    );
    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());

    // "registry_prod.account_id" is a cross-file ref (dot-notation string).
    // It should NOT produce a false positive (12-digit check etc.).
    // But list(bool) is wrong because the ref resolves to a string, not bool.
    // For now, since we can't resolve cross-file refs in the LSP,
    // at minimum we should NOT crash or hang.
    // The type mismatch may or may not be caught depending on schema availability.
    eprintln!(
        "diagnostics after format+type-change: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    // No false positive about "12 digits"
    let false_positive = diagnostics
        .iter()
        .find(|d| d.message.contains("12 digits") || d.message.contains("12 characters"));
    assert!(
        false_positive.is_none(),
        "Should not produce 12-digit false positive. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_type_warning_for_literal_mismatch() {
    let engine = test_engine();
    let doc = create_document(
        r#"exports {
  flag: bool = 'hello'
}"#,
    );
    let diagnostics = engine.analyze(&doc, None, &HashMap::new(), &HashSet::new());
    eprintln!(
        "literal mismatch diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let warning = diagnostics
        .iter()
        .find(|d| d.message.contains("expected bool"));
    assert!(
        warning.is_some(),
        "Should warn about bool vs string mismatch. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_type_warning_multiline_vs_oneline() {
    let engine = test_engine();
    // Multi-line (before format)
    let doc_multi = create_document("exports {\n  flag: bool = 'hello'\n}");
    let diag_multi = engine.analyze(&doc_multi, None, &HashMap::new(), &HashSet::new());
    eprintln!(
        "multi-line: {:?}",
        diag_multi.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    // After user types but before format - with wrong type and literal
    let doc_literal =
        create_document("exports {\n  accounts: list(bool) = ['literal1', 'literal2']\n}");
    let diag_literal = engine.analyze(&doc_literal, None, &HashMap::new(), &HashSet::new());
    eprintln!(
        "literal list(bool): {:?}",
        diag_literal.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_map_type_warning_for_cross_file_ref() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    use std::collections::HashMap;

    // Schema: registry_prod.account_id is String — incompatible with map(bool)
    let schema = ResourceSchema::new("awscc.organizations.account")
        .attribute(AttributeSchema::new("account_id", AttributeType::String));
    let schemas: HashMap<String, ResourceSchema> = vec![(schema.resource_type.clone(), schema)]
        .into_iter()
        .collect();
    let engine = custom_engine(schemas);

    let doc = create_document(
        r#"exports {
  accounts: map(bool) = {
    prod = registry_prod.account_id
    dev  = registry_dev.account_id
  }
}"#,
    );

    let mut sibling_bindings = HashMap::new();
    sibling_bindings.insert(
        "registry_prod".to_string(),
        "awscc.organizations.account".to_string(),
    );
    sibling_bindings.insert(
        "registry_dev".to_string(),
        "awscc.organizations.account".to_string(),
    );

    let diagnostics = engine.analyze(&doc, None, &sibling_bindings, &HashSet::new());

    let type_warning = diagnostics
        .iter()
        .find(|d| d.message.contains("type mismatch") || d.message.contains("expected"));
    assert!(
        type_warning.is_some(),
        "Should warn about map(bool) vs String account_id. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn no_undefined_resource_for_sibling_binding_in_exports() {
    let engine = DiagnosticEngine::new(
        Arc::new(HashMap::new()),
        vec!["awscc".to_string()],
        Arc::new(vec![]),
    );
    let doc = create_document(
        r#"exports {
  accounts: map(aws_account_id) = {
    prod = registry_prod.account_id
    dev = registry_dev.account_id
  }
}
"#,
    );

    // registry_prod and registry_dev are defined in sibling files
    let mut sibling_bindings = HashMap::new();
    sibling_bindings.insert(
        "registry_prod".to_string(),
        "awscc.organizations.account".to_string(),
    );
    sibling_bindings.insert(
        "registry_dev".to_string(),
        "awscc.organizations.account".to_string(),
    );

    let diagnostics = engine.analyze(&doc, None, &sibling_bindings, &HashSet::new());

    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource"));
    assert!(
        undefined.is_none(),
        "Sibling binding refs should not be flagged as undefined. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_missing_source_directory_is_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();

    let engine = test_engine();
    let doc = create_document(
        r#"let orgs = upstream_state {
    source = '../nonexistent'
}
"#,
    );

    let diagnostics = engine.analyze(&doc, Some(&base), &HashMap::new(), &HashSet::new());

    let diag = diagnostics
        .iter()
        .find(|d| d.message.contains("upstream_state 'orgs'"));
    assert!(
        diag.is_some(),
        "Expected a diagnostic for missing upstream_state source. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diag = diag.unwrap();
    assert!(
        diag.message.contains("../nonexistent") && diag.message.contains("does not exist"),
        "unexpected message: {}",
        diag.message
    );
    // Range should point at the source value on line 1 (0-based).
    assert_eq!(
        diag.range.start.line, 1,
        "diagnostic should point at the `source = ...` line"
    );
}

#[test]
fn upstream_state_source_check_ignores_same_source_in_provider_block() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();

    let engine = test_engine();
    // A `provider` block with the same `source = '...'` string as the
    // `upstream_state` block. The diagnostic must land on the upstream_state
    // line (line 5 here, 0-based 4), not the provider line.
    let doc = create_document(
        r#"provider aws {
    source = '../nonexistent'
    region = 'ap-northeast-1'
}
let orgs = upstream_state {
    source = '../nonexistent'
}
"#,
    );

    let diagnostics = engine.analyze(&doc, Some(&base), &HashMap::new(), &HashSet::new());

    let diag = diagnostics
        .iter()
        .find(|d| d.message.contains("upstream_state 'orgs'"));
    assert!(
        diag.is_some(),
        "expected diagnostic for upstream_state source. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(
        diag.unwrap().range.start.line,
        5,
        "diagnostic must point at the upstream_state source line, not the provider's"
    );
}

#[test]
fn upstream_state_existing_source_directory_is_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("project");
    std::fs::create_dir(&base).unwrap();
    std::fs::create_dir(tmp.path().join("upstream")).unwrap();

    let engine = test_engine();
    let doc = create_document(
        r#"let orgs = upstream_state {
    source = '../upstream'
}
"#,
    );

    let diagnostics = engine.analyze(&doc, Some(&base), &HashMap::new(), &HashSet::new());

    let diag = diagnostics
        .iter()
        .find(|d| d.message.contains("upstream_state"));
    assert!(
        diag.is_none(),
        "Existing source should not trigger diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// =====================================================================
// upstream_state field-reference diagnostics (#1990)
// =====================================================================

fn set_up_project_with_upstream(
    main_crn: &str,
    upstream_exports_crn: Option<&str>,
) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    if let Some(body) = upstream_exports_crn {
        std::fs::write(upstream.join("exports.crn"), body).unwrap();
    }
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(base.join("main.crn"), main_crn).unwrap();
    (tmp, base, "main.crn".to_string())
}

fn analyze_with_buffer(
    engine: &DiagnosticEngine,
    base: &std::path::Path,
    filename: &str,
    buffer: &str,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let doc = create_document(buffer);
    engine.analyze_with_filename(
        &doc,
        Some(filename),
        Some(base),
        &HashMap::new(),
        &HashSet::new(),
    )
}

#[test]
fn upstream_state_unknown_field_in_for_expression_is_flagged() {
    // The issue's canonical repro.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name, _ in orgs.account {
    awscc.ec2.vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: string = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("does not export `account`")),
        "got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_known_field_passes() {
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name, _ in orgs.accounts {
    awscc.ec2.vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: string = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("does not export")),
        "known field should not be flagged, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_buffer_differs_from_disk_uses_buffer() {
    // Disk: `orgs.accounts` (correct). Buffer in editor: `orgs.acc` (typo).
    // Must flag based on the buffer, not disk.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.accounts
"#,
        Some(
            r#"exports { accounts: string = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let edited = r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.acc
"#;
    let diagnostics = analyze_with_buffer(&engine, &base, &name, edited);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("does not export `acc`")),
        "buffer typo must be flagged against disk exports, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_buffer_fix_clears_diagnostic() {
    // Disk: `orgs.account` (typo, would be flagged). Buffer: `orgs.accounts`
    // (user fixed it). Must NOT flag — reflects the buffer, not disk.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.account
"#,
        Some(
            r#"exports { accounts: string = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let fixed = r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.accounts
"#;
    let diagnostics = analyze_with_buffer(&engine, &base, &name, fixed);

    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("does not export")),
        "fixed buffer should clear diagnostic even if disk is stale, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_buffer_retypo_reflags() {
    // Reproduces the user-reported sequence:
    //   disk typo → fix in buffer → retype typo in buffer.
    // Diagnostic must reappear on the second typo.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.accounts
"#,
        Some(
            r#"exports { accounts: string = "x" }
"#,
        ),
    );

    let engine = test_engine();

    // Simulate user editing to a new typo.
    let retypo = r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.acc
"#;
    let diagnostics = analyze_with_buffer(&engine, &base, &name, retypo);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("does not export `acc`")),
        "retyped typo must reflag, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_cross_file_declaration_is_checked() {
    // `let orgs = upstream_state { ... }` lives in `backend.crn`; the
    // reference lives in `main.crn`. Must still flag.
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(
        upstream.join("exports.crn"),
        r#"exports { accounts: string = "x" }
"#,
    )
    .unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(
        base.join("backend.crn"),
        r#"let orgs = upstream_state { source = '../organizations' }
"#,
    )
    .unwrap();
    let main_src = r#"for name, _ in orgs.account {
    awscc.ec2.vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#;
    std::fs::write(base.join("main.crn"), main_src).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", main_src);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("does not export `account`")),
        "cross-file upstream_state ref must be flagged, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_duplicate_bad_refs_anchor_to_distinct_sites() {
    // Two for-iterables both misspell the same field. Each diagnostic
    // must anchor to its own source site, not stack on the first.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state {
    source = '../organizations'
}

for name, _ in orgs.bad {
    awscc.ec2.vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}

for other, _ in orgs.bad {
    awscc.ec2.subnet {
        name = other
        cidr_block = '10.0.1.0/24'
    }
}
"#,
        Some(
            r#"exports { accounts: string = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    let bad_ref_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("does not export `bad`"))
        .collect();
    assert_eq!(
        bad_ref_diags.len(),
        2,
        "expected 2 diagnostics for 2 occurrences, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let lines: std::collections::HashSet<u32> =
        bad_ref_diags.iter().map(|d| d.range.start.line).collect();
    assert_eq!(
        lines.len(),
        2,
        "diagnostics should anchor to distinct lines, got lines: {:?}",
        bad_ref_diags
            .iter()
            .map(|d| d.range.start.line)
            .collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_single_line_block_source_diagnostic() {
    // `let orgs = upstream_state { source = '../x' }` on one line.
    // `find_source_value_position` must still locate the source value.
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(upstream.join("main.crn"), "not valid crn {{{").unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    let src = "let orgs = upstream_state { source = '../organizations' }\n";
    std::fs::write(base.join("main.crn"), src).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", src);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("failed to parse source")),
        "single-line upstream_state block must yield resolve-error diagnostic, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_broken_upstream_surfaces_resolve_error() {
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(upstream.join("main.crn"), "not valid crn {{{").unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    let src = r#"let orgs = upstream_state {
    source = '../organizations'
}
"#;
    std::fs::write(base.join("main.crn"), src).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", src);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("failed to parse source")),
        "broken upstream must produce resolve-error diagnostic, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}
