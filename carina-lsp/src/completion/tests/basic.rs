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

    // Find the aws.s3.bucket completion
    let s3_completion = completions
        .iter()
        .find(|c| c.label == "aws.s3.bucket")
        .expect("Should have aws.s3.bucket completion");

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
            edit.new_text.starts_with("aws.s3.bucket"),
            "new_text should start with aws.s3.bucket"
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

    // Find the aws.ec2.vpc completion
    let vpc_completion = completions
        .iter()
        .find(|c| c.label == "aws.ec2.vpc")
        .expect("Should have aws.ec2.vpc completion");

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

    // Find the aws.ec2.vpc completion (should still be offered)
    let vpc_completion = completions.iter().find(|c| c.label == "aws.ec2.vpc");
    assert!(
        vpc_completion.is_some(),
        "Should offer aws.ec2.vpc completion"
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
vpc: aws.ec2.vpc
cidr_blocks: list(cidr)
enable_https: bool = true
}

let web_sg = aws.ec2.security_group {
name = "web-sg"
}
"#;
    fs::write(module_dir.join("main.crn"), module_content).expect("Failed to write module file");

    // Create main file that imports the module
    let main_content = r#"let web_tier = import "./modules/web_tier"

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
fn module_parameter_completion_with_single_file_module() {
    use std::fs;
    use tempfile::tempdir;

    let provider = test_provider();

    // Create a temporary directory structure
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let base_path = temp_dir.path();

    // Create module directory
    let module_dir = base_path.join("modules");
    fs::create_dir_all(&module_dir).expect("Failed to create module dir");

    // Create single file module
    let module_content = r#"
arguments {
name: string
count: int = 1
}
"#;
    fs::write(module_dir.join("simple.crn"), module_content).expect("Failed to write module file");

    // Create main file that imports the module
    let main_content = r#"let simple = import "./modules/simple.crn"

simple {
n
}"#;
    let doc = create_document(main_content);

    // Cursor inside the module call block (line 3, after "n")
    let position = Position {
        line: 3,
        character: 5,
    };

    let completions = provider.complete(&doc, position, Some(base_path));

    // Should have module parameter completions
    let name_completion = completions.iter().find(|c| c.label == "name");
    assert!(
        name_completion.is_some(),
        "Should have name parameter completion"
    );

    let count_completion = completions.iter().find(|c| c.label == "count");
    assert!(
        count_completion.is_some(),
        "Should have count parameter completion"
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn instance_tenancy_completion_for_aws_vpc() {
    let provider = test_provider();
    let doc = create_document(
        r#"aws.ec2.vpc {
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
        .find(|c| c.label == "aws.ec2.vpc.InstanceTenancy.default");
    assert!(
        default_completion.is_some(),
        "Should have 'aws.ec2.vpc.InstanceTenancy.default' completion"
    );

    let dedicated_completion = completions
        .iter()
        .find(|c| c.label == "aws.ec2.vpc.InstanceTenancy.dedicated");
    assert!(
        dedicated_completion.is_some(),
        "Should have 'aws.ec2.vpc.InstanceTenancy.dedicated' completion"
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
        r#"aws.s3.bucket {
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
            .any(|c| c.label == "aws.s3.bucket.VersioningStatus.Enabled"),
        "Should complete namespaced enum values from StringEnum schema metadata"
    );
    assert!(
        completions
            .iter()
            .any(|c| c.label == "aws.s3.bucket.VersioningStatus.Suspended"),
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
        r#"aws.s3.bucket {
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
        r#"awscc.ec2.security_group {
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
fn struct_field_value_completion_for_bool() {
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
        r#"awscc.ec2.security_group {
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
    let text = r#"awscc.ec2.security_group {
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
            } if resource_type == "awscc.ec2.security_group" && attr_path == &["security_group_ingress".to_string()]
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

    // Find any ref type completion (e.g., aws.s3.bucket)
    let s3_completion = completions
        .iter()
        .find(|c| c.label == "aws.s3.bucket")
        .expect("Should have aws.s3.bucket type completion");

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
            edit.new_text, "aws.s3.bucket",
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
        .find(|c| c.label == "aws.s3.bucket")
        .expect("Should have aws.s3.bucket type completion");

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
            edit.new_text, "aws.s3.bucket",
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

    for basic_type in &["string", "int", "bool", "float"] {
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
        .find(|c| c.label == "string")
        .expect("Should have 'string' completion");
    assert_eq!(
        string_completion.kind,
        Some(CompletionItemKind::TYPE_PARAMETER)
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
        labels.contains(&"arn"),
        "Type completions should include 'arn'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"iam_policy_arn"),
        "Type completions should include 'iam_policy_arn'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"availability_zone"),
        "Type completions should include 'availability_zone'. Got: {:?}",
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
        labels.contains(&"iam_policy_arn"),
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
    let text = "fn greet(name: string): ";
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
    let text = "fn greet(name: string) {\n  let x = ";
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
    // completions should use the resource_type as namespace to produce DSL format.
    let provider = test_provider_with_nameless_enum();
    let doc = create_document(
        r#"awscc.s3.bucket {
versioning_status =
}"#,
    );
    let position = Position {
        line: 1,
        character: 24,
    };

    let completions = provider.complete(&doc, position, None);

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should produce bare enum value completions (not full namespace path)
    assert!(
        labels.contains(&"Enabled"),
        "Should produce bare enum completion. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"Suspended"),
        "Should include all enum variants as bare values"
    );
    // Should NOT have full namespaced format
    assert!(
        !completions
            .iter()
            .any(|c| c.label.contains("awscc.s3.bucket")),
        "Should not show full namespace path. Got: {:?}",
        labels
    );
    // Should NOT have quoted string format
    assert!(
        !completions.iter().any(|c| c.label == "\"Enabled\""),
        "Should not show quoted string format"
    );
}

#[test]
fn string_enum_completion_in_struct_derives_namespace() {
    // StringEnum inside a struct field should also derive namespace from resource_type.
    let provider = test_provider_with_nameless_enum();
    let doc = create_document(
        r#"awscc.s3.bucket {
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
        labels.contains(&"Enabled"),
        "Should produce bare enum completion inside struct. Got: {:?}",
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
        labels.contains(&"string"),
        "Type completions inside list() should include 'string'. Got: {:?}",
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
        labels.contains(&"string"),
        "Type completions inside map() should include 'string'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"int"),
        "Type completions inside map() should include 'int'. Got: {:?}",
        labels
    );
}
