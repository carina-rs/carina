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

use carina_core::resource::{ConcreteValue, Value};
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
            AttributeType::enum_(
                carina_core::schema::enum_identity("VersioningStatus", Some("aws.s3.Bucket")),
                Some(vec!["Enabled".to_string(), "Suspended".to_string()]),
                vec![],
                None,
                None,
            ),
        )
        .required(),
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
    let err = carina_core::schema::Schema::flat(t.clone())
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
    let err = carina_core::schema::Schema::flat(t.clone())
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "zzz".to_string(),
        )))
        .unwrap_err();
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn string_literal_expected_enum_enum_display() {
    let schema = ResourceSchema::new("test.assignment").attribute(
        AttributeSchema::new(
            "target_type",
            AttributeType::enum_(
                carina_core::schema::enum_identity("TargetType", Some("awscc.sso.Assignment")),
                Some(vec!["AWS_ACCOUNT".to_string(), "GROUP".to_string()]),
                vec![],
                None,
                None,
            ),
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "target_type".to_string(),
        Value::Concrete(ConcreteValue::String("aaa".to_string())),
    );
    insta::assert_snapshot!(render_first_error(&schema, attrs, true));
}

#[test]
fn string_literal_expected_enum_custom_namespaced_display() {
    fn validate_mode(v: &Value) -> Result<(), String> {
        match v {
            Value::Concrete(ConcreteValue::String(s)) if s == "test.r.Mode.fast" => Ok(()),
            Value::Concrete(ConcreteValue::String(s)) => {
                Err(format!("invalid Mode '{}': expected fast", s))
            }
            _ => Err("expected String".to_string()),
        }
    }
    let schema = ResourceSchema::new("test.r.mode_holder").attribute(
        AttributeSchema::new(
            "mode",
            AttributeType::enum_(
                // Structured identity matching the legacy `namespace: "test.r"`
                // shorthand prefix: provider=test, segments=[r], kind=Mode.
                // The dotted display is `test.r.Mode`, which is the prefix
                // `expand_enum_shorthand` now derives from `identity`.
                carina_core::schema::TypeIdentity::new(Some("test"), ["r"], "Mode"),
                None,
                vec![],
                Some(legacy_validator(validate_mode)),
                None,
            ),
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "mode".to_string(),
        Value::Concrete(ConcreteValue::String("aaa".to_string())),
    );
    insta::assert_snapshot!(render_first_error(&schema, attrs, true));
}
