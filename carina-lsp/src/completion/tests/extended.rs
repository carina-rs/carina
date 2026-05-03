use super::*;
use tower_lsp::lsp_types::{InsertTextFormat, TextEdit};

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

    let schema = ResourceSchema::new("list.resource")
        .attribute(AttributeSchema::new("protocols", list_enum));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

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
    let schema_a = ResourceSchema::new("a.resource")
        .attribute(AttributeSchema::new("attr_a", AttributeType::String));
    let schema_b = ResourceSchema::new("b.resource")
        .attribute(AttributeSchema::new("attr_b", AttributeType::String));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema_a);
    schemas.insert("test", schema_b);

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
    // and there's a `let rt = awscc.ec2.RouteTable { ... }` binding,
    // completion should suggest `rt.route_table_id` because:
    // - route_table_id in ec2.route has type Custom("RouteTableId")
    // - ec2.route_table has attribute route_table_id with type Custom("RouteTableId")
    let provider = test_provider();
    let doc = create_document(
        r#"let rt = awscc.ec2.RouteTable {
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
    // and there's a `let vpc = awscc.ec2.Vpc { ... }` binding,
    // completion should suggest `vpc.vpc_id` because:
    // - vpc_id in ec2.subnet has type Custom("VpcId")
    // - ec2.vpc has attribute vpc_id with type Custom("VpcId")
    let provider = test_provider();
    let doc = create_document(
        r#"let vpc = awscc.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

awscc.ec2.Subnet {
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
    // a `let rt = awscc.ec2.RouteTable` binding should NOT be suggested
    // because route_table has no attribute of type VpcId that matches.
    // (route_table does have vpc_id, but the test verifies that rt is not
    // suggested with the wrong attribute like rt.route_table_id)
    let provider = test_provider();
    let doc = create_document(
        r#"let rt = awscc.ec2.RouteTable {
    vpc_id = "vpc-123"
}

awscc.ec2.Subnet {
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
        r#"let rt = awscc.ec2.RouteTable {
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
    let completions = provider.attribute_completions_for_type("awscc.ec2.Vpc");
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
        r#"let vpc = awscc.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

awscc.ec2.Subnet {
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
        r#"awscc.ec2.Vpc {
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
        Some("join(separator: String, list: list) -> String")
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

/// Regression for #2200. Same shape as `upstream_state`: a `use { ... }`
/// block has exactly one legal attribute name (`source`), and that's what
/// the LSP must offer at the attribute-name position. Previously the code
/// fell through to `InsideResourceBlock` with an empty resource_type and
/// returned nothing.
#[test]
fn use_block_completes_source_attribute() {
    let provider = test_provider();
    let doc = create_document(
        r#"let mod = use {

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
        "use block should offer 'source' attribute. Got: {:?}",
        labels
    );
    assert_eq!(
        labels.len(),
        1,
        "use block has exactly one attribute; no other candidates should leak in. Got: {:?}",
        labels
    );
}

/// Regression for #2200. The in-line shape (`let m = use { |`) must behave
/// the same as the multi-line shape above — same single `source` candidate,
/// no fallthrough noise.
#[test]
fn use_block_completes_source_attribute_single_line() {
    let provider = test_provider();
    let source = "let mod = use { ";
    let doc = create_document(source);
    let position = Position {
        line: 0,
        character: source.chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"source"),
        "use block (single-line) should offer 'source'. Got: {:?}",
        labels
    );
    assert_eq!(
        labels.len(),
        1,
        "use block has exactly one attribute. Got: {:?}",
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
        ResourceSchema::new("resource").attribute(AttributeSchema::new("condition", map_type));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

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

    let schema = ResourceSchema::new("resource")
        .attribute(AttributeSchema::new("statement", statement_type));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

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
fn let_use_snippet_after_let_binding_omits_let() {
    let provider = test_provider();
    let text = "let m = u";
    let completions = provider.top_level_completions(
        Position {
            line: 0,
            character: text.len() as u32,
        },
        text,
        None,
    );
    let item = find_completion(&completions, "let use");
    let snippet = item.insert_text.as_deref().unwrap_or("");
    assert!(
        !snippet.contains("let "),
        "After existing `let <name> =`, `let use` snippet must not re-emit `let `. Got: {:?}",
        snippet
    );
    assert!(
        snippet.starts_with("use "),
        "Snippet should start directly with `use `. Got: {:?}",
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

#[test]
fn upstream_state_source_suggests_sibling_carina_projects() {
    use std::fs;
    use tempfile::tempdir;

    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Layout:
    //   root/envs/prod/     (current file here)   <- base_path
    //   root/envs/staging/  has a .crn  -> should be suggested as '../staging'
    //   root/envs/prod/sub/ has no .crn -> excluded (not a carina project)
    //   root/modules/web/   has a .crn  -> should be suggested as '../../modules/web'
    //   root/unrelated/     has no .crn -> excluded
    fs::create_dir_all(root.join("envs/prod")).unwrap();
    fs::create_dir_all(root.join("envs/staging")).unwrap();
    fs::create_dir_all(root.join("envs/prod/sub")).unwrap();
    fs::create_dir_all(root.join("modules/web")).unwrap();
    fs::create_dir_all(root.join("unrelated")).unwrap();
    fs::write(root.join("envs/prod/main.crn"), "").unwrap();
    fs::write(root.join("envs/staging/main.crn"), "").unwrap();
    fs::write(root.join("modules/web/main.crn"), "").unwrap();

    let provider = test_provider();
    let source = "let orgs = upstream_state {\n    source = '";
    let doc = create_document(source);
    let position = Position {
        line: 1,
        character: 14,
    };

    let completions = provider.complete(&doc, position, Some(&root.join("envs/prod")));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"'../staging'"),
        "Expected sibling '../staging' suggestion. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"'../../modules/web'"),
        "Expected uncle '../../modules/web' suggestion. Got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|l| l.contains("unrelated")),
        "Should skip directories without .crn files. Got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|l| l.contains("'../prod'")),
        "Should not suggest the current project itself. Got: {:?}",
        labels
    );
}

#[test]
fn upstream_state_source_outside_block_produces_nothing() {
    use std::fs;
    use tempfile::tempdir;

    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("envs/prod")).unwrap();
    fs::create_dir_all(root.join("envs/staging")).unwrap();
    fs::write(root.join("envs/staging/main.crn"), "").unwrap();

    let provider = test_provider();
    // source attribute at top level (not inside upstream_state) should not trigger
    // this new completion.
    let doc = create_document("source = '");
    let position = Position {
        line: 0,
        character: 10,
    };
    let completions = provider.complete(&doc, position, Some(&root.join("envs/prod")));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        !labels.iter().any(|l| l == &"'../staging'"),
        "Outside upstream_state block, source = '...' should not trigger this completion. Got: {:?}",
        labels
    );
}

#[test]
fn upstream_state_source_matches_partial_prefix() {
    use std::fs;
    use tempfile::tempdir;

    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("envs/prod")).unwrap();
    fs::create_dir_all(root.join("envs/staging")).unwrap();
    fs::create_dir_all(root.join("envs/orgs")).unwrap();
    fs::write(root.join("envs/staging/main.crn"), "").unwrap();
    fs::write(root.join("envs/orgs/main.crn"), "").unwrap();

    let provider = test_provider();
    let source = "let orgs = upstream_state {\n    source = '../or";
    let doc = create_document(source);
    let position = Position {
        line: 1,
        character: 19,
    };

    let completions = provider.complete(&doc, position, Some(&root.join("envs/prod")));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"'../orgs'"),
        "Should match sibling starting with 'or'. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"'../staging'"),
        "Should not include sibling that doesn't match the typed prefix. Got: {:?}",
        labels
    );
}

// Regression tests for #1947: top-level snippets must use single quotes.
// The old snippets used double quotes, which meant the upstream_state
// directory completion path (which only looks for `source = '...'`) never
// fired on a freshly inserted snippet.

fn top_level_snippet(text: &str, label: &str) -> String {
    let provider = test_provider();
    let completions = provider.top_level_completions(
        Position {
            line: 0,
            character: text.len() as u32,
        },
        text,
        None,
    );
    find_completion(&completions, label)
        .insert_text
        .clone()
        .unwrap_or_default()
}

#[test]
fn top_level_snippets_use_single_quotes() {
    let cases = [
        ("", "upstream_state"),
        ("let orgs = u", "upstream_state"),
        ("", "let use"),
        ("let m = u", "let use"),
        ("", "import"),
        ("", "removed"),
        ("", "moved"),
    ];
    for (context, label) in cases {
        let snippet = top_level_snippet(context, label);
        assert!(
            !snippet.contains('"'),
            "{label} snippet (context={context:?}) must not contain double quotes. Got: {snippet:?}"
        );
    }
}

#[test]
fn upstream_state_snippet_quotes_source_value() {
    for context in ["", "let orgs = u"] {
        let snippet = top_level_snippet(context, "upstream_state");
        assert!(
            snippet.contains("source = '"),
            "upstream_state snippet (context={context:?}) must wrap `source` value in single quotes. Got: {snippet:?}"
        );
    }
}

#[test]
fn upstream_state_source_suggestions_work_with_double_quotes() {
    // #1947 (2): existing files may contain `source = "..."`. The directory
    // completion should fire regardless of quote style.
    use std::fs;
    use tempfile::tempdir;

    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("envs/prod")).unwrap();
    fs::create_dir_all(root.join("envs/staging")).unwrap();
    fs::write(root.join("envs/prod/main.crn"), "").unwrap();
    fs::write(root.join("envs/staging/main.crn"), "").unwrap();

    let provider = test_provider();
    let source = "let orgs = upstream_state {\n    source = \"";
    let doc = create_document(source);
    let position = Position {
        line: 1,
        character: 14,
    };

    let completions = provider.complete(&doc, position, Some(&root.join("envs/prod")));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    assert!(
        labels.contains(&"'../staging'"),
        "Double-quoted source = \"... should still offer directory suggestions. Got: {:?}",
        labels
    );
}

/// Run the completion provider on `source` (a 2-line snippet whose second
/// line contains `source = ...<cursor>`), find the `../staging` suggestion,
/// and return its `TextEdit`.
fn staging_text_edit(source: &str) -> TextEdit {
    use std::fs;
    use tempfile::tempdir;

    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("envs/prod")).unwrap();
    fs::create_dir_all(root.join("envs/staging")).unwrap();
    fs::write(root.join("envs/prod/main.crn"), "").unwrap();
    fs::write(root.join("envs/staging/main.crn"), "").unwrap();

    let provider = test_provider();
    let doc = create_document(source);
    // Place the cursor at the end of the second line (where the user just typed).
    let last_line = source.lines().next_back().unwrap_or("");
    let position = Position {
        line: 1,
        character: last_line.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&root.join("envs/prod")));
    let staging = completions
        .iter()
        .find(|c| c.label.contains("../staging"))
        .unwrap_or_else(|| panic!("expected a staging suggestion for source {source:?}"));

    match staging.text_edit.as_ref() {
        Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) => edit.clone(),
        other => panic!("expected text_edit::Edit, got {other:?}"),
    }
}

#[test]
fn upstream_state_source_text_edit_inserts_bare_path_single_quote() {
    // #1956: the completion triggers with the cursor already inside an open
    // quote, so the inserted text must be the bare path — otherwise accepting
    // a suggestion produces nested quotes like `'../'../staging''`.
    let edit = staging_text_edit("let orgs = upstream_state {\n    source = '");
    assert_eq!(edit.new_text, "../staging");
}

#[test]
fn upstream_state_source_text_edit_inserts_bare_path_double_quote() {
    let edit = staging_text_edit("let orgs = upstream_state {\n    source = \"");
    assert_eq!(edit.new_text, "../staging");
}

#[test]
fn upstream_state_source_text_edit_range_covers_typed_partial() {
    // #1956: when the user has typed a partial path (`../st`), the text edit
    // must replace the whole partial, not just the trailing word. A plain
    // `insert_text` would let the client infer the range via word boundaries
    // and split on `/`/`.`, yielding `../../staging` instead of `../staging`.
    let source = "let orgs = upstream_state {\n    source = '../st";
    let edit = staging_text_edit(source);
    assert_eq!(edit.new_text, "../staging");
    // `    source = '` occupies cols 0..14, so the partial `../st` starts at
    // col 14 and ends at the cursor at col 19.
    assert_eq!(edit.range.start.line, 1);
    assert_eq!(edit.range.start.character, 14);
    assert_eq!(edit.range.end.line, 1);
    assert_eq!(edit.range.end.character, 19);
}

// =====================================================================
// for-iterable binding completion (#2037)
// =====================================================================

#[test]
fn for_iterable_position_suggests_let_binding() {
    // `for _ in <HERE>`: the in-scope `let` binding should appear.
    let provider = test_provider();
    let source = "let orgs = upstream_state { source = '../organizations' }\nfor name, _ in o";
    let doc = create_document(source);
    let last_line = source.lines().next_back().unwrap_or("");
    let position = Position {
        line: 1,
        character: last_line.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, None);

    assert!(
        completions.iter().any(|c| c.label == "orgs"),
        "expected `orgs` in completions, got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn for_iterable_position_suggests_module_call_binding() {
    // A module call binding (`let x = mymod { ... }`) must also appear.
    let provider = test_provider();
    let source = "\
let mymod = use { source = './mods' }
let inst = mymod { foo = 1 }
for name, _ in i";
    let doc = create_document(source);
    let last_line = source.lines().next_back().unwrap_or("");
    let position = Position {
        line: 2,
        character: last_line.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, None);

    assert!(
        completions.iter().any(|c| c.label == "inst"),
        "expected `inst` in completions, got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn for_binding_declaration_position_does_not_suggest_bindings() {
    // `for <HERE>, _ in orgs.accounts`: cursor is on the loop-variable
    // declaration (before `in`), not on the iterable. Existing bindings
    // must not appear — only the iterable position triggers ForIterable.
    let provider = test_provider();
    // Cursor after `for ` on line 1; the `, _ in orgs.accounts {` tail is
    // already on the line, so the full line is a valid-looking for-header.
    let source = "let orgs = upstream_state { source = '../organizations' }\nfor , _ in orgs.accounts {\n}\n";
    let doc = create_document(source);
    let position = Position {
        line: 1,
        character: 4, // cursor sits right after "for "
    };

    let completions = provider.complete(&doc, position, None);

    assert!(
        !completions.iter().any(|c| c.label == "orgs"),
        "binding-declaration position must not offer existing bindings, got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn for_iterable_offers_binding_declared_in_sibling_file() {
    // `let orgs = upstream_state { ... }` commonly lives in a sibling
    // `backend.crn` while the `for _ in orgs.accounts` sits in `main.crn`.
    // Completion must surface bindings from the whole directory, not just
    // the current buffer.
    let provider = test_provider();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    std::fs::write(
        base.join("backend.crn"),
        "let orgs = upstream_state { source = '../organizations' }\n",
    )
    .unwrap();
    let main = "for _, account_id in or";
    std::fs::write(base.join("main.crn"), main).unwrap();
    let doc = create_document(main);
    let position = Position {
        line: 0,
        character: main.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(base));

    assert!(
        completions.iter().any(|c| c.label == "orgs"),
        "expected `orgs` from sibling backend.crn, got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn for_iterable_fires_immediately_after_in_without_trailing_space() {
    // User types `for name, _ in` and invokes completion before adding a
    // space — the cursor is right after the `n` of `in` and the rest of
    // the line is empty. Must still offer bindings so the popup shows
    // up without forcing the user to press space first.
    let provider = test_provider();
    let source = "let orgs = upstream_state { source = '../organizations' }\nfor name, _ in";
    let doc = create_document(source);
    let last_line = source.lines().next_back().unwrap_or("");
    let position = Position {
        line: 1,
        character: last_line.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, None);

    assert!(
        completions.iter().any(|c| c.label == "orgs"),
        "expected `orgs` even with no space after `in`, got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn for_iterable_after_dot_does_not_trigger() {
    // `for _ in orgs.<HERE>` is field-access, handled by dot completion
    // (#1996) — the ForIterable context must not fire once a `.` appears
    // in the iterable partial.
    let provider = test_provider();
    let source = "let orgs = upstream_state { source = '../organizations' }\nfor name, _ in orgs.";
    let doc = create_document(source);
    let last_line = source.lines().next_back().unwrap_or("");
    let position = Position {
        line: 1,
        character: last_line.chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, None);

    assert!(
        !completions.iter().any(|c| c.label == "orgs"),
        "after-dot position must not echo the binding back, got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

// =====================================================================
// upstream_state exports completion after `<binding>.` (#1996)
// =====================================================================

fn set_up_upstream_project(
    upstream_exports: &str,
    downstream_main: &str,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(upstream.join("exports.crn"), upstream_exports).unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(base.join("main.crn"), downstream_main).unwrap();
    (tmp, base)
}

#[test]
fn upstream_state_dot_completion_in_for_iterable_lists_exports() {
    let provider = test_provider();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  accounts: map(String) = \"x\"\n  region: String = \"ap-northeast-1\"\n}\n",
        "let orgs = upstream_state { source = '../organizations' }\nfor _, id in orgs.\n",
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    // Cursor right after `orgs.` on line 1.
    let position = Position {
        line: 1,
        character: "for _, id in orgs.".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"accounts"),
        "expected `accounts` in completions, got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"region"),
        "expected `region` in completions, got: {:?}",
        labels
    );
}

#[test]
fn upstream_state_dot_completion_cross_file_binding() {
    // `let orgs = ...` declared in a sibling .crn file, referenced from
    // main.crn. The completion must still find the exports.
    let provider = test_provider();
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(
        upstream.join("exports.crn"),
        "exports {\n  accounts: map(String) = \"x\"\n}\n",
    )
    .unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(
        base.join("backend.crn"),
        "let orgs = upstream_state { source = '../organizations' }\n",
    )
    .unwrap();
    let main = "for _, id in orgs.\n";
    std::fs::write(base.join("main.crn"), main).unwrap();
    let doc = create_document(main);
    let position = Position {
        line: 0,
        character: "for _, id in orgs.".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));

    assert!(
        completions.iter().any(|c| c.label == "accounts"),
        "expected `accounts` from sibling-declared upstream_state, got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_dot_completion_text_edit_replaces_partial() {
    // `orgs.acc<cursor>` — the TextEdit must replace the `acc` partial so
    // accepting `accounts` yields `orgs.accounts`, not `orgs.accaccounts`.
    let provider = test_provider();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  accounts: map(String) = \"x\"\n}\n",
        "let orgs = upstream_state { source = '../organizations' }\nfor _, id in orgs.acc\n",
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    let position = Position {
        line: 1,
        character: "for _, id in orgs.acc".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));
    let accounts = completions
        .iter()
        .find(|c| c.label == "accounts")
        .expect("expected `accounts` completion");
    match accounts.text_edit.as_ref() {
        Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) => {
            assert_eq!(edit.new_text, "accounts");
            assert_eq!(edit.range.start.line, 1);
            // `for _, id in orgs.` occupies cols 0..18; `acc` starts at col 18
            assert_eq!(edit.range.start.character, 18);
            assert_eq!(edit.range.end.character, 21);
        }
        other => panic!("expected TextEdit::Edit, got {:?}", other),
    }
}

#[test]
fn upstream_state_dot_completion_missing_source_does_not_crash() {
    // Source directory doesn't exist — must return no completions, not panic.
    let provider = test_provider();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    let main = "let orgs = upstream_state { source = '../does-not-exist' }\nfor _, id in orgs.\n";
    std::fs::write(base.join("main.crn"), main).unwrap();
    let doc = create_document(main);
    let position = Position {
        line: 1,
        character: "for _, id in orgs.".chars().count() as u32,
    };

    let _ = provider.complete(&doc, position, Some(&base));
    // No crash is the entire assertion here.
}

#[test]
fn upstream_state_dot_completion_ignores_unrelated_let_source() {
    // `let orgs = upstream_state { ... }` declares the binding but omits
    // the source on the opening line. The next `let` declares a sibling
    // block with its own `source`. The scanner must not misattribute that
    // sibling's source to `orgs`.
    let provider = test_provider();
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(
        upstream.join("exports.crn"),
        "exports {\n  accounts: map(String) = \"x\"\n}\n",
    )
    .unwrap();
    let other = tmp.path().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("exports.crn"), "exports {\n}\n").unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    // `orgs` opens without `source` on the opening line (malformed or
    // mid-edit), then a different `let` with its own source follows.
    let main = "\
let orgs = upstream_state {
}
let other = upstream_state { source = '../other' }
for _, id in orgs.
";
    std::fs::write(base.join("main.crn"), main).unwrap();
    let doc = create_document(main);
    let position = Position {
        line: 3,
        character: "for _, id in orgs.".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));

    assert!(
        !completions.iter().any(|c| c.label == "accounts"),
        "orgs has no source set, must not resolve to other's exports"
    );
}

// #2128: surface the export's declared `TypeExpr` in the completion
// detail so the user sees `map(AwsAccountId)` instead of the generic
// "export from upstream_state `orgs`". Untyped exports keep the fallback
// phrasing.
#[test]
fn upstream_state_dot_completion_detail_shows_type_expr() {
    let provider = test_provider();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  accounts: map(AwsAccountId) = \"x\"\n  untyped = \"y\"\n}\n",
        "let orgs = upstream_state { source = '../organizations' }\nfor _, id in orgs.\n",
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    let position = Position {
        line: 1,
        character: "for _, id in orgs.".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));
    let accounts = completions
        .iter()
        .find(|c| c.label == "accounts")
        .expect("expected `accounts` in completions");
    let detail = accounts.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("map(AwsAccountId)"),
        "typed export detail must include the TypeExpr rendering, got: {:?}",
        detail
    );

    let untyped = completions
        .iter()
        .find(|c| c.label == "untyped")
        .expect("expected `untyped` in completions");
    let detail = untyped.detail.as_deref().unwrap_or("");
    assert!(
        !detail.contains('('),
        "untyped export detail must not render a bogus type, got: {:?}",
        detail
    );
    assert!(
        detail.contains("orgs"),
        "untyped fallback detail should still identify the binding, got: {:?}",
        detail
    );
}

// =====================================================================
// upstream_state depth-2 completion: dot after the export key (#2041)
// =====================================================================
// `orgs.config.<HERE>` — the export's declared `TypeExpr::Struct` lets
// us recurse into its named fields. `TypeExpr::Map(_)` / `List(_)` /
// scalars have no named children at this depth and produce no
// completions (deferred per #2041 umbrella discussion).

#[test]
fn upstream_state_depth2_dot_completion_lists_struct_fields() {
    let provider = test_provider();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  config: struct { name: String, region: String } = { name = \"x\", region = \"ap-northeast-1\" }\n}\n",
        "let orgs = upstream_state { source = '../organizations' }\nlet x = orgs.config.\n",
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    let position = Position {
        line: 1,
        character: "let x = orgs.config.".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"name") && labels.contains(&"region"),
        "expected struct fields `name` and `region` in completions, got: {:?}",
        labels
    );
}

#[test]
fn upstream_state_depth2_dot_completion_detail_shows_field_type() {
    let provider = test_provider();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  config: struct { region: String, replicas: Int } = { region = \"x\", replicas = 1 }\n}\n",
        "let orgs = upstream_state { source = '../organizations' }\nlet x = orgs.config.\n",
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    let position = Position {
        line: 1,
        character: "let x = orgs.config.".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));
    let region = completions
        .iter()
        .find(|c| c.label == "region")
        .expect("expected `region` field");
    let detail = region.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("String"),
        "field detail must surface the field's TypeExpr, got: {:?}",
        detail
    );
    let replicas = completions
        .iter()
        .find(|c| c.label == "replicas")
        .expect("expected `replicas` field");
    let detail = replicas.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("Int"),
        "Int field detail must surface as `Int`, got: {:?}",
        detail
    );
}

#[test]
fn upstream_state_depth2_dot_completion_text_edit_replaces_partial() {
    // `orgs.config.na<cursor>` — the TextEdit must replace just the
    // `na` partial so accepting `name` lands as `orgs.config.name`,
    // not `orgs.config.naname`.
    let provider = test_provider();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  config: struct { name: String } = { name = \"x\" }\n}\n",
        "let orgs = upstream_state { source = '../organizations' }\nlet x = orgs.config.na\n",
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    let position = Position {
        line: 1,
        character: "let x = orgs.config.na".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));
    let name = completions
        .iter()
        .find(|c| c.label == "name")
        .expect("expected `name` completion");
    match name.text_edit.as_ref() {
        Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) => {
            assert_eq!(edit.new_text, "name");
            // `let x = orgs.config.` occupies cols 0..20; `na` at col 20.
            let line_one = "let x = orgs.config.";
            let prefix_len = line_one.chars().count() as u32;
            assert_eq!(edit.range.start.line, 1);
            assert_eq!(edit.range.start.character, prefix_len);
            assert_eq!(edit.range.end.character, prefix_len + 2);
        }
        other => panic!("expected TextEdit::Edit, got {:?}", other),
    }
}

#[test]
fn upstream_state_depth2_dot_completion_skips_map_typed_export() {
    // `accounts: map(string)` — depth-2 completion has no named keys to
    // suggest (the map's runtime keys aren't part of the type). Falling
    // through to no-op is the documented behavior, deferred per #2041.
    let provider = test_provider();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  accounts: map(String) = \"x\"\n}\n",
        "let orgs = upstream_state { source = '../organizations' }\nlet x = orgs.accounts.\n",
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    let position = Position {
        line: 1,
        character: "let x = orgs.accounts.".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));
    assert!(
        completions.is_empty(),
        "depth-2 completion on a map(_) export must yield nothing, got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_depth2_dot_completion_unknown_key_returns_empty() {
    // `orgs.unknown.<HERE>` where `unknown` is not declared in
    // `exports { }`. The depth-2 detector should decline (no key →
    // no type to descend into) and fall through to nothing useful;
    // we assert no spurious top-level keywords leak out.
    let provider = test_provider();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  config: struct { name: String } = { name = \"x\" }\n}\n",
        "let orgs = upstream_state { source = '../organizations' }\nlet x = orgs.unknown.\n",
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    let position = Position {
        line: 1,
        character: "let x = orgs.unknown.".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, Some(&base));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        !labels.contains(&"name"),
        "unknown key must not bleed in fields from an unrelated export, got: {:?}",
        labels
    );
}

// =====================================================================
// exports block: type-aware value-position completion (#1993)
// =====================================================================

#[test]
fn exports_value_position_excludes_region_pollution() {
    // `exports { accounts: map(string) = { k = <HERE> } }` — the map
    // value type is `string`, which accepts `replace` et al. What must
    // NOT appear is provider-specific literal noise like
    // `aws.Region.*`, whose type is an enum that can't reach a plain
    // `string` entry. Regression guard for the #1993 repro: the popup
    // should be driven by the declared type, never by "everything
    // the provider knows about".
    let provider = test_provider();
    let source = "\
exports {
  accounts: map(String) = {
    k = re
  }
}
";
    let doc = create_document(source);
    let position = Position {
        line: 2,
        character: "    k = re".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        !labels.iter().any(|l| l.contains(".Region.")),
        "no region literals at map(String) value position, got: {:?}",
        labels
    );
}

#[test]
fn exports_top_level_value_excludes_region_pollution() {
    // Same guard at the top level of the exports block. The type
    // filter should keep the popup useful without falling back to the
    // old noisy "all regions + all built-ins" set.
    let provider = test_provider();
    let source = "\
exports {
  id: String = re
}
";
    let doc = create_document(source);
    let position = Position {
        line: 1,
        character: "  id: String = re".chars().count() as u32,
    };

    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        !labels.iter().any(|l| l.contains(".Region.")),
        "no region literals at string exports value position, got: {:?}",
        labels
    );
}

/// At a value position whose attribute type is a `Custom` semantic
/// subtype (e.g. `aws_account_id`), built-in function completions must not
/// be offered — none of them can produce a value of that semantic type.
#[test]
fn builtin_functions_filtered_out_for_custom_semantic_value_position() {
    let provider = test_provider_with_custom_semantic_attr();
    let doc = create_document(
        r#"awscc.sso.Assignment {
  target_id = 
}
"#,
    );
    let position = Position {
        line: 1,
        character: 15,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    for builtin in [
        "concat",
        "cidr_subnet",
        "decrypt",
        "env",
        "flatten",
        "join",
        "keys",
        "length",
        "lookup",
        "lower",
        "upper",
    ] {
        assert!(
            !labels.contains(&builtin),
            "built-in '{}' must NOT appear at aws_account_id value position. Got: {:?}",
            builtin,
            labels
        );
    }
}

/// Completion at a value position must not offer bare enum-tail tokens
/// (e.g. `GROUP`) harvested from namespaced enum values used elsewhere in
/// the file.
#[test]
fn namespaced_enum_tail_tokens_do_not_leak_into_sibling_value_position() {
    let provider = test_provider_with_custom_semantic_attr();
    let doc = create_document(
        r#"awscc.sso.Assignment {
  principal_type = awscc.sso.Assignment.PrincipalType.GROUP
  target_id = 
}
"#,
    );
    let position = Position {
        line: 2,
        character: 15,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        !labels.contains(&"GROUP"),
        "bare 'GROUP' (tail of PrincipalType.GROUP) must not leak into target_id value position. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"USER"),
        "bare 'USER' must not leak into target_id value position. Got: {:?}",
        labels
    );
}

/// Namespaced `StringEnum` completions must offer the fully-qualified
/// form (`awscc.sso.Assignment.PrincipalType.GROUP`), not the bare tail
/// (`GROUP`). The bare form is accepted by the DSL resolver but causes
/// the sibling-attribute leak exercised above.
#[test]
fn namespaced_enum_completions_offer_full_form_not_bare_tail() {
    let provider = test_provider_with_custom_semantic_attr();
    let doc = create_document(
        r#"awscc.sso.Assignment {
  principal_type = 
}
"#,
    );
    let position = Position {
        line: 1,
        character: 19,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels
            .iter()
            .any(|l| l.contains("awscc.sso.Assignment.PrincipalType.GROUP")),
        "expected fully-qualified 'awscc.sso.Assignment.PrincipalType.GROUP' completion, got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"GROUP"),
        "bare 'GROUP' must not be offered for namespaced enum — use fully-qualified form. Got: {:?}",
        labels
    );
}

/// A plain `String` attribute still sees string-returning built-ins, and
/// list-returning ones stay filtered out — guards against over-filtering.
#[test]
fn string_returning_builtins_still_offered_for_plain_string_attr() {
    let provider = test_provider_single_attr();
    let doc = create_document(
        r#"test.foo.bar {
  attr =
}
"#,
    );
    let position = Position {
        line: 1,
        character: 9,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    for builtin in ["join", "upper", "lower"] {
        assert!(
            labels.contains(&builtin),
            "string-returning '{}' must appear at plain String attr. Got: {:?}",
            builtin,
            labels
        );
    }
    assert!(
        !labels.contains(&"concat"),
        "list-returning 'concat' must not appear at String attr. Got: {:?}",
        labels
    );
}

#[test]
fn for_loop_binding_not_offered_at_incompatible_enum_attribute() {
    // `for _, account_id in orgs.accounts` where `orgs.accounts` is
    // `map(AwsAccountId)`. Inside the body, `principal_type` is a
    // `StringEnum` — `account_id` (semantic type `aws_account_id`)
    // can't type-check as `PrincipalType`, so it must be filtered out.
    let provider = test_provider_with_custom_semantic_attr();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  accounts: map(AwsAccountId) = \"x\"\n}\n",
        r#"let orgs = upstream_state { source = '../organizations' }
for _, account_id in orgs.accounts {
  awscc.sso.Assignment {
    principal_type =
  }
}
"#,
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    let position = Position {
        line: 3,
        character: "    principal_type = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(&base));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        !labels.contains(&"account_id"),
        "for-loop binding 'account_id' (aws_account_id) must not be offered at a PrincipalType enum attribute. Got: {:?}",
        labels
    );
}

#[test]
fn for_loop_binding_offered_at_matching_custom_attribute() {
    // Same repro as above but the cursor is on `target_id`, which is
    // `Custom{aws_account_id}`. The for-loop binding's element type
    // matches, so it must appear.
    let provider = test_provider_with_custom_semantic_attr();
    let (_tmp, base) = set_up_upstream_project(
        "exports {\n  accounts: map(AwsAccountId) = \"x\"\n}\n",
        r#"let orgs = upstream_state { source = '../organizations' }
for _, account_id in orgs.accounts {
  awscc.sso.Assignment {
    target_id =
  }
}
"#,
    );
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let doc = create_document(&main_src);
    let position = Position {
        line: 3,
        character: "    target_id = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(&base));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"account_id"),
        "for-loop binding 'account_id' must appear at a matching `aws_account_id` attribute. Got: {:?}",
        labels
    );
}

#[test]
fn for_loop_binding_without_resolvable_iterable_falls_back_to_unconditional() {
    // If the iterable can't be resolved (e.g. no upstream_state,
    // unknown binding), preserve the old permissive behaviour so the
    // user still gets autocomplete on the name.
    let provider = test_provider_with_custom_semantic_attr();
    let doc = create_document(
        r#"for _, account_id in unknown_source.accounts {
  awscc.sso.Assignment {
    principal_type =
  }
}
"#,
    );
    let position = Position {
        line: 2,
        character: "    principal_type = ".chars().count() as u32,
    };
    // No base path → no upstream resolution possible. The binding still
    // shows (permissive fallback).
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"account_id"),
        "unresolvable iterable should preserve the permissive fallback. Got: {:?}",
        labels
    );
}

/// Top-level `exports { region: String = ▉ }` must offer string-
/// returning built-in helpers. The annotation sits on the same line;
/// the LSP previously returned nothing because the type was ignored.
#[test]
fn exports_top_level_string_position_offers_string_builtins() {
    let provider = test_provider();
    let doc = create_document(
        r#"exports {
  region: String =
}
"#,
    );
    let position = Position {
        line: 1,
        character: "  region: String = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    for expected in ["join", "lower", "upper", "trim"] {
        assert!(
            labels.contains(&expected),
            "string-returning built-in '{}' must appear at `exports {{ region: String = ▉ }}`. Got: {:?}",
            expected,
            labels
        );
    }
}

/// Non-string built-ins must not appear when the declared type is
/// `string` — filter correctness.
#[test]
fn exports_top_level_string_position_excludes_non_string_builtins() {
    let provider = test_provider();
    let doc = create_document(
        r#"exports {
  region: String =
}
"#,
    );
    let position = Position {
        line: 1,
        character: "  region: String = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    for banned in ["flatten", "keys"] {
        assert!(
            !labels.contains(&banned),
            "list-returning '{}' must not appear at a `string` exports position. Got: {:?}",
            banned,
            labels
        );
    }
}

/// Inside a nested `{ ... }` block that is the value of
/// `accounts: map(T) = { ... }`, the element type is `T` and only
/// `T`-compatible candidates should appear. Guards against the repro
/// where `registry_dev = re|` suggests `replace`.
#[test]
fn exports_map_value_position_filters_by_element_type() {
    let provider = test_provider();
    let doc = create_document(
        r#"exports {
  accounts: map(AwsAccountId) = {
    registry_prod = x
  }
}
"#,
    );
    // Cursor on the `registry_prod = ` line after `= `.
    let position = Position {
        line: 2,
        character: "    registry_prod = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    // `replace` returns `string`; `aws_account_id` is a Custom
    // semantic subtype — no built-in produces it, so `replace` must
    // not appear.
    assert!(
        !labels.contains(&"replace"),
        "`replace` must not be suggested inside a `map(AwsAccountId)` value position. Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"join"),
        "`join` must not be suggested inside a `map(AwsAccountId)` value position. Got: {:?}",
        labels
    );
}

/// Fall back to empty when the annotation can't be resolved —
/// unknown entry, missing colon, etc. Silent beats noisy; this
/// guards against a regression that would dump every built-in.
#[test]
fn exports_value_without_type_annotation_returns_empty() {
    let provider = test_provider();
    let doc = create_document(
        r#"exports {
  mystery =
}
"#,
    );
    let position = Position {
        line: 1,
        character: "  mystery = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, None);
    // Empty is acceptable; what's NOT acceptable is dumping every
    // built-in.
    assert!(
        !completions.iter().any(|c| c.label == "replace"),
        "no annotation must not surface `replace`. Got: {:?}",
        completions.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

/// Real-world acceptance: the repro `exports.crn` used by
/// `carina-rs/infra/aws/management/organizations/`. Completion at the
/// `registry_dev = re|` position must not offer `replace` (it returns
/// a plain String, the map element type is `aws_account_id`).
#[test]
fn exports_map_value_real_world_shape_filters_unrelated_builtins() {
    let provider = test_provider();
    let source = "\
exports {
  accounts: map(AwsAccountId) = {
    registry_prod = registry_prod.account_id
    registry_dev  = re
  }
}
";
    let doc = create_document(source);
    let position = Position {
        line: 3,
        character: "  registry_dev  = re".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    for banned in ["replace", "join", "lower", "upper", "trim", "env", "lookup"] {
        assert!(
            !labels.contains(&banned),
            "String-returning '{}' must not appear in a `map(AwsAccountId)` value position. Got: {:?}",
            banned,
            labels
        );
    }
}

/// Resource-ref candidates: a binding whose resource schema exposes an
/// attribute of the target type should appear as `<binding>.<attr>`
/// at the value position. Guards the "offer matching refs" half of
/// the issue spec.
#[test]
fn exports_map_value_offers_matching_resource_refs() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    fn validate_noop(_v: &carina_core::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let account_id = AttributeType::Custom {
        semantic_name: Some("AwsAccountId".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: legacy_validator(validate_noop),
        namespace: None,
        to_dsl: None,
    };
    let schema = ResourceSchema::new("organizations.account")
        .attribute(AttributeSchema::new("account_id", account_id));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![]);

    let source = "\
let registry_prod = awscc.organizations.account {
  name = 'prod'
}

exports {
  accounts: map(AwsAccountId) = {
    registry_prod = re
  }
}
";
    let doc = create_document(source);
    let position = Position {
        line: 6,
        character: "    registry_prod = re".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"registry_prod.account_id"),
        "expected `registry_prod.account_id` at `map(AwsAccountId)` value position. Got: {:?}",
        labels
    );
}

/// List element position: `exports { items: list(string) = [▉] }` —
/// string-returning built-ins should appear inside the list.
#[test]
fn exports_list_value_position_filters_by_element_type() {
    let provider = test_provider();
    let doc = create_document(
        r#"exports {
  items: list(String) = [
    re
  ]
}
"#,
    );
    // Cursor on `    re` line.
    let position = Position {
        line: 2,
        character: "    re".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    // The list-element case is harder to detect from text alone
    // (square brackets don't change brace_depth). The conservative
    // acceptable behaviour is either (a) type-filtered suggestions or
    // (b) nothing — but never a regional/builtin dump. Guard the
    // never-dump half.
    assert!(
        !labels.iter().any(|l| l.contains(".Region.")),
        "list-element position must not regress into region dump. Got: {:?}",
        labels
    );
}

/// Repro: in the real `organizations/exports.crn`, the user types
/// `registry_dev = r|` at depth 2 (inside the `map(AwsAccountId)`
/// body with a preceding entry on the line above). LSP currently
/// returns zero LSP items — VSCode falls back to word-based. We
/// expect matching resource-ref refs.
#[test]
fn exports_map_value_multiple_entries_returns_refs() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    fn validate_noop(_v: &carina_core::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let account_id = AttributeType::Custom {
        semantic_name: Some("AwsAccountId".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: legacy_validator(validate_noop),
        namespace: None,
        to_dsl: None,
    };
    let schema = ResourceSchema::new("organizations.account")
        .attribute(AttributeSchema::new("account_id", account_id));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![]);

    let source = "\
let registry_prod = awscc.organizations.account {
  name = 'prod'
}

let registry_dev = awscc.organizations.account {
  name = 'dev'
}

exports {
  accounts: map(AwsAccountId) = {
    registry_prod = r
    registry_dev = r
  }
}
";
    let doc = create_document(source);
    // Cursor on `    registry_dev = r|` — line 11 (0-indexed).
    let position = Position {
        line: 11,
        character: "    registry_dev = r".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, None);
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"registry_dev.account_id"),
        "expected `registry_dev.account_id` with earlier `registry_prod = ...` sibling present. Got: {:?}",
        labels
    );
}

/// Sibling-file case: `exports.crn` references bindings declared in
/// `main.crn` — the real shape used by `carina-rs/infra`. Completion
/// must surface those bindings as `<binding>.<attr>` candidates
/// scanning sibling `.crn` files, not just the current buffer.
#[test]
fn exports_map_value_includes_bindings_from_sibling_files() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    fn validate_noop(_v: &carina_core::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let account_id = AttributeType::Custom {
        semantic_name: Some("AwsAccountId".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: legacy_validator(validate_noop),
        namespace: None,
        to_dsl: None,
    };
    let schema = ResourceSchema::new("organizations.account")
        .attribute(AttributeSchema::new("account_id", account_id));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![]);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    std::fs::write(
        base.join("main.crn"),
        "let registry_prod = awscc.organizations.account {\n  name = 'prod'\n}\n",
    )
    .unwrap();
    let exports_src = "\
exports {
  accounts: map(AwsAccountId) = {
    registry_prod = r
  }
}
";
    std::fs::write(base.join("exports.crn"), exports_src).unwrap();

    let doc = create_document(exports_src);
    let position = Position {
        line: 2,
        character: "    registry_prod = r".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(base));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"registry_prod.account_id"),
        "cross-file binding must be offered. Got: {:?}",
        labels
    );
}

/// Sibling-file case for Custom-type resource-ref completion inside a
/// resource block value. When the user types
/// `account_id = ▉` in `other.crn` and `registry_prod` is declared in
/// `main.crn`, the completion list must include
/// `registry_prod.account_id`.
#[test]
fn custom_type_value_ref_includes_sibling_file_bindings() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    fn validate_noop(_v: &carina_core::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let account_id = AttributeType::Custom {
        semantic_name: Some("AwsAccountId".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: legacy_validator(validate_noop),
        namespace: None,
        to_dsl: None,
    };
    let account_schema = ResourceSchema::new("organizations.account")
        .attribute(AttributeSchema::new("account_id", account_id.clone()));
    let consumer_schema = ResourceSchema::new("organizations.policy_target_attachment")
        .attribute(AttributeSchema::new("target_id", account_id));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", account_schema);
    schemas.insert("awscc", consumer_schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![]);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    std::fs::write(
        base.join("main.crn"),
        "let registry_prod = awscc.organizations.account {\n  name = 'prod'\n}\n",
    )
    .unwrap();
    let other_src = "\
let attach = awscc.organizations.policy_target_attachment {
  target_id =
}
";
    std::fs::write(base.join("other.crn"), other_src).unwrap();

    let doc = create_document(other_src);
    // Cursor at end of `  target_id = `
    let position = Position {
        line: 1,
        character: "  target_id = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(base));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"registry_prod.account_id"),
        "cross-file Custom-type ref must be offered. Got: {:?}",
        labels
    );
}

/// Sibling-file case for `arguments` parameter completion. Module shapes
/// that declare `arguments { ... }` in a dedicated `arguments.crn` must
/// still surface those parameters when the user is editing `main.crn`.
#[test]
fn argument_parameters_include_sibling_file_args() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("name", AttributeType::String));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![]);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    std::fs::write(
        base.join("arguments.crn"),
        "arguments {\n  stage_name: String\n}\n",
    )
    .unwrap();
    let main_src = "\
let b = awscc.s3.Bucket {
  name =
}
";
    std::fs::write(base.join("main.crn"), main_src).unwrap();

    let doc = create_document(main_src);
    let position = Position {
        line: 1,
        character: "  name = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(base));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"stage_name"),
        "cross-file arguments parameter must be offered. Got: {:?}",
        labels
    );
}

/// Sibling-file case for `binding.` dot-completion. Typing
/// `account_id = registry_prod.` in `other.crn` must list
/// `registry_prod`'s attributes even when the binding is declared in
/// `main.crn`.
#[test]
fn binding_dot_completion_resolves_sibling_file_binding() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    fn validate_noop(_v: &carina_core::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let account_id = AttributeType::Custom {
        semantic_name: Some("AwsAccountId".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: legacy_validator(validate_noop),
        namespace: None,
        to_dsl: None,
    };
    let account_schema = ResourceSchema::new("organizations.account")
        .attribute(AttributeSchema::new("account_id", account_id.clone()));
    let consumer_schema = ResourceSchema::new("organizations.policy_target_attachment")
        .attribute(AttributeSchema::new("target_id", account_id));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", account_schema);
    schemas.insert("awscc", consumer_schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![]);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    std::fs::write(
        base.join("main.crn"),
        "let registry_prod = awscc.organizations.account {\n  name = 'prod'\n}\n",
    )
    .unwrap();
    let other_src = "\
let attach = awscc.organizations.policy_target_attachment {
  target_id = registry_prod.
}
";
    std::fs::write(base.join("other.crn"), other_src).unwrap();

    let doc = create_document(other_src);
    let position = Position {
        line: 1,
        character: "  target_id = registry_prod.".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(base));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"registry_prod.account_id"),
        "dot-completion must resolve cross-file binding. Got: {:?}",
        labels
    );
}

/// Issue #2353: in attribute value position, an `upstream_state` binding's
/// typed `exports { ... }` field whose type matches the target attribute
/// must surface as `<binding>.<export>` — symmetric with the existing
/// resource-binding Custom-type matching path.
#[test]
fn upstream_state_export_suggested_in_value_position() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("foo.bar")
        .attribute(AttributeSchema::new("attr", AttributeType::String));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![]);

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("network")).unwrap();
    std::fs::create_dir_all(root.join("web")).unwrap();
    std::fs::write(
        root.join("network/main.crn"),
        "exports {\n  vpc_id: String = 'vpc-abc'\n}\n",
    )
    .unwrap();
    let main_src = "\
let network = upstream_state {
  source = '../network'
}

test.foo.bar {
  attr =
}
";
    std::fs::write(root.join("web/main.crn"), main_src).unwrap();

    let doc = create_document(main_src);
    let position = Position {
        line: 5,
        character: "  attr = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(&root.join("web")));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"network.vpc_id"),
        "upstream_state export with matching type must be offered as '<binding>.<export>'. Got: {:?}",
        labels
    );
}

/// Issue #2353: an `upstream_state` export whose declared type does not
/// match the target attribute must NOT be offered. Mirrors the
/// type-filtering behavior of the existing Custom-type matching path
/// for resource bindings.
#[test]
fn upstream_state_export_filtered_by_attribute_type() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("foo.bar")
        .attribute(AttributeSchema::new("attr", AttributeType::String));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![]);

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("network")).unwrap();
    std::fs::create_dir_all(root.join("web")).unwrap();
    std::fs::write(
        root.join("network/main.crn"),
        "exports {\n  vpc_id: String = 'vpc-abc'\n  count: Int = 3\n}\n",
    )
    .unwrap();
    let main_src = "\
let network = upstream_state {
  source = '../network'
}

test.foo.bar {
  attr =
}
";
    std::fs::write(root.join("web/main.crn"), main_src).unwrap();

    let doc = create_document(main_src);
    let position = Position {
        line: 5,
        character: "  attr = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(&root.join("web")));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    // Positive companion: prove the upstream pass actually ran by asserting
    // the matching-type export is offered. Otherwise the negative assertion
    // could pass for the wrong reason (pass never executed at all).
    assert!(
        labels.contains(&"network.vpc_id"),
        "matching-type export must be offered (sanity: confirms the upstream pass ran). Got: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"network.count"),
        "upstream_state export with non-matching type must not be offered. Got: {:?}",
        labels
    );
}

/// Issue #2353: exercise the directory-scoped path that matters in real
/// `infra/` projects — exports declared in a sibling `exports.crn`, not
/// `main.crn` — and confirm two distinct `upstream_state` bindings each
/// surface their own typed exports independently. Without this fixture
/// a future regression that swapped `parse_directory` for a single-file
/// read would silently break production while the prior tests stayed
/// green.
#[test]
fn upstream_state_export_multiple_bindings_and_sibling_exports_file() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("foo.bar")
        .attribute(AttributeSchema::new("attr", AttributeType::String));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![]);

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("network")).unwrap();
    std::fs::create_dir_all(root.join("shared")).unwrap();
    std::fs::create_dir_all(root.join("web")).unwrap();
    // network: exports split into a sibling `exports.crn`, `main.crn` empty.
    std::fs::write(root.join("network/main.crn"), "").unwrap();
    std::fs::write(
        root.join("network/exports.crn"),
        "exports {\n  vpc_id: String = 'vpc-abc'\n}\n",
    )
    .unwrap();
    // shared: exports in `main.crn`.
    std::fs::write(
        root.join("shared/main.crn"),
        "exports {\n  hosted_zone_id: String = 'Z123'\n}\n",
    )
    .unwrap();
    let main_src = "\
let network = upstream_state {
  source = '../network'
}

let shared = upstream_state {
  source = '../shared'
}

test.foo.bar {
  attr =
}
";
    std::fs::write(root.join("web/main.crn"), main_src).unwrap();

    let doc = create_document(main_src);
    let position = Position {
        line: 9,
        character: "  attr = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(&root.join("web")));
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"network.vpc_id"),
        "exports declared in sibling `exports.crn` must be discovered. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"shared.hosted_zone_id"),
        "second upstream_state binding must surface its exports independently. Got: {:?}",
        labels
    );
}

/// Issue #2358: a `: String` upstream export must NOT be offered as a
/// candidate at a receiver typed `Custom { semantic_name: Some(_) }`.
/// Pinned alongside the validation-side fix because completion shares
/// the same `is_type_expr_compatible_with_schema` predicate — a future
/// edit that loosens the predicate would silently regress the popup
/// even if validation still catches the bad code at apply time.
#[test]
fn upstream_state_string_export_not_offered_to_specific_custom_receiver() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    fn noop(_v: &carina_core::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let vpc_id_type = AttributeType::Custom {
        semantic_name: Some("VpcId".to_string()),
        pattern: None,
        length: None,
        base: Box::new(AttributeType::String),
        validate: legacy_validator(noop),
        namespace: None,
        to_dsl: None,
    };
    let schema = ResourceSchema::new("ec2.SecurityGroup")
        .attribute(AttributeSchema::new("vpc_id", vpc_id_type));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);
    let provider =
        CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![]);

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("network")).unwrap();
    std::fs::create_dir_all(root.join("web")).unwrap();
    std::fs::write(
        root.join("network/main.crn"),
        "exports {\n  vpc_id: String = 'vpc-abc'\n}\n",
    )
    .unwrap();
    let main_src = "\
let network = upstream_state {
  source = '../network'
}

awscc.ec2.SecurityGroup {
  vpc_id =
}
";
    std::fs::write(root.join("web/main.crn"), main_src).unwrap();

    let doc = create_document(main_src);
    let position = Position {
        line: 5,
        character: "  vpc_id = ".chars().count() as u32,
    };
    let completions = provider.complete(&doc, position, Some(&root.join("web")));
    let labels: Vec<String> = completions.iter().map(|c| c.label.clone()).collect();
    assert!(
        !labels.iter().any(|l| l == "network.vpc_id"),
        "generic-String export must not be offered at a specific Custom receiver. Got: {:?}",
        labels
    );
}
