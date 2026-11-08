//! Static (compile-time) evaluation helpers used by the for-iterable
//! and pipe/compose paths to decide whether a value can be eagerly
//! reduced.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::error::ParseError;
use crate::eval_value::EvalValue;
use crate::parser::{ParsedFunctionArgument as FunctionArgument, ParsedValue as Value};
use crate::resource::{
    AccessMissPolicy, AuthoredConcreteValue as ConcreteValue,
    AuthoredDeferredValue as DeferredValue, AuthoredInterpolationPart as InterpolationPart,
    ParserPlaceholder,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IfConditionDependency {
    Static,
    ModuleDependent,
    Runtime,
}

impl crate::resource::SealedSecretReadResult for IfConditionDependency {}
impl crate::resource::SecretReadResult for IfConditionDependency {}

impl IfConditionDependency {
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Runtime, _) | (_, Self::Runtime) => Self::Runtime,
            (Self::ModuleDependent, _) | (_, Self::ModuleDependent) => Self::ModuleDependent,
            (Self::Static, Self::Static) => Self::Static,
        }
    }
}

pub(crate) fn classify_if_condition(
    value: &crate::resource::knowledge::Classification<
        '_,
        ParserPlaceholder,
        crate::resource::RelativeBindingName,
    >,
) -> IfConditionDependency {
    use crate::resource::knowledge::PendingReason;
    if value.failure().is_some() {
        return IfConditionDependency::Runtime;
    }

    value
        .sites()
        .fold(IfConditionDependency::Static, |dependency, site| {
            dependency.combine(match site.reason() {
                PendingReason::Evaluation(_) => IfConditionDependency::Static,
                PendingReason::ModuleArgument => IfConditionDependency::ModuleDependent,
                PendingReason::ResourceSelection
                | PendingReason::BindingSelection
                | PendingReason::UpstreamExport
                | PendingReason::UpstreamAllExports
                | PendingReason::ReadBack
                | PendingReason::FunctionParameter
                | PendingReason::FunctionLocal
                | PendingReason::LoopKey
                | PendingReason::LoopIndex
                | PendingReason::LoopValue
                | PendingReason::IncompleteExpression => IfConditionDependency::Runtime,
            })
        })
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

pub(crate) fn decide_if_condition(
    value: &Value,
    config: &ProviderContext,
) -> Result<Option<bool>, IfConditionDecisionError> {
    let knowledge = value
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .map_err(|_| IfConditionDecisionError::Runtime)?;
    match classify_if_condition(&knowledge) {
        IfConditionDependency::ModuleDependent => Ok(None),
        IfConditionDependency::Runtime => Err(IfConditionDecisionError::Runtime),
        IfConditionDependency::Static => {
            let evaluated = evaluate_static_value(value.clone(), config)
                .map_err(IfConditionDecisionError::Evaluation)?;
            let known = evaluated
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .ok()
                .and_then(|value| value.into_known().ok());
            known
                .as_ref()
                .and_then(crate::resource::knowledge::known_bool)
                .map(Some)
                .ok_or_else(|| {
                    IfConditionDecisionError::NonBool(
                        crate::ValueDescription::of(&evaluated).to_string(),
                    )
                })
        }
    }
}

/// Scheduling for evaluator metadata uses Value::knowledge for value inputs.
/// A closure's static-ness is decided by whether all of its captured
/// args are themselves static. The pipe/compose paths use this when
/// they need to decide whether to eagerly apply a partial application.
pub(crate) fn is_static_eval(value: &EvalValue) -> bool {
    match value {
        EvalValue::User(v) => (v)
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .is_ok_and(|knowledge| knowledge.permits_static_evaluation()),
        EvalValue::UnexpandedStructuralHandle(_) => true,
        EvalValue::PendingValue(_) => false,
        EvalValue::ProvisionalReference { .. } => false,
        EvalValue::ComputedWithUnexpandedStructuralHandles { value, .. } => (value)
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .is_ok_and(|knowledge| knowledge.permits_static_evaluation()),
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
        Value::Deferred(DeferredValue::Secret(inner)) => {
            Ok(Value::Deferred(DeferredValue::Secret(inner.try_map(
                |value| evaluate_static_value(value.clone(), config),
            )?)))
        }
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
            ref name,
            ref args,
            source_line,
        }) => {
            if !value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .is_ok_and(|knowledge| knowledge.permits_static_evaluation())
            {
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
            let result = crate::builtins::evaluate_builtin_with_config(
                name,
                &eval_args,
                config,
                source_line,
            )
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
            if !value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .is_ok_and(|knowledge| knowledge.permits_static_evaluation())
            {
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
                None,
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
            DeferredValue::ModuleIf(_)
            | DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Coalesce { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
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
    value: &crate::resource::knowledge::Classification<
        '_,
        crate::resource::ParserPlaceholder,
        crate::resource::RelativeBindingName,
    >,
    config: &ProviderContext,
) -> Option<Value> {
    Some(value)
        .is_some_and(|knowledge| knowledge.permits_static_evaluation())
        .then(|| evaluate_static_value(value.to_value(), config))
        .and_then(Result::ok)
}

#[cfg(test)]
mod condition_dependency_tests {
    use super::*;
    #[test]
    fn attempted_failure_condition_is_runtime() {
        let value = Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![],
            reason: crate::resource::UnavailableReason::from_builtin_error(
                "already attempted".into(),
            ),
            source_line: 1.into(),
        });
        let proof = value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert_eq!(
            classify_if_condition(&proof),
            IfConditionDependency::Runtime
        );
        assert!(matches!(
            decide_if_condition(
                &value,
                &ProviderContext::new(crate::EnvironmentSnapshot::empty())
            ),
            Err(IfConditionDecisionError::Runtime)
        ));
    }
    #[test]
    fn shape_length_of_pending_members_is_one() {
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            "let result = length([aws_x.id])",
            std::path::Path::new("main.crn"),
            &config,
        )
        .unwrap();
        assert_eq!(
            evaluate_static_value(parsed.variables["result"].clone(), &config).unwrap(),
            Value::Concrete(ConcreteValue::Int(1))
        );
    }
}
