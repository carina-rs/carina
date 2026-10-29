//! `if` / `if ... else ...` expression parsing.
//!
//! Two surface forms exist:
//!
//! * statement position (`let x = if ... { ... }`) — handled by
//!   [`parse_if_expr`], which returns a [`LetBindingRhs`] so the body can
//!   be a resource, a module call, or a value.
//! * value position (interpolations, attribute values) — handled by
//!   [`parse_if_value_expr_with_mode`], which only allows a [`Value`] body and
//!   requires an `else` clause.

use crate::ValueDescription;
use crate::eval_value::EvalValue;
use crate::parser::context::ExpressionMode;
use crate::parser::{
    LetBindingKind, LetBindingRhs, ModuleCall, ParseContext, ParseError, PreprocessedFile, Rule,
    evaluate_static_value, first_inner, if_structural_handle, is_static_value, next_pair,
    parse_expression, parse_expression_eval, parse_module_call, parse_read_resource_expr,
    parse_resource_expr,
};
use crate::resource::{ConcreteValue, DataSource, Resource, Value};

/// Result of parsing an if expression body: a managed resource, a data
/// source, a module call, or a value.
pub(crate) enum IfBodyResult {
    Resource(Box<Resource>),
    DataSource(Box<DataSource>),
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
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<LetBindingRhs, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if expression")?;
    let condition_line = condition_pair.line_col().0;
    let condition_value = parse_expression_eval(condition_pair, ctx)?;

    // Pass 1 of a directory fixed point knows that a sibling name exists but
    // not whether it is a value or a structural handle. Parse one branch to
    // classify this let binding, then let the final seeded parse evaluate (or
    // reject) the now-classified condition. Crucially, do not lower the
    // provisional name to a BindingRef here.
    if matches!(condition_value, EvalValue::ProvisionalReference { .. })
        && ctx.defer_provisional_static_conditions
    {
        let if_body_pair = next_pair(&mut inner, "if body", "if expression")?;
        return parse_if_body_to_rhs(
            if_body_pair,
            ctx,
            binding_name,
            preprocessed_file,
            source_file,
        );
    }
    let condition_value = condition_value
        .into_value()
        .map_err(|error| ParseError::from_eval_value(error, condition_line))?;

    // Ensure the condition is statically evaluable
    if !is_static_value(&condition_value) {
        return Err(ParseError::InvalidExpression {
            line: condition_line,
            message: "if condition depends on a runtime value; \
                      condition must be statically known at parse time"
                .to_string(),
        });
    }

    let condition_value = evaluate_static_value(condition_value, ctx.config)?;

    // Condition must be a Bool
    let condition = match &condition_value {
        Value::Concrete(ConcreteValue::Bool(b)) => *b,
        other => {
            return Err(ParseError::InvalidExpression {
                line: condition_line,
                message: format!(
                    "if condition must be a Bool value, got: {}",
                    ValueDescription::of(other)
                ),
            });
        }
    };

    // Parse the if body
    let if_body_pair = next_pair(&mut inner, "if body", "if expression")?;

    // Check for else clause
    let else_body_pair = inner.next();

    if condition {
        // Use the if branch
        parse_if_body_to_rhs(
            if_body_pair,
            ctx,
            binding_name,
            preprocessed_file,
            source_file,
        )
    } else if let Some(else_pair) = else_body_pair {
        // Use the else branch
        let else_body = first_inner(else_pair, "else body", "else clause")?;
        parse_if_body_to_rhs(else_body, ctx, binding_name, preprocessed_file, source_file)
    } else {
        // No else clause and condition is false: produce nothing
        Ok((
            if_structural_handle(binding_name),
            vec![],
            vec![],
            vec![],
            None,
            LetBindingKind::Structural,
        ))
    }
}

/// Parse an if/else body and convert the result to a LetBindingRhs
pub(crate) fn parse_if_body_to_rhs(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<LetBindingRhs, ParseError> {
    let result = parse_if_body(pair, ctx, binding_name, preprocessed_file, source_file)?;
    match result {
        IfBodyResult::Resource(r) => Ok((
            if_structural_handle(binding_name),
            vec![*r],
            vec![],
            vec![],
            None,
            LetBindingKind::Structural,
        )),
        IfBodyResult::DataSource(d) => Ok((
            if_structural_handle(binding_name),
            vec![],
            vec![*d],
            vec![],
            None,
            LetBindingKind::Structural,
        )),
        IfBodyResult::ModuleCall(c) => {
            let handle = if_structural_handle(binding_name);
            Ok((
                handle,
                vec![],
                vec![],
                vec![c],
                None,
                LetBindingKind::Structural,
            ))
        }
        IfBodyResult::Value(v) => Ok((
            EvalValue::from_value(v),
            vec![],
            vec![],
            vec![],
            None,
            LetBindingKind::Value,
        )),
    }
}

/// Parse the body of an if expression and produce a resource, module call, or value
pub(crate) fn parse_if_body(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<IfBodyResult, ParseError> {
    let mut local_ctx = ctx.child_scope();

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
                let resource = parse_resource_expr(
                    inner,
                    &local_ctx,
                    binding_name,
                    preprocessed_file,
                    source_file,
                )?;
                return Ok(IfBodyResult::Resource(Box::new(resource)));
            }
            Rule::read_resource_expr => {
                let data_source = parse_read_resource_expr(
                    inner,
                    &local_ctx,
                    binding_name,
                    preprocessed_file,
                    source_file,
                )?;
                return Ok(IfBodyResult::DataSource(Box::new(data_source)));
            }
            Rule::module_call => {
                let mut call =
                    parse_module_call(inner, &local_ctx, preprocessed_file, source_file)?;
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
pub(crate) fn parse_if_value_expr_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if value expression")?;
    let condition_line = condition_pair.line_col().0;
    let condition_value = parse_expression_eval(condition_pair, ctx)?;
    if matches!(condition_value, EvalValue::ProvisionalReference { .. })
        && ctx.defer_provisional_static_conditions
    {
        let if_body_pair = next_pair(&mut inner, "if body", "if value expression")?;
        return parse_if_body_value_with_mode(if_body_pair, ctx, mode);
    }
    let condition_value = condition_value
        .into_value()
        .map_err(|error| ParseError::from_eval_value(error, condition_line))?;

    // Ensure the condition is statically evaluable
    if !is_static_value(&condition_value) {
        return Err(ParseError::InvalidExpression {
            line: condition_line,
            message: "if condition depends on a runtime value; \
                      condition must be statically known at parse time"
                .to_string(),
        });
    }

    let condition_value = evaluate_static_value(condition_value, ctx.config)?;

    // Condition must be a Bool
    let condition = match &condition_value {
        Value::Concrete(ConcreteValue::Bool(b)) => *b,
        other => {
            return Err(ParseError::InvalidExpression {
                line: condition_line,
                message: format!(
                    "if condition must be a Bool value, got: {}",
                    ValueDescription::of(other)
                ),
            });
        }
    };

    // Parse the if body
    let if_body_pair = next_pair(&mut inner, "if body", "if value expression")?;

    // Check for else clause
    let else_body_pair = inner.next();

    if condition {
        parse_if_body_value_with_mode(if_body_pair, ctx, mode)
    } else if let Some(else_pair) = else_body_pair {
        let else_body = first_inner(else_pair, "else body", "else clause")?;
        parse_if_body_value_with_mode(else_body, ctx, mode)
    } else {
        Err(ParseError::InvalidExpression {
            line: condition_line,
            message: "if expression in value position requires an else clause \
                      when condition is false"
                .to_string(),
        })
    }
}

fn parse_if_body_value_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    let mut local_ctx = ctx.child_scope();

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
                return crate::parser::parse_expression_eval_with_mode(inner, &local_ctx, mode);
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
