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

struct RegionConfigFactory {
    name: &'static str,
}

impl ProviderFactory for RegionConfigFactory {
    fn name(&self) -> &str {
        self.name
    }

    fn display_name(&self) -> &str {
        self.name
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        carina_core::schema::ValidatedConfigSchema::validate(std::collections::HashMap::from([(
            "region".to_string(),
            carina_core::schema::AttributeType::enum_(
                carina_core::schema::TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                Some(vec!["ap-northeast-1".to_string()]),
                vec![("ap-northeast-1".to_string(), "ap_northeast_1".to_string())],
                None,
                None,
            ),
        )]))
        .expect("test provider config schema must be valid")
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(
        &self,
        _attributes: &indexmap::IndexMap<String, carina_core::resource::Value>,
    ) -> String {
        String::new()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = carina_core::provider::ProviderResult<
                        Box<dyn carina_core::provider::Provider>,
                    >,
                > + Send
                + '_,
        >,
    > {
        unimplemented!("RegionConfigFactory::create_provider is not used in diagnostics tests")
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        Vec::new()
    }
}

pub(super) fn test_engine_with_region_config_provider(name: &'static str) -> DiagnosticEngine {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(RegionConfigFactory { name })];
    DiagnosticEngine::new(
        Arc::new(provider_mod::collect_schemas(&factories)),
        vec![name.to_string()],
        Arc::new(factories),
    )
}

pub(super) fn test_engine_reversed() -> DiagnosticEngine {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![];
    let schemas = Arc::new(provider_mod::collect_schemas(&factories));
    let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
    DiagnosticEngine::new(schemas, provider_names, Arc::new(vec![]))
}

pub(super) fn test_engine_with_nested_structs() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let inner_struct = AttributeType::list(AttributeType::struct_(
        "InnerStruct".to_string(),
        vec![
            StructField::optional("leaf_field", AttributeType::string()).build(),
            StructField::optional("leaf_int", AttributeType::int()).build(),
            StructField::provider_populated("provider_leaf", AttributeType::string()).build(),
        ],
    ));

    let outer_struct = AttributeType::list(AttributeType::struct_(
        "OuterStruct".to_string(),
        vec![
            StructField::optional("inner", inner_struct).build(),
            StructField::optional("outer_field", AttributeType::string()).build(),
        ],
    ));

    let schema = ResourceSchema::new("nested.resource")
        .attribute(AttributeSchema::optional("outer", outer_struct).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    )
}

/// Engine carrying a managed `aws.acm.Certificate` schema with `status`
/// attribute — the canonical target for `wait` diagnostics tests.
pub(super) fn test_engine_with_wait_target() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};
    let status_enum = AttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "Status"),
        Some(vec!["ISSUED".to_string()]),
        vec![("ISSUED".to_string(), "issued".to_string())],
        None,
        None,
    );
    let schema = ResourceSchema::new("acm.Certificate")
        .attribute(AttributeSchema::optional("domain_name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("status", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("enum_status", status_enum).build());
    let data_source_schema = ResourceSchema::new("acm.Certificate")
        .as_data_source()
        .attribute(AttributeSchema::optional("domain_name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("status", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );
    schemas.insert(
        "aws",
        data_source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    DiagnosticEngine::new(Arc::new(schemas), vec!["aws".to_string()], Arc::new(vec![]))
}

pub(super) fn test_engine_with_iam_policy_arn_custom_type() -> DiagnosticEngine {
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, legacy_validator,
    };

    let iam_policy_arn = AttributeType::refined_string_with_validator(
        TypeIdentity::new(Some("aws"), ["iam", "Policy"], "Arn"),
        None,
        None,
        legacy_validator(|_| Ok(())),
        None,
    );
    let schema = ResourceSchema::new("iam.Role")
        .attribute(AttributeSchema::optional("policy_arn", iam_policy_arn).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );

    DiagnosticEngine::new(Arc::new(schemas), vec!["aws".to_string()], Arc::new(vec![]))
}

pub(super) fn test_engine_with_enum_attr() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mode_enum = AttributeType::enum_(
        carina_core::schema::TypeIdentity::bare("Mode"),
        Some(vec!["fast".to_string(), "slow".to_string()]),
        vec![],
        None,
        None,
    );

    let schema = ResourceSchema::new("r.mode_holder")
        .attribute(AttributeSchema::optional("mode", mode_enum).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

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

    let mode_enum = AttributeType::enum_(
        carina_core::schema::enum_identity("Mode", Some("test.r")),
        Some(vec!["fast".to_string(), "slow".to_string()]),
        vec![],
        None,
        None,
    );

    let schema = ResourceSchema::new("r.mode_holder")
        .attribute(AttributeSchema::optional("mode", mode_enum).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

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
            ) => Err(format!(
                "invalid Mode '{}': expected fast or slow",
                s.expose_plaintext()
            )),
            _ => Err("expected string".to_string()),
        }
    }

    let mode_custom = AttributeType::enum_(
        carina_core::schema::enum_identity("Mode", Some("test.r")),
        None,
        vec![],
        Some(legacy_validator(validate_mode)),
        None,
    );

    let schema = ResourceSchema::new("r.mode_holder")
        .attribute(AttributeSchema::optional("mode", mode_custom).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    DiagnosticEngine::new(
        Arc::new(schemas),
        vec!["test".to_string()],
        Arc::new(vec![]),
    )
}

pub(super) fn test_engine_with_block_name_nested() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let transition_struct = AttributeType::struct_(
        "Transition".to_string(),
        vec![
            StructField::optional("days", AttributeType::int()).build(),
            StructField::optional("storage_class", AttributeType::string()).build(),
        ],
    );

    let config_struct = AttributeType::struct_(
        "Config".to_string(),
        vec![
            StructField::optional("transitions", AttributeType::list(transition_struct))
                .with_block_name("transition")
                .build(),
            StructField::optional("enabled", AttributeType::bool()).build(),
        ],
    );

    let schema = ResourceSchema::new("block.resource")
        .attribute(AttributeSchema::optional("config", config_struct).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

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

    let lifecycle_def = AttributeType::struct_(
        "LifecycleConfiguration".to_string(),
        vec![
            StructField::optional(
                "rules",
                AttributeType::list(AttributeType::struct_(
                    "Rule".to_string(),
                    // `id` is required: omitting it from a DSL `rule { }`
                    // block must surface a missing-required-field
                    // diagnostic. This is the positive assertion that
                    // pins the LSP Ref-peel fix — without the peel the
                    // struct-field validator never visits the
                    // Ref-typed attribute and the missing `id` slips
                    // through silently.
                    vec![StructField::required("id", AttributeType::string()).build()],
                )),
            )
            .with_block_name("rule")
            .build(),
        ],
    );

    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(
            AttributeSchema::optional(
                "lifecycle_configuration",
                AttributeType::ref_("LifecycleConfiguration".to_string()),
            )
            .build(),
        )
        .with_def("LifecycleConfiguration", lifecycle_def);

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

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
