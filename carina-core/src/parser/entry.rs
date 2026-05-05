//! Top-level parser entry points: `parse`, `parse_and_resolve`, and the
//! shared `parse_expression` helpers used by block parsers.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::CarinaParser;
use super::ProviderContext;
use super::Rule;
use super::ast::{ParsedFile, UpstreamState};
use super::blocks::attributes::{
    parse_arguments_block, parse_attributes_block, parse_exports_block,
};
use super::blocks::backend::parse_backend_block;
use super::blocks::module_call::parse_module_call;
use super::blocks::provider::{parse_provider_block, parse_require_statement};
use super::blocks::resource::parse_anonymous_resource;
use super::blocks::state::{parse_import_state_block, parse_moved_block, parse_removed_block};
use super::context::{ParseContext, first_inner};
use super::error::ParseError;
use super::expressions::for_expr::{extract_for_iterable_name, parse_for_expr};
use super::expressions::if_expr::parse_if_expr;
use super::expressions::pipe::parse_coalesce_expr;
use super::functions::parse_fn_def;
use super::let_binding::parse_let_binding_extended;
use super::resolve::{resolve_forward_references, resolve_resource_refs};
use crate::eval_value::EvalValue;
use crate::resource::{Resource, Value};
use indexmap::IndexMap;
use pest::Parser;

/// Parse a .crn file with the given configuration.
///
/// The config allows injecting a decryptor function for `decrypt()` calls
/// and custom type validators from provider crates.
pub fn parse(input: &str, config: &ProviderContext) -> Result<ParsedFile, ParseError> {
    let preprocess_result =
        crate::heredoc::preprocess_heredocs(input).map_err(|e| ParseError::InvalidExpression {
            line: 0,
            message: e.to_string(),
        })?;
    let pairs = CarinaParser::parse(Rule::file, &preprocess_result.source)?;

    let mut ctx = ParseContext::new(config);
    let mut providers = Vec::new();
    let mut resources = Vec::new();
    let mut uses = Vec::new();
    let mut module_calls = Vec::new();
    let mut arguments = Vec::new();
    let mut attribute_params = Vec::new();
    let mut export_params = Vec::new();
    let mut backend = None;
    let mut state_blocks = Vec::new();
    let mut upstream_states: Vec<UpstreamState> = Vec::new();
    let mut requires = Vec::new();
    let mut anon_for_counter = 0usize;
    let mut anon_if_counter = 0usize;

    for pair in pairs {
        if pair.as_rule() == Rule::file {
            for inner in pair.into_inner() {
                if inner.as_rule() == Rule::statement {
                    for stmt in inner.into_inner() {
                        match stmt.as_rule() {
                            Rule::backend_block => {
                                backend = Some(parse_backend_block(stmt, &ctx)?);
                            }
                            Rule::provider_block => {
                                let provider = parse_provider_block(stmt, &ctx)?;
                                providers.push(provider);
                            }
                            Rule::arguments_block => {
                                // `parse_arguments_block` registers each argument as a
                                // lexical binding inside `ctx` as it is parsed, so a
                                // later argument's default expression can reference
                                // earlier arguments (#2393).
                                let parsed_arguments = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_arguments_block(
                                        stmt,
                                        config,
                                        &mut ctx,
                                        &mut warnings,
                                    );
                                    ctx.warnings = warnings;
                                    result?
                                };
                                arguments.extend(parsed_arguments);
                            }
                            Rule::attributes_block => {
                                let parsed_attribute_params = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_attributes_block(stmt, &ctx, &mut warnings);
                                    ctx.warnings = warnings;
                                    result?
                                };
                                attribute_params.extend(parsed_attribute_params);
                            }
                            Rule::exports_block => {
                                let parsed_export_params = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_exports_block(stmt, &ctx, &mut warnings);
                                    ctx.warnings = warnings;
                                    result?
                                };
                                export_params.extend(parsed_export_params);
                            }
                            Rule::import_state_block => {
                                state_blocks.push(parse_import_state_block(stmt)?);
                            }
                            Rule::removed_block => {
                                state_blocks.push(parse_removed_block(stmt)?);
                            }
                            Rule::moved_block => {
                                state_blocks.push(parse_moved_block(stmt)?);
                            }
                            Rule::require_statement => {
                                requires.push(parse_require_statement(stmt)?);
                            }
                            Rule::for_expr => {
                                let iterable_name =
                                    extract_for_iterable_name(&stmt, anon_for_counter);
                                anon_for_counter += 1;
                                let (expanded_resources, expanded_module_calls) =
                                    parse_for_expr(stmt, &mut ctx, &iterable_name)?;
                                resources.extend(expanded_resources);
                                module_calls.extend(expanded_module_calls);
                            }
                            Rule::if_expr => {
                                let binding_name = format!("_if{}", anon_if_counter);
                                anon_if_counter += 1;
                                let (_value, expanded_resources, expanded_module_calls, _import) =
                                    parse_if_expr(stmt, &mut ctx, &binding_name)?;
                                resources.extend(expanded_resources);
                                module_calls.extend(expanded_module_calls);
                            }
                            Rule::fn_def => {
                                let user_fn = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_fn_def(stmt, &ctx, &mut warnings);
                                    ctx.warnings = warnings;
                                    result?
                                };
                                let fn_name = user_fn.name.clone();
                                // Check for shadowing builtins
                                if crate::builtins::evaluate_builtin(&fn_name, &[]).is_ok()
                                    || crate::builtins::builtin_functions()
                                        .iter()
                                        .any(|f| f.name == fn_name)
                                {
                                    return Err(ParseError::UserFunctionError(format!(
                                        "function '{fn_name}' shadows a built-in function"
                                    )));
                                }
                                if ctx.user_functions.contains_key(&fn_name) {
                                    return Err(ParseError::UserFunctionError(format!(
                                        "duplicate function definition: '{fn_name}'"
                                    )));
                                }
                                ctx.user_functions.insert(fn_name, user_fn);
                            }
                            Rule::let_binding => {
                                let (line, _) = stmt.as_span().start_pos().line_col();
                                let (
                                    name,
                                    value,
                                    expanded_resources,
                                    expanded_module_calls,
                                    maybe_import,
                                    is_structural,
                                ) = parse_let_binding_extended(stmt, &mut ctx)?;
                                if is_structural {
                                    ctx.structural_bindings.insert(name.clone());
                                }
                                let is_discard = name == "_";
                                let is_upstream_state = ctx.upstream_states.contains_key(&name);
                                if !is_discard {
                                    if ctx.variables.contains_key(&name)
                                        || ctx.resource_bindings.contains_key(&name)
                                    {
                                        return Err(ParseError::DuplicateBinding { name, line });
                                    }
                                    if !is_upstream_state {
                                        ctx.set_variable(name.clone(), value);
                                    }
                                }
                                if !expanded_resources.is_empty() {
                                    if !is_discard {
                                        // Register the binding name as a resource binding
                                        // (use the first resource as placeholder)
                                        ctx.set_resource_binding(
                                            name.clone(),
                                            expanded_resources[0].clone(),
                                        );
                                    }
                                    resources.extend(expanded_resources);
                                }
                                if !expanded_module_calls.is_empty() {
                                    for mut call in expanded_module_calls {
                                        if call.binding_name.is_none() {
                                            call.binding_name = Some(name.clone());
                                        }
                                        module_calls.push(call);
                                    }
                                    if !is_discard {
                                        // Register as a resource binding so that
                                        // `name.attr` resolves as ResourceRef
                                        let placeholder = Resource::new("_module_binding", &name);
                                        ctx.set_resource_binding(name.clone(), placeholder);
                                    }
                                }
                                if is_upstream_state && !is_discard {
                                    let placeholder = Resource::new("_upstream_state", &name);
                                    ctx.set_resource_binding(name.clone(), placeholder);
                                    upstream_states.push(ctx.upstream_states[&name].clone());
                                }
                                if let Some(use_stmt) = maybe_import {
                                    ctx.imported_modules
                                        .insert(use_stmt.alias.clone(), use_stmt.path.clone());
                                    uses.push(use_stmt);
                                }
                            }
                            Rule::module_call => {
                                let call = parse_module_call(stmt, &ctx)?;
                                module_calls.push(call);
                            }
                            Rule::anonymous_resource => {
                                let resource = parse_anonymous_resource(stmt, &ctx)?;
                                resources.push(resource);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // Second pass: resolve forward references.
    // During parsing, unknown 2-part identifiers (e.g., vpc.vpc_id where vpc is
    // declared later) become String values like "vpc.vpc_id". Now that we have the
    // full binding set, convert matching ones to ResourceRef.
    resolve_forward_references(
        &ctx.resource_bindings,
        &mut resources,
        &mut attribute_params,
        &mut module_calls,
        &mut export_params,
    );

    // "Is every ResourceRef root declared somewhere?" is a semantic
    // question the per-file parse cannot answer: the referent may live
    // in a sibling `.crn`. The check runs post-merge via
    // `check_identifier_scope(&ParsedFile)` — wired into
    // `load_configuration_with_config`. See #2126 / #2138.

    // Lower the evaluator-internal `EvalValue` bindings to user-facing
    // `Value`. Closure bindings are dropped: they are evaluator-only
    // artifacts (partial applications produced by `let f = builtin(x)`
    // and consumed by a later pipe like `data |> f()`). After the parse
    // pass finishes, only fully-reduced `Value`s belong in
    // `ParsedFile.variables`; nothing downstream knows how to handle a
    // closure. Pipe / call paths read from `ctx.variables` directly via
    // `get_variable`, so the closure was already available where it
    // mattered.
    let variables: IndexMap<String, Value> = ctx
        .variables
        .into_iter()
        .filter_map(|(name, eval)| match eval.into_value() {
            Ok(v) => Some((name, v)),
            Err(_leak) => None,
        })
        .collect();

    Ok(ParsedFile {
        providers,
        resources,
        variables,
        uses,
        module_calls,
        arguments,
        attribute_params,
        export_params,
        backend,
        state_blocks,
        user_functions: ctx.user_functions,
        upstream_states,
        requires,
        structural_bindings: ctx.structural_bindings,
        warnings: ctx.warnings,
        deferred_for_expressions: ctx.deferred_for_expressions,
    })
}

/// Parse an expression. The result is a fully-reduced `Value`: any
/// closure that surfaces during evaluation surfaces here as a
/// parse-time error. Use [`parse_expression_eval`] in pipe/compose
/// paths where partial applications are legitimate intermediates.
pub(crate) fn parse_expression(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let eval = parse_expression_eval(pair, ctx)?;
    eval.into_value()
        .map_err(|leak| ParseError::InvalidExpression {
            line: 0,
            message: format!(
                "expression evaluates to a closure '{}' (still needs {} arg(s)); finish the \
             partial application — closures are not valid as data",
                leak.name, leak.remaining_arity
            ),
        })
}

/// Parse an expression and return the raw `EvalValue`, preserving any
/// closure produced during partial application. Only the pipe/compose
/// paths and the let-binding RHS need this; everything else should
/// call [`parse_expression`] and let unfinished closures surface as
/// errors at the type boundary.
pub(crate) fn parse_expression_eval(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    let inner = first_inner(pair, "expression body", "expression")?;
    parse_coalesce_expr(inner, ctx)
}

/// Parse a .crn file and resolve resource references
pub fn parse_and_resolve(input: &str) -> Result<ParsedFile, ParseError> {
    let mut parsed = parse(input, &ProviderContext::default())?;
    resolve_resource_refs(&mut parsed)?;
    Ok(parsed)
}
