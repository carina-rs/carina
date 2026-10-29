//! Snapshot the user-facing `Display` output of the two enum-mismatch
//! `TypeError` variants so refactors that touch the structured
//! `expected: Vec<ExpectedEnumVariant>` cannot accidentally drift the
//! rendered message. This is the byte-identical guarantee from #2220's
//! acceptance criteria — the structural change in core must not
//! perturb what users see.
//!
//! Snapshots cover all five user-visible message shapes:
//!   1. `InvalidEnumVariant`, namespaced Enum
//!   2. `InvalidEnumVariant`, non-namespaced Enum (bare variants)
//!   3. `InvalidEnumVariant`, with `to_dsl` aliases listed alongside
//!   4. `StringLiteralExpectedEnum` from a quoted-literal on a Enum
//!   5. `StringLiteralExpectedEnum` from a quoted-literal on a Custom
//!      namespaced type (the `extra_message` path)
//!
//! Each schema is built inline so the test does not depend on a
//! provider plugin binary — the goal is to lock the renderer, not to
//! exercise the CLI driver.

use std::collections::HashMap;
use std::sync::Arc;

use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult};
use carina_core::resource::{ConcreteValue, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, RootlessType, SchemaKind, SchemaRegistry,
    ValidatedConfigSchema, ValidatedResourceSchema, legacy_validator,
};

fn pathless_test_type(attr_type: AttributeType) -> RootlessType {
    ValidatedConfigSchema::validate(HashMap::from([("value".to_string(), attr_type)]))
        .expect("test config type must be valid")
        .rootless_type("value")
        .expect("test config type must exist")
}

fn render_first_error(
    schema: &ResourceSchema,
    attrs: HashMap<String, Value>,
    string_literal: bool,
) -> String {
    let schema = schema
        .clone()
        .validate()
        .expect("snapshot schema must be valid");
    let resource_type = schema.parts().resource_type.to_string();
    let factories: Arc<Vec<Box<dyn ProviderFactory>>> =
        Arc::new(vec![Box::new(SnapshotSchemaFactory {
            schema: schema.clone(),
        })]);
    let resolver = carina_core::provider::provider_custom_types_from_factories(factories);
    let mut registry = SchemaRegistry::new();
    registry.insert("test", schema);
    let result = registry
        .get("test", &resource_type, SchemaKind::Resource)
        .expect("snapshot schema must be owner-bound")
        .validate_with_resolver(&attrs, &|_| string_literal, &resolver);
    let errs = result.expect_err("schema must reject the input under test");
    errs.first()
        .expect("at least one error expected")
        .to_string()
}

struct SnapshotSchemaFactory {
    schema: ValidatedResourceSchema,
}

impl ProviderFactory for SnapshotSchemaFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "snapshot schema provider"
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        String::new()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        unimplemented!("snapshot tests do not construct providers")
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![self.schema.clone()]
    }
}

#[test]
fn invalid_enum_variant_namespaced_display() {
    let schema = ResourceSchema::new("test.bucket").attribute(
        AttributeSchema::required(
            "versioning",
            AttributeType::enum_(
                carina_core::schema::enum_identity("VersioningStatus", Some("aws.s3.Bucket")),
                Some(vec!["Enabled".to_string(), "Suspended".to_string()]),
                vec![],
                None,
                None,
            ),
        )
        .build(),
    );
    // Phase 4 of carina#2986: the test exercises the wrong-variant
    // path (`InvalidEnumVariant`), which is reached only by the
    // identifier shape. A `ConcreteValue::String` here would route to
    // `StringLiteralExpectedEnum` instead — covered separately by
    // `string_literal_expected_enum_enum_display`.
    let mut attrs = HashMap::new();
    attrs.insert(
        "versioning".to_string(),
        Value::Concrete(ConcreteValue::enum_identifier(
            "aws.s3.Bucket.VersioningStatus.NotReal".to_string(),
        )),
    );
    insta::assert_snapshot!(render_first_error(&schema, attrs, false));
}

#[test]
fn invalid_enum_variant_bare_display() {
    // Phase 4 of carina#2986: identifier-shape input reaches the
    // wrong-variant matcher; a string literal goes to
    // `StringLiteralExpectedEnum`.
    let t = AttributeType::enum_(
        carina_core::schema::TypeIdentity::bare("Mode"),
        Some(vec!["fast".to_string(), "slow".to_string()]),
        vec![],
        None,
        None,
    );
    let err = pathless_test_type(t.clone())
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "zzz".to_string(),
        )))
        .unwrap_err();
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn invalid_enum_variant_with_dsl_aliases_display() {
    // Phase 4 of carina#2986: identifier-shape input.
    let t = AttributeType::enum_(
        carina_core::schema::enum_identity("VersioningStatus", Some("aws.s3.Bucket")),
        Some(vec!["Enabled".to_string(), "Suspended".to_string()]),
        vec![
            ("Enabled".to_string(), "enabled".to_string()),
            ("Suspended".to_string(), "suspended".to_string()),
        ],
        None,
        None,
    );
    let err = pathless_test_type(t.clone())
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "zzz".to_string(),
        )))
        .unwrap_err();
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn string_literal_expected_enum_enum_display() {
    let schema = ResourceSchema::new("test.assignment").attribute(
        AttributeSchema::required(
            "target_type",
            AttributeType::enum_(
                carina_core::schema::enum_identity("TargetType", Some("awscc.sso.Assignment")),
                Some(vec!["AWS_ACCOUNT".to_string(), "GROUP".to_string()]),
                vec![],
                None,
                None,
            ),
        )
        .build(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "target_type".to_string(),
        Value::Concrete(ConcreteValue::String("aaa".into())),
    );
    insta::assert_snapshot!(render_first_error(&schema, attrs, true));
}

#[test]
fn string_literal_expected_enum_custom_namespaced_display() {
    fn validate_mode(v: &Value) -> Result<(), String> {
        match v {
            Value::Concrete(ConcreteValue::String(s)) if s == "test.r.Mode.fast" => Ok(()),
            Value::Concrete(ConcreteValue::String(s)) => Err(format!(
                "invalid Mode '{}': expected fast",
                s.expose_plaintext()
            )),
            _ => Err("expected String".to_string()),
        }
    }
    let schema = ResourceSchema::new("test.r.mode_holder").attribute(
        AttributeSchema::required(
            "mode",
            AttributeType::enum_(
                // Structured identity matching the legacy `namespace: "test.r"`
                // shorthand prefix: provider=test, segments=[r], kind=Mode.
                // The dotted enum identity is `test.r.Mode`. The quoted
                // String below must nevertheless reach the validator
                // literally; only a typed EnumIdentifier is expanded.
                carina_core::schema::TypeIdentity::new(Some("test"), ["r"], "Mode"),
                None,
                vec![],
                Some(legacy_validator(validate_mode)),
                None,
            ),
        )
        .build(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "mode".to_string(),
        Value::Concrete(ConcreteValue::String("aaa".into())),
    );
    insta::assert_snapshot!(render_first_error(&schema, attrs, true));
}
