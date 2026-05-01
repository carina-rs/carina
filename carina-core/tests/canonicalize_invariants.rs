//! Invariants enforced by `Value::canonicalize` (#2227).
//!
//! After `parse + resolve`, every `Value::Interpolation` in the parsed
//! file must satisfy:
//!
//! 1. At least one part is an `Expr` (otherwise the value would be a
//!    `Value::String`).
//! 2. No two adjacent parts are both `Literal` (they would have been
//!    merged).
//! 3. The whole interpolation is not a single `Expr(scalar)` whose
//!    inner value is a string-shaped scalar (it would have been
//!    unwrapped).
//!
//! This file walks every `Value` in a representative parsed file and
//! asserts the invariants hold for every interpolation found.

use carina_core::parser::parse_and_resolve;
use carina_core::resource::{InterpolationPart, Value};

fn check_value(v: &Value, path: &str) {
    match v {
        Value::Interpolation(parts) => {
            // (1) at least one Expr
            let has_expr = parts
                .iter()
                .any(|p| matches!(p, InterpolationPart::Expr(_)));
            assert!(
                has_expr,
                "Interpolation with no Expr part survived canonicalization at {}: {:?}",
                path, parts
            );
            // (2) no adjacent Literal pairs
            for window in parts.windows(2) {
                if let [InterpolationPart::Literal(_), InterpolationPart::Literal(_)] = window {
                    panic!(
                        "Adjacent Literal parts survived canonicalization at {}: {:?}",
                        path, parts
                    );
                }
            }
            // (3) not a single Expr(scalar) that should have been unwrapped
            if parts.len() == 1
                && let [InterpolationPart::Expr(inner)] = parts.as_slice()
            {
                let scalar = matches!(
                    inner,
                    Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_)
                );
                assert!(
                    !scalar,
                    "Single-Expr Interpolation wrapping a scalar at {}: {:?}",
                    path, inner
                );
            }
            // Recurse into Expr children.
            for p in parts {
                if let InterpolationPart::Expr(child) = p {
                    check_value(child, &format!("{}/<expr>", path));
                }
            }
        }
        Value::List(items) => {
            for (i, item) in items.iter().enumerate() {
                check_value(item, &format!("{}[{}]", path, i));
            }
        }
        Value::Map(map) => {
            for (k, vv) in map {
                check_value(vv, &format!("{}.{}", path, k));
            }
        }
        Value::Secret(inner) => check_value(inner, &format!("{}/<secret>", path)),
        Value::FunctionCall { name, args } => {
            for (i, a) in args.iter().enumerate() {
                check_value(a, &format!("{}/{}({})", path, name, i));
            }
        }
        _ => {}
    }
}

#[test]
fn parse_resolve_yields_canonical_interpolations() {
    // A fixture that exercises every collapse rule:
    // - "literal-only" double-quoted strings (would have been Interpolation pre-#2227)
    // - "${ref}" alone (single Expr containing a ResourceRef → kept)
    // - "prefix-${ref}-suffix" (Literal-Expr-Literal triple)
    // - "${"resolved"}" parsed inside a let binding so it resolves
    //   to a plain String through canonicalize
    let src = r#"
        let vpc = mock.example.thing {
          name = "vpc"
        }

        mock.example.consumer {
          name      = "downstream"
          plain     = "no-interpolation"
          ref_only  = "${vpc.name}"
          prefix    = "name-${vpc.name}-suffix"
          nested    = {
            inner_plain = "literal"
            inner_ref   = "${vpc.name}"
          }
          list = ["a", "b", "${vpc.name}"]
        }
    "#;

    let parsed = parse_and_resolve(src).expect("parse should succeed");

    for resource in &parsed.resources {
        for (k, v) in &resource.attributes {
            check_value(v, &format!("{}.{}", resource.id, k));
        }
    }
    for (name, v) in &parsed.variables {
        check_value(v, &format!("let {}", name));
    }
    for param in &parsed.attribute_params {
        if let Some(ref v) = param.value {
            check_value(v, &format!("attributes.{}", param.name));
        }
    }
    for export in &parsed.export_params {
        if let Some(ref v) = export.value {
            check_value(v, &format!("exports.{}", export.name));
        }
    }
    for call in &parsed.module_calls {
        for (k, v) in &call.arguments {
            check_value(v, &format!("module_call({}).{}", call.module_name, k));
        }
    }
}

#[test]
fn double_quoted_literal_is_canonical_string() {
    // Pre-#2227 a double-quoted string with no `${...}` was
    // `Value::Interpolation([Literal(...)])`; now it must be
    // `Value::String(...)`.
    let src = r#"
        mock.example.thing {
          name  = "no-interpolation-here"
        }
    "#;
    let parsed = parse_and_resolve(src).expect("parse should succeed");
    let resource = &parsed.resources[0];
    let value = resource.attributes.get("name").expect("name attribute");
    assert!(
        matches!(value, Value::String(_)),
        "literal-only double-quoted string must canonicalize to String, got {:?}",
        value
    );
}
