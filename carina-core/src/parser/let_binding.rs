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
use super::blocks::provider::parse_provider_expr;
use super::blocks::resource::{parse_read_resource_expr, parse_resource_expr};
use super::blocks::use_stmt::parse_use_expr;
use super::context::{ExpressionMode, ParseBindingRef, ParseContext, first_inner, next_pair};
use super::error::ParseError;
use super::expressions::for_expr::{LoopParseTarget, parse_for_expr};
use super::expressions::if_expr::parse_if_expr;
use super::expressions::integer_arithmetic::{
    combine_integer_operands, parse_additive_expr_with_mode, parse_integer_operator,
    parse_multiplicative_expr_with_mode,
};
use super::expressions::pipe::{
    apply_pipe_step, combine_coalesce_operands, combine_compose_operands, parse_pipe_expr_with_mode,
};
use super::expressions::primary::{
    apply_postfix_steps, parse_postfix_eval_with_mode, parse_primary_eval,
    parse_primary_eval_with_mode,
};
use super::expressions::wait_expr::parse_wait_expr;
use super::grammar::{ParserBindingName, PreprocessedFile};
use super::parse_expression_eval;
use super::util::descend_expression;
use crate::eval_value::{EvalValue, StructuralHandle, StructuralHandleKind};
use crate::parser::{ParsedDataSource as DataSource, ParsedResource as Resource};

/// Tuple returned by the let-binding parser. The RHS is `EvalValue`
/// rather than `Value` so partial applications (closures) can survive
/// until a later pipe finishes them; the surrounding parse pass lowers
/// each binding to `Value` at the end of `parse(...)`.
///
/// Managed resources and data sources are carried in separate `Vec`s so
/// the caller can route each into the matching typed slice of
/// [`ParsedFile`](super::ast::ParsedFile) (carina#3181).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LetBindingKind {
    Value,
    Structural,
    StructuralAlias,
}

pub(crate) type LetBindingRhs = (
    EvalValue,
    Vec<Resource>,
    Vec<DataSource>,
    Vec<ModuleCall>,
    Option<UseStatement>,
    LetBindingKind,
);

/// Parser-internal handle returned while a structural declaration is being
/// assembled. Top-level parsing records the binding as `Structural` and drops
/// this value, but using a typed reference here prevents a structural handle
/// from ever masquerading as an ordinary string during nested parsing.
pub(crate) fn structural_handle(
    binding_name: &str,
    resource_type: super::ast::ResourceTypePath,
) -> EvalValue {
    EvalValue::structural_handle(StructuralHandle::new(
        StructuralHandleKind::Resource(resource_type),
        binding_name,
    ))
}

pub(crate) fn for_structural_handle(binding_name: &str) -> EvalValue {
    EvalValue::structural_handle(StructuralHandle::new(
        StructuralHandleKind::For,
        binding_name,
    ))
}

fn upstream_state_structural_handle(binding_name: &str) -> EvalValue {
    EvalValue::structural_handle(StructuralHandle::new(
        StructuralHandleKind::UpstreamState,
        binding_name,
    ))
}

fn provider_structural_handle(binding_name: &str) -> EvalValue {
    EvalValue::structural_handle(StructuralHandle::new(
        StructuralHandleKind::Provider,
        binding_name,
    ))
}

fn wait_structural_handle(binding_name: &str) -> EvalValue {
    EvalValue::structural_handle(StructuralHandle::new(
        StructuralHandleKind::Wait,
        binding_name,
    ))
}

pub(crate) fn if_structural_handle(binding_name: &str) -> EvalValue {
    EvalValue::structural_handle(StructuralHandle::new(
        StructuralHandleKind::If,
        binding_name,
    ))
}

pub(crate) fn module_structural_handle(binding_name: &str) -> EvalValue {
    EvalValue::structural_handle(StructuralHandle::new(
        StructuralHandleKind::Module,
        binding_name,
    ))
}

/// Extended parse_let_binding that also handles module calls, imports, and for expressions.
///
/// Returns `(name, value, resources, data_sources, module_calls, import,
/// binding_kind, is_structurally_required)`.
/// `is_structural` is true when the RHS is an if/for/read expression, meaning the
/// `let` binding is structurally required and should not trigger unused-binding warnings.
#[allow(clippy::type_complexity)]
pub(super) fn parse_let_binding_extended(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<
    (
        String,
        EvalValue,
        Vec<Resource>,
        Vec<DataSource>,
        Vec<ModuleCall>,
        Option<UseStatement>,
        LetBindingKind,
        bool,
    ),
    ParseError,
> {
    let source_span = crate::parser::SourceSpan::from_pest(&pair, preprocessed_file);
    let mut inner = pair.into_inner();
    let name = ParserBindingName::from_let_binding_pair(next_pair(
        &mut inner,
        "binding name",
        "let binding",
    )?);
    let rhs_pair = next_pair(&mut inner, "expression", "let binding")?;

    // `use` is the only meta-statement allowed as a let-binding RHS. The
    // grammar permits it here and only here (see carina.pest); everywhere
    // else it is a parse error.
    if rhs_pair.as_rule() == Rule::use_expr {
        let use_stmt = parse_use_expr(rhs_pair, name.as_str(), ctx, source_span, source_file)?;
        let value = EvalValue::structural_handle(StructuralHandle::new(
            StructuralHandleKind::Use,
            name.as_str(),
        ));
        return Ok((
            name.into_string(),
            value,
            vec![],
            vec![],
            vec![],
            Some(use_stmt),
            LetBindingKind::Structural,
            false,
        ));
    }

    // Detect if the RHS is a structurally-required expression (if/for/read)
    let is_structurally_required = detect_structural_rhs(&rhs_pair);

    // Check if it's a module call, resource expression, or for expression
    let (value, expanded_resources, data_sources, module_calls, maybe_import, binding_kind) =
        parse_expression_with_resource_or_module(
            rhs_pair,
            ctx,
            &name,
            preprocessed_file,
            source_file,
        )?;

    Ok((
        name.into_string(),
        value,
        expanded_resources,
        data_sources,
        module_calls,
        maybe_import,
        binding_kind,
        is_structurally_required,
    ))
}

/// Detect if an expression pair's innermost primary is an if/for/read/upstream_state expression.
fn detect_structural_rhs(pair: &pest::iterators::Pair<Rule>) -> bool {
    let Some(descent) = descend_expression(pair.clone(), false) else {
        return false;
    };
    descent
        .primary()
        .into_inner()
        .next()
        .is_some_and(|inner| is_structurally_required_rule(inner.as_rule()))
}

fn is_structurally_required_rule(rule: Rule) -> bool {
    matches!(
        rule,
        Rule::if_expr
            | Rule::for_expr
            | Rule::read_resource_expr
            | Rule::upstream_state_expr
            | Rule::wait_expr
    )
}

fn parsed_rhs_is_declaration(rhs: &LetBindingRhs) -> bool {
    matches!(rhs.5, LetBindingKind::Structural)
        || !rhs.1.is_empty()
        || !rhs.2.is_empty()
        || !rhs.3.is_empty()
        || rhs.4.is_some()
}

fn arithmetic_on_declaration_error(line: usize) -> ParseError {
    ParseError::InvalidExpression {
        line,
        message: "integer arithmetic cannot be applied to a resource, module, data-source, or import declaration"
            .to_string(),
    }
}

/// Parse expression with potential resource, module call, or import.
fn parse_expression_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &ParserBindingName,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<LetBindingRhs, ParseError> {
    let descent = descend_expression(pair, false).ok_or_else(|| ParseError::InternalError {
        expected: "expression precedence chain ending in a primary".to_string(),
        context: "let-binding right-hand side".to_string(),
    })?;
    let primary = descent.primary();
    let let_mode = ExpressionMode::with_source(source_file, preprocessed_file);
    let integer_operand = let_mode.for_integer_operand();
    let primary_is_integer_operand = [Rule::multiplicative_expr, Rule::additive_expr]
        .into_iter()
        .filter_map(|rule| descent.wrapper(rule))
        .any(|wrapper| wrapper.into_inner().nth(1).is_some());
    let primary_mode = if primary_is_integer_operand {
        &integer_operand
    } else {
        &let_mode
    };
    let parsed_primary = parse_primary_with_resource_or_module(
        primary,
        ctx,
        binding_name,
        preprocessed_file,
        source_file,
        primary_mode,
    )?;
    let is_declaration = parsed_rhs_is_declaration(&parsed_primary);
    let (mut value, expanded_resources, data_sources, module_calls, maybe_import, mut binding_kind) =
        parsed_primary;
    let postfix = descent
        .wrapper(Rule::postfix_expr)
        .expect("expression descent contains a postfix wrapper");
    let postfix_line = ctx.original_position(&postfix)?.line();
    let mut postfix_inner = postfix.into_inner();
    let _primary = next_pair(
        &mut postfix_inner,
        "primary expression",
        "postfix expression",
    )?;
    let postfix_steps = postfix_inner.collect::<Vec<_>>();
    if !postfix_steps.is_empty() {
        if is_declaration || matches!(binding_kind, LetBindingKind::StructuralAlias) {
            return Err(ParseError::InvalidExpression {
                line: postfix_line,
                message: "postfix access cannot be applied to a resource or module declaration"
                    .to_string(),
            });
        }
        value = apply_postfix_steps(value, postfix_steps, ctx)?;
    }

    let multiplicative = descent
        .wrapper(Rule::multiplicative_expr)
        .expect("expression descent contains a multiplicative wrapper");
    let mut multiplicative_inner = multiplicative.into_inner();
    let _postfix = next_pair(
        &mut multiplicative_inner,
        "postfix expression",
        "multiplicative expression",
    )?;
    while let Some(operator) = multiplicative_inner.next() {
        let operator_line = ctx.original_position(&operator)?.line();
        if is_declaration {
            return Err(arithmetic_on_declaration_error(operator_line));
        }
        let rhs = next_pair(
            &mut multiplicative_inner,
            "postfix expression",
            "multiplicative expression rhs",
        )?;
        let (op, span) = parse_integer_operator(operator, ctx)?;
        value = combine_integer_operands(
            value,
            op,
            parse_postfix_eval_with_mode(rhs, ctx, &integer_operand)?,
            span,
        )?;
        binding_kind = LetBindingKind::Value;
    }

    let additive = descent
        .wrapper(Rule::additive_expr)
        .expect("expression descent contains an additive wrapper");
    let mut additive_inner = additive.into_inner();
    let _multiplicative = next_pair(
        &mut additive_inner,
        "multiplicative expression",
        "additive expression",
    )?;
    while let Some(operator) = additive_inner.next() {
        let operator_line = ctx.original_position(&operator)?.line();
        if is_declaration {
            return Err(arithmetic_on_declaration_error(operator_line));
        }
        let rhs = next_pair(
            &mut additive_inner,
            "multiplicative expression",
            "additive expression rhs",
        )?;
        let (op, span) = parse_integer_operator(operator, ctx)?;
        let rhs = parse_multiplicative_expr_with_mode(rhs, ctx, &integer_operand)?;
        value = combine_integer_operands(value, op, rhs, span)?;
        binding_kind = LetBindingKind::Value;
    }

    let compose = descent
        .wrapper(Rule::compose_expr)
        .expect("expression descent contains a compose wrapper");
    let mut compose_inner = compose.into_inner();
    let _additive = next_pair(
        &mut compose_inner,
        "additive expression",
        "compose expression",
    )?;
    for rhs in compose_inner {
        value =
            combine_compose_operands(value, parse_additive_expr_with_mode(rhs, ctx, &let_mode)?)?;
        binding_kind = LetBindingKind::Value;
    }

    let pipe = descent
        .wrapper(Rule::pipe_expr)
        .expect("expression descent contains a pipe wrapper");
    let mut pipe_inner = pipe.into_inner();
    let _compose = next_pair(&mut pipe_inner, "compose expression", "pipe expression")?;
    for function_call in pipe_inner {
        let position = ctx.original_position(&function_call)?;
        let (line, column) = (position.line(), position.column());
        let mut function_inner = function_call.into_inner();
        let function_name = next_pair(&mut function_inner, "function name", "pipe function call")?
            .as_str()
            .to_string();
        let extra_args = function_inner
            .map(|argument| parse_expression_eval(argument, ctx))
            .collect::<Result<Vec<_>, _>>()?;
        value = apply_pipe_step(value, function_name, extra_args, ctx, line, column)?;
        binding_kind = LetBindingKind::Value;
    }

    let coalesce = descent
        .wrapper(Rule::coalesce_expr)
        .expect("expression descent contains a coalesce wrapper");
    let coalesce_line = ctx.original_position(&coalesce)?.line();
    let mut coalesce_inner = coalesce.into_inner();
    let _pipe = next_pair(
        &mut coalesce_inner,
        "pipe expression",
        "coalesce expression",
    )?;
    let mut defaults = coalesce_inner
        .map(|default| parse_pipe_expr_with_mode(default, ctx, &let_mode))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(mut default) = defaults.pop() {
        while let Some(previous) = defaults.pop() {
            default = combine_coalesce_operands(previous, default, coalesce_line)?;
        }
        value = combine_coalesce_operands(value, default, coalesce_line)?;
        binding_kind = LetBindingKind::Value;
    }

    Ok((
        value,
        expanded_resources,
        data_sources,
        module_calls,
        maybe_import,
        binding_kind,
    ))
}

fn parse_primary_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &ParserBindingName,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
    fallback_mode: &ExpressionMode<'_>,
) -> Result<LetBindingRhs, ParseError> {
    let inner = first_inner(pair, "value", "primary expression")?;
    parse_primary_inner_with_resource_or_module(
        inner,
        ctx,
        binding_name,
        preprocessed_file,
        source_file,
        fallback_mode,
    )
}

pub(crate) fn parse_integer_operand_primary(
    inner: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<EvalValue, ParseError> {
    let position = ctx.original_position(&inner)?;
    let (line, column) = (position.line(), position.column());
    let binding_name = ParserBindingName::integer_operand(line, column);
    let mut operand_ctx = ctx.clone();
    let operand_mode =
        ExpressionMode::with_source(source_file, preprocessed_file).for_integer_operand();
    let parsed = parse_primary_inner_with_resource_or_module(
        inner,
        &mut operand_ctx,
        &binding_name,
        preprocessed_file,
        source_file,
        &operand_mode,
    )?;
    if parsed_rhs_is_declaration(&parsed) {
        return Err(arithmetic_on_declaration_error(line));
    }
    Ok(parsed.0)
}

fn parse_primary_inner_with_resource_or_module(
    inner: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &ParserBindingName,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
    fallback_mode: &ExpressionMode<'_>,
) -> Result<LetBindingRhs, ParseError> {
    let binding_name_str = binding_name.as_str();
    if inner.as_rule() == Rule::variable_ref {
        let mut reference_parts = inner.clone().into_inner();
        if let Some(identifier) = reference_parts.next()
            && reference_parts.next().is_none()
            && matches!(
                ctx.lookup_binding(identifier.as_str()),
                ParseBindingRef::Structural(_)
            )
        {
            let value = parse_primary_eval(inner, ctx)?;
            return Ok((
                value,
                vec![],
                vec![],
                vec![],
                None,
                LetBindingKind::StructuralAlias,
            ));
        }
    }

    match inner.as_rule() {
        Rule::read_resource_expr => {
            let data_source = parse_read_resource_expr(
                inner,
                ctx,
                binding_name_str,
                preprocessed_file,
                source_file,
            )?;
            let resource_type = super::ast::ResourceTypePath::new(
                data_source.id.provider.clone(),
                data_source.id.resource_type.clone(),
            );
            Ok((
                structural_handle(binding_name_str, resource_type),
                vec![],
                vec![data_source],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::upstream_state_expr => {
            let line = ctx.original_position(&inner)?.line();
            let us = parse_upstream_state_expr(inner, binding_name_str)?;
            if ctx.upstream_states.contains_key(&us.binding) {
                return Err(ParseError::DuplicateBinding {
                    name: us.binding,
                    line,
                });
            }
            ctx.upstream_states.insert(us.binding.clone(), us);
            Ok((
                upstream_state_structural_handle(binding_name_str),
                vec![],
                vec![],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::provider_expr => {
            let line = ctx.original_position(&inner)?.line();
            let config = parse_provider_expr(inner, ctx, binding_name_str)?;
            if ctx
                .named_provider_instances
                .iter()
                .any(|p| p.binding.as_deref() == Some(binding_name_str))
            {
                return Err(ParseError::DuplicateBinding {
                    name: binding_name_str.to_string(),
                    line,
                });
            }
            ctx.named_provider_instances.push(config);
            Ok((
                provider_structural_handle(binding_name_str),
                vec![],
                vec![],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::wait_expr => {
            let line = ctx.original_position(&inner)?.line();
            let wb = parse_wait_expr(inner, binding_name_str)?;
            if ctx.wait_bindings.contains_key(wb.binding.as_str()) {
                return Err(ParseError::DuplicateBinding {
                    name: wb.binding.into_string(),
                    line,
                });
            }
            ctx.wait_bindings
                .insert(wb.binding.as_str().to_string(), wb);
            Ok((
                wait_structural_handle(binding_name_str),
                vec![],
                vec![],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::resource_expr => {
            let resource =
                parse_resource_expr(inner, ctx, binding_name_str, preprocessed_file, source_file)?;
            let resource_type = super::ast::ResourceTypePath::new(
                resource.id.provider.clone(),
                resource.id.resource_type.clone(),
            );
            Ok((
                structural_handle(binding_name_str, resource_type),
                vec![resource],
                vec![],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::for_expr => {
            let target = LoopParseTarget::bound(binding_name);
            let (resources, data_sources, module_calls) =
                parse_for_expr(inner, ctx, target, preprocessed_file, source_file)?;
            Ok((
                for_structural_handle(binding_name_str),
                resources,
                data_sources,
                module_calls,
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::if_expr => {
            let target = LoopParseTarget::bound(binding_name);
            let (value, resources, data_sources, module_calls, import, binding_kind) =
                parse_if_expr(inner, ctx, target, preprocessed_file, source_file)?;
            Ok((
                value,
                resources,
                data_sources,
                module_calls,
                import,
                binding_kind,
            ))
        }
        Rule::module_call => {
            let call = parse_module_call(inner, ctx, preprocessed_file, source_file)?;
            Ok((
                module_structural_handle(binding_name_str),
                vec![],
                vec![],
                vec![call],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::function_call => {
            let value = parse_primary_eval(inner, ctx)?;
            Ok((value, vec![], vec![], vec![], None, LetBindingKind::Value))
        }
        _ => {
            let value = parse_primary_eval_with_mode(inner, ctx, fallback_mode)?;
            Ok((value, vec![], vec![], vec![], None, LetBindingKind::Value))
        }
    }
}
