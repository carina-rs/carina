//! Static (compile-time) evaluation helpers used by the for-iterable
//! and pipe/compose paths to decide whether a value can be eagerly
//! reduced.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::error::ParseError;
use crate::eval_value::EvalValue;
use crate::resource::{AccessMissPolicy, ConcreteValue, DeferredValue, InterpolationPart, Value};

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
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args.iter().all(|argument| {
            argument.handle().is_some() || argument.value().is_some_and(is_static_value)
        }),
        Value::Deferred(DeferredValue::Access(access)) => is_static_value(access.base()),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            parts.iter().all(|part| match part {
                InterpolationPart::Literal(_) => true,
                InterpolationPart::Expr(value) => is_static_value(value),
            })
        }
        Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::Coalesce { .. })
        | Value::Deferred(DeferredValue::Unavailable { .. })
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
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
        EvalValue::StructuralHandle(_) => true,
        EvalValue::PendingValue(_) => false,
        EvalValue::ProvisionalReference { .. } => false,
        EvalValue::ComputedWithStructuralHandles { value, .. } => is_static_value(value),
        EvalValue::DeferredForValue { .. } => false,
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
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let evaluated = parts
                .into_iter()
                .map(|part| match part {
                    InterpolationPart::Literal(value) => Ok(InterpolationPart::Literal(value)),
                    InterpolationPart::Expr(value) => {
                        evaluate_static_value(value, config).map(InterpolationPart::Expr)
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::Interpolation(evaluated)).canonicalize())
        }
        Value::Deferred(DeferredValue::FunctionCall {
            ref name, ref args, ..
        }) => {
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
            let evaluated_args: Result<Vec<crate::resource::FunctionArgument>, ParseError> = args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| evaluate_static_value(value.clone(), config))
                })
                .collect();
            let evaluated_args = evaluated_args?;
            let eval_args: Vec<EvalValue> = evaluated_args
                .iter()
                .map(crate::parser::functions::function_argument_as_eval)
                .collect();
            let result = crate::builtins::evaluate_builtin_with_config(name, &eval_args, config)
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("for iterable function call '{name}' failed: {e}"),
                })?;
            result
                .into_value()
                .map_err(|error| ParseError::from_eval_value(error, 0))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            ref function,
            ref functions,
            ref args,
            source_line,
        }) => {
            if !is_static_value(&value) {
                return Err(ParseError::InvalidExpression {
                    line: source_line.get().unwrap_or(function.body.source_line),
                    message: format!(
                        "for iterable function call '{}' depends on a runtime value; all arguments must be statically known at parse time",
                        function.name
                    ),
                });
            }
            let line = source_line.get().unwrap_or(function.body.source_line);
            let evaluated_args = args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| evaluate_static_value(value.clone(), config))
                })
                .collect::<Result<Vec<_>, _>>()?;
            super::functions::evaluate_resolved_user_function(
                function,
                functions,
                &evaluated_args,
                config,
                line,
            )
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let base = evaluate_static_value(access.base().clone(), config)?;
            access
                .rebuild_with_base(base, AccessMissPolicy::Error)
                .map_err(|error| ParseError::InvalidExpression {
                    line: access.source_line(),
                    message: error.to_string(),
                })
        }
        Value::Deferred(DeferredValue::Unavailable {
            name,
            reason,
            source_line,
            ..
        }) => Err(ParseError::InvalidExpression {
            line: source_line.get().unwrap_or(0),
            message: format!("{name}(): {reason}"),
        }),
        value @ Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | value @ Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Coalesce { .. }
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => Ok(value),
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
