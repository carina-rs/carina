//! Static (compile-time) evaluation helpers used by the for-iterable
//! and pipe/compose paths to decide whether a value can be eagerly
//! reduced.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::error::ParseError;
use crate::eval_value::AuthoredEvalValue as EvalValue;
use crate::resource::{
    AccessMissPolicy, AuthoredConcreteValue as ConcreteValue,
    AuthoredDeferredValue as DeferredValue, AuthoredFunctionArgument as FunctionArgument,
    AuthoredInterpolationPart as InterpolationPart, AuthoredValue as Value,
};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IfConditionDependency {
    Static,
    ModuleDependent,
    Runtime,
}

impl IfConditionDependency {
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Runtime, _) | (_, Self::Runtime) => Self::Runtime,
            (Self::ModuleDependent, _) | (_, Self::ModuleDependent) => Self::ModuleDependent,
            (Self::Static, Self::Static) => Self::Static,
        }
    }
}

/// Classify the dependencies of an `if` condition without evaluating it.
///
/// `is_static_value` remains the compatibility authority for eager root
/// conditions. Only values that authority rejects are inspected for the
/// narrower module-template case: every non-static leaf must be a module
/// argument (possibly composed through collections, access, or pure calls).
pub(crate) fn classify_if_condition(value: &Value) -> IfConditionDependency {
    if is_static_value(value) {
        IfConditionDependency::Static
    } else {
        classify_non_static_if_condition(value, false, &mut HashSet::new())
    }
}

fn classify_non_static_if_condition(
    value: &Value,
    function_placeholders_static: bool,
    visiting_functions: &mut HashSet<usize>,
) -> IfConditionDependency {
    if is_static_value(value) {
        return IfConditionDependency::Static;
    }

    match value {
        Value::Concrete(ConcreteValue::List(items)) => {
            items
                .iter()
                .fold(IfConditionDependency::Static, |dependency, item| {
                    dependency.combine(classify_non_static_if_condition(
                        item,
                        function_placeholders_static,
                        visiting_functions,
                    ))
                })
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            map.values()
                .fold(IfConditionDependency::Static, |dependency, item| {
                    dependency.combine(classify_non_static_if_condition(
                        item,
                        function_placeholders_static,
                        visiting_functions,
                    ))
                })
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        ) => IfConditionDependency::Static,
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => deferred.values().into_iter().fold(
            IfConditionDependency::Static,
            |dependency, value| {
                dependency.combine(classify_non_static_if_condition(
                    value,
                    function_placeholders_static,
                    visiting_functions,
                ))
            },
        ),
        Value::Deferred(DeferredValue::FunctionCall {
            name: _,
            args,
            source_line: _,
        }) => args
            .iter()
            .fold(IfConditionDependency::Static, |dependency, argument| {
                if argument.handle().is_some() {
                    dependency
                } else {
                    dependency.combine(classify_non_static_if_condition(
                        argument.walker_value(),
                        function_placeholders_static,
                        visiting_functions,
                    ))
                }
            }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions: _,
            args,
            source_line: _,
        }) => {
            let argument_dependency =
                args.iter()
                    .fold(IfConditionDependency::Static, |dependency, argument| {
                        if argument.handle().is_some() {
                            dependency
                        } else {
                            dependency.combine(classify_non_static_if_condition(
                                argument.walker_value(),
                                function_placeholders_static,
                                visiting_functions,
                            ))
                        }
                    });
            if argument_dependency == IfConditionDependency::Runtime {
                return argument_dependency;
            }

            let function_id = std::sync::Arc::as_ptr(function) as usize;
            if !visiting_functions.insert(function_id) {
                return IfConditionDependency::Runtime;
            }
            let body_dependency = function
                .local_lets
                .iter()
                .map(|(_, value, _)| value)
                .chain(std::iter::once(&function.body.value))
                .fold(IfConditionDependency::Static, |dependency, value| {
                    dependency.combine(classify_non_static_if_condition(
                        value,
                        true,
                        visiting_functions,
                    ))
                });
            visiting_functions.remove(&function_id);
            argument_dependency.combine(body_dependency)
        }
        Value::Deferred(DeferredValue::Access(access)) => classify_non_static_if_condition(
            access.base(),
            function_placeholders_static,
            visiting_functions,
        ),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().fold(
            IfConditionDependency::Static,
            |dependency, part| match part {
                InterpolationPart::Literal(_) => dependency,
                InterpolationPart::Expr(value) => {
                    dependency.combine(classify_non_static_if_condition(
                        value,
                        function_placeholders_static,
                        visiting_functions,
                    ))
                }
            },
        ),
        Value::Deferred(DeferredValue::Secret(inner)) => classify_non_static_if_condition(
            inner,
            function_placeholders_static,
            visiting_functions,
        ),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            classify_non_static_if_condition(
                value,
                function_placeholders_static,
                visiting_functions,
            )
            .combine(classify_non_static_if_condition(
                default,
                function_placeholders_static,
                visiting_functions,
            ))
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            classify_non_static_if_condition(
                expression.lhs(),
                function_placeholders_static,
                visiting_functions,
            )
            .combine(classify_non_static_if_condition(
                expression.rhs(),
                function_placeholders_static,
                visiting_functions,
            ))
        }
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::UnknownReason::ModuleArgument {
                name: _,
                source_line: _,
                diagnostic_name: _,
            },
        )) => IfConditionDependency::ModuleDependent,
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::UnknownReason::FnParam { name: _ }
            | crate::resource::UnknownReason::FnLocal { name: _ },
        )) if function_placeholders_static => IfConditionDependency::Static,
        Value::Deferred(
            DeferredValue::ResourceRef { path: _ }
            | DeferredValue::BindingRef { binding: _ }
            | DeferredValue::Unavailable {
                name: _,
                args: _,
                reason: _,
                source_line: _,
            }
            | DeferredValue::RedactedSecret { hash: _ }
            | DeferredValue::Unknown(
                crate::resource::UnknownReason::UpstreamExport { path: _ }
                | crate::resource::UnknownReason::UpstreamAllExports { binding: _ }
                | crate::resource::UnknownReason::ForKey
                | crate::resource::UnknownReason::ForIndex
                | crate::resource::UnknownReason::ForValue
                | crate::resource::UnknownReason::FnParam { name: _ }
                | crate::resource::UnknownReason::FnLocal { name: _ }
                | crate::resource::UnknownReason::EmptyInterpolation
                | crate::resource::UnknownReason::PendingReRead(_),
            ),
        ) => IfConditionDependency::Runtime,
    }
}

pub(crate) const RUNTIME_IF_CONDITION_MESSAGE: &str =
    "if condition depends on a runtime value; condition must be statically known at parse time";

#[derive(Debug)]
pub(crate) enum IfConditionDecisionError {
    Runtime,
    NonBool(String),
    Evaluation(ParseError),
}

impl IfConditionDecisionError {
    pub(crate) fn into_parse_error(self, line: usize) -> ParseError {
        match self {
            Self::Runtime => ParseError::InvalidExpression {
                line,
                message: RUNTIME_IF_CONDITION_MESSAGE.to_string(),
            },
            Self::NonBool(description) => ParseError::InvalidExpression {
                line,
                message: format!("if condition must be a Bool value, got: {description}"),
            },
            Self::Evaluation(error) => error,
        }
    }
}

impl std::fmt::Display for IfConditionDecisionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime => formatter.write_str(RUNTIME_IF_CONDITION_MESSAGE),
            Self::NonBool(description) => {
                write!(
                    formatter,
                    "if condition must be a Bool value, got: {description}"
                )
            }
            Self::Evaluation(error) => error.fmt(formatter),
        }
    }
}

/// Decide a condition that is either eager-static or a reusable module
/// template. `None` means argument substitution must decide it later.
pub(crate) fn decide_if_condition(
    value: &Value,
    config: &ProviderContext,
) -> Result<Option<bool>, IfConditionDecisionError> {
    match classify_if_condition(value) {
        IfConditionDependency::ModuleDependent => Ok(None),
        IfConditionDependency::Runtime => Err(IfConditionDecisionError::Runtime),
        IfConditionDependency::Static => {
            let evaluated = evaluate_static_value(value.clone(), config)
                .map_err(IfConditionDecisionError::Evaluation)?;
            match evaluated {
                Value::Concrete(ConcreteValue::Bool(condition)) => Ok(Some(condition)),
                other => Err(IfConditionDecisionError::NonBool(
                    crate::ValueDescription::of(&other).to_string(),
                )),
            }
        }
    }
}

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
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            is_static_value(expression.lhs()) && is_static_value(expression.rhs())
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            parts.iter().all(|part| match part {
                InterpolationPart::Literal(_) => true,
                InterpolationPart::Expr(value) => is_static_value(value),
            })
        }
        Value::Deferred(DeferredValue::ModuleIf(_))
        | Value::Deferred(DeferredValue::ResourceRef { .. })
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
            let evaluated_args: Result<Vec<FunctionArgument>, ParseError> = args
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
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let span = expression.operator_span();
            let lhs = evaluate_static_value(expression.lhs().clone(), config)?;
            let rhs = evaluate_static_value(expression.rhs().clone(), config)?;
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| ParseError::integer_arithmetic(span, 0, error))
        }
        Value::Deferred(DeferredValue::Unavailable {
            name,
            args: _,
            reason,
            source_line,
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
            DeferredValue::ModuleIf(_)
            | DeferredValue::ResourceRef { path: _ }
            | DeferredValue::BindingRef { binding: _ }
            | DeferredValue::Coalesce {
                value: _,
                default: _,
            }
            | DeferredValue::RedactedSecret { hash: _ }
            | DeferredValue::Unknown(
                crate::resource::UnknownReason::UpstreamExport { path: _ }
                | crate::resource::UnknownReason::UpstreamAllExports { binding: _ }
                | crate::resource::UnknownReason::ForKey
                | crate::resource::UnknownReason::ForIndex
                | crate::resource::UnknownReason::ForValue
                | crate::resource::UnknownReason::FnParam { name: _ }
                | crate::resource::UnknownReason::ModuleArgument {
                    name: _,
                    source_line: _,
                    diagnostic_name: _,
                }
                | crate::resource::UnknownReason::FnLocal { name: _ }
                | crate::resource::UnknownReason::EmptyInterpolation
                | crate::resource::UnknownReason::PendingReRead(_),
            ),
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
