use std::path::Path;

use carina_core::parser::{FloatLiteralError, ParseError, ProviderContext, parse_file};
use carina_core::resource::{ConcreteValue, ExpandedConcreteValue, Value};
use carina_core::{EnvironmentSnapshot, NonFiniteFloatError};

fn finite(value: f64) -> carina_core::FiniteF64 {
    carina_core::FiniteF64::new(value).unwrap()
}

fn overflowing_literal() -> String {
    format!("1{}.0", "0".repeat(400))
}

fn assert_overflow_rejected(source: &str, literal_line: usize) {
    let path = Path::new("non_finite_float.crn");
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    let finite_source = source.replace(&overflowing_literal(), "1.0");
    parse_file(&finite_source, path, &context).expect("the finite control must parse");
    let expected_category = if source.contains(&format!("-{}", overflowing_literal())) {
        NonFiniteFloatError::NegativeInfinity
    } else {
        NonFiniteFloatError::PositiveInfinity
    };
    let error = parse_file(source, path, &context).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains(&format!("Invalid float literal at line {literal_line}")),
        "{message}"
    );
    let mut inner = &error;
    loop {
        inner = match inner {
            ParseError::Located { error, .. } => error,
            ParseError::AtElement(finding) => finding.detail(),
            ParseError::InvalidFloatLiteral {
                line,
                source: FloatLiteralError::NonFinite(source),
            } => {
                assert_eq!(*line, literal_line);
                assert_eq!(*source, expected_category);
                assert!(
                    std::error::Error::source(inner)
                        .unwrap()
                        .source()
                        .unwrap()
                        .is::<NonFiniteFloatError>()
                );
                break;
            }
            other => panic!("expected typed float admission failure, got {other:?}"),
        };
    }
}

#[test]
fn parser_rejects_overflowing_float_literal() {
    for sign in ["", "-"] {
        assert_overflow_rejected(
            &format!("\n\nlet ratio = {sign}{}\n", overflowing_literal()),
            3,
        );
    }
}

#[test]
fn validate_expr_rejects_overflowing_float_literal() {
    for sign in ["", "-"] {
        let source = format!(
            "arguments {{\nratio: Float {{\nvalidation {{\ncondition = ratio < {sign}{}\nerror_message = \"too large\"\n}}\n}}\n}}\n",
            overflowing_literal()
        );
        assert_overflow_rejected(&source, 4);
    }
}

#[test]
fn multiline_wait_until_rejects_overflowing_float_literal() {
    for sign in ["", "-"] {
        let source = format!(
            "let source = mock.test.Thing {{ name = 'source' }}\nlet ready = wait source {{\nuntil = source.ratio ==\n{sign}{}\n}}\n",
            overflowing_literal()
        );
        assert_overflow_rejected(&source, 4);
    }
}

#[test]
fn finite_extrema_and_signed_zeros_remain_literal_values() {
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    for raw in [f64::MIN, -0.0, 0.0, f64::MAX] {
        let source = format!("let ratio = {raw:.1}\n");
        let file = parse_file(&source, Path::new("finite.crn"), &context).unwrap();
        let Value::Concrete(ConcreteValue::Float(value)) = &file.variables["ratio"] else {
            panic!("expected float")
        };
        assert_eq!(value.get().to_bits(), raw.to_bits());
    }
}

#[test]
fn tagged_serde_rejects_overflow_and_preserves_finite_float_bits() {
    for json in [r#"{"Float":1e999}"#, r#"{"Float":-1e999}"#] {
        assert!(serde_json::from_str::<ExpandedConcreteValue>(json).is_err());
        assert!(serde_json::from_str::<carina_core::parser::ValidateExpr>(json).is_err());
    }
    for raw in [f64::MIN, -0.0, 0.0, f64::from_bits(1), f64::MAX] {
        let value: ExpandedConcreteValue = ConcreteValue::Float(finite(raw));
        let json = serde_json::to_string(&value).unwrap();
        let ConcreteValue::Float(decoded) =
            serde_json::from_str::<ExpandedConcreteValue>(&json).unwrap()
        else {
            panic!("expected float")
        };
        assert_eq!(decoded.get().to_bits(), raw.to_bits());
        let ast: carina_core::parser::ValidateExpr =
            carina_core::parser::ValidateExpr::Float(finite(raw));
        let json = serde_json::to_string(&ast).unwrap();
        let carina_core::parser::ValidateExpr::Float(decoded) =
            serde_json::from_str::<carina_core::parser::ValidateExpr>(&json).unwrap()
        else {
            panic!("expected float")
        };
        assert_eq!(decoded.get().to_bits(), raw.to_bits());
    }
}
