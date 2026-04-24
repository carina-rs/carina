use super::*;

#[test]
#[ignore = "requires provider schemas"]
fn top_level_completion_replaces_prefix() {
    let provider = test_provider();
    let doc = create_document("aws.s");
    // Cursor at end of "aws.s" (line 0, col 5)
    let position = Position {
        line: 0,
        character: 5,
    };

    let completions = provider.complete(&doc, position, None);

    // Find the aws.s3.Bucket completion
    let s3_completion = completions
        .iter()
        .find(|c| c.label == "aws.s3.Bucket")
        .expect("Should have aws.s3.Bucket completion");

    // Verify it uses text_edit, not insert_text
    assert!(
        s3_completion.text_edit.is_some(),
        "Should use text_edit for resource type completion"
    );

    // Verify the text_edit range starts at column 0 (beginning of "aws.s")
    if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &s3_completion.text_edit {
        assert_eq!(
            edit.range.start.character, 0,
            "Should replace from start of prefix"
        );
        assert_eq!(edit.range.end.character, 5, "Should replace up to cursor");
        assert!(
            edit.new_text.starts_with("aws.s3.Bucket"),
            "new_text should start with aws.s3.Bucket"
        );
    } else {
        panic!("Expected CompletionTextEdit::Edit");
    }
}

#[test]
#[ignore = "requires provider schemas"]
fn top_level_completion_with_leading_whitespace() {
    let provider = test_provider();
    let doc = create_document("    aws.e");
    // Cursor at end of "    aws.e" (line 0, col 9)
    let position = Position {
        line: 0,
        character: 9,
    };

    let completions = provider.complete(&doc, position, None);

    // Find the aws.ec2.Vpc completion
    let vpc_completion = completions
        .iter()
        .find(|c| c.label == "aws.ec2.Vpc")
        .expect("Should have aws.ec2.Vpc completion");

    if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &vpc_completion.text_edit {
        // Should replace from column 4 (after whitespace) to cursor at 9
        assert_eq!(
            edit.range.start.character, 4,
            "Should replace from after whitespace"
        );
        assert_eq!(edit.range.end.character, 9, "Should replace up to cursor");
    } else {
        panic!("Expected CompletionTextEdit::Edit");
    }
}

#[test]
#[ignore = "requires provider schemas"]
fn top_level_completion_at_line_start() {
    let provider = test_provider();
    let doc = create_document("a");
    // Cursor at end of "a" (line 0, col 1)
    let position = Position {
        line: 0,
        character: 1,
    };

    let completions = provider.complete(&doc, position, None);

    // Find the aws.ec2.Vpc completion (should still be offered)
    let vpc_completion = completions.iter().find(|c| c.label == "aws.ec2.Vpc");
    assert!(
        vpc_completion.is_some(),
        "Should offer aws.ec2.Vpc completion"
    );

    if let Some(c) = vpc_completion
        && let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &c.text_edit
    {
        assert_eq!(
            edit.range.start.character, 0,
            "Should replace from line start"
        );
        assert_eq!(edit.range.end.character, 1, "Should replace up to cursor");
    }
}

#[test]
fn module_parameter_completion_with_directory_module() {
    use std::fs;
    use tempfile::tempdir;

    let provider = test_provider();

    // Create a temporary directory structure
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let base_path = temp_dir.path();

    // Create module directory
    let module_dir = base_path.join("modules").join("web_tier");
    fs::create_dir_all(&module_dir).expect("Failed to create module dir");

    // Create main.crn with argument parameters
    let module_content = r#"
arguments {
vpc: aws.ec2.Vpc
cidr_blocks: list(Cidr)
enable_https: Bool = true
}

let web_sg = aws.ec2.SecurityGroup {
name = "web-sg"
}
"#;
    fs::write(module_dir.join("main.crn"), module_content).expect("Failed to write module file");

    // Create main file that imports the module
    let main_content = r#"let web_tier = use { source = "./modules/web_tier" }

web_tier {

}"#;
    let doc = create_document(main_content);

    // Cursor inside the module call block (line 3, after whitespace)
    let position = Position {
        line: 3,
        character: 4,
    };

    let completions = provider.complete(&doc, position, Some(base_path));

    // Should have module parameter completions
    assert!(!completions.is_empty(), "Should have completions");

    // Check for specific parameters
    let vpc_completion = completions.iter().find(|c| c.label == "vpc");
    assert!(
        vpc_completion.is_some(),
        "Should have vpc parameter completion"
    );
    if let Some(c) = vpc_completion {
        assert!(
            c.detail.as_ref().is_some_and(|d| d.contains("required")),
            "vpc should be marked as required"
        );
    }

    let cidr_completion = completions.iter().find(|c| c.label == "cidr_blocks");
    assert!(
        cidr_completion.is_some(),
        "Should have cidr_blocks parameter completion"
    );

    let https_completion = completions.iter().find(|c| c.label == "enable_https");
    assert!(
        https_completion.is_some(),
        "Should have enable_https parameter completion"
    );
    if let Some(c) = https_completion {
        assert!(
            !c.detail.as_ref().is_some_and(|d| d.contains("required")),
            "enable_https should NOT be marked as required (has default)"
        );
    }
}

#[test]
#[ignore = "requires provider schemas"]
fn instance_tenancy_completion_for_aws_vpc() {
    let provider = test_provider();
    let doc = create_document(
        r#"aws.ec2.Vpc {
name = "my-vpc"
instance_tenancy =
}"#,
    );
    // Cursor after "instance_tenancy = " (line 2, col 23)
    let position = Position {
        line: 2,
        character: 23,
    };

    let completions = provider.complete(&doc, position, None);

    // Should have namespaced instance_tenancy completions
    let default_completion = completions
        .iter()
        .find(|c| c.label == "aws.ec2.Vpc.InstanceTenancy.default");
    assert!(
        default_completion.is_some(),
        "Should have 'aws.ec2.Vpc.InstanceTenancy.default' completion"
    );

    let dedicated_completion = completions
        .iter()
        .find(|c| c.label == "aws.ec2.Vpc.InstanceTenancy.dedicated");
    assert!(
        dedicated_completion.is_some(),
        "Should have 'aws.ec2.Vpc.InstanceTenancy.dedicated' completion"
    );
}

// Note: instance_tenancy_completion_for_awscc_vpc test was removed
// because generated schemas use AttributeType::String for instance_tenancy
// instead of the custom InstanceTenancy type that provides completions.

#[test]
#[ignore = "requires provider schemas"]
fn string_enum_completion_for_aws_s3_bucket_versioning_status() {
    let provider = test_provider();
    let doc = create_document(
        r#"aws.s3.Bucket {
versioning_status =
}"#,
    );
    let position = Position {
        line: 1,
        character: 24,
    };

    let completions = provider.complete(&doc, position, None);

    assert!(
        completions
            .iter()
            .any(|c| c.label == "aws.s3.Bucket.VersioningStatus.Enabled"),
        "Should complete namespaced enum values from StringEnum schema metadata"
    );
    assert!(
        completions
            .iter()
            .any(|c| c.label == "aws.s3.Bucket.VersioningStatus.Suspended"),
        "Should include all enum variants"
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn string_enum_completion_for_awscc_ipam_pool_address_family() {
    let provider = test_provider();
    let doc = create_document(
        r#"awscc.ec2.ipam_pool {
address_family =
}"#,
    );
    let position = Position {
        line: 1,
        character: 21,
    };

    let completions = provider.complete(&doc, position, None);

    assert!(
        completions
            .iter()
            .any(|c| c.label == "awscc.ec2.ipam_pool.AddressFamily.IPv4"),
        "Should complete awscc enum values from StringEnum schema metadata"
    );
    assert!(
        completions
            .iter()
            .any(|c| c.label == "awscc.ec2.ipam_pool.AddressFamily.IPv6"),
        "Should include all enum variants"
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn versioning_status_completion_for_s3_bucket() {
    let provider = test_provider();
    let doc = create_document(
        r#"aws.s3.Bucket {
name = "my-bucket"

}"#,
    );
    // Cursor inside s3_bucket block (line 2)
    let position = Position {
        line: 2,
        character: 4,
    };

    let completions = provider.complete(&doc, position, None);

    // Should have versioning_status as attribute completion
    let versioning_completion = completions.iter().find(|c| c.label == "versioning_status");
    assert!(
        versioning_completion.is_some(),
        "Should have 'versioning_status' attribute completion"
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn struct_field_completion_inside_nested_block() {
    let provider = test_provider();
    let doc = create_document(
        r#"awscc.ec2.SecurityGroup {
group_description = "test"
security_group_ingress {

}
}"#,
    );
    // Cursor inside the nested block (line 3)
    let position = Position {
        line: 3,
        character: 8,
    };

    let completions = provider.complete(&doc, position, None);

    // Should have struct field completions
    let ip_protocol = completions.iter().find(|c| c.label == "ip_protocol");
    assert!(
        ip_protocol.is_some(),
        "Should have ip_protocol field completion"
    );

    let from_port = completions.iter().find(|c| c.label == "from_port");
    assert!(
        from_port.is_some(),
        "Should have from_port field completion"
    );

    let to_port = completions.iter().find(|c| c.label == "to_port");
    assert!(to_port.is_some(), "Should have to_port field completion");

    // ip_protocol should be marked as required
    if let Some(c) = ip_protocol {
        assert!(
            c.detail.as_ref().is_some_and(|d| d.contains("required")),
            "ip_protocol should be marked as required"
        );
    }

    // Should NOT have top-level resource attributes like group_description
    let group_desc = completions.iter().find(|c| c.label == "group_description");
    assert!(
        group_desc.is_none(),
        "Should not have resource-level attributes inside struct block"
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn struct_field_value_completion_for_bool() {
    // Requires the real `awscc.ec2.flow_log` schema so the destination_options
    // struct resolves; previously this test passed only because the
    // fallback path returned `true`/`false` to every unresolved attribute
    // (the exact pollution #1974 fixed).
    let provider = test_provider();
    // flow_log's destination_options has Bool fields
    let doc = create_document(
        r#"let flow_log = awscc.ec2.flow_log {
destination_options {
    hive_compatible_partitions =
}
}"#,
    );
    // Cursor after "hive_compatible_partitions = " (line 2)
    let position = Position {
        line: 2,
        character: 37,
    };

    let completions = provider.complete(&doc, position, None);

    let true_completion = completions.iter().find(|c| c.label == "true");
    assert!(
        true_completion.is_some(),
        "Should have 'true' completion for Bool struct field"
    );

    let false_completion = completions.iter().find(|c| c.label == "false");
    assert!(
        false_completion.is_some(),
        "Should have 'false' completion for Bool struct field"
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn struct_field_completion_inside_second_repeated_block() {
    let provider = test_provider();
    let doc = create_document(
        r#"awscc.ec2.SecurityGroup {
group_description = "test"
security_group_ingress {
    ip_protocol = "tcp"
    from_port = 80
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}
security_group_ingress {

}
}"#,
    );
    // Cursor inside the second nested block (line 9)
    let position = Position {
        line: 9,
        character: 8,
    };

    let completions = provider.complete(&doc, position, None);

    // Should have struct field completions in the second block too
    let ip_protocol = completions.iter().find(|c| c.label == "ip_protocol");
    assert!(
        ip_protocol.is_some(),
        "Should have ip_protocol field completion in second repeated block"
    );

    let from_port = completions.iter().find(|c| c.label == "from_port");
    assert!(
        from_port.is_some(),
        "Should have from_port field completion in second repeated block"
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn context_detection_returns_struct_context() {
    let provider = test_provider();
    let text = r#"awscc.ec2.SecurityGroup {
group_description = "test"
security_group_ingress {

}
}"#;
    // Cursor inside nested block
    let context = provider.get_completion_context(
        text,
        Position {
            line: 3,
            character: 8,
        },
    );
    assert!(
        matches!(
            context,
            CompletionContext::InsideStructBlock {
                ref resource_type,
                ref attr_path,
            } if resource_type == "awscc.ec2.SecurityGroup" && attr_path == &["security_group_ingress".to_string()]
        ),
        "Should detect InsideStructBlock context, got: {:?}",
        context
    );
}

#[test]
fn context_detection_type_position_in_arguments() {
    let provider = test_provider();
    let text = "arguments {\nvpc: aws.";
    let context = provider.get_completion_context(
        text,
        Position {
            line: 1,
            character: 9, // cursor after "vpc: aws."
        },
    );
    assert!(
        matches!(context, CompletionContext::InTypePosition),
        "Should detect InTypePosition for type annotation after colon, got: {:?}",
        context
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn type_completion_uses_text_edit_to_replace_from_colon() {
    let provider = test_provider();
    // User has typed "vpc: aws." inside arguments block
    let doc = create_document("arguments {\nvpc: aws.");
    let position = Position {
        line: 1,
        character: 9, // cursor after "vpc: aws."
    };

    let completions = provider.complete(&doc, position, None);

    // Find any ref type completion (e.g., aws.s3.Bucket)
    let s3_completion = completions
        .iter()
        .find(|c| c.label == "aws.s3.Bucket")
        .expect("Should have aws.s3.Bucket type completion");

    // Must use text_edit (not insert_text) to avoid duplication with dotted identifiers
    assert!(
        s3_completion.text_edit.is_some(),
        "Type completion should use text_edit to handle dotted identifiers correctly"
    );

    // The text_edit range should start right after "vpc: " (column 5) and end at cursor (column 9)
    if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &s3_completion.text_edit {
        assert_eq!(
            edit.range.start.character, 5,
            "Should replace from right after colon and space"
        );
        assert_eq!(
            edit.range.end.character, 9,
            "Should replace up to cursor position"
        );
        assert_eq!(
            edit.new_text, "aws.s3.Bucket",
            "Insert text should be the resource type"
        );
    } else {
        panic!("Expected CompletionTextEdit::Edit");
    }
}

#[test]
#[ignore = "requires provider schemas"]
fn type_completion_with_empty_type() {
    let provider = test_provider();
    // User has typed "vpc: " inside arguments block
    let doc = create_document("arguments {\nvpc: ");
    let position = Position {
        line: 1,
        character: 5, // cursor right after "vpc: "
    };

    let completions = provider.complete(&doc, position, None);

    let s3_completion = completions
        .iter()
        .find(|c| c.label == "aws.s3.Bucket")
        .expect("Should have aws.s3.Bucket type completion");

    if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &s3_completion.text_edit {
        assert_eq!(
            edit.range.start.character, 5,
            "Should replace from right after colon and space"
        );
        assert_eq!(
            edit.range.end.character, 5,
            "Should replace up to cursor position"
        );
        assert_eq!(
            edit.new_text, "aws.s3.Bucket",
            "Insert text should be the resource type"
        );
    } else {
        panic!("Expected CompletionTextEdit::Edit");
    }
}

#[test]
fn provider_block_completion_suggests_region() {
    let provider = test_provider();
    let doc = create_document(
        r#"provider awscc {
    r
}"#,
    );
    // Cursor after "r" inside provider block (line 1, col 5)
    let position = Position {
        line: 1,
        character: 5,
    };

    let completions = provider.complete(&doc, position, None);

    // Should have "region" as a completion
    let region_completion = completions.iter().find(|c| c.label == "region");
    assert!(
        region_completion.is_some(),
        "Should have 'region' attribute completion inside provider block. Got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn provider_block_region_value_completion() {
    let provider = test_provider();
    let doc = create_document(
        r#"provider awscc {
    region =
}"#,
    );
    // Cursor after "region = " (line 1, col 12)
    let position = Position {
        line: 1,
        character: 12,
    };

    let completions = provider.complete(&doc, position, None);

    // Should have region value completions (like awscc.Region.ap_northeast_1)
    let has_region_value = completions
        .iter()
        .any(|c| c.label.contains("Region.ap_northeast_1"));
    assert!(
        has_region_value,
        "Should have region value completions after 'region = '. Got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn context_detection_inside_provider_block() {
    let provider = test_provider();
    let text = r#"provider awscc {
    r
}"#;
    let context = provider.get_completion_context(
        text,
        Position {
            line: 1,
            character: 5,
        },
    );
    assert!(
        matches!(context, CompletionContext::InsideProviderBlock { ref provider_name } if provider_name == "awscc"),
        "Should detect InsideProviderBlock context, got: {:?}",
        context
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn provider_block_region_completions_use_matching_namespace() {
    let provider = test_provider();

    // Inside "provider awscc { region = }", completions should only contain awscc.Region.*
    let doc = create_document(
        r#"provider awscc {
    region =
}"#,
    );
    let position = Position {
        line: 1,
        character: 12,
    };

    let completions = provider.complete(&doc, position, None);

    // Should have awscc.Region completions
    let has_awscc_region = completions
        .iter()
        .any(|c| c.label.starts_with("awscc.Region."));
    assert!(
        has_awscc_region,
        "Inside 'provider awscc', should have awscc.Region.* completions. Got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );

    // Should NOT have aws.Region completions
    let has_aws_region = completions
        .iter()
        .any(|c| c.label.starts_with("aws.Region."));
    assert!(
        !has_aws_region,
        "Inside 'provider awscc', should NOT have aws.Region.* completions. Got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn provider_block_region_completions_use_aws_namespace() {
    let provider = test_provider();

    // Inside "provider aws { region = }", completions should only contain aws.Region.*
    let doc = create_document(
        r#"provider aws {
    region =
}"#,
    );
    let position = Position {
        line: 1,
        character: 12,
    };

    let completions = provider.complete(&doc, position, None);

    // Should have aws.Region completions
    let has_aws_region = completions
        .iter()
        .any(|c| c.label.starts_with("aws.Region."));
    assert!(
        has_aws_region,
        "Inside 'provider aws', should have aws.Region.* completions. Got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );

    // Should NOT have awscc.Region completions
    let has_awscc_region = completions
        .iter()
        .any(|c| c.label.starts_with("awscc.Region."));
    assert!(
        !has_awscc_region,
        "Inside 'provider aws', should NOT have awscc.Region.* completions. Got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn type_completion_includes_basic_types() {
    let provider = test_provider();
    let doc = create_document("arguments {\nvpc: ");
    let position = Position {
        line: 1,
        character: 5,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    for basic_type in &["String", "Int", "Bool", "Float"] {
        assert!(
            labels.contains(basic_type),
            "Type completions should include '{}'. Got: {:?}",
            basic_type,
            labels
        );
    }

    // Basic types should have TYPE_PARAMETER kind
    let string_completion = completions
        .iter()
        .find(|c| c.label == "String")
        .expect("Should have 'String' completion");
    assert_eq!(
        string_completion.kind,
        Some(CompletionItemKind::TYPE_PARAMETER)
    );

    // Built-in custom types should always appear (no provider needed)
    for builtin_custom in &["Ipv4Cidr", "Ipv4Address", "Ipv6Cidr", "Ipv6Address"] {
        assert!(
            labels.contains(builtin_custom),
            "Type completions should include built-in custom type '{}'. Got: {:?}",
            builtin_custom,
            labels
        );
    }
}

#[test]
fn completion_at_type_position_proposes_pascal_case_primitives() {
    let provider = test_provider();
    let doc = create_document("arguments {\nx: ");
    let position = Position {
        line: 1,
        character: 3,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(labels.contains(&"String"), "got: {:?}", labels);
    assert!(labels.contains(&"Int"), "got: {:?}", labels);
    assert!(labels.contains(&"Bool"), "got: {:?}", labels);
    assert!(labels.contains(&"Float"), "got: {:?}", labels);
    assert!(
        !labels.contains(&"string"),
        "lowercase 'string' should no longer appear, got: {:?}",
        labels
    );
}

#[test]
fn type_completion_includes_generic_constructors() {
    let provider = test_provider();
    let doc = create_document("arguments {\nitems: ");
    let position = Position {
        line: 1,
        character: 7,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"list("),
        "Type completions should include 'list('. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"map("),
        "Type completions should include 'map('. Got: {:?}",
        labels
    );
}

#[test]
fn type_completion_includes_custom_types() {
    let provider = test_provider_with_custom_types();
    let doc = create_document("arguments {\naddr: ");
    let position = Position {
        line: 1,
        character: 6,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Custom types from provider validators should appear
    assert!(
        labels.contains(&"Arn"),
        "Type completions should include 'Arn'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"IamPolicyArn"),
        "Type completions should include 'IamPolicyArn'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"AvailabilityZone"),
        "Type completions should include 'AvailabilityZone'. Got: {:?}",
        labels
    );
}

#[test]
fn type_completion_custom_types_inside_list() {
    let provider = test_provider_with_custom_types();
    let doc = create_document("arguments {\npolicies: list(");
    let position = Position {
        line: 1,
        character: 15,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"IamPolicyArn"),
        "Custom types should appear inside list(). Got: {:?}",
        labels
    );
}

#[test]
fn context_detection_type_position_in_fn_parameter() {
    let provider = test_provider();
    let text = "fn greet(name: ";
    let context = provider.get_completion_context(
        text,
        Position {
            line: 0,
            character: 15,
        },
    );
    assert!(
        matches!(context, CompletionContext::InTypePosition),
        "Should detect InTypePosition for fn parameter type annotation, got: {:?}",
        context
    );
}

#[test]
fn context_detection_type_position_in_fn_return_type() {
    let provider = test_provider();
    let text = "fn greet(name: String): ";
    let context = provider.get_completion_context(
        text,
        Position {
            line: 0,
            character: 24,
        },
    );
    assert!(
        matches!(context, CompletionContext::InTypePosition),
        "Should detect InTypePosition for fn return type annotation, got: {:?}",
        context
    );
}

#[test]
fn context_detection_not_type_position_inside_fn_body() {
    let provider = test_provider();
    let text = "fn greet(name: String) {\n  let x = ";
    let context = provider.get_completion_context(
        text,
        Position {
            line: 1,
            character: 10,
        },
    );
    assert!(
        !matches!(context, CompletionContext::InTypePosition),
        "Should NOT detect InTypePosition inside fn body, got: {:?}",
        context
    );
}

#[test]
fn context_detection_type_position_in_attributes() {
    let provider = test_provider();
    let text = "attributes {\noutput: ";
    let context = provider.get_completion_context(
        text,
        Position {
            line: 1,
            character: 8,
        },
    );
    assert!(
        matches!(context, CompletionContext::InTypePosition),
        "Should detect InTypePosition for attributes block type annotation, got: {:?}",
        context
    );
}

#[test]
fn string_enum_completion_derives_namespace_from_resource_type() {
    // When a StringEnum has name but no namespace (WASM provider case),
    // completions derive the namespace from the resource type and emit
    // fully-qualified identifiers.
    let provider = test_provider_with_nameless_enum();
    let doc = create_document(
        r#"awscc.s3.Bucket {
versioning_status =
}"#,
    );
    let position = Position {
        line: 1,
        character: 24,
    };

    let completions = provider.complete(&doc, position, None);

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"awscc.s3.Bucket.VersioningStatus.Enabled"),
        "expected fully-qualified enum identifier; got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"awscc.s3.Bucket.VersioningStatus.Suspended"),
        "expected all enum variants in fully-qualified form; got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"Enabled") && !labels.contains(&"Suspended"),
        "bare tail tokens must not be offered for namespaced enums; got: {:?}",
        labels
    );
    // Should NOT have quoted string format.
    assert!(
        !completions
            .iter()
            .any(|c| c.label == "\"Enabled\"" || c.label == "'Enabled'"),
        "Should not show quoted string format"
    );
}

#[test]
fn string_enum_completion_in_struct_derives_namespace() {
    // StringEnum inside a struct field also resolves via the resource type
    // and emits the fully-qualified form.
    let provider = test_provider_with_nameless_enum();
    let doc = create_document(
        r#"awscc.s3.Bucket {
versioning_configuration {
    status =
}
}"#,
    );
    let position = Position {
        line: 2,
        character: 14,
    };

    let completions = provider.complete(&doc, position, None);

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"awscc.s3.Bucket.VersioningStatus.Enabled"),
        "expected fully-qualified enum identifier inside struct; got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"Enabled"),
        "bare tail token must not be offered inside struct; got: {:?}",
        labels
    );
}

#[test]
fn type_completion_inside_list_shows_basic_types() {
    let provider = test_provider();
    let doc = create_document("arguments {\nitems: list(s");
    let position = Position {
        line: 1,
        character: 13,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"String"),
        "Type completions inside list() should include 'String'. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"list("),
        "Type completions inside list() should NOT include 'list('. Got: {:?}",
        labels
    );
}

#[test]
fn type_completion_inside_map_shows_basic_types() {
    let provider = test_provider();
    let doc = create_document("arguments {\ndata: map(");
    let position = Position {
        line: 1,
        character: 11,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"String"),
        "Type completions inside map() should include 'String'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"Int"),
        "Type completions inside map() should include 'Int'. Got: {:?}",
        labels
    );
}

#[test]
fn module_binding_completion_at_top_level() {
    let provider = test_provider();
    let doc = create_document("let github = use { source = './modules/github-oidc' }\n\ng");
    let position = Position {
        line: 2,
        character: 1,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"github"),
        "Top-level completions should include module binding 'github'. Got: {:?}",
        labels
    );

    let github_completion = completions.iter().find(|c| c.label == "github").unwrap();
    assert_eq!(github_completion.kind, Some(CompletionItemKind::MODULE));
}

#[test]
fn module_call_scaffolding_includes_arguments() {
    let tmp = tempfile::tempdir().unwrap();
    let module_dir = tmp.path().join("modules").join("web");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(
        module_dir.join("main.crn"),
        "arguments {\n  name: String\n  port: Int\n}\n",
    )
    .unwrap();

    let provider = test_provider();
    let doc = create_document("let web = use { source = './modules/web' }\n\nw");
    let position = Position {
        line: 2,
        character: 1,
    };

    let completions = provider.complete(&doc, position, Some(tmp.path()));
    let web_completion = completions
        .iter()
        .find(|c| c.label == "web")
        .expect("Should have 'web' completion");

    let snippet = web_completion.insert_text.as_deref().unwrap();
    assert!(
        snippet.contains("name") && snippet.contains("port"),
        "Scaffold should include all arguments. Got:\n{}",
        snippet
    );
}

#[test]
fn unknown_attribute_fallback_has_no_type_pollution() {
    // When value completion cannot resolve the attribute's type (the
    // attribute isn't in the schema), the fallback must not inject
    // concrete values of arbitrary types. `true`/`false` belong to Bool;
    // `aws.Region.*` belong to Region. Built-in functions are fine —
    // they're type-neutral.
    use carina_core::schema::{AttributeSchema, AttributeType, CompletionValue, ResourceSchema};
    use std::sync::Arc;

    let schema = ResourceSchema::new("test.foo.bar")
        .attribute(AttributeSchema::new("known_attr", AttributeType::String));
    let mut schemas = HashMap::new();
    schemas.insert("test.foo.bar".to_string(), schema);
    let regions = vec![CompletionValue {
        value: "aws.Region.ap_northeast_1".to_string(),
        description: "Tokyo".to_string(),
    }];
    let provider = CompletionProvider::new(
        Arc::new(schemas),
        vec!["test".to_string(), "aws".to_string()],
        regions,
        vec![],
    );
    // Cursor after `nonexistent_attr = ` — `nonexistent_attr` has no schema.
    let doc = create_document("test.foo.bar {\n  nonexistent_attr = \n}\n");
    let position = Position {
        line: 1,
        character: 22,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        !labels.contains(&"true") && !labels.contains(&"false"),
        "Bool values must not leak into unknown-attribute fallback. Got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|l| l.starts_with("aws.Region.")),
        "Region values must not leak into unknown-attribute fallback. Got: {:?}",
        labels
    );
    // Sanity: built-in functions are still offered (type-neutral).
    assert!(
        labels.contains(&"join"),
        "Built-in functions should still appear. Got: {:?}",
        labels
    );
}

#[test]
fn string_enum_completion_inside_for_loop_body() {
    // Regression for #1974: inside a `for` body, the enclosing resource_type
    // must still be detected so StringEnum completions fire. Previously the
    // for's opening `{` tripped the context detector into brace_depth >= 1
    // before the resource block's `{`, and `extract_resource_type` was only
    // consulted at brace_depth == 0 — so the resource schema was missed,
    // falling through to `generic_value_completions` (regions, true/false).
    let provider = test_provider_with_enum_and_regions();
    let doc = create_document(
        r#"let items = [1, 2]
for item in items {
  awscc.s3.Bucket {
    versioning_status =
  }
}
"#,
    );
    // Cursor after `    versioning_status = ` on line 3 (0-indexed)
    let position = Position {
        line: 3,
        character: 25,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"awscc.s3.Bucket.VersioningStatus.Enabled"),
        "StringEnum 'Enabled' must still be offered (fully-qualified) inside a for body. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"awscc.s3.Bucket.VersioningStatus.Suspended"),
        "StringEnum 'Suspended' must still be offered (fully-qualified) inside a for body. Got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|l| l.starts_with("aws.Region.")),
        "Region values must not pollute StringEnum completions inside for body. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"true") && !labels.contains(&"false"),
        "Boolean values must not pollute StringEnum completions. Got: {:?}",
        labels
    );
}

#[test]
fn string_enum_completion_inside_nested_for_loop_body() {
    // Two stacked `for` bodies still have to see through to the resource
    // type inside. Regression safety net for the for_body_depth tracker.
    let provider = test_provider_with_enum_and_regions();
    let doc = create_document(
        r#"for a in [1] {
  for b in [2] {
    awscc.s3.Bucket {
      versioning_status =
    }
  }
}
"#,
    );
    // Cursor on line 3 after `      versioning_status = `
    let position = Position {
        line: 3,
        character: 27,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"awscc.s3.Bucket.VersioningStatus.Enabled"),
        "Enum candidate (fully-qualified) must reach nested for body. Got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|l| l.starts_with("aws.Region.")),
        "No region pollution in nested for body. Got: {:?}",
        labels
    );
}

#[test]
fn for_loop_binding_suggested_in_body_value_position() {
    let provider = test_provider_single_attr();
    let doc =
        create_document("for name, account_id in items {\n  test.foo.bar {\n    attr = \n  }\n}\n");
    // Cursor after `    attr = ` on line 2 (0-indexed)
    let position = Position {
        line: 2,
        character: 11,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"name"),
        "map-form for binding 'name' should be suggested in body. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"account_id"),
        "map-form for binding 'account_id' should be suggested in body. Got: {:?}",
        labels
    );
}

#[test]
fn for_loop_binding_not_suggested_outside_body() {
    let provider = test_provider_single_attr();
    let doc = create_document(
        "for item in items {\n  test.foo.bar {\n    attr = x\n  }\n}\ntest.foo.bar {\n  attr = \n}\n",
    );
    // Cursor on line 6 after `  attr = ` — outside the for body
    let position = Position {
        line: 6,
        character: 9,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        !labels.contains(&"item"),
        "for-loop binding should not leak outside its body. Got: {:?}",
        labels
    );
}

#[test]
fn for_loop_discard_not_suggested() {
    let provider = test_provider_single_attr();
    let doc =
        create_document("for _, account_id in items {\n  test.foo.bar {\n    attr = \n  }\n}\n");
    let position = Position {
        line: 2,
        character: 11,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        !labels.contains(&"_"),
        "'_' discard marker must not be suggested. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"account_id"),
        "named binding alongside discard should still be suggested. Got: {:?}",
        labels
    );
}

#[test]
fn import_path_completion_lists_directories_only() {
    // Modules are directory-scoped (issue #1997). Stray `.crn` files next to
    // module directories must NOT be suggested as import targets — the
    // resolver would reject them with NotADirectory.
    let tmp = tempfile::tempdir().unwrap();
    let modules_dir = tmp.path().join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::write(
        modules_dir.join("web.crn"),
        "arguments {\n  name: String\n}\n",
    )
    .unwrap();
    std::fs::create_dir_all(modules_dir.join("shared")).unwrap();

    let provider = test_provider();
    let source = "let web = use { source = './modules/";
    let doc = create_document(source);
    let position = Position {
        line: 0,
        character: source.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(tmp.path()));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"shared/"),
        "Should suggest 'shared/' directory. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"web"),
        "Must NOT suggest 'web' for a standalone .crn file. Got: {:?}",
        labels
    );
}

/// Regression for #2196.
///
/// When the user starts typing a path with just `.`, we used to return zero
/// completions because `.` was treated as a filename prefix and the hidden
/// filter at the top of `import_path_completions` rejected every matching
/// entry (`.`, `..`, `.something`). The fix offers `./` and `../` as
/// navigation anchors so the user can start walking the tree.
#[test]
fn import_path_completion_offers_dot_anchors_for_partial_dot() {
    let tmp = tempfile::tempdir().unwrap();
    // Leaf dir with no sibling module dirs — mirrors infra/aws/management/github-oidc/.
    let leaf = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();

    let provider = test_provider();
    let source = "let web = use { source = '.";
    let doc = create_document(source);
    let position = Position {
        line: 0,
        character: source.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&leaf));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"./"),
        "Should offer './' anchor for partial '.'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"../"),
        "Should offer '../' anchor for partial '.'. Got: {:?}",
        labels
    );
}

/// Regression for #2196.
///
/// Empty partial (`source = '|'`) must still give the user an entry point
/// when the current directory has no module subdirs. The anchors `./` and
/// `../` are that entry point.
#[test]
fn import_path_completion_offers_dot_anchors_for_empty_partial() {
    let tmp = tempfile::tempdir().unwrap();
    let leaf = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();

    let provider = test_provider();
    let source = "let web = use { source = '";
    let doc = create_document(source);
    let position = Position {
        line: 0,
        character: source.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&leaf));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"./"),
        "Should offer './' anchor for empty partial. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"../"),
        "Should offer '../' anchor for empty partial. Got: {:?}",
        labels
    );
}

/// Regression for #2196.
///
/// Partial `..` is the user starting to type `../`. Same anchor behavior as
/// `.` — must not get eaten by the hidden-file filter.
#[test]
fn import_path_completion_offers_dot_anchors_for_partial_double_dot() {
    let tmp = tempfile::tempdir().unwrap();
    let leaf = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();

    let provider = test_provider();
    let source = "let web = use { source = '..";
    let doc = create_document(source);
    let position = Position {
        line: 0,
        character: source.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&leaf));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"../"),
        "Should offer '../' anchor for partial '..'. Got: {:?}",
        labels
    );
}

/// Regression for #2196.
///
/// Once the user has committed to a direction (`../`), the anchors should
/// NOT keep showing up — the user wants the contents of the parent dir,
/// not more navigation markers.
#[test]
fn import_path_completion_does_not_offer_anchors_after_slash() {
    let tmp = tempfile::tempdir().unwrap();
    // Structure:
    //   tmp/leaf/           (base_path — no siblings of its own)
    //   tmp/sibling_a/
    //   tmp/sibling_b/
    let leaf = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::create_dir_all(tmp.path().join("sibling_a")).unwrap();
    std::fs::create_dir_all(tmp.path().join("sibling_b")).unwrap();

    let provider = test_provider();
    let source = "let web = use { source = '../";
    let doc = create_document(source);
    let position = Position {
        line: 0,
        character: source.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&leaf));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"sibling_a/"),
        "Should list sibling_a/ from parent dir. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"sibling_b/"),
        "Should list sibling_b/ from parent dir. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"./"),
        "Should NOT offer './' anchor when partial already ends with '/'. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"../"),
        "Should NOT offer '../' anchor when partial already ends with '/'. Got: {:?}",
        labels
    );
}

/// Regression guard for #2196: path completion inside `use { source = '...' }`
/// must work when `use {` and the `source` attribute are on separate lines.
/// This is the real shape users type in practice; the single-line form was
/// covered by `import_path_completion_lists_directories_only` but the
/// multi-line shape broke after the `import` → `use` rename (#2186).
#[test]
fn use_source_path_completion_works_multiline() {
    let tmp = tempfile::tempdir().unwrap();
    let modules_dir = tmp.path().join("modules");
    std::fs::create_dir_all(modules_dir.join("network")).unwrap();
    std::fs::create_dir_all(modules_dir.join("shared")).unwrap();

    let provider = test_provider();
    // Realistic multi-line shape — `use {` on line 0, `source = '...` on line 1.
    let source = "let net = use {\n  source = './modules/";
    let doc = create_document(source);
    let last_line = source.lines().next_back().unwrap_or("");
    let position = Position {
        line: 1,
        character: last_line.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(tmp.path()));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"network/"),
        "Should suggest 'network/' directory under './modules/'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"shared/"),
        "Should suggest 'shared/' directory under './modules/'. Got: {:?}",
        labels
    );
}
