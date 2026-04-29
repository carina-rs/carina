//! Static (compile-time) evaluation helpers used by the for-iterable
//! and pipe/compose paths to decide whether a value can be eagerly
//! reduced.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::error::ParseError;
use crate::eval_value::EvalValue;
use crate::resource::Value;

/// Check whether a Value is fully static (no runtime dependencies).
pub(crate) fn is_static_value(value: &Value) -> bool {
    match value {
        Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => true,
        Value::List(items) => items.iter().all(is_static_value),
        Value::Map(map) => map.values().all(is_static_value),
        Value::FunctionCall { args, .. } => args.iter().all(is_static_value),
        Value::ResourceRef { .. } | Value::Interpolation(_) => false,
        Value::Secret(inner) => is_static_value(inner),
    }
}

/// `is_static_value` for the evaluator-internal `EvalValue` type.
/// A closure's static-ness is decided by whether all of its captured
/// args are themselves static. The pipe/compose paths use this when
/// they need to decide whether to eagerly apply a partial application.
pub(crate) fn is_static_eval(value: &EvalValue) -> bool {
    match value {
        EvalValue::User(v) => is_static_value(v),
        EvalValue::Closure { captured_args, .. } => captured_args.iter().all(is_static_eval),
    }
}

/// If `value` is a FunctionCall with all static arguments, eagerly evaluate it.
/// Nested FunctionCalls in arguments are evaluated recursively first.
pub(crate) fn evaluate_static_value(
    value: Value,
    config: &ProviderContext,
) -> Result<Value, ParseError> {
    match value {
        Value::FunctionCall { ref name, ref args } => {
            if !is_static_value(&value) {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "for iterable function call '{name}' depends on a runtime value; \
                         all arguments must be statically known at parse time"
                    ),
                });
            }
            // Recursively evaluate any nested FunctionCall arguments
            let evaluated_args: Result<Vec<Value>, ParseError> = args
                .iter()
                .cloned()
                .map(|v| evaluate_static_value(v, config))
                .collect();
            let evaluated_args = evaluated_args?;
            let eval_args: Vec<EvalValue> = evaluated_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            let result = crate::builtins::evaluate_builtin_with_config(name, &eval_args, config)
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("for iterable function call '{name}' failed: {e}"),
                })?;
            result
                .into_value()
                .map_err(|leak| ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "for iterable function call '{name}' returned a closure '{}' \
                     (still needs {} arg(s)); finish the partial application",
                        leak.name, leak.remaining_arity
                    ),
                })
        }
        other => Ok(other),
    }
}
