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

    let list_enum = AttributeType::List(Box::new(AttributeType::StringEnum {
        name: "Protocol".to_string(),
        values: vec!["tcp".to_string(), "udp".to_string(), "icmp".to_string()],
        namespace: None,
        to_dsl: None,
    }));

    let schema = ResourceSchema::new("test.list.resource")
        .attribute(AttributeSchema::new("protocols", list_enum));

    let mut schemas = HashMap::new();
    schemas.insert("test.list.resource".to_string(), schema);

    let provider = CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![]);

    let completions =
        provider.completions_for_type(&AttributeType::List(Box::new(AttributeType::StringEnum {
            name: "Protocol".to_string(),
            values: vec!["tcp".to_string(), "udp".to_string(), "icmp".to_string()],
            namespace: None,
            to_dsl: None,
        })));

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
    // Should also include env()
    assert!(
        labels.contains(&"env"),
        "Should offer 'env' for Union. Got: {:?}",
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
