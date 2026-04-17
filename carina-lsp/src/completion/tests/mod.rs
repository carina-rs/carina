use std::sync::Arc;

use super::*;
use crate::document::Document;
use carina_core::parser::ProviderContext;
use carina_core::provider::ProviderFactory;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

mod basic;
mod extended;

pub(super) fn create_document(content: &str) -> Document {
    Document::new(content.to_string(), Arc::new(ProviderContext::default()))
}

pub(super) fn find_completion<'a>(
    completions: &'a [CompletionItem],
    label: &str,
) -> &'a CompletionItem {
    completions
        .iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("completion '{}' not found", label))
}

/// Assert that every item's label and insert_text are wrapped with `quote`.
pub(super) fn assert_all_wrapped(items: &[CompletionItem], quote: char, kind: &str) {
    assert!(!items.is_empty(), "Expected at least one {kind} completion");
    for item in items {
        assert!(
            item.label.starts_with(quote) && item.label.ends_with(quote),
            "{kind} completion label must be wrapped with `{quote}`. Got: {:?}",
            item.label
        );
        let text = item.insert_text.as_deref().unwrap_or("");
        assert!(
            text.starts_with(quote) && text.ends_with(quote),
            "{kind} completion insert_text must be wrapped with `{quote}`. Got: {:?}",
            text
        );
    }
}

pub(super) fn test_provider() -> CompletionProvider {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![];
    let schemas = Arc::new(carina_core::provider::collect_schemas(&factories));
    let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
    let region_completions: Vec<CompletionValue> = factories
        .iter()
        .flat_map(|f| f.config_completions().remove("region").unwrap_or_default())
        .collect();
    let custom_type_names: Vec<String> = vec![];
    CompletionProvider::new(
        schemas,
        provider_names,
        region_completions,
        custom_type_names,
    )
}

pub(super) fn test_provider_with_custom_types() -> CompletionProvider {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![];
    let schemas = Arc::new(carina_core::provider::collect_schemas(&factories));
    let provider_names: Vec<String> = vec!["awscc".to_string()];
    let region_completions: Vec<CompletionValue> = vec![];
    let custom_type_names = vec![
        "arn".to_string(),
        "iam_policy_arn".to_string(),
        "availability_zone".to_string(),
    ];
    CompletionProvider::new(
        schemas,
        provider_names,
        region_completions,
        custom_type_names,
    )
}

pub(super) fn test_provider_with_block_name_nested() -> CompletionProvider {
    // Nested struct where a StructField has block_name set.
    // Schema: config -> transitions (List<Struct>, block_name="transition") -> each has "days" + "storage_class"
    let transition_struct = AttributeType::Struct {
        name: "Transition".to_string(),
        fields: vec![
            StructField::new("days", AttributeType::Int),
            StructField::new("storage_class", AttributeType::String),
        ],
    };

    let config_struct = AttributeType::Struct {
        name: "Config".to_string(),
        fields: vec![
            StructField::new("transitions", AttributeType::list(transition_struct))
                .with_block_name("transition"),
            StructField::new("enabled", AttributeType::Bool),
        ],
    };

    let schema = ResourceSchema::new("test.block.resource")
        .attribute(AttributeSchema::new("config", config_struct));

    let mut schemas = HashMap::new();
    schemas.insert("test.block.resource".to_string(), schema);

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

pub(super) fn test_provider_with_nested_structs() -> CompletionProvider {
    let inner_struct = AttributeType::Struct {
        name: "InnerStruct".to_string(),
        fields: vec![
            StructField::new("leaf_field", AttributeType::String),
            StructField::new("leaf_bool", AttributeType::Bool),
        ],
    };

    let outer_struct = AttributeType::Struct {
        name: "OuterStruct".to_string(),
        fields: vec![
            StructField::new("inner", inner_struct),
            StructField::new("outer_field", AttributeType::String),
        ],
    };

    let schema = ResourceSchema::new("test.nested.resource")
        .attribute(AttributeSchema::new("outer", outer_struct));

    let mut schemas = HashMap::new();
    schemas.insert("test.nested.resource".to_string(), schema);

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

/// Minimal provider with a single resource type `test.foo.bar` that has one
/// `attr` string attribute. Enough to exercise value-position completions
/// without needing real provider schemas.
pub(super) fn test_provider_single_attr() -> CompletionProvider {
    let schema = ResourceSchema::new("test.foo.bar")
        .attribute(AttributeSchema::new("attr", AttributeType::String));
    let mut schemas = HashMap::new();
    schemas.insert("test.foo.bar".to_string(), schema);
    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

/// Provider with StringEnum that has name but no namespace (simulates WASM provider).
pub(super) fn test_provider_with_nameless_enum() -> CompletionProvider {
    // Top-level attribute with StringEnum (no namespace)
    let status_enum = AttributeType::StringEnum {
        name: "VersioningStatus".to_string(),
        values: vec!["Enabled".to_string(), "Suspended".to_string()],
        namespace: None,
        to_dsl: None,
    };

    // Nested struct field with StringEnum (no namespace)
    let versioning_struct = AttributeType::Struct {
        name: "VersioningConfiguration".to_string(),
        fields: vec![StructField::new("status", status_enum.clone())],
    };

    let schema = ResourceSchema::new("awscc.s3.bucket")
        .attribute(AttributeSchema::new("versioning_status", status_enum))
        .attribute(AttributeSchema::new(
            "versioning_configuration",
            versioning_struct,
        ));

    let mut schemas = HashMap::new();
    schemas.insert("awscc.s3.bucket".to_string(), schema);

    CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![])
}
