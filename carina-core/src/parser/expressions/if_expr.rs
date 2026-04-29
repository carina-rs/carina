//! `if` / `if ... else ...` expression parsing.
//!
//! Two surface forms exist:
//!
//! * statement position (`let x = if ... { ... }`) — handled by
//!   [`parse_if_expr`], which returns a [`LetBindingRhs`] so the body can
//!   be a resource, a module call, or a value.
//! * value position (interpolations, attribute values) — handled by
//!   [`parse_if_value_expr`], which only allows a [`Value`] body and
//!   requires an `else` clause.

use crate::eval_value::EvalValue;
use crate::parser::{
    LetBindingRhs, ModuleCall, ParseContext, ParseError, Rule, evaluate_static_value, first_inner,
    is_static_value, next_pair, parse_expression, parse_module_call, parse_read_resource_expr,
    parse_resource_expr,
};
use crate::resource::{Resource, Value};

/// Result of parsing an if expression body: a resource, a module call, or a value
pub(crate) enum IfBodyResult {
    Resource(Box<Resource>),
    ModuleCall(ModuleCall),
    Value(Value),
}

/// Parse an if expression and conditionally include resources/module calls/values.
///
/// `if condition { body }` includes the body when condition is true.
/// `if condition { body } else { body }` selects one branch.
///
/// The condition must evaluate to a static Bool value at parse time.
pub(crate) fn parse_if_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if expression")?;
    let condition_value = parse_expression(condition_pair, ctx)?;

    // Ensure the condition is statically evaluable
    if !is_static_value(&condition_value) {
        return Err(ParseError::InvalidExpression {
            line: 0,
            message: "if condition depends on a runtime value; \
                      condition must be statically known at parse time"
                .to_string(),
        });
    }

    let condition_value = evaluate_static_value(condition_value, ctx.config)?;

    // Condition must be a Bool
    let condition = match &condition_value {
        Value::Bool(b) => *b,
        other => {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!("if condition must be a Bool value, got: {:?}", other),
            });
        }
    };

    // Parse the if body
    let if_body_pair = next_pair(&mut inner, "if body", "if expression")?;

    // Check for else clause
    let else_body_pair = inner.next();

    if condition {
        // Use the if branch
        parse_if_body_to_rhs(if_body_pair, ctx, binding_name)
    } else if let Some(else_pair) = else_body_pair {
        // Use the else branch
        let else_body = first_inner(else_pair, "else body", "else clause")?;
        parse_if_body_to_rhs(else_body, ctx, binding_name)
    } else {
        // No else clause and condition is false: produce nothing
        let ref_value = Value::String(format!("${{if:{}}}", binding_name));
        Ok((EvalValue::from_value(ref_value), vec![], vec![], None))
    }
}

/// Parse an if/else body and convert the result to a LetBindingRhs
pub(crate) fn parse_if_body_to_rhs(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let result = parse_if_body(pair, ctx, binding_name)?;
    match result {
        IfBodyResult::Resource(r) => {
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((EvalValue::from_value(ref_value), vec![*r], vec![], None))
        }
        IfBodyResult::ModuleCall(c) => {
            let value = Value::String(format!("${{module:{}}}", c.module_name));
            Ok((EvalValue::from_value(value), vec![], vec![c], None))
        }
        IfBodyResult::Value(v) => Ok((EvalValue::from_value(v), vec![], vec![], None)),
    }
}

/// Parse the body of an if expression and produce a resource, module call, or value
pub(crate) fn parse_if_body(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<IfBodyResult, ParseError> {
    let mut local_ctx = ctx.clone();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::if_local_binding => {
                let mut binding_inner = inner.into_inner();
                let name = next_pair(&mut binding_inner, "binding name", "if local binding")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut binding_inner, "binding value", "if local binding")?,
                    &local_ctx,
                )?;
                local_ctx.set_variable(name, value);
            }
            Rule::resource_expr => {
                let resource = parse_resource_expr(inner, &local_ctx, binding_name)?;
                return Ok(IfBodyResult::Resource(Box::new(resource)));
            }
            Rule::read_resource_expr => {
                let resource = parse_read_resource_expr(inner, &local_ctx, binding_name)?;
                return Ok(IfBodyResult::Resource(Box::new(resource)));
            }
            Rule::module_call => {
                let mut call = parse_module_call(inner, &local_ctx)?;
                call.binding_name = Some(binding_name.to_string());
                return Ok(IfBodyResult::ModuleCall(call));
            }
            Rule::expression => {
                let value = parse_expression(inner, &local_ctx)?;
                return Ok(IfBodyResult::Value(value));
            }
            _ => {}
        }
    }

    Err(ParseError::InternalError {
        expected: "resource expression, module call, or value expression".to_string(),
        context: "if body".to_string(),
    })
}

/// Parse an if/else expression in value position (attribute values, not let bindings).
///
/// Unlike `parse_if_expr()` which returns `LetBindingRhs` (resources, module calls, or values),
/// this function only returns `Value`. The condition must be a static Bool.
/// An else clause is required when the condition is false (a value must always be determined).
pub(crate) fn parse_if_value_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if value expression")?;
    let condition_value = parse_expression(condition_pair, ctx)?;

    // Ensure the condition is statically evaluable
    if !is_static_value(&condition_value) {
        return Err(ParseError::InvalidExpression {
            line: 0,
            message: "if condition depends on a runtime value; \
                      condition must be statically known at parse time"
                .to_string(),
        });
    }

    let condition_value = evaluate_static_value(condition_value, ctx.config)?;

    // Condition must be a Bool
    let condition = match &condition_value {
        Value::Bool(b) => *b,
        other => {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!("if condition must be a Bool value, got: {:?}", other),
            });
        }
    };

    // Parse the if body
    let if_body_pair = next_pair(&mut inner, "if body", "if value expression")?;

    // Check for else clause
    let else_body_pair = inner.next();

    if condition {
        parse_if_body_value(if_body_pair, ctx)
    } else if let Some(else_pair) = else_body_pair {
        let else_body = first_inner(else_pair, "else body", "else clause")?;
        parse_if_body_value(else_body, ctx)
    } else {
        Err(ParseError::InvalidExpression {
            line: 0,
            message: "if expression in value position requires an else clause \
                      when condition is false"
                .to_string(),
        })
    }
}

/// Parse the body of an if expression in value position and return only the value.
pub(crate) fn parse_if_body_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let mut local_ctx = ctx.clone();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::if_local_binding => {
                let mut binding_inner = inner.into_inner();
                let name = next_pair(&mut binding_inner, "binding name", "if local binding")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut binding_inner, "binding value", "if local binding")?,
                    &local_ctx,
                )?;
                local_ctx.set_variable(name, value);
            }
            Rule::expression => {
                return parse_expression(inner, &local_ctx);
            }
            Rule::resource_expr | Rule::read_resource_expr | Rule::module_call => {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: "resource expressions and module calls cannot be used \
                              in if value expressions; use a let binding instead"
                        .to_string(),
                });
            }
            _ => {}
        }
    }

    Err(ParseError::InternalError {
        expected: "value expression".to_string(),
        context: "if value expression body".to_string(),
    })
}
