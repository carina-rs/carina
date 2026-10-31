use super::*;

#[test]
#[should_panic(expected = "document source path must match the analyzed path")]
fn analyze_rejects_a_mismatched_document_source_path() {
    let fixture = tempfile::tempdir().expect("fixture");
    let doc = create_document("provider mock {}\n");
    let _ = test_engine().analyze(&doc, Some(&fixture.path().join("main.crn")));
}

#[test]
fn provider_instance_routing_is_reported_once_for_multifile_directory() {
    let fixture = tempfile::tempdir().expect("provider-routing fixture");
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
        .expect("provider declaration");
    let source = r#"mock.test.resource {
  name = "example"
  directives { provider = typo }
}
"#;
    let main_path = fixture.path().join("main.crn");
    std::fs::write(&main_path, source).expect("routed resource");

    let diagnostics = test_engine().analyze(
        &create_document_at(source, main_path.clone()),
        Some(&main_path),
    );
    let routing_error = "`typo` is not a known provider instance";
    let occurrences = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains(routing_error))
        .count();

    assert_eq!(
        occurrences, 1,
        "LSP must report the routing error exactly once: {diagnostics:#?}",
    );
}

#[test]
fn provider_config_enum_accepts_identity_or_configured_provider_namespace_only() {
    for (provider_name, raw) in [
        ("awscc", "awscc.Region.ap_northeast_1"),
        ("awscc", "aws.Region.ap_northeast_1"),
    ] {
        let engine = test_engine_with_region_config_provider(provider_name);
        let doc = create_document(&format!(
            "provider {provider_name} {{\n  region = {raw}\n}}\n"
        ));
        let diagnostics = engine.analyze(&doc, None);
        assert!(
            diagnostics.is_empty(),
            "{raw} must be accepted in provider {provider_name}; got: {:?}",
            diagnostics
                .iter()
                .map(|diagnostic| &diagnostic.message)
                .collect::<Vec<_>>()
        );
    }

    for (provider_name, raw) in [
        ("awscc", "gcp.Region.ap_northeast_1"),
        ("aws", "awscc.Region.ap_northeast_1"),
    ] {
        let engine = test_engine_with_region_config_provider(provider_name);
        let doc = create_document(&format!(
            "provider {provider_name} {{\n  region = {raw}\n}}\n"
        ));
        let diagnostics = engine.analyze(&doc, None);
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .message
                    .contains(&format!("provider {provider_name}: region:"))
                    && diagnostic.message.contains(raw)
            }),
            "{raw} must be rejected in provider {provider_name}; got: {:?}",
            diagnostics
                .iter()
                .map(|diagnostic| &diagnostic.message)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn reserved_resource_attribute_surfaces_parser_diagnostic() {
    let engine = test_engine();
    let doc = create_document("let bucket = aws.s3.Bucket {\n  \"_x\" = 1\n}\n");

    let diagnostics = engine.analyze(&doc, None);
    let expected_message =
        "Reserved attribute name '_x': names starting with '_' are reserved for Carina";
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message == expected_message)
        .unwrap_or_else(|| {
            panic!(
                "reserved resource attribute must surface as an LSP diagnostic, got: {:?}",
                diagnostics
                    .iter()
                    .map(|diagnostic| &diagnostic.message)
                    .collect::<Vec<_>>()
            )
        });

    assert_eq!(diagnostic.range.start.line, 1);
    assert_eq!(diagnostic.range.start.character, 2);
    assert_eq!(diagnostic.range.end.line, 1);
    assert_eq!(diagnostic.range.end.character, 6);
}

#[test]
fn deferred_for_unsupported_bodies_surface_parser_diagnostics() {
    for (body, body_name) in [
        ("child { item = x }", "child"),
        ("read mock.iam.Roles { name_regex = x }", "mock.iam.Roles"),
    ] {
        let fixture = tempfile::tempdir().expect("fixture");
        std::fs::create_dir(fixture.path().join("child")).expect("child module directory");
        std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
            .expect("providers fixture");
        std::fs::write(
            fixture.path().join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .expect("backend fixture");
        std::fs::write(
            fixture.path().join("child/main.crn"),
            "arguments { item: String }\nmock.test.resource { name = item }\n",
        )
        .expect("child module fixture");
        let source = format!(
            "let child = use {{ source = './child' }}\n\
             let src = mock.test.resource {{ name = 'src' }}\n\
             let items = for x in src.list {{ {body} }}\n"
        );
        let main_path = fixture.path().join("main.crn");
        std::fs::write(&main_path, &source).expect("main fixture");

        let diagnostics = test_engine().analyze(
            &create_document_at(&source, main_path.clone()),
            Some(&main_path),
        );
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.message.contains("for x in src.list")
                    && diagnostic.message.contains(body_name)
            })
            .unwrap_or_else(|| panic!("missing deferred-for diagnostic: {diagnostics:#?}"));

        assert_eq!(diagnostic.range.start.line, 2);
        let loop_line = source.lines().nth(2).expect("loop source line");
        assert_eq!(
            diagnostic.range.start.character,
            loop_line.find("for").expect("for column") as u32,
            "diagnostic must start at the authored for-expression: {diagnostic:#?}"
        );
        assert_eq!(
            diagnostic.range.end.character,
            loop_line.chars().count() as u32,
            "diagnostic must end at the authored for-expression: {diagnostic:#?}"
        );
        assert!(diagnostic.message.contains("managed resource"));
        assert!(
            diagnostic
                .message
                .contains("still unresolved after module expansion"),
            "{}",
            diagnostic.message
        );
    }
}

#[test]
#[ignore = "requires provider schemas"]
fn unknown_field_in_struct_block() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.SecurityGroup {
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
#[ignore = "requires provider schemas"]
fn type_mismatch_in_struct_field() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.SecurityGroup {
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
#[ignore = "requires provider schemas"]
fn resource_ref_type_mismatch() {
    let engine = test_engine();
    // vpc.vpc_id is AwsResourceId, but ipv4_ipam_pool_id expects IpamPoolId
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
cidr_block = "10.0.0.0/16"
}

let vpc2 = awscc.ec2.Vpc {
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

let vpc = awscc.ec2.Vpc {
cidr_block = "10.0.0.0/16"
}

let subnet = awscc.ec2.Subnet {
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
#[ignore = "requires provider schemas"]
fn unknown_field_in_second_repeated_block() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.SecurityGroup {
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
#[ignore = "requires provider schemas"]
fn block_syntax_rejected_for_bare_struct() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider aws {
region = aws.Region.ap_northeast_1
}

aws.ec2.Subnet {
name = "my-subnet"
vpc_id = "vpc-123"
cidr_block = "10.0.1.0/24"

private_dns_name_options_on_launch {
    hostname_type = aws.ec2.Subnet.HostnameType.resource_name
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
#[ignore = "requires provider schemas"]
fn block_syntax_rejected_for_bare_struct_multiple_blocks() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider aws {
region = aws.Region.ap_northeast_1
}

aws.ec2.Subnet {
name = "my-subnet"
vpc_id = "vpc-123"
cidr_block = "10.0.1.0/24"

private_dns_name_options_on_launch {
    hostname_type = aws.ec2.Subnet.HostnameType.resource_name
}

private_dns_name_options_on_launch {
    hostname_type = aws.ec2.Subnet.HostnameType.ip_name
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
#[ignore = "requires provider schemas"]
fn lint_list_literal_for_list_struct() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.SecurityGroup {
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

let sg = awscc.ec2.SecurityGroup {
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

let sg = awscc.ec2.SecurityGroup {
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
#[ignore = "requires provider schemas"]
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

let bucket = aws.s3.Bucket {
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

let bucket = aws.s3.Bucket {
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
        "aws.s3.Bucket diagnostics should not depend on factory order.\n\
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

let vpc = awscc.ec2.Vpc {
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
        "awscc.ec2.Vpc diagnostics should not depend on factory order.\n\
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

aws.s3.Bucket {
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
fn duplicate_attribute_warning_anchors_inline_map_key() {
    let engine = test_engine();
    let doc = create_document(
        r#"let thing = mock.test.Thing {
    tags = { Name = "first", Name = "second" }
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("Duplicate attribute 'Name'"))
        .unwrap_or_else(|| panic!("missing inline duplicate diagnostic: {diagnostics:#?}"));

    assert_eq!(diagnostic.range.start.line, 1);
    assert_eq!(diagnostic.range.start.character, 29);
    assert_eq!(diagnostic.range.end.line, 1);
    assert_eq!(diagnostic.range.end.character, 33);
}

#[test]
fn duplicate_attribute_warning_uses_quoted_key_span() {
    let engine = test_engine();
    let doc = create_document(
        r#"let thing = mock.test.Thing {
    tags = { "a.b" = 1, "a.b" = 2 }
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("Duplicate attribute 'a.b'"))
        .unwrap_or_else(|| panic!("missing quoted-key diagnostic: {diagnostics:#?}"));

    assert_eq!(diagnostic.range.start.line, 1);
    assert_eq!(diagnostic.range.start.character, 24);
    assert_eq!(diagnostic.range.end.line, 1);
    assert_eq!(diagnostic.range.end.character, 29);
}

#[test]
fn duplicate_attribute_warning_uses_original_line_after_heredoc() {
    let engine = test_engine();
    let doc = create_document(
        r#"let thing = mock.test.Thing {
    policy = <<EOT
hello
EOT
    a = 1
    a = 2
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("Duplicate attribute 'a'"))
        .unwrap_or_else(|| panic!("missing heredoc duplicate diagnostic: {diagnostics:#?}"));

    assert_eq!(diagnostic.range.start.line, 5);
    assert_eq!(diagnostic.range.start.character, 4);
    assert_eq!(diagnostic.range.end.line, 5);
    assert_eq!(diagnostic.range.end.character, 5);
}

#[test]
fn no_duplicate_warning_for_same_name_in_nested_map() {
    let engine = test_engine();
    let doc = create_document(
        r#"let policy = awscc.cloudfront.ResponseHeadersPolicy {
    response_headers_policy_config = {
        security_headers_config = {
            content_security_policy = {
                content_security_policy = "default-src 'self'"
                override = true
            }
        }
    }
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let duplicate_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("Duplicate attribute"))
        .collect();
    assert!(
        duplicate_diagnostics.is_empty(),
        "Nested map key must not conflict with its containing attribute. Got: {duplicate_diagnostics:?}"
    );
}

#[test]
fn no_duplicate_warning_for_unique_attrs() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
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

#[test]
fn carina_diagnostic_helper_produces_correct_fields() {
    use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

    let diag = carina_diagnostic(
        5,
        10,
        20,
        DiagnosticSeverity::ERROR,
        "test error message".to_string(),
    );

    assert_eq!(
        diag.range,
        Range {
            start: Position {
                line: 5,
                character: 10,
            },
            end: Position {
                line: 5,
                character: 20,
            },
        }
    );
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diag.source, Some("carina".to_string()));
    assert_eq!(diag.message, "test error message");
    // Verify default fields are not set
    assert!(diag.code.is_none());
    assert!(diag.code_description.is_none());
    assert!(diag.tags.is_none());
    assert!(diag.related_information.is_none());
    assert!(diag.data.is_none());
}

#[test]
fn carina_diagnostic_helper_with_multiline_range() {
    use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

    let diag = carina_diagnostic_range(
        Range {
            start: Position {
                line: 3,
                character: 0,
            },
            end: Position {
                line: 4,
                character: 0,
            },
        },
        DiagnosticSeverity::ERROR,
        "resource-level error".to_string(),
    );

    assert_eq!(diag.range.start.line, 3);
    assert_eq!(diag.range.end.line, 4);
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diag.source, Some("carina".to_string()));
}

#[test]
fn unused_for_binding_emits_lsp_diagnostic() {
    // A for-loop binding never referenced in the body should surface as an
    // LSP diagnostic — not only a CLI warning — so editor users see it too.
    let engine = test_engine();
    let doc = create_document(
        r#"let things = { a = 1, b = 2 }

for name, account_id in things {
  awscc.ec2.Vpc {
    cidr_block = account_id
  }
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unused = diagnostics
        .iter()
        .find(|d| d.message.contains("for-loop binding 'name'"));
    assert!(
        unused.is_some(),
        "expected LSP diagnostic for unused for-loop binding 'name', got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let d = unused.unwrap();
    assert_eq!(d.source, Some("carina".to_string()));
    // Diagnostic should sit on the `for` line (1-indexed line 3 in source = 0-indexed 2)
    assert_eq!(d.range.start.line, 2);
}

#[test]
fn unused_for_bindings_on_different_lines_emit_distinct_lsp_diagnostics() {
    let engine = test_engine();
    let doc = create_document(
        r#"for x in [1, 2] {
  awscc.ec2.Vpc { cidr_block = "10.0.0.0/16" }
}

for x in [3, 4] {
  awscc.ec2.Vpc { cidr_block = "10.0.1.0/24" }
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let unused: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("for-loop binding 'x' is unused")
        })
        .collect();

    assert_eq!(
        unused.len(),
        2,
        "each authored loop must emit its own LSP diagnostic: {diagnostics:#?}"
    );
    assert_eq!(
        unused
            .iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>(),
        vec![0, 4]
    );
}

#[test]
fn used_for_binding_no_lsp_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"let things = { a = 1, b = 2 }

for key, value in things {
  awscc.ec2.Vpc {
    name = key
    cidr_block = value
  }
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("for-loop binding")),
        "used for-loop bindings should not produce diagnostics, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_warning_not_in_lsp() {
    // upstream_state "validate does not inspect" is a CLI-time informational
    // note, not an editor-surface problem. Do NOT emit as LSP diagnostic.
    let engine = test_engine();
    let doc = create_document(
        r#"let network = upstream_state {
  source = "../network"
}

awscc.ec2.SecurityGroup {
  group_description = "Web SG"
  vpc_id = network.vpc_id
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("validate does not inspect")),
        "upstream_state informational warning must stay CLI-only, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// #2309 — find_attribute_value_range coverage
// ---------------------------------------------------------------------------

#[test]
fn value_range_covers_dotted_identifier() {
    let engine = super::test_engine();
    let doc = super::create_document(
        "let _ = aws.s3.Bucket {\n  versioning = aws.s3.VersioningStatus.NotReal\n}\n",
    );
    let range = engine
        .find_attribute_value_range(&doc, "versioning", None)
        .expect("range located");
    assert_eq!(range.start.line, 1);
    // Compute expected columns from the line text directly so the
    // assertion stays robust to incidental indentation tweaks.
    let text = doc.text();
    let line = text.lines().nth(1).unwrap();
    let value_start = line.find("aws.s3.VersioningStatus.NotReal").unwrap() as u32;
    let value_end = value_start + "aws.s3.VersioningStatus.NotReal".len() as u32;
    assert_eq!(range.start.character, value_start);
    assert_eq!(range.end.character, value_end);
}

#[test]
fn value_range_includes_quotes_for_string_literal() {
    // Code action for `StringLiteralExpectedEnum` replaces the quoted
    // literal *with* its surrounding quotes — applying the fix drops
    // the quotes and writes the bare identifier.
    let engine = super::test_engine();
    let doc =
        super::create_document("awscc.sso.Assignment {\n  target_type = \"AWS_ACCOUNT\"\n}\n");
    let range = engine
        .find_attribute_value_range(&doc, "target_type", None)
        .expect("range located");
    let text = doc.text();
    let line = text.lines().nth(1).unwrap();
    let value_start = line.find('"').unwrap() as u32;
    let value_end = (line.rfind('"').unwrap() + 1) as u32;
    assert_eq!(range.start.line, 1);
    assert_eq!(range.start.character, value_start);
    assert_eq!(range.end.character, value_end);
}

#[test]
fn value_range_returns_none_for_block_syntax_value() {
    // `attr = { ... }` has no single offending identifier — the code
    // action declines to fix it (no range, hence no quick-fix).
    let engine = super::test_engine();
    let doc = super::create_document("aws.s3.Bucket {\n  tags = { Env = \"prod\" }\n}\n");
    assert!(
        engine
            .find_attribute_value_range(&doc, "tags", None)
            .is_none()
    );
}

#[test]
fn value_range_uses_character_offsets_for_multibyte_literal() {
    // LSP `Position.character` is a UTF-16/codepoint offset, not a byte
    // offset. Quoted-literal values that contain multi-byte chars
    // (e.g. `"あ"`) must still produce a `Range` whose `end.character`
    // equals the codepoint count, not the UTF-8 byte length — otherwise
    // an editor applying the quick-fix would corrupt surrounding text.
    let engine = super::test_engine();
    let doc = super::create_document("aws.s3.Bucket {\n  versioning = \"あ\"\n}\n");
    let range = engine
        .find_attribute_value_range(&doc, "versioning", None)
        .expect("range located");
    let text = doc.text();
    let line = text.lines().nth(1).unwrap();
    // The literal `"あ"` is 3 codepoints: `"`, `あ`, `"`.
    let value_start_chars = line.chars().take_while(|c| *c != '"').count() as u32;
    assert_eq!(range.start.line, 1);
    assert_eq!(range.start.character, value_start_chars);
    assert_eq!(range.end.character, value_start_chars + 3);
}

#[test]
fn enum_with_non_string_value_still_emits_diagnostic() {
    // Regression: when #2309 routed Enum through
    // `build_enum_diagnostic`, the fall-back match arm for
    // Enum was removed. That hid `TypeError::TypeMismatch`
    // diagnostics when the user typed a non-string value (e.g. an
    // integer) against an enum-typed attribute. The fall-through
    // generic-message arm restored it; this test pins the behavior.
    let engine = super::test_engine_with_enum_attr();
    let doc = super::create_document("test.r.mode_holder {\n  name = \"a\"\n  mode = 42\n}\n");
    let diagnostics = engine.analyze(&doc, None);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.to_ascii_lowercase().contains("type mismatch")
                || d.message.to_ascii_lowercase().contains("expected")),
        "non-string value against Enum must still produce a diagnostic, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn value_range_locates_value_separated_by_tabs() {
    // Round 4 widened the separator handling in `find_attribute_value_range`
    // from spaces-only to spaces *or* tabs so a hand-formatted buffer
    // (or one mid-edit, before the formatter has run) still surfaces
    // the quick-fix. Pin the behavior so a future tightening can't
    // silently regress it.
    let engine = super::test_engine();
    let doc = super::create_document(
        "let _ = aws.s3.Bucket {\n\tversioning\t=\taws.s3.VersioningStatus.NotReal\n}\n",
    );
    let range = engine
        .find_attribute_value_range(&doc, "versioning", None)
        .expect("range located even with tab separators");
    assert_eq!(range.start.line, 1);
    let text = doc.text();
    let line = text.lines().nth(1).unwrap();
    let value_start = line.find("aws.s3.VersioningStatus.NotReal").unwrap() as u32;
    let value_end = value_start + "aws.s3.VersioningStatus.NotReal".len() as u32;
    assert_eq!(range.start.character, value_start);
    assert_eq!(range.end.character, value_end);
}

/// Issue #2623 regression: the closed-set string types from #2611
/// (`'dev' | 'prod'`) must not surface a parser-level "expected
/// type_expr" diagnostic in the LSP. The grammar accepts the shape;
/// LSP and CLI share that grammar, so this test pins the no-error
/// behavior so a future grammar refactor can't silently break it.
#[test]
fn arguments_literal_union_type_does_not_surface_parser_error() {
    let engine = test_engine();
    let doc = create_document(
        "arguments {\n  oidc_provider_arn: IamOidcProviderArn\n  environment      : 'dev' | 'prod'\n}\n",
    );
    let diagnostics = engine.analyze(&doc, None);
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("expected type_expr")),
        "literal-union argument type must not produce `expected type_expr`; got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// carina#3239 LSP parity: when a project has loaded provider schemas,
/// an unknown bare custom-type
/// name in an `arguments { ... }` declaration must surface as an
/// editor diagnostic — the same finding the CLI's `carina validate`
/// produces. Without this coverage the bug-headline case
/// (`IamOidcProviderArn` left over from a provider rename) shows zero
/// diagnostics in the editor and only fails at validate time, breaking
/// the [[feedback_validate_lsp_parity]] contract.
///
/// The test path runs `analyze(&doc, None)` which short-circuits the
/// directory-merged parse and forces the buffer-only fallback in the
/// LSP walker wiring — the load-bearing path Round 5 review caught.
#[test]
fn arguments_unknown_custom_type_surfaces_lsp_diagnostic() {
    let engine = test_engine_with_wait_target();
    let doc = create_document("arguments {\n  oidc_provider_arn: IamOidcProviderArn\n}\n");
    let diagnostics = engine.analyze(&doc, None);
    assert!(
        diagnostics.iter().any(|d| {
            d.message.contains("unknown custom type") && d.message.contains("IamOidcProviderArn")
        }),
        "LSP must surface the unknown bare custom-type name when at \
         least one provider is registered; got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn arguments_unknown_dotted_custom_type_surfaces_lsp_diagnostic_without_misleading_suggestion() {
    let engine = test_engine_with_iam_policy_arn_custom_type();
    let doc = create_document("arguments {\n  fake: aws.iam.TotallyFake.Arn\n}\n");
    let diagnostics = engine.analyze(&doc, None);
    let messages = diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>();

    assert!(
        diagnostics.iter().any(|d| {
            d.message.contains("unknown custom type")
                && d.message.contains("aws.iam.TotallyFake.Arn")
        }),
        "LSP must reject unregistered dotted custom types; got: {:?}",
        messages
    );
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("suggestion: use 'aws.iam.Policy.Arn'")),
        "LSP must not collapse a far dotted typo to an unrelated registered identity; got: {:?}",
        messages
    );
}

#[test]
fn arguments_dotted_custom_type_suggests_nearest_identity_for_distance1_typo() {
    let engine = test_engine_with_iam_policy_arn_custom_type();
    let doc = create_document("arguments {\n  policy: aws.iam.Plicy.Arn\n}\n");
    let diagnostics = engine.analyze(&doc, None);
    let messages = diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>();

    assert!(
        diagnostics.iter().any(|d| {
            d.message.contains("unknown custom type") && d.message.contains("aws.iam.Plicy.Arn")
        }),
        "LSP must reject the misspelled dotted custom type; got: {:?}",
        messages
    );
    assert!(
        diagnostics.iter().any(|d| {
            d.message.contains("suggestion: use 'aws.iam.Policy.Arn'")
                && !d.message.contains("IamPolicyArn")
        }),
        "LSP must suggest the nearest dotted identity, not the bare name; got: {:?}",
        messages
    );
}

/// Companion to the test above: when no provider schemas are loaded (the
/// LSP's orphaned-file fallback), the
/// strict check must *not* fire, otherwise every user-defined custom
/// type in an orphaned file would redline. This pins the separately named
/// `DiagnosticEngine::without_loaded_provider_schemas` path.
#[test]
fn arguments_unknown_custom_type_silent_without_providers() {
    let engine = test_engine();
    let doc = create_document("arguments {\n  oidc_provider_arn: IamOidcProviderArn\n}\n");
    let diagnostics = engine.analyze(&doc, None);
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("unknown custom type")),
        "LSP must not surface unknown-custom-type errors when no \
         provider is registered (orphaned-file fallback); got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// Issue #2623 regression: a single-literal annotation
/// (`'dev'` alone) is the degenerate case of the same grammar
/// production. Pinning it separately so a future tightening of the
/// `type_expr_atom` rule doesn't accidentally re-introduce a
/// "expected type_expr" diagnostic on this shape.
#[test]
fn arguments_single_string_literal_type_does_not_surface_parser_error() {
    let engine = test_engine();
    let doc = create_document("arguments {\n  environment: 'dev'\n}\n");
    let diagnostics = engine.analyze(&doc, None);
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("expected type_expr")),
        "single-literal argument type must not produce `expected type_expr`; got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}
