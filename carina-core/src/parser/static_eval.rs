//! Static (compile-time) evaluation helpers used by the for-iterable
//! and pipe/compose paths to decide whether a value can be eagerly
//! reduced.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::error::ParseError;
use crate::eval_value::EvalValue;
use crate::resource::{ConcreteValue, DeferredValue, Value};

/// Check whether a Value is fully static (no runtime dependencies).
pub(crate) fn is_static_value(value: &Value) -> bool {
    match value {
        Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Concrete(ConcreteValue::Int(_))
        | Value::Concrete(ConcreteValue::Float(_))
        | Value::Concrete(ConcreteValue::Bool(_))
        | Value::Concrete(ConcreteValue::Duration(_))
        | Value::Concrete(ConcreteValue::StringList(_)) => true,
        Value::Concrete(ConcreteValue::List(items)) => items.iter().all(is_static_value),
        Value::Concrete(ConcreteValue::Map(map)) => map.values().all(is_static_value),
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            args.iter().all(is_static_value)
        }
        Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::Interpolation(_))
        | Value::Deferred(DeferredValue::Unknown(_)) => false,
        Value::Deferred(DeferredValue::Secret(inner)) => is_static_value(inner),
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
        Value::Concrete(ConcreteValue::List(items)) => Ok(Value::Concrete(ConcreteValue::List(
            items
                .into_iter()
                .map(|item| evaluate_static_value(item, config))
                .collect::<Result<Vec<_>, _>>()?,
        ))),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut evaluated = indexmap::IndexMap::with_capacity(map.len());
            for (key, value) in map {
                evaluated.insert(key, evaluate_static_value(value, config)?);
            }
            Ok(Value::Concrete(ConcreteValue::Map(evaluated)))
        }
        Value::Deferred(DeferredValue::Secret(inner)) => Ok(Value::Deferred(
            DeferredValue::Secret(Box::new(evaluate_static_value(*inner, config)?)),
        )),
        Value::Deferred(DeferredValue::FunctionCall { ref name, ref args }) => {
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

/// Resolve a statically-known function call for schema validation.
///
/// Editors retain the authored AST so reference diagnostics can inspect it,
/// but schema checks still need the same known values as the CLI's resolved
/// parse. Invalid or runtime-dependent calls remain deferred for their own
/// diagnostics.
pub fn evaluate_static_value_for_validation(
    value: &Value,
    config: &ProviderContext,
) -> Option<Value> {
    is_static_value(value)
        .then(|| evaluate_static_value(value.clone(), config))
        .and_then(Result::ok)
}
