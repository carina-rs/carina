use super::*;
use crate::document::Document;
use carina_core::provider::{self as provider_mod, ProviderFactory};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

mod basic;
mod extended;

pub(super) fn create_document(content: &str) -> Document {
    Document::new(content.to_string())
}

pub(super) fn test_provider() -> CompletionProvider {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![
        Box::new(carina_provider_aws::AwsProviderFactory),
        Box::new(carina_provider_awscc::AwsccProviderFactory),
    ];
    let schemas = Arc::new(provider_mod::collect_schemas(&factories));
    let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
    let region_completions: Vec<CompletionValue> = factories
        .iter()
        .flat_map(|f| f.region_completions())
        .collect();
    CompletionProvider::new(schemas, provider_names, region_completions)
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

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![])
}
