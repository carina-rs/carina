//! `if` / `if ... else ...` expression parsing.
//!
//! Two surface forms exist:
//!
//! * statement position (`let x = if ... { ... }`) — handled by
//!   [`parse_if_expr`], which returns a [`LetBindingRhs`] so the body can
//!   be a resource, a module call, or a value.
//! * value position (interpolations, attribute values) — handled by
//!   [`parse_if_value_expr_with_mode`], which only allows a [`Value`] body and
//!   requires an `else` branch so the false case also has a value.

use crate::eval_value::EvalValue;
use crate::parser::context::ExpressionMode;
use crate::parser::expressions::for_expr::LoopParseTarget;
use crate::parser::{
    DeferredIfBody, DeferredIfCondition, DeferredStructuralIf, DeferredValueIf, LetBindingKind,
    LetBindingRhs, ModuleCall, ParseContext, ParseError, PreprocessedFile, Rule,
    decide_if_condition, first_inner, if_structural_handle, next_pair, parse_expression,
    parse_expression_eval, parse_module_call, parse_read_resource_expr, parse_resource_for_target,
};
use crate::parser::{
    ParsedDataSource as DataSource, ParsedResource as Resource, ParsedValue as Value,
};
use crate::resource::{DeferredValue, LoopKind};

/// Result of parsing an if expression body: a managed resource, a data
/// source, a module call, or a value.
pub(crate) enum IfBodyResult {
    Resource(Box<Resource>),
    DataSource(Box<DataSource>),
    ModuleCall(ModuleCall),
    Value(Value),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IfBodyKind {
    Structural,
    Value,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IfBodyForm {
    Resource,
    DataSource,
    ModuleCall,
    Value,
    ResourceProducingFor,
    UnsupportedStructuralExpression,
}

struct ClassifiedIfBody<'i> {
    pair: pest::iterators::Pair<'i, Rule>,
    form: IfBodyForm,
}

impl ClassifiedIfBody<'_> {
    fn kind(&self) -> IfBodyKind {
        match self.form {
            IfBodyForm::Value => IfBodyKind::Value,
            IfBodyForm::Resource
            | IfBodyForm::DataSource
            | IfBodyForm::ModuleCall
            | IfBodyForm::ResourceProducingFor
            | IfBodyForm::UnsupportedStructuralExpression => IfBodyKind::Structural,
        }
    }

    fn reject_resource_producing_for(&self, condition_line: usize) -> Result<(), ParseError> {
        if self.form == IfBodyForm::ResourceProducingFor {
            return Err(resource_producing_for_if_branch_error(condition_line));
        }
        Ok(())
    }
}

fn classify_if_body<'i>(
    pair: pest::iterators::Pair<'i, Rule>,
) -> Result<ClassifiedIfBody<'i>, ParseError> {
    let tail = pair
        .clone()
        .into_inner()
        .next_back()
        .ok_or_else(|| ParseError::InternalError {
            expected: "resource expression, module call, or value expression".to_string(),
            context: "if body classification".to_string(),
        })?;
    let form = match tail.as_rule() {
        Rule::resource_expr => IfBodyForm::Resource,
        Rule::read_resource_expr => IfBodyForm::DataSource,
        Rule::module_call => IfBodyForm::ModuleCall,
        Rule::expression if expression_root_rule(&tail) == Some(Rule::for_expr) => {
            IfBodyForm::ResourceProducingFor
        }
        Rule::expression if expression_contains_structural_declaration(&tail) => {
            IfBodyForm::UnsupportedStructuralExpression
        }
        Rule::expression => IfBodyForm::Value,
        _ => Err(ParseError::InternalError {
            expected: "resource expression, module call, or value expression".to_string(),
            context: "if body classification".to_string(),
        })?,
    };
    Ok(ClassifiedIfBody { pair, form })
}

fn expression_root_rule(pair: &pest::iterators::Pair<Rule>) -> Option<Rule> {
    let mut current = pair.clone();
    loop {
        match current.as_rule() {
            Rule::expression
            | Rule::coalesce_expr
            | Rule::pipe_expr
            | Rule::compose_expr
            | Rule::additive_expr
            | Rule::multiplicative_expr
            | Rule::postfix_expr => current = current.into_inner().next()?,
            Rule::primary => {
                let inner = current.into_inner().next()?;
                if inner.as_rule() == Rule::expression {
                    current = inner;
                } else {
                    return Some(inner.as_rule());
                }
            }
            rule => return Some(rule),
        }
    }
}

fn expression_contains_structural_declaration(pair: &pest::iterators::Pair<Rule>) -> bool {
    pair.clone().into_inner().any(|inner| {
        matches!(
            inner.as_rule(),
            Rule::resource_expr | Rule::read_resource_expr | Rule::module_call
        ) || expression_contains_structural_declaration(&inner)
    })
}

fn validate_if_body_shapes(
    then_body: &ClassifiedIfBody<'_>,
    else_body: Option<&ClassifiedIfBody<'_>>,
    condition_line: usize,
) -> Result<(), ParseError> {
    then_body.reject_resource_producing_for(condition_line)?;
    if let Some(else_body) = else_body {
        else_body.reject_resource_producing_for(condition_line)?;
    }
    let then_kind = then_body.kind();
    let else_kind = else_body.map(ClassifiedIfBody::kind);
    match (then_kind, else_kind) {
        (IfBodyKind::Value, None) => Err(ParseError::InvalidExpression {
            line: condition_line,
            message: "an `if` that produces a value must have an `else` branch, because there is no value to use when the condition is false; add `else { <value> }`"
                .to_string(),
        }),
        (IfBodyKind::Value, Some(IfBodyKind::Structural)) => Err(ParseError::InvalidExpression {
            line: condition_line,
            message: "both branches of this `if` must be the same kind: the `then` branch produces a value but the `else` branch declares resources/module calls; make both branches values, or both branches resource/module declarations"
                .to_string(),
        }),
        (IfBodyKind::Structural, Some(IfBodyKind::Value)) => Err(ParseError::InvalidExpression {
            line: condition_line,
            message: "both branches of this `if` must be the same kind: the `then` branch declares resources/module calls but the `else` branch produces a value; make both branches values, or both branches resource/module declarations"
                .to_string(),
        }),
        (IfBodyKind::Structural, None)
        | (IfBodyKind::Value, Some(IfBodyKind::Value))
        | (IfBodyKind::Structural, Some(IfBodyKind::Structural)) => Ok(()),
    }
}

fn resource_producing_for_if_branch_error(line: usize) -> ParseError {
    ParseError::InvalidExpression {
        line,
        message: "resource-producing `for` expressions cannot be used directly as `if` branches; condition the `for` iterable instead, for example `for item in (if flag { items } else { [] }) { ... }`"
            .to_string(),
    }
}

fn structural_if_value_error(line: usize) -> ParseError {
    ParseError::InvalidExpression {
        line,
        message: "resource expressions and module calls cannot be used in if value expressions; use a let binding instead"
            .to_string(),
    }
}

/// Parse an if expression and conditionally include resources/module calls/values.
///
/// `if condition { body }` includes the body when condition is true.
/// `if condition { body } else { body }` selects one branch.
///
/// In root files the condition must evaluate to a static Bool at parse time.
/// A module-argument-only condition is retained as a parser template and
/// selected after call-site argument substitution.
pub(crate) fn parse_if_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    target: LoopParseTarget,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<LetBindingRhs, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if expression")?;
    let condition_line = condition_pair.line_col().0;
    let condition_source = condition_pair.as_str().trim().to_string();
    let condition_value = parse_expression_eval(condition_pair, ctx)?;

    // Pass 1 of a directory fixed point knows that a sibling name exists but
    // not whether it is a value or a structural handle. Parse one branch to
    // classify this let binding, then let the final seeded parse evaluate (or
    // reject) the now-classified condition. Crucially, do not lower the
    // provisional name to a BindingRef here.
    if matches!(condition_value, EvalValue::ProvisionalReference { .. })
        && ctx.defer_provisional_static_conditions
    {
        let if_body = classify_if_body(next_pair(&mut inner, "if body", "if expression")?)?;
        if_body.reject_resource_producing_for(condition_line)?;
        return parse_if_body_to_rhs(if_body, ctx, &target, preprocessed_file, source_file);
    }
    let condition_value = condition_value
        .into_value()
        .map_err(|error| ParseError::from_eval_value(error, condition_line))?;

    let decision = decide_if_condition(&condition_value, ctx.config)
        .map_err(|error| error.into_parse_error(condition_line))?;
    let if_body = classify_if_body(next_pair(&mut inner, "if body", "if expression")?)?;
    let else_body = inner
        .next()
        .map(|else_pair| first_inner(else_pair, "else body", "else clause"))
        .transpose()?
        .map(classify_if_body)
        .transpose()?;
    validate_if_body_shapes(&if_body, else_body.as_ref(), condition_line)?;

    if decision.is_none() {
        let then_branch = parse_if_body(if_body, ctx, &target, preprocessed_file, source_file)?;
        let else_branch = else_body
            .map(|else_body| parse_if_body(else_body, ctx, &target, preprocessed_file, source_file))
            .transpose()?;
        let condition = DeferredIfCondition::new_with_source_expression(
            condition_value,
            source_file.to_path_buf(),
            condition_line,
            condition_source,
        );
        return defer_if_body(condition, then_branch, else_branch, ctx, &target);
    }

    if decision.expect("module-dependent decision returned above") {
        // Use the if branch
        parse_if_body_to_rhs(if_body, ctx, &target, preprocessed_file, source_file)
    } else if let Some(else_body) = else_body {
        // Use the else branch
        parse_if_body_to_rhs(else_body, ctx, &target, preprocessed_file, source_file)
    } else {
        // No else clause and condition is false: produce nothing
        Ok((
            if_structural_handle(target.scheduler_key()),
            vec![],
            vec![],
            vec![],
            None,
            LetBindingKind::Structural,
        ))
    }
}

fn defer_if_body(
    condition: DeferredIfCondition,
    then_branch: IfBodyResult,
    else_branch: Option<IfBodyResult>,
    ctx: &mut ParseContext,
    target: &LoopParseTarget,
) -> Result<LetBindingRhs, ParseError> {
    match (then_branch, else_branch) {
        (IfBodyResult::Value(then_value), Some(IfBodyResult::Value(else_value))) => Ok((
            EvalValue::from_value(Value::Deferred(DeferredValue::ModuleIf(Box::new(
                DeferredValueIf::new(condition, then_value, else_value),
            )))),
            vec![],
            vec![],
            vec![],
            None,
            LetBindingKind::Value,
        )),
        (IfBodyResult::Value(_), None)
        | (IfBodyResult::Value(_), Some(_))
        | (_, Some(IfBodyResult::Value(_))) => Err(ParseError::InternalError {
            expected: "validated if branch shapes".to_string(),
            context: "deferred module if".to_string(),
        }),
        (then_branch, else_branch) => {
            let then_branch = structural_if_body(then_branch)?;
            let else_branch = else_branch.map(structural_if_body).transpose()?;
            ctx.deferred_module_ifs.push(DeferredStructuralIf::new(
                condition,
                then_branch,
                else_branch,
            ));
            Ok((
                if_structural_handle(target.scheduler_key()),
                vec![],
                vec![],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
    }
}

fn structural_if_body(body: IfBodyResult) -> Result<DeferredIfBody, ParseError> {
    match body {
        IfBodyResult::Resource(resource) => Ok(DeferredIfBody::Resource(resource)),
        IfBodyResult::DataSource(data_source) => Ok(DeferredIfBody::DataSource(data_source)),
        IfBodyResult::ModuleCall(call) => Ok(DeferredIfBody::ModuleCall(call)),
        IfBodyResult::Value(_) => Err(ParseError::InternalError {
            expected: "structural if body".to_string(),
            context: "deferred module if".to_string(),
        }),
    }
}

/// Parse an if/else body and convert the result to a LetBindingRhs
fn parse_if_body_to_rhs(
    body: ClassifiedIfBody<'_>,
    ctx: &ParseContext,
    target: &LoopParseTarget,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<LetBindingRhs, ParseError> {
    let result = parse_if_body(body, ctx, target, preprocessed_file, source_file)?;
    match result {
        IfBodyResult::Resource(r) => Ok((
            if_structural_handle(target.scheduler_key()),
            vec![*r],
            vec![],
            vec![],
            None,
            LetBindingKind::Structural,
        )),
        IfBodyResult::DataSource(d) => Ok((
            if_structural_handle(target.scheduler_key()),
            vec![],
            vec![*d],
            vec![],
            None,
            LetBindingKind::Structural,
        )),
        IfBodyResult::ModuleCall(c) => {
            let handle = if_structural_handle(target.scheduler_key());
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
fn parse_if_body(
    body: ClassifiedIfBody<'_>,
    ctx: &ParseContext,
    target: &LoopParseTarget,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<IfBodyResult, ParseError> {
    let ClassifiedIfBody { pair, form } = body;
    let body_line = pair.line_col().0;
    let mut local_ctx = ctx.child_scope();

    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::if_local_binding {
            let mut binding_inner = inner.into_inner();
            let name = next_pair(&mut binding_inner, "binding name", "if local binding")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut binding_inner, "binding value", "if local binding")?,
                &local_ctx,
            )?;
            local_ctx.set_variable(name, value);
            continue;
        }

        return match (form, inner.as_rule()) {
            (IfBodyForm::Resource, Rule::resource_expr) => {
                let resource = parse_resource_for_target(
                    inner,
                    &local_ctx,
                    target.if_resource_target(),
                    preprocessed_file,
                    source_file,
                )?;
                Ok(IfBodyResult::Resource(Box::new(resource)))
            }
            (IfBodyForm::DataSource, Rule::read_resource_expr) => {
                target.bound_read_target(LoopKind::If)?;
                let data_source = parse_read_resource_expr(
                    inner,
                    &local_ctx,
                    target.scheduler_key(),
                    preprocessed_file,
                    source_file,
                )?;
                Ok(IfBodyResult::DataSource(Box::new(data_source)))
            }
            (IfBodyForm::ModuleCall, Rule::module_call) => {
                let mut call =
                    parse_module_call(inner, &local_ctx, preprocessed_file, source_file)?;
                call.binding_name = match target {
                    LoopParseTarget::Bound { .. } => Some(target.scheduler_key().to_string()),
                    LoopParseTarget::Unbound(_) => None,
                };
                call.set_loop_origin(target.if_origin());
                Ok(IfBodyResult::ModuleCall(call))
            }
            (IfBodyForm::Value, Rule::expression) => {
                let value = parse_expression(inner, &local_ctx)?;
                Ok(IfBodyResult::Value(value))
            }
            (IfBodyForm::ResourceProducingFor, Rule::expression) => {
                Err(resource_producing_for_if_branch_error(body_line))
            }
            (IfBodyForm::UnsupportedStructuralExpression, Rule::expression) => {
                Err(structural_if_value_error(body_line))
            }
            _ => Err(ParseError::InternalError {
                expected: "the classified if body form to match its parsed tail".to_string(),
                context: "if body".to_string(),
            }),
        };
    }

    Err(ParseError::InternalError {
        expected: "resource expression, module call, or value expression".to_string(),
        context: "if body".to_string(),
    })
}

/// Parse an if/else expression in value position (attribute values, not let bindings).
///
/// Unlike `parse_if_expr()` which returns `LetBindingRhs` (resources, module calls, or values),
/// this function only returns `Value`. Root conditions must be static;
/// module-argument-only conditions remain parser templates until expansion.
/// An else branch is always required because a value must be determined for
/// either result of the condition.
pub(crate) fn parse_if_value_expr_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if value expression")?;
    let condition_line = condition_pair.line_col().0;
    let condition_source = condition_pair.as_str().trim().to_string();
    let condition_value = parse_expression_eval(condition_pair, ctx)?;
    if matches!(condition_value, EvalValue::ProvisionalReference { .. })
        && ctx.defer_provisional_static_conditions
    {
        let if_body = classify_if_body(next_pair(&mut inner, "if body", "if value expression")?)?;
        if_body.reject_resource_producing_for(condition_line)?;
        return parse_if_body_value_with_mode(if_body, ctx, mode);
    }
    let condition_value = condition_value
        .into_value()
        .map_err(|error| ParseError::from_eval_value(error, condition_line))?;

    // Preserve eager ordering: decide and type-check the condition before
    // inspecting either branch.
    let decision = decide_if_condition(&condition_value, ctx.config)
        .map_err(|error| error.into_parse_error(condition_line))?;

    let if_body = classify_if_body(next_pair(&mut inner, "if body", "if value expression")?)?;
    let else_body = inner
        .next()
        .map(|else_pair| first_inner(else_pair, "else body", "else clause"))
        .transpose()?
        .map(classify_if_body)
        .transpose()?;
    if_body.reject_resource_producing_for(condition_line)?;
    if if_body.kind() == IfBodyKind::Structural && else_body.is_none() {
        return Err(structural_if_value_error(condition_line));
    }
    validate_if_body_shapes(&if_body, else_body.as_ref(), condition_line)?;
    let else_body = else_body.expect("value if branch shape requires an else branch");

    if decision.is_none() {
        let then_value = parse_if_body_value_with_mode(if_body, ctx, mode)?
            .into_value()
            .map_err(|error| ParseError::from_eval_value(error, condition_line))?;
        let else_value = parse_if_body_value_with_mode(else_body, ctx, mode)?
            .into_value()
            .map_err(|error| ParseError::from_eval_value(error, condition_line))?;
        return Ok(EvalValue::from_value(Value::Deferred(
            DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
                DeferredIfCondition::new_with_source_expression(
                    condition_value,
                    ctx.source_file().to_path_buf(),
                    condition_line,
                    condition_source,
                ),
                then_value,
                else_value,
            ))),
        )));
    }

    let condition = decision.expect("module-dependent decision returned above");

    if condition {
        parse_if_body_value_with_mode(if_body, ctx, mode)
    } else {
        parse_if_body_value_with_mode(else_body, ctx, mode)
    }
}

fn parse_if_body_value_with_mode(
    body: ClassifiedIfBody<'_>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    let ClassifiedIfBody { pair, form } = body;
    let body_line = pair.line_col().0;
    let mut local_ctx = ctx.child_scope();

    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::if_local_binding {
            let mut binding_inner = inner.into_inner();
            let name = next_pair(&mut binding_inner, "binding name", "if local binding")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut binding_inner, "binding value", "if local binding")?,
                &local_ctx,
            )?;
            local_ctx.set_variable(name, value);
            continue;
        }

        return match (form, inner.as_rule()) {
            (IfBodyForm::Value, Rule::expression) => {
                let line = inner.line_col().0;
                let value =
                    crate::parser::parse_expression_eval_with_mode(inner, &local_ctx, mode)?;
                if matches!(value, EvalValue::PendingValue(_)) {
                    return Ok(value);
                }
                value
                    .into_value()
                    .map(EvalValue::from_value)
                    .map_err(|error| ParseError::from_eval_value(error, line))
            }
            (IfBodyForm::ResourceProducingFor, Rule::expression) => {
                Err(resource_producing_for_if_branch_error(body_line))
            }
            (
                IfBodyForm::Resource
                | IfBodyForm::DataSource
                | IfBodyForm::ModuleCall
                | IfBodyForm::UnsupportedStructuralExpression,
                _,
            ) => Err(structural_if_value_error(body_line)),
            _ => Err(ParseError::InternalError {
                expected: "the classified if value body form to match its parsed tail".to_string(),
                context: "if value expression body".to_string(),
            }),
        };
    }

    Err(ParseError::InternalError {
        expected: "value expression".to_string(),
        context: "if value expression body".to_string(),
    })
}
