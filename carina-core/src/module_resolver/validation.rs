//! Expression evaluator for module `validate` and `require` blocks.
//!
//! This module implements a mini-language interpreter for:
//! - Per-argument `validate` expressions (single variable scope)
//! - Cross-argument `require` constraints (full argument map scope, with null support)

use std::collections::HashMap;

use crate::parser::{CompareOp, ValidateExpr};
use crate::resource::Value;

/// Format a Value for use in error messages.
pub(super) fn format_value_for_error(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{}\"", s),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(items) => format!("[...] (length {})", items.len()),
        Value::Map(map) => format!("{{...}} (length {})", map.len()),
        _ => format!("{:?}", value),
    }
}

/// Evaluate a validate expression with the given argument name and value.
/// Returns Ok(true) if validation passes, Ok(false) if it fails.
pub(super) fn evaluate_validate_expr(
    expr: &ValidateExpr,
    arg_name: &str,
    arg_value: &Value,
) -> Result<bool, String> {
    let result = eval_validate(expr, arg_name, arg_value)?;
    match result {
        ValidateValue::Bool(b) => Ok(b),
        other => Err(format!(
            "validate expression must return a boolean, got {:?}",
            other
        )),
    }
}

/// Internal value type for validate expression evaluation
#[derive(Debug, Clone)]
enum ValidateValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

/// Evaluate a validate expression node, returning a ValidateValue
fn eval_validate(
    expr: &ValidateExpr,
    arg_name: &str,
    arg_value: &Value,
) -> Result<ValidateValue, String> {
    match expr {
        ValidateExpr::Bool(b) => Ok(ValidateValue::Bool(*b)),
        ValidateExpr::Int(n) => Ok(ValidateValue::Int(*n)),
        ValidateExpr::Float(f) => Ok(ValidateValue::Float(*f)),
        ValidateExpr::String(s) => Ok(ValidateValue::String(s.clone())),
        ValidateExpr::Null => {
            Err("null is not supported in per-argument validation expressions".to_string())
        }
        ValidateExpr::Var(name) => {
            if name == arg_name {
                match arg_value {
                    Value::Int(n) => Ok(ValidateValue::Int(*n)),
                    Value::Float(f) => Ok(ValidateValue::Float(*f)),
                    Value::Bool(b) => Ok(ValidateValue::Bool(*b)),
                    Value::String(s) => Ok(ValidateValue::String(s.clone())),
                    other => Err(format!(
                        "unsupported value type for validation: {:?}",
                        other
                    )),
                }
            } else {
                Err(format!(
                    "unknown variable '{}' in validate expression (expected '{}')",
                    name, arg_name
                ))
            }
        }
        ValidateExpr::Compare { lhs, op, rhs } => {
            let left = eval_validate(lhs, arg_name, arg_value)?;
            let right = eval_validate(rhs, arg_name, arg_value)?;
            let result = compare_validate_values(&left, op, &right)?;
            Ok(ValidateValue::Bool(result))
        }
        ValidateExpr::And(lhs, rhs) => {
            let left = eval_validate(lhs, arg_name, arg_value)?;
            match left {
                ValidateValue::Bool(false) => Ok(ValidateValue::Bool(false)),
                ValidateValue::Bool(true) => {
                    let right = eval_validate(rhs, arg_name, arg_value)?;
                    match right {
                        ValidateValue::Bool(b) => Ok(ValidateValue::Bool(b)),
                        _ => Err("right operand of && must be boolean".to_string()),
                    }
                }
                _ => Err("left operand of && must be boolean".to_string()),
            }
        }
        ValidateExpr::Or(lhs, rhs) => {
            let left = eval_validate(lhs, arg_name, arg_value)?;
            match left {
                ValidateValue::Bool(true) => Ok(ValidateValue::Bool(true)),
                ValidateValue::Bool(false) => {
                    let right = eval_validate(rhs, arg_name, arg_value)?;
                    match right {
                        ValidateValue::Bool(b) => Ok(ValidateValue::Bool(b)),
                        _ => Err("right operand of || must be boolean".to_string()),
                    }
                }
                _ => Err("left operand of || must be boolean".to_string()),
            }
        }
        ValidateExpr::Not(inner) => {
            let val = eval_validate(inner, arg_name, arg_value)?;
            match val {
                ValidateValue::Bool(b) => Ok(ValidateValue::Bool(!b)),
                _ => Err("operand of ! must be boolean".to_string()),
            }
        }
        ValidateExpr::FunctionCall { name, args } => {
            eval_validate_function(name, args, arg_name, arg_value)
        }
    }
}

/// Compare two ValidateValues with the given operator
fn compare_validate_values(
    left: &ValidateValue,
    op: &CompareOp,
    right: &ValidateValue,
) -> Result<bool, String> {
    match (left, right) {
        (ValidateValue::Int(a), ValidateValue::Int(b)) => Ok(match op {
            CompareOp::Gte => a >= b,
            CompareOp::Lte => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Lt => a < b,
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
        }),
        (ValidateValue::Float(a), ValidateValue::Float(b)) => Ok(match op {
            CompareOp::Gte => a >= b,
            CompareOp::Lte => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Lt => a < b,
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
        }),
        (ValidateValue::Int(a), ValidateValue::Float(b)) => {
            let a = *a as f64;
            Ok(match op {
                CompareOp::Gte => a >= *b,
                CompareOp::Lte => a <= *b,
                CompareOp::Gt => a > *b,
                CompareOp::Lt => a < *b,
                CompareOp::Eq => a == *b,
                CompareOp::Ne => a != *b,
            })
        }
        (ValidateValue::Float(a), ValidateValue::Int(b)) => {
            let b = *b as f64;
            Ok(match op {
                CompareOp::Gte => *a >= b,
                CompareOp::Lte => *a <= b,
                CompareOp::Gt => *a > b,
                CompareOp::Lt => *a < b,
                CompareOp::Eq => *a == b,
                CompareOp::Ne => *a != b,
            })
        }
        (ValidateValue::String(a), ValidateValue::String(b)) => Ok(match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            _ => return Err("strings only support == and != comparisons".to_string()),
        }),
        (ValidateValue::Bool(a), ValidateValue::Bool(b)) => Ok(match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            _ => return Err("booleans only support == and != comparisons".to_string()),
        }),
        _ => Err(format!("cannot compare {:?} with {:?}", left, right)),
    }
}

/// Evaluate a function call in a validate expression
fn eval_validate_function(
    name: &str,
    args: &[ValidateExpr],
    arg_name: &str,
    arg_value: &Value,
) -> Result<ValidateValue, String> {
    match name {
        "len" | "length" => {
            if args.len() != 1 {
                return Err(format!("{}() expects 1 argument, got {}", name, args.len()));
            }
            // For Var references, access the original Value directly to support
            // List and Map types (which can't be represented as ValidateValue).
            if let ValidateExpr::Var(var_name) = &args[0]
                && var_name == arg_name
            {
                return match arg_value {
                    Value::String(s) => Ok(ValidateValue::Int(s.len() as i64)),
                    Value::List(items) => Ok(ValidateValue::Int(items.len() as i64)),
                    Value::Map(map) => Ok(ValidateValue::Int(map.len() as i64)),
                    _ => Err(format!(
                        "{}() argument must be a string, list, or map",
                        name
                    )),
                };
            }
            // For non-Var expressions (e.g., string literals), evaluate normally
            let val = eval_validate(&args[0], arg_name, arg_value)?;
            match val {
                ValidateValue::String(s) => Ok(ValidateValue::Int(s.len() as i64)),
                _ => Err(format!(
                    "{}() argument must be a string, list, or map",
                    name
                )),
            }
        }
        _ => Err(format!(
            "unknown function '{}' in validate expression",
            name
        )),
    }
}

/// Evaluate a require expression with access to all argument values.
/// Returns Ok(true) if the constraint is satisfied, Ok(false) if it fails.
pub(super) fn evaluate_require_expr(
    expr: &ValidateExpr,
    args: &HashMap<String, Value>,
) -> Result<bool, String> {
    let result = eval_require(expr, args)?;
    match result {
        RequireValue::Bool(b) => Ok(b),
        other => Err(format!(
            "require expression must return a boolean, got {:?}",
            other
        )),
    }
}

/// Internal value type for require expression evaluation
#[derive(Debug, Clone)]
enum RequireValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Null,
}

/// Evaluate a require expression node with access to all argument values
fn eval_require(
    expr: &ValidateExpr,
    args: &HashMap<String, Value>,
) -> Result<RequireValue, String> {
    match expr {
        ValidateExpr::Bool(b) => Ok(RequireValue::Bool(*b)),
        ValidateExpr::Int(n) => Ok(RequireValue::Int(*n)),
        ValidateExpr::Float(f) => Ok(RequireValue::Float(*f)),
        ValidateExpr::String(s) => Ok(RequireValue::String(s.clone())),
        ValidateExpr::Null => Ok(RequireValue::Null),
        ValidateExpr::Var(name) => {
            if let Some(value) = args.get(name) {
                match value {
                    Value::Int(n) => Ok(RequireValue::Int(*n)),
                    Value::Float(f) => Ok(RequireValue::Float(*f)),
                    Value::Bool(b) => Ok(RequireValue::Bool(*b)),
                    Value::String(s) => Ok(RequireValue::String(s.clone())),
                    other => Err(format!(
                        "unsupported value type for require expression: {:?}",
                        other
                    )),
                }
            } else {
                Err(format!("unknown variable '{}' in require expression", name))
            }
        }
        ValidateExpr::Compare { lhs, op, rhs } => {
            let left = eval_require(lhs, args)?;
            let right = eval_require(rhs, args)?;
            let result = compare_require_values(&left, op, &right)?;
            Ok(RequireValue::Bool(result))
        }
        ValidateExpr::And(lhs, rhs) => {
            let left = eval_require(lhs, args)?;
            match left {
                RequireValue::Bool(false) => Ok(RequireValue::Bool(false)),
                RequireValue::Bool(true) => {
                    let right = eval_require(rhs, args)?;
                    match right {
                        RequireValue::Bool(b) => Ok(RequireValue::Bool(b)),
                        _ => Err("right operand of && must be boolean".to_string()),
                    }
                }
                _ => Err("left operand of && must be boolean".to_string()),
            }
        }
        ValidateExpr::Or(lhs, rhs) => {
            let left = eval_require(lhs, args)?;
            match left {
                RequireValue::Bool(true) => Ok(RequireValue::Bool(true)),
                RequireValue::Bool(false) => {
                    let right = eval_require(rhs, args)?;
                    match right {
                        RequireValue::Bool(b) => Ok(RequireValue::Bool(b)),
                        _ => Err("right operand of || must be boolean".to_string()),
                    }
                }
                _ => Err("left operand of || must be boolean".to_string()),
            }
        }
        ValidateExpr::Not(inner) => {
            let val = eval_require(inner, args)?;
            match val {
                RequireValue::Bool(b) => Ok(RequireValue::Bool(!b)),
                _ => Err("operand of ! must be boolean".to_string()),
            }
        }
        ValidateExpr::FunctionCall {
            name,
            args: fn_args,
        } => eval_require_function(name, fn_args, args),
    }
}

/// Compare two RequireValues with the given operator
fn compare_require_values(
    left: &RequireValue,
    op: &CompareOp,
    right: &RequireValue,
) -> Result<bool, String> {
    // Handle null comparisons
    match (left, right) {
        (RequireValue::Null, RequireValue::Null) => {
            return Ok(matches!(op, CompareOp::Eq));
        }
        (RequireValue::Null, _) | (_, RequireValue::Null) => {
            return Ok(matches!(op, CompareOp::Ne));
        }
        _ => {}
    }

    match (left, right) {
        (RequireValue::Int(a), RequireValue::Int(b)) => Ok(match op {
            CompareOp::Gte => a >= b,
            CompareOp::Lte => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Lt => a < b,
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
        }),
        (RequireValue::Float(a), RequireValue::Float(b)) => Ok(match op {
            CompareOp::Gte => a >= b,
            CompareOp::Lte => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Lt => a < b,
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
        }),
        (RequireValue::Int(a), RequireValue::Float(b)) => {
            let a = *a as f64;
            Ok(match op {
                CompareOp::Gte => a >= *b,
                CompareOp::Lte => a <= *b,
                CompareOp::Gt => a > *b,
                CompareOp::Lt => a < *b,
                CompareOp::Eq => a == *b,
                CompareOp::Ne => a != *b,
            })
        }
        (RequireValue::Float(a), RequireValue::Int(b)) => {
            let b = *b as f64;
            Ok(match op {
                CompareOp::Gte => *a >= b,
                CompareOp::Lte => *a <= b,
                CompareOp::Gt => *a > b,
                CompareOp::Lt => *a < b,
                CompareOp::Eq => *a == b,
                CompareOp::Ne => *a != b,
            })
        }
        (RequireValue::String(a), RequireValue::String(b)) => Ok(match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            _ => return Err("strings only support == and != comparisons".to_string()),
        }),
        (RequireValue::Bool(a), RequireValue::Bool(b)) => Ok(match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            _ => return Err("booleans only support == and != comparisons".to_string()),
        }),
        _ => Err(format!("cannot compare {:?} with {:?}", left, right)),
    }
}

/// Evaluate a function call in a require expression
fn eval_require_function(
    name: &str,
    fn_args: &[ValidateExpr],
    args: &HashMap<String, Value>,
) -> Result<RequireValue, String> {
    match name {
        "len" | "length" => {
            if fn_args.len() != 1 {
                return Err(format!(
                    "{}() expects 1 argument, got {}",
                    name,
                    fn_args.len()
                ));
            }
            // For Var references, access the original Value directly to support
            // List and Map types (which can't be represented as RequireValue).
            if let ValidateExpr::Var(var_name) = &fn_args[0]
                && let Some(value) = args.get(var_name)
            {
                return match value {
                    Value::String(s) => Ok(RequireValue::Int(s.len() as i64)),
                    Value::List(items) => Ok(RequireValue::Int(items.len() as i64)),
                    Value::Map(map) => Ok(RequireValue::Int(map.len() as i64)),
                    _ => Err(format!(
                        "{}() argument must be a string, list, or map",
                        name
                    )),
                };
            }
            // For non-Var expressions, evaluate normally
            let val = eval_require(&fn_args[0], args)?;
            match val {
                RequireValue::String(s) => Ok(RequireValue::Int(s.len() as i64)),
                _ => Err(format!(
                    "{}() argument must be a string, list, or map",
                    name
                )),
            }
        }
        _ => Err(format!("unknown function '{}' in require expression", name)),
    }
}
