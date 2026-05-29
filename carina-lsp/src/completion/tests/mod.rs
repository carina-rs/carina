use std::sync::Arc;

use super::*;
use crate::document::Document;
use carina_core::parser::ProviderContext;
use carina_core::provider::ProviderFactory;
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, legacy_validator,
};

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
    let transition_struct = AttributeType::struct_(
        "Transition".to_string(),
        vec![
            StructField::new("days", AttributeType::int()),
            StructField::new("storage_class", AttributeType::string()),
        ],
    );

    let config_struct = AttributeType::struct_(
        "Config".to_string(),
        vec![
            StructField::new("transitions", AttributeType::list(transition_struct))
                .with_block_name("transition"),
            StructField::new("enabled", AttributeType::bool()),
        ],
    );

    let schema = ResourceSchema::new("block.resource")
        .attribute(AttributeSchema::new("config", config_struct));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

pub(super) fn test_provider_with_nested_structs() -> CompletionProvider {
    let inner_struct = AttributeType::struct_(
        "InnerStruct".to_string(),
        vec![
            StructField::new("leaf_field", AttributeType::string()),
            StructField::new("leaf_bool", AttributeType::bool()),
        ],
    );

    let outer_struct = AttributeType::struct_(
        "OuterStruct".to_string(),
        vec![
            StructField::new("inner", inner_struct),
            StructField::new("outer_field", AttributeType::string()),
        ],
    );

    let schema = ResourceSchema::new("nested.resource")
        .attribute(AttributeSchema::new("outer", outer_struct));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

/// Minimal provider with a single resource type `test.foo.bar` that has one
/// `attr` string attribute. Enough to exercise value-position completions
/// without needing real provider schemas.
pub(super) fn test_provider_single_attr() -> CompletionProvider {
    let schema = ResourceSchema::new("foo.bar")
        .attribute(AttributeSchema::new("attr", AttributeType::string()));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);
    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

/// Like `test_provider_with_nameless_enum` but with a non-empty
/// `region_completions_data`, so tests can detect region pollution in
/// type-incompatible completion paths (see #1974).
pub(super) fn test_provider_with_enum_and_regions() -> CompletionProvider {
    let status_enum = AttributeType::string_enum(
        "VersioningStatus".to_string(),
        vec!["Enabled".to_string(), "Suspended".to_string()],
        None,
        vec![],
    );
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("versioning_status", status_enum));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);

    let region_completions: Vec<CompletionValue> = vec![
        CompletionValue {
            value: "aws.Region.ap_northeast_1".to_string(),
            description: "Asia Pacific (Tokyo)".to_string(),
        },
        CompletionValue {
            value: "aws.Region.us_east_1".to_string(),
            description: "US East (N. Virginia)".to_string(),
        },
    ];

    CompletionProvider::new(
        Arc::new(schemas),
        vec!["awscc".to_string(), "aws".to_string()],
        region_completions,
        vec![],
    )
}

/// Provider with StringEnum that has name but no namespace (simulates WASM provider).
pub(super) fn test_provider_with_nameless_enum() -> CompletionProvider {
    // Top-level attribute with StringEnum (no namespace)
    let status_enum = AttributeType::string_enum(
        "VersioningStatus".to_string(),
        vec!["Enabled".to_string(), "Suspended".to_string()],
        None,
        vec![],
    );

    // Nested struct field with StringEnum (no namespace)
    let versioning_struct = AttributeType::struct_(
        "VersioningConfiguration".to_string(),
        vec![StructField::new("status", status_enum.clone())],
    );

    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("versioning_status", status_enum))
        .attribute(AttributeSchema::new(
            "versioning_configuration",
            versioning_struct,
        ));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);

    CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![])
}

/// Provider whose `awscc.ec2.Vpc` schema mirrors the real one as far
/// as #2357 is concerned: a `vpc_id` attribute typed
/// `Custom { semantic_name: "VpcId", base: String }`. Pairs with
/// `awscc.ec2.SecurityGroup`'s `vpc_id` (same `Custom { VpcId }`) so
/// the upstream-export REFERENCE pass has a typed receiver to match
/// against.
pub(super) fn test_provider_with_vpc_and_security_group() -> CompletionProvider {
    fn noop_validate(_v: &carina_core::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let vpc_id_custom = AttributeType::custom(
        Some(carina_core::schema::TypeIdentity::bare("VpcId")),
        AttributeType::string(),
        None,
        None,
        legacy_validator(noop_validate),
        None,
    );
    let vpc = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::string()))
        .attribute(AttributeSchema::new("vpc_id", vpc_id_custom.clone()));
    let security_group = ResourceSchema::new("ec2.SecurityGroup")
        .attribute(AttributeSchema::new("vpc_id", vpc_id_custom));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", vpc);
    schemas.insert("awscc", security_group);
    CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![])
}

/// Provider exposing both a namespaced `StringEnum` (`principal_type`) and a
/// `Custom` semantic subtype (`target_id` → `aws_account_id`) on the same
/// resource. Used to reproduce the two value-position completion leaks
/// reported in the parent issue.
pub(super) fn test_provider_with_custom_semantic_attr() -> CompletionProvider {
    fn noop_validate(_v: &carina_core::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let account_id = AttributeType::custom(
        Some(carina_core::schema::TypeIdentity::bare("aws_account_id")),
        AttributeType::string(),
        None,
        None,
        legacy_validator(noop_validate),
        None,
    );
    let principal_type = AttributeType::string_enum(
        "PrincipalType".to_string(),
        vec!["GROUP".to_string(), "USER".to_string()],
        Some(carina_core::schema::string_enum_identity(
            "PrincipalType",
            Some("awscc.sso.Assignment"),
        )),
        vec![],
    );
    let schema = ResourceSchema::new("sso.Assignment")
        .attribute(AttributeSchema::new("principal_type", principal_type))
        .attribute(AttributeSchema::new("target_id", account_id));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);
    CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![])
}
