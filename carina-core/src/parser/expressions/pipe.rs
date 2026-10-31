//! Pipe (`|>`), function-composition (`>>`), and null-coalescing (`??`)
//! expression parsers.
//!
//! Together with [`super::primary::parse_primary_eval`] these form the
//! `EvalValue`-returning expression chain. They can carry a closure
//! through the chain (partial application); upstream callers lower the
//! final `EvalValue` to a `Value` at the type boundary.

use crate::eval_value::EvalValue;
use crate::parser::context::ExpressionMode;
use crate::parser::expressions::primary::parse_postfix_eval_with_mode;
use crate::parser::functions::{
    apply_parser_closure, evaluate_user_function_eval, lower_function_arguments,
    record_deferred_function_type_check, reject_builtin_handle_arguments,
};
use crate::parser::{
    ParseContext, ParseError, Rule, eval_type_name, is_static_eval, next_pair,
    parse_expression_eval,
};
use crate::resource::{DeferredValue, Value};

/// Apply one `|> function(extra_args...)` step.
///
/// Both ordinary expressions and the extended top-level `let` parser use
/// this exact desugaring path so closure handling, Ref-typed user-function
/// arguments, and deferred calls cannot drift.
pub(crate) fn apply_pipe_step(
    value: EvalValue,
    func_name: String,
    extra_args: Vec<EvalValue>,
    ctx: &ParseContext,
    line: usize,
    column: usize,
) -> Result<EvalValue, ParseError> {
    if let Some(EvalValue::Closure {
        name: fn_name,
        captured_args,
        remaining_arity,
    }) = ctx.get_variable(&func_name)
    {
        let mut all_args = extra_args.clone();
        all_args.push(value.clone());
        return apply_parser_closure(
            fn_name,
            captured_args,
            *remaining_arity,
            all_args,
            ctx,
            line,
        );
    }

    let mut eval_args = extra_args;
    eval_args.push(value);

    if crate::builtins::builtin_arity(&func_name).is_some() {
        reject_builtin_handle_arguments(&eval_args, line)?;
    }

    if let Some(user_fn) = ctx.resolved_user_function(&func_name) {
        if !ctx.seeded_user_functions.contains(&func_name) && eval_args.iter().all(is_static_eval) {
            return evaluate_user_function_eval(&user_fn, eval_args, ctx, line)
                .map(EvalValue::from_value);
        }
        let args = lower_function_arguments(eval_args, line)?;
        record_deferred_function_type_check(&user_fn, &args, ctx, line);
        return Ok(EvalValue::from_value(Value::Deferred(
            DeferredValue::ResolvedFunctionCall {
                function: user_fn,
                functions: ctx.shared_user_function_scope(),
                args,
                source_line: (line).into(),
            },
        )));
    }

    let args = lower_function_arguments(eval_args, line)?;
    let builtin = crate::builtins::BuiltinFunction::from_name(&func_name).ok_or_else(|| {
        ParseError::UnknownFunction {
            name: func_name.clone(),
            line,
            column,
        }
    })?;

    if let Some(arity) = crate::builtins::builtin_arity(&func_name)
        && args.len() < arity
        && args
            .iter()
            .all(|argument| argument.value().is_some_and(crate::parser::is_static_value))
    {
        let eval_args: Vec<EvalValue> = args
            .iter()
            .map(|argument| {
                EvalValue::from_value(
                    argument
                        .value()
                        .expect("static function argument is a value")
                        .clone(),
                )
            })
            .collect();
        return crate::builtins::evaluate_builtin_with_config(&func_name, &eval_args, ctx.config)
            .map_err(|message| ParseError::InvalidExpression {
                line,
                message: format!("{func_name}(): {message}"),
            });
    }

    Ok(EvalValue::from_value(Value::Deferred(
        DeferredValue::FunctionCall {
            name: builtin,
            args,
            source_line: (line).into(),
        },
    )))
}

pub(crate) fn parse_coalesce_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    parse_coalesce_expr_with_mode(pair, ctx, &ExpressionMode::untyped())
}

pub(crate) fn parse_coalesce_expr_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    let line = pair.line_col().0;
    let mut values = pair
        .into_inner()
        .map(|pair| parse_pipe_expr_with_mode(pair, ctx, mode))
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = values
        .pop()
        .expect("coalesce_expr grammar requires at least one pipe_expr");

    // `??` is right-associative: `a ?? b ?? c` is `a ?? (b ?? c)`.
    // Parse every operand before folding so syntax and diagnostics in later
    // defaults are retained even when an earlier concrete value wins.
    while let Some(value) = values.pop() {
        result = combine_coalesce_operands(value, result, line)?;
    }
    Ok(result)
}

fn combine_coalesce_operands(
    value: EvalValue,
    default: EvalValue,
    line: usize,
) -> Result<EvalValue, ParseError> {
    if matches!(value, EvalValue::PendingValue(_)) {
        return Ok(value);
    }
    if matches!(default, EvalValue::PendingValue(_)) {
        return Ok(default);
    }
    match value {
        // A concrete user value is present by construction. Do not fold a
        // computed structural value merely because its payload currently
        // looks concrete: doing so would discard its parser metadata.
        concrete @ EvalValue::User(Value::Concrete(_)) => Ok(concrete),
        EvalValue::User(value) => {
            let default = default
                .into_value()
                .map_err(|error| ParseError::from_eval_value(error, line))?;
            Ok(EvalValue::from_value(Value::Deferred(
                DeferredValue::Coalesce {
                    value: Box::new(value),
                    default: Box::new(default),
                },
            )))
        }
        EvalValue::ComputedWithUnexpandedStructuralHandles { value, metadata } => {
            let default = default
                .into_value()
                .map_err(|error| ParseError::from_eval_value(error, line))?;
            Ok(EvalValue::ComputedWithUnexpandedStructuralHandles {
                value: Value::Deferred(DeferredValue::Coalesce {
                    value: Box::new(value),
                    default: Box::new(default),
                }),
                metadata,
            })
        }
        other => {
            let value = other
                .into_value()
                .map_err(|error| ParseError::from_eval_value(error, line))?;
            let default = default
                .into_value()
                .map_err(|error| ParseError::from_eval_value(error, line))?;
            Ok(EvalValue::from_value(Value::Deferred(
                DeferredValue::Coalesce {
                    value: Box::new(value),
                    default: Box::new(default),
                },
            )))
        }
    }
}

fn parse_compose_expr_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();
    let has_composition = inner.clone().count() > 1;
    let first = next_pair(&mut inner, "postfix expression", "compose expression")?;
    let operand_mode = has_composition.then(ExpressionMode::untyped);
    let operand_mode = operand_mode.as_ref().unwrap_or(mode);
    let mut value = parse_postfix_eval_with_mode(first, ctx, operand_mode)?;

    // Collect remaining primaries for >> composition
    for rhs_pair in inner {
        let rhs = parse_postfix_eval_with_mode(rhs_pair, ctx, operand_mode)?;

        // Both sides must be Closures
        if !value.is_closure() {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!(
                    "left side of >> must be a Closure (partially applied function), got {}",
                    eval_type_name(&value)
                ),
            });
        }
        if !rhs.is_closure() {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!(
                    "right side of >> must be a Closure (partially applied function), got {}",
                    eval_type_name(&rhs)
                ),
            });
        }

        // Build a composed closure: __compose__ with the chain stored in captured_args
        // If the left side is already a __compose__, extend the chain; otherwise start a new one
        let functions = if let EvalValue::Closure {
            name,
            captured_args,
            ..
        } = &value
            && name == "__compose__"
        {
            let mut fns = captured_args.clone();
            fns.push(rhs);
            fns
        } else {
            vec![value, rhs]
        };

        value = EvalValue::closure("__compose__", functions, 1);
    }

    Ok(value)
}

fn parse_pipe_expr_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();
    let has_pipe = inner.clone().count() > 1;
    let compose = next_pair(&mut inner, "compose expression", "pipe expression")?;
    let operand_mode = has_pipe.then(ExpressionMode::untyped);
    let mut value =
        parse_compose_expr_with_mode(compose, ctx, operand_mode.as_ref().unwrap_or(mode))?;

    // Desugar pipe: `x |> f(args)` becomes `f(x, args)`
    for func_call_pair in inner {
        let (line, column) = func_call_pair.line_col();
        let mut fc_inner = func_call_pair.into_inner();
        let func_name = next_pair(&mut fc_inner, "function name", "pipe function call")?
            .as_str()
            .to_string();
        let extra_args: Result<Vec<EvalValue>, ParseError> = fc_inner
            .map(|arg| parse_expression_eval(arg, ctx))
            .collect();
        let extra_args = extra_args?;

        value = apply_pipe_step(value, func_name, extra_args, ctx, line, column)?;
    }

    Ok(value)
}
