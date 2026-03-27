use super::*;

#[test]
fn unknown_field_in_struct_block() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = "tcp"
    unknown_field = "bad"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown_field_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown field 'unknown_field'"));
    assert!(
        unknown_field_diag.is_some(),
        "Should warn about unknown field in struct block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn type_mismatch_in_struct_field() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = "tcp"
    from_port = "not_a_number"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_mismatch = diagnostics.iter().find(|d| {
        (d.message.contains("Type mismatch") && d.message.contains("Int"))
            || d.message.contains("Expected integer")
    });
    assert!(
        type_mismatch.is_some(),
        "Should warn about type mismatch for Int field. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn resource_ref_type_mismatch() {
    let engine = test_engine();
    // vpc.vpc_id is AwsResourceId, but ipv4_ipam_pool_id expects IpamPoolId
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
cidr_block = "10.0.0.0/16"
}

let vpc2 = awscc.ec2.vpc {
ipv4_ipam_pool_id = vpc.vpc_id
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("IpamPoolId"));
    assert!(
        type_mismatch.is_some(),
        "Should warn about type mismatch for ResourceRef. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn resource_ref_compatible_type() {
    let engine = test_engine();
    // ipam_pool.ipam_pool_id is IpamPoolId, and ipv4_ipam_pool_id expects IpamPoolId -> OK
    // Using vpc.vpc_id in a vpc_id field (same type) should not produce a warning
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
cidr_block = "10.0.0.0/16"
}

let subnet = awscc.ec2.subnet {
vpc_id = vpc.vpc_id
cidr_block = "10.0.1.0/24"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("AwsResourceId"));
    assert!(
        type_mismatch.is_none(),
        "Should NOT warn about compatible ResourceRef types. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn unknown_field_in_second_repeated_block() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = "tcp"
    from_port = 80
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}
security_group_ingress {
    ip_protocol = "tcp"
    from_port = 443
    to_port = 443
    cidr_ip = "0.0.0.0/0"
    bad_field = "oops"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let bad_field_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown field 'bad_field'"));
    assert!(
        bad_field_diag.is_some(),
        "Should warn about unknown field in second repeated block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    // The diagnostic should point to the second block, not the first.
    // LSP uses 0-indexed lines, so line 17 = line 18 in 1-indexed.
    let diag = bad_field_diag.unwrap();
    assert_eq!(
        diag.range.start.line, 17,
        "Diagnostic should point to line 17 (0-indexed, in second block), got line {}",
        diag.range.start.line
    );
}

#[test]
fn block_syntax_rejected_for_bare_struct() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider aws {
region = aws.Region.ap_northeast_1
}

aws.ec2.subnet {
name = "my-subnet"
vpc_id = "vpc-123"
cidr_block = "10.0.1.0/24"

private_dns_name_options_on_launch {
    hostname_type = aws.ec2.subnet.HostnameType.resource_name
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let block_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("cannot use block syntax"));
    assert!(
        block_diag.is_some(),
        "Should error on block syntax for bare Struct. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    let diag = block_diag.unwrap();
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diag.message
            .contains("use map assignment: private_dns_name_options_on_launch = { ... }")
    );
}

#[test]
fn block_syntax_rejected_for_bare_struct_multiple_blocks() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider aws {
region = aws.Region.ap_northeast_1
}

aws.ec2.subnet {
name = "my-subnet"
vpc_id = "vpc-123"
cidr_block = "10.0.1.0/24"

private_dns_name_options_on_launch {
    hostname_type = aws.ec2.subnet.HostnameType.resource_name
}

private_dns_name_options_on_launch {
    hostname_type = aws.ec2.subnet.HostnameType.ip_name
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    // Both blocks should get errors
    let block_count = diagnostics
        .iter()
        .filter(|d| d.message.contains("cannot use block syntax"))
        .count();
    assert_eq!(
        block_count,
        2,
        "Should have 2 block syntax diagnostics (one per block), got {}. All diagnostics: {:?}",
        block_count,
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn lint_list_literal_for_list_struct() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
security_group_ingress = [{
    ip_protocol = "tcp"
    from_port = 80
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}]
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let lint_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Prefer block syntax"));
    assert!(
        lint_diag.is_some(),
        "Should emit HINT for list literal syntax on List<Struct>. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    let diag = lint_diag.unwrap();
    assert_eq!(diag.severity, Some(DiagnosticSeverity::HINT));
    assert!(diag.message.contains("security_group_ingress"));
}

#[test]
fn lint_block_syntax_no_warning() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = "tcp"
    from_port = 80
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let lint_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Prefer block syntax"));
    assert!(
        lint_diag.is_none(),
        "Block syntax should NOT produce lint warning. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn lint_string_attr_no_warning() {
    let engine = test_engine();
    // group_description is a String attribute — lint should not flag it
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
group_description = "Test security group"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let lint_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Prefer block syntax"));
    assert!(
        lint_diag.is_none(),
        "String attributes should NOT produce lint warning. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn data_source_without_read_keyword_errors() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider aws {
region = aws.Region.ap_northeast_1
}

aws.sts.caller_identity {}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let data_source_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("data source") && d.message.contains("read"));
    assert!(
        data_source_diag.is_some(),
        "Should error when data source is used without `read`. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn data_source_with_read_keyword_no_error() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider aws {
region = aws.Region.ap_northeast_1
}

let identity = read aws.sts.caller_identity {}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let data_source_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("data source") && d.message.contains("read"));
    assert!(
        data_source_diag.is_none(),
        "Should NOT error when data source is used with `read`. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn regular_resource_without_read_no_data_source_error() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider aws {
region = aws.Region.ap_northeast_1
}

let bucket = aws.s3.bucket {
name = "my-bucket"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let data_source_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("data source"));
    assert!(
        data_source_diag.is_none(),
        "Regular resource should NOT trigger data source error. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn detect_provider_aws_resource_independent_of_factory_order() {
    let doc = create_document(
        r#"provider aws {
region = aws.Region.ap_northeast_1
}

let bucket = aws.s3.bucket {
name = "my-bucket"
}"#,
    );

    let engine = test_engine();
    let engine_rev = test_engine_reversed();

    let diags_normal = engine.analyze(&doc, None);
    let diags_reversed = engine_rev.analyze(&doc, None);

    let messages_normal: Vec<_> = diags_normal.iter().map(|d| &d.message).collect();
    let messages_reversed: Vec<_> = diags_reversed.iter().map(|d| &d.message).collect();

    assert_eq!(
        messages_normal, messages_reversed,
        "aws.s3.bucket diagnostics should not depend on factory order.\n\
         Normal: {:?}\n\
         Reversed: {:?}",
        messages_normal, messages_reversed
    );
}

#[test]
fn detect_provider_awscc_resource_independent_of_factory_order() {
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
cidr_block = "10.0.0.0/16"
}"#,
    );

    let engine = test_engine();
    let engine_rev = test_engine_reversed();

    let diags_normal = engine.analyze(&doc, None);
    let diags_reversed = engine_rev.analyze(&doc, None);

    let messages_normal: Vec<_> = diags_normal.iter().map(|d| &d.message).collect();
    let messages_reversed: Vec<_> = diags_reversed.iter().map(|d| &d.message).collect();

    assert_eq!(
        messages_normal, messages_reversed,
        "awscc.ec2.vpc diagnostics should not depend on factory order.\n\
         Normal: {:?}\n\
         Reversed: {:?}",
        messages_normal, messages_reversed
    );
}

#[test]
fn detect_provider_anonymous_resource_independent_of_factory_order() {
    // Anonymous resource (no let binding) — verify detection works the same
    // regardless of factory order
    let engine = test_engine();
    let engine_rev = test_engine_reversed();

    let doc = create_document(
        r#"provider aws {
region = aws.Region.ap_northeast_1
}

aws.s3.bucket {
name = "test-bucket"
}"#,
    );

    let diags_normal = engine.analyze(&doc, None);
    let diags_reversed = engine_rev.analyze(&doc, None);

    let messages_normal: Vec<_> = diags_normal.iter().map(|d| &d.message).collect();
    let messages_reversed: Vec<_> = diags_reversed.iter().map(|d| &d.message).collect();

    assert_eq!(
        messages_normal, messages_reversed,
        "Diagnostics should be identical regardless of factory order.\n\
         Normal: {:?}\n\
         Reversed: {:?}",
        messages_normal, messages_reversed
    );
}

#[test]
fn duplicate_attribute_warning() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let igw_attachment = awscc.ec2.vpc_gateway_attachment {
    vpc_id              = vpc.vpc_id
    internet_gateway_id = igw.internet_gateway_id
    internet_gateway_id = igw.internet_gateway_id
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let dup_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("Duplicate attribute 'internet_gateway_id'")
    });
    assert!(
        dup_diag.is_some(),
        "Should warn about duplicate attribute. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    let diag = dup_diag.unwrap();
    // Duplicate is on line 7 (0-indexed)
    assert_eq!(diag.range.start.line, 7);
    assert_eq!(
        diag.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)
    );
}

#[test]
fn no_duplicate_warning_for_unique_attrs() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let dup_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Duplicate attribute"));
    assert!(
        dup_diag.is_none(),
        "Should not warn about duplicates when there are none. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}
