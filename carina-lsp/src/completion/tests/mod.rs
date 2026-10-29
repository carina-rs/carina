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

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

pub(super) fn test_provider_with_nested_structs() -> CompletionProvider {
    let inner_struct = AttributeType::struct_(
        "InnerStruct".to_string(),
        vec![
            StructField::optional("leaf_field", AttributeType::string()).build(),
            StructField::optional("leaf_bool", AttributeType::bool()).build(),
            StructField::provider_populated("provider_leaf", AttributeType::bool()).build(),
        ],
    );

    let outer_struct = AttributeType::struct_(
        "OuterStruct".to_string(),
        vec![
            StructField::optional("inner", inner_struct).build(),
            StructField::optional("outer_field", AttributeType::string()).build(),
        ],
    );

    let schema = ResourceSchema::new("nested.resource")
        .attribute(AttributeSchema::optional("outer", outer_struct).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

pub(super) fn test_provider_with_shared_definition_paths() -> CompletionProvider {
    let schema = ResourceSchema::new("shared.resource")
        .with_def(
            "Tag",
            AttributeType::struct_(
                "Tag",
                vec![StructField::required("key", AttributeType::string()).build()],
            ),
        )
        .attribute(
            AttributeSchema::optional("tags", AttributeType::list(AttributeType::ref_("Tag")))
                .build(),
        )
        .attribute(
            AttributeSchema::provider_populated(
                "effective_tags",
                AttributeType::list(AttributeType::ref_("Tag")),
            )
            .build(),
        );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

pub(super) fn test_provider_with_list_of_structs() -> CompletionProvider {
    let rule = AttributeType::struct_(
        "Rule",
        vec![
            StructField::optional("name", AttributeType::string()).build(),
            StructField::optional("enabled", AttributeType::bool()).build(),
        ],
    );
    let schema = ResourceSchema::new("Policy")
        .attribute(AttributeSchema::optional("rules", AttributeType::list(rule)).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

pub(super) fn test_provider_for_enclosing_block_regressions() -> CompletionProvider {
    let tag_key = AttributeType::enum_(
        carina_core::schema::TypeIdentity::bare("TagKey"),
        Some(vec!["Name".to_string(), "Env".to_string()]),
        vec![],
        None,
        None,
    );
    let nested = AttributeType::struct_(
        "Nested",
        vec![
            StructField::optional("inner", AttributeType::string()).build(),
            StructField::optional("outer_field", AttributeType::list(AttributeType::string()))
                .build(),
            StructField::optional(
                "tags",
                AttributeType::map_with_key(tag_key, AttributeType::string()),
            )
            .build(),
        ],
    );
    let schema = ResourceSchema::new("review.Resource")
        .attribute(
            AttributeSchema::optional("attr", AttributeType::list(AttributeType::string())).build(),
        )
        .attribute(AttributeSchema::optional("outer", nested.clone()).build())
        .attribute(AttributeSchema::optional("x", nested).build())
        .attribute(AttributeSchema::optional("policy", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

/// Minimal provider with a single resource type `test.foo.bar` that has one
/// `attr` string attribute. Enough to exercise value-position completions
/// without needing real provider schemas.
pub(super) fn test_provider_single_attr() -> CompletionProvider {
    let schema = ResourceSchema::new("foo.bar")
        .attribute(AttributeSchema::optional("attr", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    CompletionProvider::new(Arc::new(schemas), vec!["test".to_string()], vec![], vec![])
}

/// Like `test_provider_with_nameless_enum` but with a non-empty
/// `region_completions_data`, so tests can detect region pollution in
/// type-incompatible completion paths (see #1974).
pub(super) fn test_provider_with_enum_and_regions() -> CompletionProvider {
    let status_enum = AttributeType::enum_(
        carina_core::schema::TypeIdentity::bare("VersioningStatus"),
        Some(vec!["Enabled".to_string(), "Suspended".to_string()]),
        vec![],
        None,
        None,
    );
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::optional("versioning_status", status_enum).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

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

/// Provider with Enum that has name but no namespace (simulates WASM provider).
pub(super) fn test_provider_with_nameless_enum() -> CompletionProvider {
    // Top-level attribute with Enum (no namespace)
    let status_enum = AttributeType::enum_(
        carina_core::schema::TypeIdentity::bare("VersioningStatus"),
        Some(vec!["Enabled".to_string(), "Suspended".to_string()]),
        vec![],
        None,
        None,
    );

    // Nested struct field with Enum (no namespace)
    let versioning_struct = AttributeType::struct_(
        "VersioningConfiguration".to_string(),
        vec![StructField::optional("status", status_enum.clone()).build()],
    );

    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::optional("versioning_status", status_enum).build())
        .attribute(
            AttributeSchema::optional("versioning_configuration", versioning_struct).build(),
        );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

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
    let vpc_id_custom = AttributeType::refined_string_with_validator(
        carina_core::schema::TypeIdentity::bare("VpcId"),
        None,
        None,
        legacy_validator(noop_validate),
        None,
    );
    let vpc = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", vpc_id_custom.clone()).build());
    let security_group = ResourceSchema::new("ec2.SecurityGroup")
        .attribute(AttributeSchema::optional("vpc_id", vpc_id_custom).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        vpc.validate().expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        security_group
            .validate()
            .expect("schema fixture must be valid"),
    );
    CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![])
}

/// Provider exposing both a namespaced `Enum` (`principal_type`) and a
/// `Custom` semantic subtype (`target_id` → `aws_account_id`) on the same
/// resource. Used to reproduce the two value-position completion leaks
/// reported in the parent issue.
pub(super) fn test_provider_with_custom_semantic_attr() -> CompletionProvider {
    fn noop_validate(_v: &carina_core::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let account_id = AttributeType::refined_string_with_validator(
        carina_core::schema::TypeIdentity::bare("AwsAccountId"),
        None,
        None,
        legacy_validator(noop_validate),
        None,
    );
    let principal_type = AttributeType::enum_(
        carina_core::schema::enum_identity("PrincipalType", Some("awscc.sso.Assignment")),
        Some(vec!["GROUP".to_string(), "USER".to_string()]),
        vec![],
        None,
        None,
    );
    let schema = ResourceSchema::new("sso.Assignment")
        .attribute(AttributeSchema::optional("principal_type", principal_type).build())
        .attribute(AttributeSchema::optional("target_id", account_id).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![])
}

fn enum_shadow_schemas() -> Vec<carina_core::schema::ValidatedResourceSchema> {
    let event_type = AttributeType::enum_(
        carina_core::schema::enum_identity("EventType", Some("awscc.cloudfront.Distribution")),
        Some(vec![
            "ViewerRequest".to_string(),
            "OriginRequest".to_string(),
        ]),
        vec![
            ("ViewerRequest".to_string(), "viewer_request".to_string()),
            ("OriginRequest".to_string(), "origin_request".to_string()),
        ],
        None,
        None,
    );
    let source = ResourceSchema::new("test.Source")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build());
    let distribution = ResourceSchema::new("cloudfront.Distribution")
        .attribute(AttributeSchema::optional("event_type", event_type).build());
    vec![
        source
            .validate()
            .expect("source schema fixture must be valid"),
        distribution
            .validate()
            .expect("distribution schema fixture must be valid"),
    ]
}

struct EnumShadowFactory;

impl ProviderFactory for EnumShadowFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "enum shadow test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
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
        unimplemented!("completion tests do not instantiate providers")
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        enum_shadow_schemas()
    }
}

pub(super) fn create_enum_shadow_document(content: &str) -> Document {
    let factories: Arc<Vec<Box<dyn ProviderFactory>>> = Arc::new(vec![Box::new(EnumShadowFactory)]);
    let schemas = carina_core::provider::collect_schemas(factories.as_slice());
    let resource_types = ProviderContext::resource_types_from_schema_registry(&schemas);
    let custom_types = carina_core::provider::provider_custom_types_from_factories(factories);
    Document::new(
        content.to_string(),
        Arc::new(ProviderContext {
            decryptor: None,
            custom_types,
            resource_types,
        }),
    )
}

pub(super) fn test_provider_with_enum_shadow_schema() -> CompletionProvider {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(EnumShadowFactory)];
    let schemas = carina_core::provider::collect_schemas(&factories);
    CompletionProvider::new(Arc::new(schemas), vec!["awscc".to_string()], vec![], vec![])
}
