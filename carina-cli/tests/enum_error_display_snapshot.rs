//! Snapshot the user-facing `Display` output of the two enum-mismatch
//! `TypeError` variants so refactors that touch the structured
//! `expected: Vec<ExpectedEnumVariant>` cannot accidentally drift the
//! rendered message. This is the byte-identical guarantee from #2220's
//! acceptance criteria — the structural change in core must not
//! perturb what users see.
//!
//! Snapshots cover all five user-visible message shapes:
//!   1. `InvalidEnumVariant`, namespaced StringEnum
//!   2. `InvalidEnumVariant`, non-namespaced StringEnum (bare variants)
//!   3. `InvalidEnumVariant`, with `to_dsl` aliases listed alongside
//!   4. `StringLiteralExpectedEnum` from a quoted-literal on a StringEnum
//!   5. `StringLiteralExpectedEnum` from a quoted-literal on a Custom
//!      namespaced type (the `extra_message` path)
//!
//! Each schema is built inline so the test does not depend on a
//! provider plugin binary — the goal is to lock the renderer, not to
//! exercise the CLI driver.

use std::collections::HashMap;

use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

fn render_first_error(
    schema: &ResourceSchema,
    attrs: HashMap<String, Value>,
    string_literal: bool,
) -> String {
    let result = if string_literal {
        schema.validate_with_origins(&attrs, &|_| true)
    } else {
        schema.validate(&attrs)
    };
    let errs = result.expect_err("schema must reject the input under test");
    errs.first()
        .expect("at least one error expected")
        .to_string()
}

#[test]
fn invalid_enum_variant_namespaced_display() {
    let schema = ResourceSchema::new("test.bucket").attribute(
        AttributeSchema::new(
            "versioning",
            AttributeType::StringEnum {
                name: "VersioningStatus".to_string(),
                values: vec!["Enabled".to_string(), "Suspended".to_string()],
                namespace: Some("aws.s3.Bucket".to_string()),
                to_dsl: None,
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "versioning".to_string(),
        Value::String("aws.s3.Bucket.VersioningStatus.NotReal".to_string()),
    );
    insta::assert_snapshot!(render_first_error(&schema, attrs, false));
}

#[test]
fn invalid_enum_variant_bare_display() {
    let t = AttributeType::StringEnum {
        name: "Mode".to_string(),
        values: vec!["fast".to_string(), "slow".to_string()],
        namespace: None,
        to_dsl: None,
    };
    let err = t.validate(&Value::String("zzz".to_string())).unwrap_err();
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn invalid_enum_variant_with_to_dsl_aliases_display() {
    fn lower(v: &str) -> String {
        v.to_ascii_lowercase()
    }
    let t = AttributeType::StringEnum {
        name: "VersioningStatus".to_string(),
        values: vec!["Enabled".to_string(), "Suspended".to_string()],
        namespace: Some("aws.s3.Bucket".to_string()),
        to_dsl: Some(lower),
    };
    let err = t.validate(&Value::String("zzz".to_string())).unwrap_err();
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn string_literal_expected_enum_string_enum_display() {
    let schema = ResourceSchema::new("test.assignment").attribute(
        AttributeSchema::new(
            "target_type",
            AttributeType::StringEnum {
                name: "TargetType".to_string(),
                values: vec!["AWS_ACCOUNT".to_string(), "GROUP".to_string()],
                namespace: Some("awscc.sso.Assignment".to_string()),
                to_dsl: None,
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert("target_type".to_string(), Value::String("aaa".to_string()));
    insta::assert_snapshot!(render_first_error(&schema, attrs, true));
}

#[test]
fn string_literal_expected_enum_custom_namespaced_display() {
    fn validate_mode(v: &Value) -> Result<(), String> {
        match v {
            Value::String(s) if s == "test.r.Mode.fast" => Ok(()),
            Value::String(s) => Err(format!("invalid Mode '{}': expected fast", s)),
            _ => Err("expected String".to_string()),
        }
    }
    let schema = ResourceSchema::new("test.r.mode_holder").attribute(
        AttributeSchema::new(
            "mode",
            AttributeType::Custom {
                semantic_name: Some("Mode".to_string()),
                base: Box::new(AttributeType::String),
                pattern: None,
                length: None,
                validate: legacy_validator(validate_mode),
                namespace: Some("test.r".to_string()),
                to_dsl: None,
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert("mode".to_string(), Value::String("aaa".to_string()));
    insta::assert_snapshot!(render_first_error(&schema, attrs, true));
}
