//! Extended `let` binding parser. Handles every RHS form a top-level
//! `let` accepts: ordinary expressions, resource literals, `read`,
//! `upstream_state`, module calls, `use`, `if`, `for`, and pipes whose
//! intermediate value may be a closure.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::Rule;
use super::ast::{ModuleCall, UseStatement};
use super::blocks::backend::parse_upstream_state_expr;
use super::blocks::module_call::parse_module_call;
use super::blocks::resource::{parse_read_resource_expr, parse_resource_expr};
use super::blocks::use_stmt::parse_use_expr;
use super::context::{ParseContext, first_inner, next_pair};
use super::error::ParseError;
use super::expressions::for_expr::parse_for_expr;
use super::expressions::if_expr::parse_if_expr;
use super::expressions::primary::parse_primary_eval;
use super::parse_expression;
use super::static_eval::is_static_value;
use super::util::eval_type_name;
use crate::eval_value::EvalValue;
use crate::resource::{Resource, Value};

/// Tuple returned by the let-binding parser. The RHS is `EvalValue`
/// rather than `Value` so partial applications (closures) can survive
/// until a later pipe finishes them; the surrounding parse pass lowers
/// each binding to `Value` at the end of `parse(...)`.
pub(crate) type LetBindingRhs = (
    EvalValue,
    Vec<Resource>,
    Vec<ModuleCall>,
    Option<UseStatement>,
);

/// Extended parse_let_binding that also handles module calls, imports, and for expressions.
///
/// Returns `(name, value, resources, module_calls, import, is_structural)`.
/// `is_structural` is true when the RHS is an if/for/read expression, meaning the
/// `let` binding is structurally required and should not trigger unused-binding warnings.
#[allow(clippy::type_complexity)]
pub(super) fn parse_let_binding_extended(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
) -> Result<
    (
        String,
        EvalValue,
        Vec<Resource>,
        Vec<ModuleCall>,
        Option<UseStatement>,
        bool,
    ),
    ParseError,
> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "binding name", "let binding")?
        .as_str()
        .to_string();
    let rhs_pair = next_pair(&mut inner, "expression", "let binding")?;

    // `use` is the only meta-statement allowed as a let-binding RHS. The
    // grammar permits it here and only here (see carina.pest); everywhere
    // else it is a parse error.
    if rhs_pair.as_rule() == Rule::use_expr {
        let use_stmt = parse_use_expr(rhs_pair, &name, ctx)?;
        let value = Value::String(format!("${{use:{}}}", use_stmt.path));
        return Ok((
            name,
            EvalValue::from_value(value),
            vec![],
            vec![],
            Some(use_stmt),
            false,
        ));
    }

    // Detect if the RHS is a structurally-required expression (if/for/read)
    let is_structural = detect_structural_rhs(&rhs_pair);

    // Check if it's a module call, resource expression, or for expression
    let (value, expanded_resources, module_calls, maybe_import) =
        parse_expression_with_resource_or_module(rhs_pair, ctx, &name)?;

    Ok((
        name,
        value,
        expanded_resources,
        module_calls,
        maybe_import,
        is_structural,
    ))
}

/// Detect if an expression pair's innermost primary is an if/for/read/upstream_state expression.
fn detect_structural_rhs(pair: &pest::iterators::Pair<Rule>) -> bool {
    // Walk into expression -> pipe_expr -> compose_expr -> primary -> inner
    fn find_inner_rule(pair: &pest::iterators::Pair<Rule>) -> Option<Rule> {
        let inner = pair.clone().into_inner().next()?;
        match inner.as_rule() {
            Rule::if_expr
            | Rule::for_expr
            | Rule::read_resource_expr
            | Rule::upstream_state_expr => Some(inner.as_rule()),
            Rule::pipe_expr | Rule::compose_expr | Rule::coalesce_expr | Rule::expression => {
                find_inner_rule(&inner)
            }
            Rule::primary => {
                let primary_inner = inner.into_inner().next()?;
                match primary_inner.as_rule() {
                    Rule::if_expr
                    | Rule::for_expr
                    | Rule::read_resource_expr
                    | Rule::upstream_state_expr => Some(primary_inner.as_rule()),
                    _ => None,
                }
            }
            _ => None,
        }
    }
    find_inner_rule(pair).is_some()
}

/// Parse expression with potential resource, module call, or import.
fn parse_expression_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let coalesce = first_inner(pair, "expression", "expression with resource or module")?;
    let pipe = first_inner(coalesce, "pipe expression", "coalesce expression")?;
    parse_pipe_expr_with_resource_or_module(pipe, ctx, binding_name)
}

fn parse_pipe_expr_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let mut inner = pair.into_inner();
    let compose_pair = next_pair(&mut inner, "compose expression", "pipe expression")?;

    // Unwrap compose_expr: get its inner pairs
    let mut compose_inner = compose_pair.into_inner();
    let primary = next_pair(
        &mut compose_inner,
        "primary expression",
        "compose expression",
    )?;
    let (mut value, expanded_resources, module_calls, maybe_import) =
        parse_primary_with_resource_or_module(primary, ctx, binding_name)?;

    // Handle >> composition within the compose_expr
    let compose_rhs: Vec<_> = compose_inner.collect();
    if !compose_rhs.is_empty() {
        // Process the compose chain
        for rhs_pair in compose_rhs {
            let rhs = parse_primary_eval(rhs_pair, ctx)?;

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
                let mut fns = captured_args.clone();
                fns.push(rhs);
                fns
            } else {
                vec![value, rhs]
            };

            value = EvalValue::closure("__compose__", functions, 1);
        }
    }

    // Desugar pipe: `x |> f(args)` becomes `f(x, args)`
    for func_call_pair in inner {
        let mut fc_inner = func_call_pair.into_inner();
        let func_name = next_pair(&mut fc_inner, "function name", "pipe function call")?
            .as_str()
            .to_string();
        let extra_args: Result<Vec<Value>, ParseError> =
            fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
        let extra_args = extra_args?;

        // Lower the running pipe value to a `Value` for the builtin
        // dispatch path (which expects fully-reduced data arguments).
        // Closures are handled separately just below.
        let pipe_value_for_args = match &value {
            EvalValue::User(v) => Some(v.clone()),
            EvalValue::Closure { .. } => None,
        };

        // Check if the pipe target is a Closure variable
        if let Some(EvalValue::Closure {
            name: fn_name,
            captured_args,
            remaining_arity,
        }) = ctx.get_variable(&func_name)
        {
            // Build closure-application args. The pipe value (`x` in
            // `x |> f`) goes as the last argument; we keep it as
            // EvalValue so a chained closure can pipe through.
            let mut all_args: Vec<EvalValue> = extra_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            all_args.push(value.clone());
            if extra_args.iter().all(is_static_value)
                && pipe_value_for_args.as_ref().is_some_and(is_static_value)
            {
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

        // Build args for the non-closure dispatch path: at this point
        // we need a `Vec<Value>`, so the running pipe value must be a
        // user-facing value (not a closure).
        let pipe_value = match pipe_value_for_args {
            Some(v) => v,
            None => {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "cannot pipe a closure into '{}' — finish the partial application first",
                        func_name
                    ),
                });
            }
        };
        let mut args = extra_args;
        args.push(pipe_value);

        // Eagerly evaluate partial application for builtin pipe targets
        if let Some(arity) = crate::builtins::builtin_arity(&func_name)
            && args.len() < arity
            && args.iter().all(is_static_value)
        {
            let eval_args: Vec<EvalValue> =
                args.iter().cloned().map(EvalValue::from_value).collect();
            value =
                crate::builtins::evaluate_builtin_with_config(&func_name, &eval_args, ctx.config)
                    .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("{}(): {}", func_name, e),
                })?;
            continue;
        }

        value = EvalValue::from_value(Value::FunctionCall {
            name: func_name,
            args,
        });
    }

    Ok((value, expanded_resources, module_calls, maybe_import))
}

fn parse_primary_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let inner = first_inner(pair, "value", "primary expression")?;

    match inner.as_rule() {
        Rule::read_resource_expr => {
            let resource = parse_read_resource_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((
                EvalValue::from_value(ref_value),
                vec![resource],
                vec![],
                None,
            ))
        }
        Rule::upstream_state_expr => {
            let (line, _) = inner.as_span().start_pos().line_col();
            let us = parse_upstream_state_expr(inner, binding_name)?;
            if ctx.upstream_states.contains_key(&us.binding) {
                return Err(ParseError::DuplicateBinding {
                    name: us.binding,
                    line,
                });
            }
            ctx.upstream_states.insert(us.binding.clone(), us);
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((EvalValue::from_value(ref_value), vec![], vec![], None))
        }
        Rule::resource_expr => {
            let resource = parse_resource_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((
                EvalValue::from_value(ref_value),
                vec![resource],
                vec![],
                None,
            ))
        }
        Rule::for_expr => {
            let (resources, module_calls) = parse_for_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{for:{}}}", binding_name));
            Ok((
                EvalValue::from_value(ref_value),
                resources,
                module_calls,
                None,
            ))
        }
        Rule::if_expr => {
            let (value, resources, module_calls, import) = parse_if_expr(inner, ctx, binding_name)?;
            Ok((value, resources, module_calls, import))
        }
        Rule::module_call => {
            let call = parse_module_call(inner, ctx)?;
            let value = Value::String(format!("${{module:{}}}", call.module_name));
            Ok((EvalValue::from_value(value), vec![], vec![call], None))
        }
        Rule::function_call => {
            let value = parse_primary_eval(inner, ctx)?;
            Ok((value, vec![], vec![], None))
        }
        _ => {
            let value = parse_primary_eval(inner, ctx)?;
            Ok((value, vec![], vec![], None))
        }
    }
}
