//! `validate` expression parser.
//!
//! Parses the boolean expression that follows `validate ` in argument
//! validation blocks. The AST type [`super::super::ValidateExpr`] still
//! lives in `parser/mod.rs` (it moves in part 2 of #2262); only the
//! parser logic and the [`CompareOp`] enum live here.

use crate::parser::{ParseError, Rule, ValidateExpr, first_inner, next_pair};

/// Comparison operator used inside [`ValidateExpr::Compare`].
#[derive(Debug, Clone, PartialEq)]
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
) -> Result<ValidateExpr, ParseError> {
    match pair.as_rule() {
        Rule::validate_expr => {
            let inner = first_inner(pair, "validate_or_expr", "validate_expr")?;
            parse_validate_expr(inner)
        }
        Rule::validate_or_expr => {
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "validate_and_expr", "validate_or_expr")?;
            let mut result = parse_validate_expr(first)?;
            for next in inner {
                let right = parse_validate_expr(next)?;
                result = ValidateExpr::Or(Box::new(result), Box::new(right));
            }
            Ok(result)
        }
        Rule::validate_and_expr => {
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "validate_not_expr", "validate_and_expr")?;
            let mut result = parse_validate_expr(first)?;
            for next in inner {
                let right = parse_validate_expr(next)?;
                result = ValidateExpr::And(Box::new(result), Box::new(right));
            }
            Ok(result)
        }
        Rule::validate_not_expr => {
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "operand", "validate_not_expr")?;
            if first.as_rule() == Rule::validate_not_expr {
                // This is the "!" ~ validate_not_expr branch
                let operand = parse_validate_expr(first)?;
                Ok(ValidateExpr::Not(Box::new(operand)))
            } else {
                // This is the validate_comparison branch
                parse_validate_expr(first)
            }
        }
        Rule::validate_comparison => {
            let mut inner = pair.into_inner();
            let lhs_pair = next_pair(&mut inner, "validate_primary", "validate_comparison")?;
            let lhs = parse_validate_expr(lhs_pair)?;
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
                let rhs_pair =
                    next_pair(&mut inner, "validate_primary", "validate_comparison rhs")?;
                let rhs = parse_validate_expr(rhs_pair)?;
                Ok(ValidateExpr::Compare {
                    lhs: Box::new(lhs),
                    op,
                    rhs: Box::new(rhs),
                })
            } else {
                Ok(lhs)
            }
        }
        Rule::validate_primary => {
            let inner = first_inner(pair, "value", "validate_primary")?;
            parse_validate_expr(inner)
        }
        Rule::validate_function_call => {
            let mut inner = pair.into_inner();
            let name = next_pair(&mut inner, "function name", "validate_function_call")?
                .as_str()
                .to_string();
            let mut args = Vec::new();
            for arg_pair in inner {
                args.push(parse_validate_expr(arg_pair)?);
            }
            Ok(ValidateExpr::FunctionCall { name, args })
        }
        Rule::null_literal => Ok(ValidateExpr::Null),
        Rule::boolean => Ok(ValidateExpr::Bool(pair.as_str() == "true")),
        Rule::float => {
            let f: f64 = pair
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("Invalid float: {}", e),
                })?;
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
        Rule::string => {
            // Simple string parsing (no interpolation support in validate expressions)
            let raw = pair.as_str();
            // Strip surrounding quotes
            let s = &raw[1..raw.len() - 1];
            Ok(ValidateExpr::String(s.to_string()))
        }
        Rule::variable_ref => {
            // Variable reference - just the identifier name
            // For validate expressions, we only support simple variable names
            let inner = first_inner(pair, "identifier", "variable_ref")?;
            Ok(ValidateExpr::Var(inner.as_str().to_string()))
        }
        other => Err(ParseError::InvalidExpression {
            line: 0,
            message: format!("Unexpected rule in validate expression: {:?}", other),
        }),
    }
}
