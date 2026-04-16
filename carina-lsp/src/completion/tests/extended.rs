use super::*;
use tower_lsp::lsp_types::InsertTextFormat;

#[test]
#[ignore = "requires provider schemas"]
fn availability_zone_completions_use_dynamic_prefix() {
    let provider = test_provider();

    // availability_zone_completions should use the namespace and type_name to build the prefix
    let completions = provider.availability_zone_completions("awscc", "AvailabilityZone");

    // Should have completions
    assert!(
        !completions.is_empty(),
        "Should generate AZ completions from region data"
    );

    // All completions should use the dynamic prefix
    for item in &completions {
        assert!(
            item.label.starts_with("awscc.AvailabilityZone."),
            "Label should start with 'awscc.AvailabilityZone.', got: {}",
            item.label
        );
    }

    // Should include specific regions from the factory data
    let has_tokyo = completions
        .iter()
        .any(|c| c.label == "awscc.AvailabilityZone.ap_northeast_1a");
    assert!(has_tokyo, "Should include Tokyo region AZs");

    // Detail should include region display name
    let tokyo_a = completions
        .iter()
        .find(|c| c.label == "awscc.AvailabilityZone.ap_northeast_1a")
        .unwrap();
    assert_eq!(
        tokyo_a.detail.as_deref(),
        Some("Tokyo Zone a"),
        "Detail should show region name and zone letter"
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn struct_field_completions_via_block_name() {
    let provider = test_provider();
    // Use singular "operating_region" (block_name) to get struct fields
    let completions =
        provider.struct_field_completions("awscc.ec2.ipam", &["operating_region".to_string()]);
    assert!(
        !completions.is_empty(),
        "Should provide struct field completions via block_name"
    );
    let field_names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        field_names.contains(&"region_name"),
        "Should include region_name field. Got: {:?}",
        field_names
    );
}

/// Create a CompletionProvider with a schema that has deeply nested structs for testing.
/// Schema: test.nested.resource has an attribute "outer" which is a Struct
/// containing a field "inner" which is also a Struct containing a field "leaf_field".
#[test]
fn nested_struct_completion_depth_2() {
    let provider = test_provider_with_nested_structs();
    let text = r#"let r = test.nested.resource {
outer {
    inner {

    }
}
}"#;
    let context = provider.get_completion_context(
        text,
        Position {
            line: 3,
            character: 12,
        },
    );
    assert!(
        matches!(
            context,
            CompletionContext::InsideStructBlock {
                ref resource_type,
                ref attr_path,
            } if resource_type == "test.nested.resource"
                && attr_path == &["outer".to_string(), "inner".to_string()]
        ),
        "Should detect InsideStructBlock with nested path, got: {:?}",
        context
    );

    // Verify actual completions work
    let completions = provider.struct_field_completions(
        "test.nested.resource",
        &["outer".to_string(), "inner".to_string()],
    );
    let field_names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        field_names.contains(&"leaf_field"),
        "Should include leaf_field in nested completions. Got: {:?}",
        field_names
    );
    assert!(
        field_names.contains(&"leaf_bool"),
        "Should include leaf_bool in nested completions. Got: {:?}",
        field_names
    );
}

#[test]
fn nested_struct_after_equals_depth_2() {
    let provider = test_provider_with_nested_structs();
    let text = r#"let r = test.nested.resource {
outer {
    inner {
        leaf_field =
    }
}
}"#;
    let context = provider.get_completion_context(
        text,
        Position {
            line: 3,
            character: 25,
        },
    );
    assert!(
        matches!(
            context,
            CompletionContext::AfterEqualsInStruct {
                ref resource_type,
                ref attr_path,
                ref field_name,
            } if resource_type == "test.nested.resource"
                && attr_path == &["outer".to_string(), "inner".to_string()]
                && field_name == "leaf_field"
        ),
        "Should detect AfterEqualsInStruct with nested path, got: {:?}",
        context
    );
}

#[test]
fn list_string_enum_completions() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let list_enum = AttributeType::list(AttributeType::StringEnum {
        name: "Protocol".to_string(),
        values: vec!["tcp".to_string(), "udp".to_string(), "icmp".to_string()],
        namespace: None,
        to_dsl: None,
    });

    let schema = ResourceSchema::new("test.list.resource")
        .attribute(AttributeSchema::new("protocols", list_enum));

    let mut schemas = HashMap::new();
    schemas.insert("test.list.resource".to_string(), schema);

    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![]);

    let completions = provider.completions_for_type(
        &AttributeType::list(AttributeType::StringEnum {
            name: "Protocol".to_string(),
            values: vec!["tcp".to_string(), "udp".to_string(), "icmp".to_string()],
            namespace: None,
            to_dsl: None,
        }),
        None,
    );

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"'tcp'"),
        "Should offer tcp as completion for List(StringEnum). Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"'udp'"),
        "Should offer udp as completion for List(StringEnum). Got: {:?}",
        labels
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn attribute_completions_include_block_name_snippet() {
    let provider = test_provider();
    let completions = provider.attribute_completions_for_type("awscc.ec2.ipam");
    let block_name_completion = completions.iter().find(|c| c.label == "operating_region");
    assert!(
        block_name_completion.is_some(),
        "Should offer block_name 'operating_region' as a completion. Labels: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
    let item = block_name_completion.unwrap();
    assert_eq!(item.kind, Some(CompletionItemKind::SNIPPET));
    assert!(
        item.detail.as_ref().unwrap().contains("operating_regions"),
        "Detail should reference canonical name"
    );
}

#[test]
fn union_completions_include_member_types() {
    use carina_core::schema::AttributeType;

    let provider = test_provider();
    let completions = provider.completions_for_type(
        &AttributeType::Union(vec![
            AttributeType::StringEnum {
                name: "Mode".to_string(),
                values: vec!["active".to_string(), "passive".to_string()],
                namespace: None,
                to_dsl: None,
            },
            AttributeType::Bool,
        ]),
        None,
    );

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should have StringEnum completions
    assert!(
        labels.contains(&"'active'"),
        "Should offer 'active' from StringEnum member. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"'passive'"),
        "Should offer 'passive' from StringEnum member. Got: {:?}",
        labels
    );
    // Should have Bool completions
    assert!(
        labels.contains(&"true"),
        "Should offer 'true' from Bool member. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"false"),
        "Should offer 'false' from Bool member. Got: {:?}",
        labels
    );
}

#[test]
fn union_completions_dedup_labels() {
    use carina_core::schema::AttributeType;

    let provider = test_provider();
    let completions = provider.completions_for_type(
        &AttributeType::Union(vec![AttributeType::Bool, AttributeType::Bool]),
        None,
    );

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    let true_count = labels.iter().filter(|&&l| l == "true").count();
    assert_eq!(
        true_count, 1,
        "Should deduplicate 'true' in Union completions. Got: {:?}",
        labels
    );
}

#[test]
fn map_completions_delegate_to_inner_type() {
    use carina_core::schema::AttributeType;

    let provider = test_provider();
    let completions = provider.completions_for_type(&AttributeType::map(AttributeType::Bool), None);

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"true"),
        "Map(Bool) should offer 'true'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"false"),
        "Map(Bool) should offer 'false'. Got: {:?}",
        labels
    );
}

#[test]
fn attribute_completions_return_empty_when_resource_type_unknown() {
    // When the resource type is not found in schemas (e.g., resource type detection failed),
    // attribute_completions_for_type should return an empty list instead of
    // falling back to all attributes from all schemas.
    let provider = test_provider();
    let completions = provider.attribute_completions_for_type("nonexistent.resource.type");
    assert!(
        completions.is_empty(),
        "Should return no completions for unknown resource type, but got {} completions: {:?}",
        completions.len(),
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn no_completions_for_unknown_resource_type_in_block() {
    // End-to-end test: when inside a resource block whose type can't be detected,
    // the completion should return empty rather than all attributes from all schemas.
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    // Create a provider with two schemas
    let schema_a = ResourceSchema::new("test.a.resource")
        .attribute(AttributeSchema::new("attr_a", AttributeType::String));
    let schema_b = ResourceSchema::new("test.b.resource")
        .attribute(AttributeSchema::new("attr_b", AttributeType::String));

    let mut schemas = HashMap::new();
    schemas.insert("test.a.resource".to_string(), schema_a);
    schemas.insert("test.b.resource".to_string(), schema_b);

    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![]);

    // Simulate being inside a block where resource type detection yields empty string
    let completions = provider.attribute_completions_for_type("");
    assert!(
        completions.is_empty(),
        "Should return no completions when resource type is empty string, but got {} completions",
        completions.len()
    );
}

#[test]
fn nested_struct_completions_via_block_name_in_path() {
    // When a user writes `config { transition { ... } }` where "transition" is
    // the block_name for field "transitions", the path resolution at depth > 1
    // should find the struct fields via StructField.block_name.
    let provider = test_provider_with_block_name_nested();
    // Path: config -> transition (block_name for "transitions")
    let completions = provider.struct_field_completions(
        "test.block.resource",
        &["config".to_string(), "transition".to_string()],
    );
    let field_names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        field_names.contains(&"days"),
        "Should resolve struct fields when nested path uses block_name 'transition'. Got: {:?}",
        field_names
    );
    assert!(
        field_names.contains(&"storage_class"),
        "Should resolve struct fields when nested path uses block_name 'transition'. Got: {:?}",
        field_names
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn type_based_completion_for_route_table_id() {
    // When editing `route_table_id = ` inside an ec2.route block,
    // and there's a `let rt = awscc.ec2.route_table { ... }` binding,
    // completion should suggest `rt.route_table_id` because:
    // - route_table_id in ec2.route has type Custom("RouteTableId")
    // - ec2.route_table has attribute route_table_id with type Custom("RouteTableId")
    let provider = test_provider();
    let doc = create_document(
        r#"let rt = awscc.ec2.route_table {
    vpc_id = "vpc-123"
}

awscc.ec2.route {
    route_table_id =
}"#,
    );
    // Cursor after "route_table_id = " (line 5)
    let position = Position {
        line: 5,
        character: 22,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should suggest rt.route_table_id (type-based match: RouteTableId)
    assert!(
        labels.contains(&"rt.route_table_id"),
        "Should suggest rt.route_table_id for route_table_id attribute (type-based). Got: {:?}",
        labels
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn type_based_completion_for_vpc_id() {
    // When editing `vpc_id = ` inside an ec2.subnet block,
    // and there's a `let vpc = awscc.ec2.vpc { ... }` binding,
    // completion should suggest `vpc.vpc_id` because:
    // - vpc_id in ec2.subnet has type Custom("VpcId")
    // - ec2.vpc has attribute vpc_id with type Custom("VpcId")
    let provider = test_provider();
    let doc = create_document(
        r#"let vpc = awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}

awscc.ec2.subnet {
    vpc_id =
}"#,
    );
    // Cursor after "vpc_id = " (line 5)
    let position = Position {
        line: 5,
        character: 14,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should suggest vpc.vpc_id (type-based match: VpcId)
    assert!(
        labels.contains(&"vpc.vpc_id"),
        "Should suggest vpc.vpc_id for vpc_id attribute (type-based). Got: {:?}",
        labels
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn type_based_completion_does_not_suggest_wrong_type() {
    // When editing `vpc_id = ` inside an ec2.subnet block,
    // a `let rt = awscc.ec2.route_table` binding should NOT be suggested
    // because route_table has no attribute of type VpcId that matches.
    // (route_table does have vpc_id, but the test verifies that rt is not
    // suggested with the wrong attribute like rt.route_table_id)
    let provider = test_provider();
    let doc = create_document(
        r#"let rt = awscc.ec2.route_table {
    vpc_id = "vpc-123"
}

awscc.ec2.subnet {
    vpc_id =
}"#,
    );
    // Cursor after "vpc_id = " (line 5)
    let position = Position {
        line: 5,
        character: 14,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should NOT suggest rt.route_table_id (wrong type: RouteTableId != VpcId)
    assert!(
        !labels.contains(&"rt.route_table_id"),
        "Should NOT suggest rt.route_table_id for vpc_id attribute (type mismatch). Got: {:?}",
        labels
    );

    // rt.vpc_id SHOULD be suggested (route_table has vpc_id of type VpcId)
    assert!(
        labels.contains(&"rt.vpc_id"),
        "Should suggest rt.vpc_id for vpc_id attribute (type-based match). Got: {:?}",
        labels
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn type_based_completion_does_not_suggest_region_or_boolean() {
    // For reference attributes like route_table_id (Custom("RouteTableId")),
    // completions should NOT include Region values or boolean values.
    // This is the catch-all fix from #906.
    let provider = test_provider();
    let doc = create_document(
        r#"let rt = awscc.ec2.route_table {
    vpc_id = "vpc-123"
}

awscc.ec2.route {
    route_table_id =
}"#,
    );
    let position = Position {
        line: 5,
        character: 22,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should NOT suggest boolean values
    assert!(
        !labels.contains(&"true"),
        "Should NOT suggest 'true' for route_table_id. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"false"),
        "Should NOT suggest 'false' for route_table_id. Got: {:?}",
        labels
    );

    // Should NOT suggest Region values
    let has_region = labels.iter().any(|l| l.contains("Region."));
    assert!(
        !has_region,
        "Should NOT suggest Region values for route_table_id. Got: {:?}",
        labels
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn readonly_attributes_excluded_from_resource_block_completions() {
    // Read-only attributes (e.g., vpc_id, arn) should NOT appear as completion
    // candidates inside a resource block, since users cannot set them.
    let provider = test_provider();
    let completions = provider.attribute_completions_for_type("awscc.ec2.vpc");
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // vpc_id is read-only on ec2.vpc — should NOT be suggested
    assert!(
        !labels.contains(&"vpc_id"),
        "Read-only attribute 'vpc_id' should NOT appear in resource block completions. Got: {:?}",
        labels
    );

    // cidr_block is writable — should be suggested
    assert!(
        labels.contains(&"cidr_block"),
        "Writable attribute 'cidr_block' should appear in resource block completions. Got: {:?}",
        labels
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn readonly_attributes_still_available_for_value_references() {
    // Read-only attributes should still be suggested when completing a value reference
    // (e.g., `vpc_id = vpc.vpc_id` on the right-hand side of `=`).
    let provider = test_provider();
    let doc = create_document(
        r#"let vpc = awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}

awscc.ec2.subnet {
    vpc_id =
}"#,
    );
    let position = Position {
        line: 5,
        character: 14,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // vpc_id on ec2.vpc is read-only, but should be suggested as a reference value
    assert!(
        labels.contains(&"vpc.vpc_id"),
        "Read-only attribute 'vpc.vpc_id' should be available as a value reference. Got: {:?}",
        labels
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn type_based_completion_excludes_self_reference() {
    // When editing `internet_gateway_id = ` inside a vpc_gateway_attachment block,
    // and the block itself is bound as `let igw_attachment = awscc.ec2.vpc_gateway_attachment { ... }`,
    // completion should NOT suggest `igw_attachment.internet_gateway_id` (self-reference).
    // It should only suggest references from OTHER bindings.
    let provider = test_provider();
    let doc = create_document(
        r#"let igw = awscc.ec2.internet_gateway {
}

let igw_attachment = awscc.ec2.vpc_gateway_attachment {
    internet_gateway_id =
}"#,
    );
    // Cursor after "internet_gateway_id = " (line 4)
    let position = Position {
        line: 4,
        character: 27,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should NOT suggest igw_attachment.internet_gateway_id (self-reference)
    let has_self_ref = labels.iter().any(|l| l.starts_with("igw_attachment."));
    assert!(
        !has_self_ref,
        "Should NOT suggest self-references (igw_attachment.*). Got: {:?}",
        labels
    );

    // Should suggest igw.internet_gateway_id (from another binding)
    assert!(
        labels.contains(&"igw.internet_gateway_id"),
        "Should suggest igw.internet_gateway_id from another binding. Got: {:?}",
        labels
    );
}

#[test]
fn builtin_function_completions_in_value_position() {
    let provider = test_provider();
    let doc = create_document(
        r#"awscc.ec2.vpc {
    cidr_block =
}"#,
    );
    let position = Position {
        line: 1,
        character: 18,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should include built-in function names
    assert!(
        labels.contains(&"join"),
        "Should suggest 'join' function. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"upper"),
        "Should suggest 'upper' function. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"cidr_subnet"),
        "Should suggest 'cidr_subnet' function. Got: {:?}",
        labels
    );
}

#[test]
fn builtin_function_completions_have_function_kind_and_signature() {
    let provider = test_provider();
    let completions = provider.builtin_function_completions();

    let join = completions
        .iter()
        .find(|c| c.label == "join")
        .expect("Should have join completion");

    assert_eq!(join.kind, Some(CompletionItemKind::FUNCTION));
    assert_eq!(
        join.detail.as_deref(),
        Some("join(separator: string, list: list) -> string")
    );
    assert_eq!(join.insert_text.as_deref(), Some("join($0)"));
    assert_eq!(join.insert_text_format, Some(InsertTextFormat::SNIPPET));
}

#[test]
fn builtin_function_completions_cover_all_functions() {
    let provider = test_provider();
    let completions = provider.builtin_function_completions();
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    let expected = [
        "cidr_subnet",
        "concat",
        "env",
        "flatten",
        "join",
        "keys",
        "length",
        "lookup",
        "lower",
        "map",
        "max",
        "min",
        "replace",
        "secret",
        "split",
        "trim",
        "upper",
        "values",
    ];
    for name in &expected {
        assert!(
            labels.contains(name),
            "Should include '{}' function completion. Got: {:?}",
            name,
            labels
        );
    }
}

#[test]
#[ignore = "requires provider schemas"]
fn resource_ref_completion_after_dot_uses_text_edit_to_avoid_duplication() {
    // When the user has typed `internet_gateway_id = igw.` and triggers completion,
    // the completion item should use a text_edit that replaces from the start of "igw"
    // to the cursor position, so accepting the completion produces
    // `internet_gateway_id = igw.internet_gateway_id` (not `igw.igw.internet_gateway_id`).
    let provider = test_provider();
    let doc = create_document(
        r#"let igw = awscc.ec2.internet_gateway {
}

let igw_attachment = awscc.ec2.vpc_gateway_attachment {
    internet_gateway_id = igw.
}"#,
    );
    // Cursor after "igw." on line 4 (character 30)
    let position = Position {
        line: 4,
        character: 30,
    };

    let completions = provider.complete(&doc, position, None);

    // Find the igw.internet_gateway_id completion
    let igw_completion = completions
        .iter()
        .find(|c| c.label == "igw.internet_gateway_id")
        .expect("Should suggest igw.internet_gateway_id");

    // The completion must use text_edit (not just insert_text) to avoid duplication.
    // The text_edit range should cover from the start of "igw" to the cursor position,
    // so that "igw." gets replaced with "igw.internet_gateway_id".
    assert!(
        igw_completion.text_edit.is_some(),
        "Resource reference completion should use text_edit to avoid prefix duplication. \
         Got insert_text: {:?}",
        igw_completion.insert_text
    );

    if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &igw_completion.text_edit {
        assert_eq!(
            edit.new_text, "igw.internet_gateway_id",
            "text_edit new_text should be the full reference"
        );
        // The range should start at column 26 (where "igw" starts, after "    internet_gateway_id = ")
        assert_eq!(
            edit.range.start.character, 26,
            "text_edit range should start at the binding name prefix"
        );
        assert_eq!(
            edit.range.end.character, 30,
            "text_edit range should end at the cursor position"
        );
    } else {
        panic!("text_edit should be a TextEdit, not an InsertReplaceEdit");
    }
}

#[test]
#[ignore = "requires provider schemas"]
fn after_binding_dot_shows_resource_attributes_not_builtins() {
    // When the user types `igw.` after `=`, completion should show
    // the binding's resource attributes (e.g., internet_gateway_id),
    // NOT built-in functions (cidr_subnet, concat, flatten, etc.).
    let provider = test_provider();
    let doc = create_document(
        r#"let igw = awscc.ec2.internet_gateway {
}

awscc.ec2.vpc_gateway_attachment {
    internet_gateway_id = igw.
}"#,
    );
    // Cursor after "igw." on line 4 (4 spaces + "internet_gateway_id = igw." = 30 chars)
    let position = Position {
        line: 4,
        character: 30,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should include the binding's resource attributes
    assert!(
        labels.contains(&"igw.internet_gateway_id"),
        "Should suggest igw.internet_gateway_id. Got: {:?}",
        labels
    );

    // Should NOT include built-in functions
    assert!(
        !labels.contains(&"cidr_subnet"),
        "Should NOT suggest built-in function 'cidr_subnet' after 'igw.'. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"concat"),
        "Should NOT suggest built-in function 'concat' after 'igw.'. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"join"),
        "Should NOT suggest built-in function 'join' after 'igw.'. Got: {:?}",
        labels
    );
}

#[test]
fn top_level_completion_suggests_upstream_state() {
    let provider = test_provider();
    let completions = provider.top_level_completions(
        Position {
            line: 0,
            character: 0,
        },
        "",
        None,
    );
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"upstream_state"),
        "Top-level completions should include 'upstream_state'. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"remote_state"),
        "Top-level completions should not include 'remote_state'. Got: {:?}",
        labels
    );
}

#[test]
fn upstream_state_block_completes_source_attribute() {
    let provider = test_provider();
    let doc = create_document(
        r#"let orgs = upstream_state {

}"#,
    );
    // Cursor on the empty line inside the block
    let position = Position {
        line: 1,
        character: 0,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"source"),
        "upstream_state block should offer 'source' attribute. Got: {:?}",
        labels
    );
}

#[test]
fn struct_field_completion_with_assignment_syntax() {
    // Bug #1627: `outer = {` (assignment syntax) was not detected as nested struct block
    let provider = test_provider_with_nested_structs();
    let text = r#"let r = test.nested.resource {
    outer = {

    }
}"#;
    let context = provider.get_completion_context(
        text,
        Position {
            line: 2,
            character: 8,
        },
    );
    assert!(
        matches!(
            context,
            CompletionContext::InsideStructBlock {
                ref resource_type,
                ref attr_path,
            } if resource_type == "test.nested.resource"
                && attr_path == &["outer".to_string()]
        ),
        "Should detect InsideStructBlock for assignment syntax 'outer = {{}}', got: {:?}",
        context
    );

    // Verify completions return struct fields, not top-level attributes
    let completions = provider.complete(
        &create_document(text),
        Position {
            line: 2,
            character: 8,
        },
        None,
    );
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"inner"),
        "Should have 'inner' struct field. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"outer_field"),
        "Should have 'outer_field' struct field. Got: {:?}",
        labels
    );
}

/// When cursor is after `outer =` (a Struct-typed attribute), the completion
/// should offer `{` snippet, not built-in functions like `cidr_subnet`.
#[test]
fn struct_attr_value_completion_shows_brace_not_builtins() {
    let provider = test_provider_with_nested_structs();
    let doc = create_document(
        r#"test.nested.resource {
outer =
}"#,
    );
    // Cursor after "outer =" (line 1)
    let position = Position {
        line: 1,
        character: 8,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should NOT contain built-in functions
    assert!(
        !labels.contains(&"cidr_subnet"),
        "Struct attribute value should not show built-in functions. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"flatten"),
        "Struct attribute value should not show built-in functions. Got: {:?}",
        labels
    );

    // Should contain a `{` completion for opening a struct block
    let has_brace = completions
        .iter()
        .any(|c| c.insert_text.as_deref().is_some_and(|t| t.contains('{')));
    assert!(
        has_brace,
        "Struct attribute value should offer '{{}}' snippet. Got labels: {:?}",
        labels
    );
}

#[test]
fn map_key_completions_from_string_enum_key_type() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    use std::collections::HashMap;
    use std::sync::Arc;

    // Create a schema with a Map attribute whose key is StringEnum
    let condition_keys = vec![
        "string_equals".to_string(),
        "string_like".to_string(),
        "arn_like".to_string(),
    ];
    let map_type = AttributeType::map_with_key(
        AttributeType::StringEnum {
            name: "ConditionOperator".to_string(),
            values: condition_keys.clone(),
            namespace: None,
            to_dsl: None,
        },
        AttributeType::map(AttributeType::String),
    );
    let schema =
        ResourceSchema::new("test.resource").attribute(AttributeSchema::new("condition", map_type));

    let mut schemas = HashMap::new();
    schemas.insert("test.resource".to_string(), schema);

    let provider = super::super::CompletionProvider::new(Arc::new(schemas), vec![], vec![], vec![]);

    // Simulate being inside `condition = { | }` — attr_path = ["condition"]
    let completions =
        provider.struct_field_completions("test.resource", &["condition".to_string()]);

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"string_equals"),
        "Map key completions should include 'string_equals'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"string_like"),
        "Map key completions should include 'string_like'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"arn_like"),
        "Map key completions should include 'arn_like'. Got: {:?}",
        labels
    );
    assert_eq!(labels.len(), 3, "Should have exactly 3 completions");

    // Verify insert_text includes " = " suffix
    let first = &completions[0];
    assert!(
        first.insert_text.as_deref().unwrap_or("").contains(" = "),
        "Insert text should include ' = '"
    );
}

#[test]
fn union_struct_field_completions() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
    use std::collections::HashMap;
    use std::sync::Arc;

    // principal: Union([Struct { fields: [service, aws, federated] }, String])
    let principal_type = AttributeType::Union(vec![
        AttributeType::Struct {
            name: "Principal".to_string(),
            fields: vec![
                StructField::new("service", AttributeType::String),
                StructField::new("aws", AttributeType::String),
                StructField::new("federated", AttributeType::String),
            ],
        },
        AttributeType::String,
    ]);

    let statement_type = AttributeType::Struct {
        name: "Statement".to_string(),
        fields: vec![StructField::new("principal", principal_type)],
    };

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::new("statement", statement_type));

    let mut schemas = HashMap::new();
    schemas.insert("test.resource".to_string(), schema);

    let provider = super::super::CompletionProvider::new(Arc::new(schemas), vec![], vec![], vec![]);

    // Inside principal = { | } — attr_path = ["statement", "principal"]
    let completions = provider.struct_field_completions(
        "test.resource",
        &["statement".to_string(), "principal".to_string()],
    );

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"federated"),
        "Union(Struct) should suggest 'federated'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"service"),
        "Union(Struct) should suggest 'service'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"aws"),
        "Union(Struct) should suggest 'aws'. Got: {:?}",
        labels
    );
}

#[test]
fn upstream_state_snippet_on_fresh_line_includes_let() {
    let provider = test_provider();
    let completions = provider.top_level_completions(
        Position {
            line: 0,
            character: 0,
        },
        "",
        None,
    );
    let item = find_completion(&completions, "upstream_state");
    let snippet = item.insert_text.as_deref().unwrap_or("");
    assert!(
        snippet.starts_with("let "),
        "On fresh line upstream_state should still insert full 'let ... = upstream_state {{...}}'. Got: {:?}",
        snippet
    );
}

#[test]
fn upstream_state_snippet_after_let_binding_omits_let() {
    // Bug #1930: typing `let orgs = u` then picking `upstream_state` produced
    // `let orgs = let binding = upstream_state {...}`. The snippet must drop
    // its leading `let ${1:binding} = ` when the line already has `let <name> =`.
    let provider = test_provider();
    let text = "let orgs = u";
    let completions = provider.top_level_completions(
        Position {
            line: 0,
            character: text.len() as u32,
        },
        text,
        None,
    );
    let item = find_completion(&completions, "upstream_state");
    let snippet = item.insert_text.as_deref().unwrap_or("");
    assert!(
        !snippet.contains("let "),
        "After existing `let <name> =`, upstream_state snippet must not re-emit `let `. Got: {:?}",
        snippet
    );
    assert!(
        snippet.starts_with("upstream_state"),
        "Snippet should start directly with `upstream_state`. Got: {:?}",
        snippet
    );
}

#[test]
fn read_snippet_after_let_binding_omits_let() {
    let provider = test_provider();
    let text = "let b = r";
    let completions = provider.top_level_completions(
        Position {
            line: 0,
            character: text.len() as u32,
        },
        text,
        None,
    );
    let item = find_completion(&completions, "read");
    let snippet = item.insert_text.as_deref().unwrap_or("");
    assert!(
        !snippet.contains("let "),
        "After existing `let <name> =`, read snippet must not re-emit `let `. Got: {:?}",
        snippet
    );
    assert!(
        snippet.starts_with("read "),
        "Snippet should start directly with `read `. Got: {:?}",
        snippet
    );
}

#[test]
fn let_import_snippet_after_let_binding_omits_let() {
    let provider = test_provider();
    let text = "let m = i";
    let completions = provider.top_level_completions(
        Position {
            line: 0,
            character: text.len() as u32,
        },
        text,
        None,
    );
    let item = find_completion(&completions, "let import");
    let snippet = item.insert_text.as_deref().unwrap_or("");
    assert!(
        !snippet.contains("let "),
        "After existing `let <name> =`, `let import` snippet must not re-emit `let `. Got: {:?}",
        snippet
    );
    assert!(
        snippet.starts_with("import "),
        "Snippet should start directly with `import `. Got: {:?}",
        snippet
    );
}

#[test]
fn read_snippet_on_fresh_line_includes_let() {
    let provider = test_provider();
    let completions = provider.top_level_completions(
        Position {
            line: 0,
            character: 0,
        },
        "",
        None,
    );
    let item = find_completion(&completions, "read");
    let snippet = item.insert_text.as_deref().unwrap_or("");
    assert!(
        snippet.starts_with("let "),
        "On fresh line read snippet should still include `let ... = read ...`. Got: {:?}",
        snippet
    );
}

#[test]
fn ipv4_cidr_completions_use_single_quotes() {
    let provider = test_provider();
    assert_all_wrapped(&provider.cidr_completions(), '\'', "CIDR");
}

#[test]
fn ipv6_cidr_completions_use_single_quotes() {
    let provider = test_provider();
    assert_all_wrapped(&provider.ipv6_cidr_completions(), '\'', "IPv6 CIDR");
}

#[test]
fn arn_completion_uses_single_quotes() {
    let provider = test_provider();
    assert_all_wrapped(&provider.arn_completions(), '\'', "ARN");
}
