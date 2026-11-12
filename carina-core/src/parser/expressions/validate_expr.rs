//! `validate` expression parser.
//!
//! Parses the boolean expression that follows `validate ` in argument
//! validation blocks. The AST type [`super::super::ValidateExpr`] still
//! lives in `parser/mod.rs` (it moves in part 2 of #2262); only the
//! parser logic and the [`CompareOp`] enum live here.

use super::integer_arithmetic::parse_integer_operator;
use crate::parser::{
    ParseContext, ParseError, ParserValidateExpr as ValidateExpr, Rule, first_inner, next_pair,
};

/// Position of a string whose authored text is retained without interpolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintStringPosition {
    RequireCondition,
    ValidationCondition,
    RequireMessage,
    WaitCondition,
}

impl std::fmt::Display for ConstraintStringPosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RequireCondition => "`require` condition",
            Self::ValidationCondition => "validation condition",
            Self::RequireMessage => "`require` message",
            Self::WaitCondition => "`wait` condition",
        })
    }
}

/// A grammar-checked string literal with no interpolation nodes.
///
/// The saved form retains quotes and escapes: the raw contents alone cannot
/// distinguish a single-quoted literal from an old, unevaluated interpolation.
/// Decoding repeats the same grammar check as live parsing.
/// Decode errors describe only the literal problem; the saved-plan CLI supplies
/// recovery commands using the recorded project path.
///
/// ```compile_fail
/// use carina_core::parser::ValidateStringLiteral;
/// let _: ValidateStringLiteral = "${secret('private')}".to_string().into();
/// ```
/// ```compile_fail
/// use carina_core::parser::ValidateStringLiteral;
/// let _ = ValidateStringLiteral {
///     contents: "${secret('private')}".to_string(),
///     quote: '"',
/// };
/// ```
#[derive(Debug, Clone, Eq)]
pub struct ValidateStringLiteral {
    contents: String,
    quote: char,
}

#[derive(Debug)]
struct LiteralFailure {
    interpolation: Option<std::ops::Range<usize>>,
}

impl PartialEq for ValidateStringLiteral {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl PartialEq<str> for ValidateStringLiteral {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<&str> for ValidateStringLiteral {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl ValidateStringLiteral {
    // Source is always reparsed here. A caller's pest rule tag or child tree
    // cannot establish literal-ness, because safe code can manufacture pairs.
    fn from_source(source: &str) -> Result<Self, LiteralFailure> {
        let invalid = || LiteralFailure {
            interpolation: None,
        };
        let pair = crate::parser::grammar::complete_string_pair(source).map_err(|_| invalid())?;
        if let Some(interpolation) = pair
            .into_inner()
            .flatten()
            .find(|part| part.as_rule() == Rule::interpolation)
        {
            let span = interpolation.as_span();
            return Err(LiteralFailure {
                interpolation: Some(span.start()..span.end()),
            });
        }
        let (quote, contents) = if let Some(contents) =
            source.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
        {
            ('"', contents)
        } else if let Some(contents) = source.strip_prefix('\'').and_then(|s| s.strip_suffix('\''))
        {
            ('\'', contents)
        } else {
            return Err(invalid());
        };
        Ok(Self {
            contents: contents.to_owned(),
            quote,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.contents
    }

    pub(in crate::parser) fn is_single_quoted(&self) -> bool {
        self.quote == '\''
    }
}

impl serde::Serialize for ValidateStringLiteral {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{}{}{}", self.quote, self.contents, self.quote))
    }
}

impl<'de> serde::Deserialize<'de> for ValidateStringLiteral {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let source = String::deserialize(deserializer)?;
        Self::from_source(&source)
            .map_err(|_| serde::de::Error::custom("invalid validation string literal"))
    }
}

pub(in crate::parser) fn parse_constraint_string(
    pair: pest::iterators::Pair<'_, Rule>,
    ctx: &ParseContext,
    kind: ConstraintStringPosition,
) -> Result<ValidateStringLiteral, ParseError> {
    ValidateStringLiteral::from_source(pair.as_str()).map_err(|error| {
        let location = ctx.literal_string_location(&pair, error.interpolation.clone());
        let span = location.span();
        let error = if error.interpolation.is_some() {
            ParseError::InterpolationInConstraintString {
                kind,
                line: span.start_line(),
                column: span.start_column(),
                span,
            }
        } else {
            ParseError::InvalidExpression {
                line: span.start_line(),
                message: format!("invalid string literal in a {kind}"),
            }
        };
        error.at(&location)
    })
}

// Literal-only value helpers preserve their existing diagnostics and choose
// whether to unescape after this single admission. They never trust pair children.
pub(in crate::parser) fn parse_literal_string_pair(
    pair: pest::iterators::Pair<'_, Rule>,
) -> Result<ValidateStringLiteral, ParseError> {
    ValidateStringLiteral::from_source(pair.as_str()).map_err(|error| {
        let line = error
            .interpolation
            .and_then(|range| {
                pest::Position::new(
                    pair.as_span().get_input(),
                    pair.as_span().start() + range.start,
                )
            })
            .map(|position| position.line_col().0)
            .unwrap_or_else(|| pair.line_col().0);
        ParseError::InterpolationNotAllowedInLiteralString { line }
    })
}

/// Comparison operator used inside [`ValidateExpr::Compare`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CompareOp {
    Gte,
    Lte,
    Gt,
    Lt,
    Eq,
    Ne,
}

pub(crate) fn parse_validate_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    position: ConstraintStringPosition,
) -> Result<ValidateExpr, ParseError> {
    match pair.as_rule() {
        Rule::validate_expr => {
            let inner = first_inner(pair, "validate_or_expr", "validate_expr")?;
            parse_validate_expr(inner, ctx, position)
        }
        Rule::validate_or_expr => {
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "validate_and_expr", "validate_or_expr")?;
            let mut result = parse_validate_expr(first, ctx, position)?;
            for next in inner {
                let right = parse_validate_expr(next, ctx, position)?;
                result = ValidateExpr::Or(Box::new(result), Box::new(right));
            }
            Ok(result)
        }
        Rule::validate_and_expr => {
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "validate_not_expr", "validate_and_expr")?;
            let mut result = parse_validate_expr(first, ctx, position)?;
            for next in inner {
                let right = parse_validate_expr(next, ctx, position)?;
                result = ValidateExpr::And(Box::new(result), Box::new(right));
            }
            Ok(result)
        }
        Rule::validate_not_expr => {
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "operand", "validate_not_expr")?;
            if first.as_rule() == Rule::validate_not_expr {
                // This is the "!" ~ validate_not_expr branch
                let operand = parse_validate_expr(first, ctx, position)?;
                Ok(ValidateExpr::Not(Box::new(operand)))
            } else {
                // This is the validate_comparison branch
                parse_validate_expr(first, ctx, position)
            }
        }
        Rule::validate_comparison => {
            let mut inner = pair.into_inner();
            let lhs_pair = next_pair(&mut inner, "validate_additive_expr", "validate_comparison")?;
            let lhs = parse_validate_expr(lhs_pair, ctx, position)?;
            if let Some(op_pair) = inner.next() {
                let op = match op_pair.as_str() {
                    ">=" => CompareOp::Gte,
                    "<=" => CompareOp::Lte,
                    ">" => CompareOp::Gt,
                    "<" => CompareOp::Lt,
                    "==" => CompareOp::Eq,
                    "!=" => CompareOp::Ne,
                    other => {
                        return Err(ParseError::InvalidExpression {
                            line: 0,
                            message: format!("Unknown comparison operator: {}", other),
                        });
                    }
                };
                let rhs_pair = next_pair(
                    &mut inner,
                    "validate_additive_expr",
                    "validate_comparison rhs",
                )?;
                let rhs = parse_validate_expr(rhs_pair, ctx, position)?;
                Ok(ValidateExpr::Compare {
                    lhs: Box::new(lhs),
                    op,
                    rhs: Box::new(rhs),
                })
            } else {
                Ok(lhs)
            }
        }
        Rule::validate_additive_expr | Rule::validate_multiplicative_expr => {
            let context = if pair.as_rule() == Rule::validate_additive_expr {
                "validate_additive_expr"
            } else {
                "validate_multiplicative_expr"
            };
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "integer operand", context)?;
            let mut result = parse_validate_expr(first, ctx, position)?;
            while let Some(operator) = inner.next() {
                let rhs = next_pair(&mut inner, "integer operand", context)?;
                let (op, operator_span) = parse_integer_operator(operator, ctx)?;
                result = ValidateExpr::IntegerArithmetic {
                    lhs: Box::new(result),
                    op,
                    rhs: Box::new(parse_validate_expr(rhs, ctx, position)?),
                    operator_span,
                };
            }
            Ok(result)
        }
        Rule::validate_primary => {
            let inner = first_inner(pair, "value", "validate_primary")?;
            parse_validate_expr(inner, ctx, position)
        }
        Rule::validate_function_call => {
            let line = pair.line_col().0;
            let mut inner = pair.into_inner();
            let name = next_pair(&mut inner, "function name", "validate_function_call")?
                .as_str()
                .to_string();
            if matches!(
                crate::builtins::Builtin::from_name(&name),
                Some(crate::builtins::Builtin::Secret)
            ) {
                return Err(ParseError::SecretInConstraint { line });
            }
            let mut args = Vec::new();
            for arg_pair in inner {
                args.push(parse_validate_expr(arg_pair, ctx, position)?);
            }
            Ok(ValidateExpr::FunctionCall { name, args })
        }
        Rule::null_literal => Ok(ValidateExpr::Null),
        Rule::boolean => Ok(ValidateExpr::Bool(pair.as_str() == "true")),
        Rule::float => {
            let f = super::super::error::parse_float_literal(pair.as_str(), pair.line_col().0)?;
            Ok(ValidateExpr::Float(f))
        }
        Rule::number => {
            let n: i64 = pair
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("Invalid number: {}", e),
                })?;
            Ok(ValidateExpr::Int(n))
        }
        Rule::duration_literal => {
            let line = pair.line_col().0;
            let secs = super::primary::parse_duration_secs(pair.as_str(), line)?;
            Ok(ValidateExpr::Duration(std::time::Duration::from_secs(secs)))
        }
        Rule::string => Ok(ValidateExpr::String(parse_constraint_string(
            pair, ctx, position,
        )?)),
        Rule::variable_ref => {
            let mut parts = pair.into_inner();
            let argument = next_pair(&mut parts, "identifier", "variable_ref")?
                .as_str()
                .to_string();
            let mut path = Vec::new();
            for part in parts {
                path.push(super::primary::parse_postfix_path_segment(part, ctx)?);
            }
            if path.is_empty() {
                Ok(ValidateExpr::Var(argument))
            } else {
                Ok(ValidateExpr::Access { argument, path })
            }
        }
        other => Err(ParseError::InvalidExpression {
            line: 0,
            message: format!("Unexpected rule in validate expression: {:?}", other),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round1_manufactured_string_pairs_cannot_bypass_literal_admission() {
        for source in [r#""${secret('CANARY_4386')}""#, "", "\"", "'ok' trailing"] {
            let pair = pest::state(source, |p| p.rule(Rule::string, |p| p.match_string(source)))
                .unwrap()
                .next()
                .unwrap();
            let config = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
            let file = std::sync::Arc::new(crate::parser::preprocess_file(source).unwrap());
            let ctx = ParseContext::new(&config, std::path::Path::new("synthetic.crn"), file);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                parse_validate_expr(pair, &ctx, ConstraintStringPosition::RequireCondition)
            }));
            assert!(result.is_ok(), "literal admission must not panic");
            let result = result.unwrap();
            assert!(
                result.is_err(),
                "manufactured pairs must be checked against the grammar"
            );
            let error = result.unwrap_err();
            assert!(!format!("{error} {error:?}").contains("CANARY_4386"));
        }
    }

    #[test]
    fn round1_fabricated_literal_children_cannot_bypass_key_or_literal_parsers() {
        let source = r#""${secret('CANARY_4386')}""#;
        let fabricate = || {
            pest::state(source, |p| {
                p.rule(Rule::string, |p| {
                    p.rule(Rule::double_quoted_string, |p| {
                        p.match_string("\"")
                            .and_then(|p| {
                                p.rule(Rule::string_part, |p| {
                                    p.rule(Rule::string_literal, |p| {
                                        p.match_string(&source[1..source.len() - 1])
                                    })
                                })
                            })
                            .and_then(|p| p.match_string("\""))
                    })
                })
            })
            .unwrap()
            .next()
            .unwrap()
        };
        let literal = super::super::string_literal::parse_string_literal(fabricate());
        assert!(
            literal.is_err(),
            "literal parsing must recheck source, not fabricated children"
        );
        let key = crate::parser::context::extract_key_string(fabricate());
        assert!(
            key.is_err(),
            "key parsing must use the same checked admission"
        );
        for error in [literal.unwrap_err(), key.unwrap_err()] {
            assert!(!format!("{error} {error:?}").contains("CANARY_4386"));
        }
    }

    #[test]
    fn validation_literal_values_and_source_proof_survive_serde() {
        for source in [
            r#""plain""#,
            r#"'${secret("literal text")}'"#,
            r#""\${secret('literal text')}""#,
            r#""line\nquote\"slash\\tab\t""#,
            r#"'quote\'slash\\'"#,
            "''",
        ] {
            let literal = ValidateStringLiteral::from_source(source).unwrap();
            assert_eq!(
                literal.as_str().as_bytes(),
                &source.as_bytes()[1..source.len() - 1]
            );
            let wire = serde_json::to_string(&literal).unwrap();
            assert_eq!(
                serde_json::from_str::<ValidateStringLiteral>(&wire).unwrap(),
                literal
            );
        }
        let single: ValidateStringLiteral =
            serde_json::from_value(serde_json::json!("'plain'")).unwrap();
        let double: ValidateStringLiteral =
            serde_json::from_value(serde_json::json!("\"plain\"")).unwrap();
        assert_eq!(
            single, double,
            "quote provenance must not change literal equality"
        );
        assert_ne!(
            serde_json::to_value(single).unwrap(),
            serde_json::to_value(double).unwrap()
        );
    }

    #[test]
    fn validation_literal_decode_rejects_old_contents_and_interpolation_without_echoing() {
        for source in [
            "old raw contents",
            "${secret('CANARY_4386')}",
            r#""${secret('CANARY_4386')}""#,
            r#""prefix ${secret({ CANARY_4386 = 'value' })} suffix""#,
            r#"'' "${secret('CANARY_4386')}""#,
            r#""\\${secret('CANARY_4386')}""#,
            r#""${}""#,
        ] {
            let wire = serde_json::json!({"String": source});
            let error = serde_json::from_value::<crate::parser::ValidateExpr>(wire).unwrap_err();
            assert_eq!(error.to_string(), "invalid validation string literal");
            assert!(!format!("{error:?} {error}").contains("CANARY_4386"));
        }
    }
}
