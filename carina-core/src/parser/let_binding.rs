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
use super::context::{ParseBindingRef, ParseContext, first_inner, next_pair};
use super::error::ParseError;
use super::expressions::for_expr::parse_for_expr;
use super::expressions::if_expr::parse_if_expr;
use super::expressions::pipe::{apply_pipe_step, parse_coalesce_expr};
use super::expressions::primary::{apply_postfix_steps, parse_postfix_eval, parse_primary_eval};
use super::expressions::wait_expr::parse_wait_expr;
use super::grammar::PreprocessedFile;
use super::parse_expression_eval;
use super::util::eval_type_name;
use crate::eval_value::{EvalValue, StructuralHandle, StructuralHandleKind};
use crate::resource::{DataSource, Resource};

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
    let name = next_pair(&mut inner, "binding name", "let binding")?
        .as_str()
        .to_string();
    let rhs_pair = next_pair(&mut inner, "expression", "let binding")?;

    // `use` is the only meta-statement allowed as a let-binding RHS. The
    // grammar permits it here and only here (see carina.pest); everywhere
    // else it is a parse error.
    if rhs_pair.as_rule() == Rule::use_expr {
        let use_stmt = parse_use_expr(rhs_pair, &name, ctx, source_span, source_file)?;
        let value =
            EvalValue::structural_handle(StructuralHandle::new(StructuralHandleKind::Use, &name));
        return Ok((
            name,
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
    let is_structural = detect_structural_rhs(&rhs_pair);

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
        name,
        value,
        expanded_resources,
        data_sources,
        module_calls,
        maybe_import,
        binding_kind,
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
            | Rule::upstream_state_expr
            | Rule::wait_expr => Some(inner.as_rule()),
            Rule::pipe_expr
            | Rule::compose_expr
            | Rule::postfix_expr
            | Rule::coalesce_expr
            | Rule::expression => find_inner_rule(&inner),
            Rule::primary => {
                let primary_inner = inner.into_inner().next()?;
                match primary_inner.as_rule() {
                    Rule::if_expr
                    | Rule::for_expr
                    | Rule::read_resource_expr
                    | Rule::upstream_state_expr
                    | Rule::wait_expr => Some(primary_inner.as_rule()),
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
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<LetBindingRhs, ParseError> {
    let coalesce = first_inner(pair, "expression", "expression with resource or module")?;
    if coalesce.clone().into_inner().nth(1).is_some() {
        let value = parse_coalesce_expr(coalesce, ctx)?;
        return Ok((
            value,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            LetBindingKind::Value,
        ));
    }
    let pipe = first_inner(coalesce, "pipe expression", "coalesce expression")?;
    parse_pipe_expr_with_resource_or_module(pipe, ctx, binding_name, preprocessed_file, source_file)
}

fn parse_pipe_expr_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<LetBindingRhs, ParseError> {
    let mut inner = pair.into_inner();
    let compose_pair = next_pair(&mut inner, "compose expression", "pipe expression")?;

    // Unwrap compose_expr: get its inner pairs
    let mut compose_inner = compose_pair.into_inner();
    let postfix = next_pair(
        &mut compose_inner,
        "postfix expression",
        "compose expression",
    )?;
    let postfix_line = postfix.line_col().0;
    let mut postfix_inner = postfix.into_inner();
    let primary = next_pair(
        &mut postfix_inner,
        "primary expression",
        "postfix expression",
    )?;
    let (mut value, expanded_resources, data_sources, module_calls, maybe_import, mut binding_kind) =
        parse_primary_with_resource_or_module(
            primary,
            ctx,
            binding_name,
            preprocessed_file,
            source_file,
        )?;
    if !postfix_inner.is_empty() {
        if matches!(
            binding_kind,
            LetBindingKind::Structural | LetBindingKind::StructuralAlias
        ) || !expanded_resources.is_empty()
            || !data_sources.is_empty()
            || !module_calls.is_empty()
            || maybe_import.is_some()
        {
            return Err(ParseError::InvalidExpression {
                line: postfix_line,
                message: "postfix access cannot be applied to a resource or module declaration"
                    .to_string(),
            });
        }
        value = apply_postfix_steps(value, postfix_inner, ctx)?;
    }

    // Handle >> composition within the compose_expr
    let compose_rhs: Vec<_> = compose_inner.collect();
    if !compose_rhs.is_empty() {
        // Process the compose chain
        for rhs_pair in compose_rhs {
            let rhs = parse_postfix_eval(rhs_pair, ctx)?;

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
    binding_name: &str,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<LetBindingRhs, ParseError> {
    let inner = first_inner(pair, "value", "primary expression")?;

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
            let data_source =
                parse_read_resource_expr(inner, ctx, binding_name, preprocessed_file, source_file)?;
            let resource_type = super::ast::ResourceTypePath::new(
                data_source.id.provider.clone(),
                data_source.id.resource_type.clone(),
            );
            Ok((
                structural_handle(binding_name, resource_type),
                vec![],
                vec![data_source],
                vec![],
                None,
                LetBindingKind::Structural,
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
            Ok((
                upstream_state_structural_handle(binding_name),
                vec![],
                vec![],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::provider_expr => {
            let (line, _) = inner.as_span().start_pos().line_col();
            let config = parse_provider_expr(inner, ctx, binding_name)?;
            if ctx
                .named_provider_instances
                .iter()
                .any(|p| p.binding.as_deref() == Some(binding_name))
            {
                return Err(ParseError::DuplicateBinding {
                    name: binding_name.to_string(),
                    line,
                });
            }
            ctx.named_provider_instances.push(config);
            Ok((
                provider_structural_handle(binding_name),
                vec![],
                vec![],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::wait_expr => {
            let (line, _) = inner.as_span().start_pos().line_col();
            let wb = parse_wait_expr(inner, binding_name)?;
            if ctx.wait_bindings.contains_key(wb.binding.as_str()) {
                return Err(ParseError::DuplicateBinding {
                    name: wb.binding.into_string(),
                    line,
                });
            }
            ctx.wait_bindings
                .insert(wb.binding.as_str().to_string(), wb);
            Ok((
                wait_structural_handle(binding_name),
                vec![],
                vec![],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::resource_expr => {
            let resource =
                parse_resource_expr(inner, ctx, binding_name, preprocessed_file, source_file)?;
            let resource_type = super::ast::ResourceTypePath::new(
                resource.id.provider.clone(),
                resource.id.resource_type.clone(),
            );
            Ok((
                structural_handle(binding_name, resource_type),
                vec![resource],
                vec![],
                vec![],
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::for_expr => {
            let (resources, data_sources, module_calls) =
                parse_for_expr(inner, ctx, binding_name, preprocessed_file, source_file)?;
            Ok((
                for_structural_handle(binding_name),
                resources,
                data_sources,
                module_calls,
                None,
                LetBindingKind::Structural,
            ))
        }
        Rule::if_expr => {
            let (value, resources, data_sources, module_calls, import, binding_kind) =
                parse_if_expr(inner, ctx, binding_name, preprocessed_file, source_file)?;
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
                module_structural_handle(binding_name),
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
            let value = parse_primary_eval(inner, ctx)?;
            Ok((value, vec![], vec![], vec![], None, LetBindingKind::Value))
        }
    }
}
