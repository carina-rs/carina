use std::sync::Arc;

use super::*;
use crate::document::Document;
use carina_core::parser::ProviderContext;
use carina_core::provider::{self as provider_mod, ProviderFactory};

mod basic;
mod extended;

pub(super) fn create_document(content: &str) -> Document {
    Document::new(content.to_string(), Arc::new(ProviderContext::default()))
}

pub(super) fn test_engine() -> DiagnosticEngine {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![];
    let schemas = Arc::new(provider_mod::collect_schemas(&factories));
    let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
    let factories = Arc::new(factories);
    DiagnosticEngine::new(schemas, provider_names, factories)
}

pub(super) fn test_engine_reversed() -> DiagnosticEngine {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![];
    let schemas = Arc::new(provider_mod::collect_schemas(&factories));
    let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
    let factories = Arc::new(factories);
    DiagnosticEngine::new(schemas, provider_names, factories)
}

pub(super) fn test_engine_with_nested_structs() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let inner_struct = AttributeType::list(AttributeType::Struct {
        name: "InnerStruct".to_string(),
        fields: vec![
            StructField::new("leaf_field", AttributeType::String),
            StructField::new("leaf_int", AttributeType::Int),
        ],
    });

    let outer_struct = AttributeType::list(AttributeType::Struct {
        name: "OuterStruct".to_string(),
        fields: vec![
            StructField::new("inner", inner_struct),
            StructField::new("outer_field", AttributeType::String),
        ],
    });

    let schema = ResourceSchema::new("test.nested.resource")
        .attribute(AttributeSchema::new("outer", outer_struct));

    let mut schemas = HashMap::new();
    schemas.insert("test.nested.resource".to_string(), schema);

    DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    )
}

pub(super) fn test_engine_with_block_name_nested() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

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

    DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    )
}

pub(super) fn custom_engine(
    schemas: HashMap<String, carina_core::schema::ResourceSchema>,
) -> DiagnosticEngine {
    let provider_names: Vec<String> = schemas
        .keys()
        .filter_map(|k| k.split('.').next())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    DiagnosticEngine::new(Arc::new(schemas), provider_names, Arc::new(vec![]))
}
