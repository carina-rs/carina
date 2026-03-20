use super::*;

#[test]
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

    let provider = CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![]);

    let completions =
        provider.completions_for_type(&AttributeType::list(AttributeType::StringEnum {
            name: "Protocol".to_string(),
            values: vec!["tcp".to_string(), "udp".to_string(), "icmp".to_string()],
            namespace: None,
            to_dsl: None,
        }));

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"\"tcp\""),
        "Should offer tcp as completion for List(StringEnum). Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"\"udp\""),
        "Should offer udp as completion for List(StringEnum). Got: {:?}",
        labels
    );
}

#[test]
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
    let completions = provider.completions_for_type(&AttributeType::Union(vec![
        AttributeType::StringEnum {
            name: "Mode".to_string(),
            values: vec!["active".to_string(), "passive".to_string()],
            namespace: None,
            to_dsl: None,
        },
        AttributeType::Bool,
    ]));

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

    // Should have StringEnum completions
    assert!(
        labels.contains(&"\"active\""),
        "Should offer 'active' from StringEnum member. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"\"passive\""),
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
    let completions = provider.completions_for_type(&AttributeType::Union(vec![
        AttributeType::Bool,
        AttributeType::Bool,
    ]));

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
    let completions =
        provider.completions_for_type(&AttributeType::Map(Box::new(AttributeType::Bool)));

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

    let provider = CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![]);

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
