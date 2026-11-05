//! Pipe (`|>`), function-composition (`>>`), and null-coalescing (`??`)
//! expression parsers.
//!
//! Together with [`super::primary::parse_primary_eval`] these form the
//! `EvalValue`-returning expression chain. They can carry a closure
//! through the chain (partial application); upstream callers lower the
//! final `EvalValue` to a `Value` at the type boundary.

use crate::eval_value::EvalValue;
use crate::parser::ParsedValue as Value;
use crate::parser::context::ExpressionMode;
use crate::parser::expressions::integer_arithmetic::parse_additive_expr_with_mode;
use crate::parser::functions::{
    apply_parser_closure, evaluate_user_function_eval, lower_function_arguments,
    record_deferred_function_type_check, reject_builtin_handle_arguments,
};
use crate::parser::{
    ParseContext, ParseError, Rule, eval_type_name, is_static_eval, next_pair,
    parse_expression_eval,
};
use crate::resource::DeferredValue;

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
    location: crate::parser::DeclarationLocation,
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
        if !ctx.config.custom_types_loaded() {
            let arguments = lower_function_arguments(eval_args.clone(), line)?;
            record_deferred_function_type_check(
                &user_fn,
                &arguments,
                ctx,
                line,
                Some(column),
                location.clone(),
            );
        }
        if !ctx.seeded_user_functions.contains(&func_name) && eval_args.iter().all(is_static_eval) {
            return evaluate_user_function_eval(&user_fn, eval_args, ctx, line)
                .map(EvalValue::from_value);
        }
        let args = lower_function_arguments(eval_args, line)?;
        record_deferred_function_type_check(&user_fn, &args, ctx, line, Some(column), location);
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
    let operands = pair.into_inner();
    let count = operands.len();
    let mut values = operands
        .enumerate()
        .map(|(index, pair)| {
            let mut operand = mode.clone();
            for _ in 0..index {
                operand = operand.at_branch(crate::parser::ValueBranch::CoalesceDefault);
            }
            if index + 1 < count {
                operand = operand.at_branch(crate::parser::ValueBranch::CoalesceValue);
            }
            let start = ctx.parsed_binding_uses.borrow().records().len();
            let value = parse_pipe_expr_with_mode(pair, ctx, &operand)?;
            let end = ctx.parsed_binding_uses.borrow().records().len();
            Ok::<_, ParseError>((value, operand, start..end))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (mut result, _, _) = values
        .pop()
        .expect("coalesce_expr grammar requires at least one pipe_expr");

    // `??` is right-associative: `a ?? b ?? c` is `a ?? (b ?? c)`.
    // Parse every operand before folding so syntax and diagnostics in later
    // defaults are retained even when an earlier concrete value wins.
    while let Some((value, operand, records)) = values.pop() {
        // Resource diagnostics own every authored operand, even if evaluation
        // will choose a known left value. Runtime reference resolution still
        // folds this expression normally after the authored snapshot is kept.
        result = if ctx.preserve_resource_value_branches
            && mode.has_resource_sink()
            && !matches!(value, EvalValue::PendingValue(_))
            && !matches!(result, EvalValue::PendingValue(_))
        {
            EvalValue::from_value(Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(
                    value
                        .into_value()
                        .map_err(|error| ParseError::from_eval_value(error, line))?,
                ),
                default: Box::new(
                    result
                        .into_value()
                        .map_err(|error| ParseError::from_eval_value(error, line))?,
                ),
            }))
        } else {
            // The selected concrete operand no longer has a coalesce node.
            // Its binding uses must follow that same fold in the value walk.
            if matches!(&value, EvalValue::User(Value::Concrete(_))) {
                operand.fold_value_branch(ctx, records);
            }
            combine_coalesce_operands(value, result, line)?
        };
    }
    Ok(result)
}

pub(crate) fn combine_coalesce_operands(
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
    let first = next_pair(&mut inner, "additive expression", "compose expression")?;
    let operand_mode = has_composition.then(ExpressionMode::untyped);
    let operand_mode = operand_mode.as_ref().unwrap_or(mode);
    let mut value = parse_additive_expr_with_mode(first, ctx, operand_mode)?;

    // Collect remaining primaries for >> composition
    for rhs_pair in inner {
        let rhs = parse_additive_expr_with_mode(rhs_pair, ctx, operand_mode)?;

        value = combine_compose_operands(value, rhs)?;
    }

    Ok(value)
}

pub(crate) fn combine_compose_operands(
    value: EvalValue,
    rhs: EvalValue,
) -> Result<EvalValue, ParseError> {
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

    let functions = if let EvalValue::Closure {
        name,
        captured_args,
        ..
    } = &value
        && name == "__compose__"
    {
        let mut functions = captured_args.clone();
        functions.push(rhs);
        functions
    } else {
        vec![value, rhs]
    };

    Ok(EvalValue::closure("__compose__", functions, 1))
}

pub(crate) fn parse_pipe_expr_with_mode(
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
        let location = ctx.declaration_location(&func_call_pair);
        let (line, column) = func_call_pair.line_col();
        let mut fc_inner = func_call_pair.into_inner();
        let func_name = next_pair(&mut fc_inner, "function name", "pipe function call")?
            .as_str()
            .to_string();
        let extra_args: Result<Vec<EvalValue>, ParseError> = fc_inner
            .map(|arg| parse_expression_eval(arg, ctx))
            .collect();
        let extra_args = extra_args?;

        value = apply_pipe_step(value, func_name, extra_args, ctx, line, column, location)?;
    }

    Ok(value)
}
