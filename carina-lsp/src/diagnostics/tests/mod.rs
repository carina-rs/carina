use std::sync::Arc;

use super::*;
use crate::document::Document;
use carina_core::parser::ProviderContext;
use carina_core::provider::{self as provider_mod, ProviderFactory};
use carina_core::schema::SchemaRegistry;

mod basic;
mod extended;

pub(super) fn create_document(content: &str) -> Document {
    Document::new(content.to_string(), Arc::new(ProviderContext::default()))
}

pub(super) fn test_engine() -> DiagnosticEngine {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![];
    let schemas = Arc::new(provider_mod::collect_schemas(&factories));
    let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
    DiagnosticEngine::new(schemas, provider_names, Arc::new(vec![]))
}

pub(super) fn test_engine_reversed() -> DiagnosticEngine {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![];
    let schemas = Arc::new(provider_mod::collect_schemas(&factories));
    let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
    DiagnosticEngine::new(schemas, provider_names, Arc::new(vec![]))
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

    let schema = ResourceSchema::new("nested.resource")
        .attribute(AttributeSchema::new("outer", outer_struct));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

    DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    )
}

/// Engine carrying a managed `aws.acm.Certificate` schema with `status`
/// attribute — the canonical target for `wait` diagnostics tests.
pub(super) fn test_engine_with_wait_target() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let schema = ResourceSchema::new("acm.Certificate")
        .attribute(AttributeSchema::new("domain_name", AttributeType::String))
        .attribute(AttributeSchema::new("status", AttributeType::String));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("aws", schema);
    DiagnosticEngine::new(Arc::new(schemas), vec!["aws".to_string()], Arc::new(vec![]))
}

pub(super) fn test_engine_with_enum_attr() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mode_enum = AttributeType::StringEnum {
        name: "Mode".to_string(),
        values: vec!["fast".to_string(), "slow".to_string()],
        identity: None,
        dsl_aliases: vec![],
    };

    let schema =
        ResourceSchema::new("r.mode_holder").attribute(AttributeSchema::new("mode", mode_enum));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

    DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    )
}

/// Variant of `test_engine_with_enum_attr` whose `mode` attribute is a
/// namespaced enum — exercises the shape-mismatch branch of #2094 where
/// the LSP diagnostic should say "got a string literal" for
/// `mode = "aaa"`.
pub(super) fn test_engine_with_namespaced_enum_attr() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mode_enum = AttributeType::StringEnum {
        name: "Mode".to_string(),
        values: vec!["fast".to_string(), "slow".to_string()],
        identity: Some(carina_core::schema::string_enum_identity(
            "Mode",
            Some("test.r"),
        )),
        dsl_aliases: vec![],
    };

    let schema =
        ResourceSchema::new("r.mode_holder").attribute(AttributeSchema::new("mode", mode_enum));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

    DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    )
}

/// Engine whose `mode` attribute is a `Custom` type with a namespace — the
/// other shape #2094 wants to treat as "expects an enum identifier".
pub(super) fn test_engine_with_custom_namespaced_attr() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    fn validate_mode(v: &carina_core::resource::Value) -> Result<(), String> {
        match v {
            carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(s),
            ) if s == "test.r.Mode.fast" || s == "test.r.Mode.slow" => Ok(()),
            carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(s),
            ) => Err(format!("invalid Mode '{}': expected fast or slow", s)),
            _ => Err("expected string".to_string()),
        }
    }

    let mode_custom = AttributeType::CustomEnum {
        identity: carina_core::schema::string_enum_identity("Mode", Some("test.r")),
        base: Box::new(AttributeType::String),
        validate: legacy_validator(validate_mode),
        to_dsl: None,
    };

    let schema =
        ResourceSchema::new("r.mode_holder").attribute(AttributeSchema::new("mode", mode_custom));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

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

    let schema = ResourceSchema::new("block.resource")
        .attribute(AttributeSchema::new("config", config_struct));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

    DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    )
}

/// Engine carrying a single resource whose `lifecycle_configuration`
/// attribute is typed `Ref("LifecycleConfiguration")` — mirrors the
/// awscc S3 Bucket shape that drove carina#3349. The `rules` field
/// inside the resolved def carries `block_name("rule")`, so DSL
/// `rule { } rule { }` blocks must be renamed to the canonical `rules`
/// field by `resolve_block_names` and the per-keystroke LSP diagnostic
/// pass must shape-match through `Ref` to reach struct-field
/// validation. Both code paths previously had `_ => {}` arms that
/// silently dropped `Ref`.
pub(super) fn test_engine_with_ref_lifecycle_like_schema() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let lifecycle_def = AttributeType::Struct {
        name: "LifecycleConfiguration".to_string(),
        fields: vec![
            StructField::new(
                "rules",
                AttributeType::list(AttributeType::Struct {
                    name: "Rule".to_string(),
                    // `id` is required: omitting it from a DSL `rule { }`
                    // block must surface a missing-required-field
                    // diagnostic. This is the positive assertion that
                    // pins the LSP Ref-peel fix — without the peel the
                    // struct-field validator never visits the
                    // Ref-typed attribute and the missing `id` slips
                    // through silently.
                    fields: vec![StructField::new("id", AttributeType::String).required()],
                }),
            )
            .with_block_name("rule"),
        ],
    };

    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new(
            "lifecycle_configuration",
            AttributeType::Ref("LifecycleConfiguration".to_string()),
        ))
        .with_def("LifecycleConfiguration", lifecycle_def);

    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);

    DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["awscc".to_string()],
        Arc::new(vec![]),
    )
}

pub(super) fn custom_engine(schemas: SchemaRegistry) -> DiagnosticEngine {
    let provider_names: Vec<String> = schemas
        .iter()
        .map(|(provider, _, _, _)| provider.to_string())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    DiagnosticEngine::new(Arc::new(schemas), provider_names, Arc::new(vec![]))
}
