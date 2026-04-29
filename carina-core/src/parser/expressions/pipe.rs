//! Pipe (`|>`), function-composition (`>>`), and null-coalescing (`??`)
//! expression parsers.
//!
//! Together with [`super::primary::parse_primary_eval`] these form the
//! `EvalValue`-returning expression chain. They can carry a closure
//! through the chain (partial application); upstream callers lower the
//! final `EvalValue` to a `Value` at the type boundary.

use crate::eval_value::EvalValue;
use crate::parser::expressions::primary::parse_primary_eval;
use crate::parser::{
    ParseContext, ParseError, Rule, eval_type_name, evaluate_user_function, is_static_eval,
    is_static_value, next_pair, parse_expression,
};
use crate::resource::Value;

pub(crate) fn parse_coalesce_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();
    let first = next_pair(&mut inner, "pipe expression", "coalesce expression")?;
    let value = parse_pipe_expr(first, ctx)?;

    // If there's a ?? right-hand side, check if left is an unresolved reference
    if let Some(rhs_pair) = inner.next() {
        let default = parse_pipe_expr(rhs_pair, ctx)?;
        match &value {
            EvalValue::User(Value::ResourceRef { .. }) => Ok(default),
            _ => Ok(value),
        }
    } else {
        Ok(value)
    }
}

pub(crate) fn parse_compose_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();
    let first = next_pair(&mut inner, "primary expression", "compose expression")?;
    let mut value = parse_primary_eval(first, ctx)?;

    // Collect remaining primaries for >> composition
    for rhs_pair in inner {
        let rhs = parse_primary_eval(rhs_pair, ctx)?;

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

pub(crate) fn parse_pipe_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();
    let compose = next_pair(&mut inner, "compose expression", "pipe expression")?;
    let mut value = parse_compose_expr(compose, ctx)?;

    // Desugar pipe: `x |> f(args)` becomes `f(x, args)`
    for func_call_pair in inner {
        let mut fc_inner = func_call_pair.into_inner();
        let func_name = next_pair(&mut fc_inner, "function name", "pipe function call")?
            .as_str()
            .to_string();
        let extra_args: Result<Vec<Value>, ParseError> =
            fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
        let extra_args = extra_args?;

        // Check if the pipe target is a Closure variable. The pipe
        // value (the running `value`) is appended last as an
        // EvalValue, so a closure carried in the binding can finish
        // applying through subsequent pipes.
        if let Some(EvalValue::Closure {
            name: fn_name,
            captured_args,
            remaining_arity,
        }) = ctx.get_variable(&func_name)
        {
            let mut all_args: Vec<EvalValue> = extra_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            all_args.push(value.clone());
            if all_args.iter().all(is_static_eval) {
                value = crate::builtins::apply_closure_with_config(
                    fn_name,
                    captured_args,
                    *remaining_arity,
                    &all_args,
                    ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: e,
                })?;
                continue;
            }
        }

        // Build args for the non-closure dispatch path. The running
        // pipe value must be a user-facing value here; a closure
        // would mean the user piped a partial application into a
        // non-closure-aware call, which we surface as a parse error.
        let pipe_value = match value {
            EvalValue::User(v) => v,
            EvalValue::Closure {
                ref name,
                remaining_arity,
                ..
            } => {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "cannot pipe a closure '{}' (still needs {} arg(s)) into '{}' \
                         — finish the partial application first",
                        name, remaining_arity, func_name
                    ),
                });
            }
        };
        let mut args = extra_args;
        args.push(pipe_value);

        // Try to eagerly evaluate user-defined function calls
        if ctx.user_functions.contains_key(&func_name) && args.iter().all(is_static_value) {
            let user_fn = ctx.user_functions.get(&func_name).unwrap().clone();
            value = EvalValue::from_value(evaluate_user_function(&user_fn, &args, ctx)?);
        } else if let Some(arity) = crate::builtins::builtin_arity(&func_name) {
            // Eagerly evaluate partial application for builtin pipe targets
            if args.len() < arity && args.iter().all(is_static_value) {
                let eval_args: Vec<EvalValue> =
                    args.iter().cloned().map(EvalValue::from_value).collect();
                value = crate::builtins::evaluate_builtin_with_config(
                    &func_name, &eval_args, ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("{}(): {}", func_name, e),
                })?;
            } else {
                value = EvalValue::from_value(Value::FunctionCall {
                    name: func_name,
                    args,
                });
            }
        } else {
            value = EvalValue::from_value(Value::FunctionCall {
                name: func_name,
                args,
            });
        }
    }

    Ok(value)
}
