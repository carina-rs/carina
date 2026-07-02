//! Top-level parser entry points: `parse`, `parse_and_resolve`, and the
//! shared `parse_expression` helpers used by block parsers.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::CarinaParser;
use super::ProviderContext;
use super::Rule;
use super::ast::{ParsedFile, UpstreamState, WaitBinding};
use super::blocks::attributes::{
    parse_arguments_block, parse_attributes_block, parse_exports_block,
};
use super::blocks::backend::parse_backend_block;
use super::blocks::module_call::parse_module_call;
use super::blocks::provider::{parse_provider_block, parse_require_statement};
use super::blocks::resource::parse_anonymous_resource;
use super::blocks::state::{parse_import_state_block, parse_moved_block, parse_removed_block};
use super::context::{ParseContext, first_inner};
use super::error::{
    ParseError, ParseWarning, ParseWarningSpan, SINGLE_QUOTED_INTERPOLATION_WARNING_MESSAGE,
    WarningKind,
};
use super::expressions::for_expr::{extract_for_iterable_name, parse_for_expr};
use super::expressions::if_expr::parse_if_expr;
use super::expressions::pipe::parse_coalesce_expr;
use super::functions::parse_fn_def;
use super::let_binding::parse_let_binding_extended;
use super::resolve::{
    finalize_provider_configs, resolve_provider_unresolved_attributes, resolve_resource_refs,
};
use crate::eval_value::EvalValue;
use crate::resource::{DataSource, DeferredValue, Resource, Value};
use indexmap::IndexMap;
use pest::Parser;
use pest::error::LineColLocation;

#[derive(Clone, Copy)]
pub(crate) struct BindingSeed<'a> {
    name: &'a str,
    kind: SeedKind<'a>,
}

#[derive(Clone, Copy)]
pub(crate) enum SeedKind<'a> {
    Value(&'a Value),
    Structural,
}

impl<'a> BindingSeed<'a> {
    pub(crate) fn value(name: &'a str, value: &'a Value) -> Self {
        Self {
            name,
            kind: SeedKind::Value(value),
        }
    }

    pub(crate) fn structural(name: &'a str) -> Self {
        Self {
            name,
            kind: SeedKind::Structural,
        }
    }

    pub(crate) fn name(&self) -> &'a str {
        self.name
    }

    pub(crate) fn kind(&self) -> SeedKind<'a> {
        self.kind
    }
}

/// Parse a .crn file with the given configuration.
///
/// The config allows injecting a decryptor function for `decrypt()` calls
/// and custom type validators from provider crates.
///
/// **Single-file API.** Sibling-defined names (a `let` in another `.crn`,
/// an `arguments {}` declared in `main.crn` and referenced from a
/// sibling) are *not* in scope. Production code paths that read a
/// directory of `.crn` files must go through
/// [`crate::config_loader::parse_directory_files`], which collects the
/// sibling binding-name union, re-parses to compute sibling-aware `let`
/// values, and then re-parses again with the resolved directory scope.
/// See #2817 (directory-aware parse) and #3394 (pass-1 sibling-aware
/// value seeds) for the broader contract.
pub fn parse(input: &str, config: &ProviderContext) -> Result<ParsedFile, ParseError> {
    let parsed = parse_with_seeded_bindings(input, config, &[])?;
    super::resolve::reject_cyclic_let_bindings(&parsed)?;
    Ok(parsed)
}

/// Parse a .crn file with `seeds` pre-registered as lexical bindings.
///
/// Each seed carries the directory-wide binding name and, for plain
/// value-shaped `let` bindings, the pass-1 value from the merged file.
/// Value seeds are installed directly in `ctx.variables`; all other
/// binding kinds use the existing [`Value::Deferred(DeferredValue::BindingRef)`]
/// placeholder and a placeholder `Resource` in `ctx.resource_bindings`.
/// This keeps value lets value-shaped while structural sibling names
/// still resolve through the normal `ctx.get_variable` /
/// `ctx.is_resource_binding` paths instead of degrading to the
/// literal-string fallback in `primary.rs::variable_ref` /
/// `parse_namespaced_id_value`.
///
/// The seed list is the directory aggregate: every binding declared in
/// any sibling `.crn` (resource bindings, argument names, attribute
/// names, export names, user-function names, `use` aliases,
/// `upstream_state` bindings). The caller is responsible for collecting
/// it from a directory-wide parse pass and passing the union here.
///
/// Names already declared inside `input` itself (re-introduced by the
/// regular parse) win over the seeded placeholder — the parser overwrites
/// the seed entry the moment it processes the local `let` / `arguments`
/// / etc. that introduces the name.
pub fn parse_with_seeded_bindings(
    input: &str,
    config: &ProviderContext,
    seeds: &[BindingSeed<'_>],
) -> Result<ParsedFile, ParseError> {
    parse_with_seeded_bindings_inner(input, config, seeds, true)
}

pub(crate) fn parse_with_seeded_bindings_without_literal_warnings(
    input: &str,
    config: &ProviderContext,
    seeds: &[BindingSeed<'_>],
) -> Result<ParsedFile, ParseError> {
    parse_with_seeded_bindings_inner(input, config, seeds, false)
}

fn parse_with_seeded_bindings_inner(
    input: &str,
    config: &ProviderContext,
    seeds: &[BindingSeed<'_>],
    collect_literal_warnings: bool,
) -> Result<ParsedFile, ParseError> {
    let preprocess_result =
        crate::heredoc::preprocess_heredocs(input).map_err(|e| ParseError::InvalidExpression {
            line: 0,
            message: e.to_string(),
        })?;
    let pairs = CarinaParser::parse(Rule::file, &preprocess_result.source)
        .map_err(|e| map_pest_error_lines(e, &preprocess_result.line_map))?;
    let single_quote_warnings = if collect_literal_warnings {
        collect_single_quoted_interpolation_warnings(pairs.clone(), &preprocess_result.line_map)
    } else {
        Vec::new()
    };

    let mut ctx = ParseContext::new(config);
    ctx.warnings.extend(single_quote_warnings);
    seed_bindings(&mut ctx, seeds);
    let mut providers = Vec::new();
    let mut resources = Vec::new();
    let mut data_sources: Vec<DataSource> = Vec::new();
    let mut uses = Vec::new();
    let mut module_calls = Vec::new();
    let mut arguments = Vec::new();
    let mut attribute_params = Vec::new();
    let mut export_params = Vec::new();
    let mut backend = None;
    let mut state_blocks = Vec::new();
    let mut upstream_states: Vec<UpstreamState> = Vec::new();
    let mut wait_bindings: Vec<WaitBinding> = Vec::new();
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
                                state_blocks.push(parse_import_state_block(stmt, &ctx)?);
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
                                let (
                                    expanded_resources,
                                    expanded_data_sources,
                                    expanded_module_calls,
                                ) = parse_for_expr(stmt, &mut ctx, &iterable_name)?;
                                resources.extend(expanded_resources);
                                data_sources.extend(expanded_data_sources);
                                module_calls.extend(expanded_module_calls);
                            }
                            Rule::if_expr => {
                                let binding_name = format!("_if{}", anon_if_counter);
                                anon_if_counter += 1;
                                let (
                                    _value,
                                    expanded_resources,
                                    expanded_data_sources,
                                    expanded_module_calls,
                                    _import,
                                ) = parse_if_expr(stmt, &mut ctx, &binding_name)?;
                                resources.extend(expanded_resources);
                                data_sources.extend(expanded_data_sources);
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
                                    expanded_data_sources,
                                    expanded_module_calls,
                                    maybe_import,
                                    is_structural,
                                ) = parse_let_binding_extended(stmt, &mut ctx)?;
                                if is_structural {
                                    ctx.structural_bindings.insert(name.clone());
                                }
                                let is_discard = name == "_";
                                let is_upstream_state = ctx.upstream_states.contains_key(&name);
                                let is_wait_binding = ctx.wait_bindings.contains_key(&name);
                                let is_provider_instance = ctx
                                    .named_provider_instances
                                    .iter()
                                    .any(|p| p.binding.as_deref() == Some(&name));
                                if !is_discard {
                                    // A seeded placeholder (from a sibling-file
                                    // declaration during the directory-aware
                                    // Pass-2 parse, #2817) is not a real
                                    // duplicate — the local declaration is the
                                    // real one for *this* file. Drop the seed
                                    // mark so subsequent in-file redeclarations
                                    // still trip the duplicate check.
                                    let shadows_seed = ctx.is_seeded_binding(&name);
                                    if !shadows_seed
                                        && (ctx.variables.contains_key(&name)
                                            || ctx.resource_bindings.contains_key(&name))
                                    {
                                        return Err(ParseError::DuplicateBinding { name, line });
                                    }
                                    if shadows_seed {
                                        ctx.unmark_seeded(&name);
                                    }
                                    if is_upstream_state || is_wait_binding || is_provider_instance
                                    {
                                        // upstream_state, wait, and named provider
                                        // instance lets do not bind a user-facing
                                        // value; the binding name is consumed by a
                                        // dedicated side-channel (`upstream_states`,
                                        // `wait_bindings`, `providers`) and would
                                        // collide downstream if also stored as a
                                        // regular variable. When a seed pre-installed
                                        // a placeholder under this name, drop it so
                                        // the ParsedFile we hand back doesn't leak the
                                        // seeded `ResourceRef` into downstream walkers
                                        // (#2817).
                                        if shadows_seed {
                                            ctx.variables.shift_remove(&name);
                                        }
                                    } else {
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
                                if !expanded_data_sources.is_empty() {
                                    if !is_discard {
                                        // Register the binding name so `name.attr`
                                        // resolves as a `ResourceRef`. Data sources
                                        // are a distinct type from `Resource`,
                                        // so a placeholder managed binding stands in
                                        // for resolution purposes — same shape as the
                                        // `_module_binding` / `_wait` placeholders.
                                        let placeholder = Resource::new("_data_source", &name);
                                        ctx.set_resource_binding(name.clone(), placeholder);
                                    }
                                    data_sources.extend(expanded_data_sources);
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
                                let is_wait = ctx.wait_bindings.contains_key(&name);
                                if is_wait && !is_discard {
                                    // Register a placeholder resource binding so that
                                    // `<wait-binding>.<attr>` parses as `ResourceRef`.
                                    // Downstream resolution (Phase 4 of #2825) treats
                                    // it as passthrough of the target's snapshot.
                                    let placeholder = Resource::new("_wait", &name);
                                    ctx.set_resource_binding(name.clone(), placeholder);
                                    wait_bindings.push(ctx.wait_bindings[&name].clone());
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

    providers.extend(std::mem::take(&mut ctx.named_provider_instances));

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

    // carina#3181: managed resources and data sources are collected into
    // separate typed `Vec`s from the start — `resources` is managed-only,
    // `data_sources` holds the `read`-keyword resources. The parser never
    // synthesizes composition resources (that is the module expander's job),
    // so `compositions` is empty here.

    Ok(ParsedFile {
        providers,
        resources,
        data_sources,
        // Virtual resources are synthesized by module-call expansion,
        // not the parser — left empty here, populated in `expander.rs`.
        compositions: Vec::new(),
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
        wait_bindings,
        requires,
        structural_bindings: ctx.structural_bindings,
        warnings: ctx.warnings,
        deferred_for_expressions: ctx.deferred_for_expressions,
        // The parser never synthesizes composition resources, so it
        // never records lineage — the trace starts empty here and is
        // populated by `module_resolver::expander` (#3306).
        expansion_trace: crate::resource::ExpansionTrace::new(),
    })
}

fn collect_single_quoted_interpolation_warnings(
    pairs: pest::iterators::Pairs<'_, Rule>,
    line_map: &[usize],
) -> Vec<ParseWarning> {
    let mut warnings = Vec::new();
    for pair in pairs {
        collect_single_quoted_interpolation_warnings_from_pair(pair, line_map, &mut warnings);
    }
    warnings
}

fn collect_single_quoted_interpolation_warnings_from_pair(
    pair: pest::iterators::Pair<'_, Rule>,
    line_map: &[usize],
    warnings: &mut Vec<ParseWarning>,
) {
    if pair.as_rule() == Rule::single_quoted_string
        && let Some(content_pair) = pair.clone().into_inner().next()
    {
        let content = content_pair.as_str();
        for (start, end) in interpolation_like_spans(content) {
            let (preprocessed_line, column) = line_column_after_prefix(
                content_pair.as_span().start_pos().line_col(),
                &content[..start],
            );
            let snippet = &content[start..end];
            let (preprocessed_end_line, end_column) =
                line_column_after_prefix((preprocessed_line, column), snippet);
            let span = ParseWarningSpan {
                start_line: original_line(preprocessed_line, line_map),
                start_column: column,
                end_line: original_line(preprocessed_end_line, line_map),
                end_column,
            };
            warnings.push(ParseWarning {
                file: None,
                line: span.start_line,
                kind: WarningKind::SingleQuotedInterpolation,
                span: Some(span),
                message: SINGLE_QUOTED_INTERPOLATION_WARNING_MESSAGE.to_string(),
            });
        }
    }

    for child in pair.into_inner() {
        collect_single_quoted_interpolation_warnings_from_pair(child, line_map, warnings);
    }
}

fn interpolation_like_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut search_start = 0;
    while let Some(relative_start) = content[search_start..].find("${") {
        let start = search_start + relative_start;
        let Some(relative_close) = content[start + 2..].find('}') else {
            break;
        };
        let end = start + 2 + relative_close + 1;
        spans.push((start, end));
        search_start = end;
    }
    spans
}

fn line_column_after_prefix(
    (base_line, base_column): (usize, usize),
    prefix: &str,
) -> (usize, usize) {
    prefix
        .chars()
        .fold((base_line, base_column), |(line, column), ch| {
            if ch == '\n' {
                (line + 1, 1)
            } else {
                (line, column + 1)
            }
        })
}

fn original_line(preprocessed_line: usize, line_map: &[usize]) -> usize {
    line_map
        .get(preprocessed_line.saturating_sub(1))
        .copied()
        .unwrap_or(preprocessed_line)
}

fn map_pest_error_lines(
    mut error: pest::error::Error<Rule>,
    line_map: &[usize],
) -> pest::error::Error<Rule> {
    error.line_col = match error.line_col {
        LineColLocation::Pos((line, column)) => {
            LineColLocation::Pos((original_line(line, line_map), column))
        }
        LineColLocation::Span((start_line, start_column), (end_line, end_column)) => {
            LineColLocation::Span(
                (original_line(start_line, line_map), start_column),
                (original_line(end_line, line_map), end_column),
            )
        }
    };
    error
}

/// Pre-register `seeds` as lexical bindings in `ctx`.
///
/// Plain value seeds mirror a local `let name = <value>`: only
/// `ctx.variables` is populated. Structural seeds mirror the
/// placeholder shape used for resource/module/data/upstream bindings:
/// `ctx.variables` receives a [`Value::Deferred(DeferredValue::BindingRef)`]
/// and `ctx.resource_bindings` receives a `_seeded` resource marker.
/// Storing the structural seed as `ResourceRef` with an empty
/// `attribute` field would be a type-level lie and previously surfaced
/// as the empty-field diagnostic in #2847.
///
/// Empty seed list is a no-op — single-file callers (the legacy
/// `parse(input, config)` wrapper, parser tests) pay no cost.
fn seed_bindings(ctx: &mut ParseContext<'_>, seeds: &[BindingSeed<'_>]) {
    for seed in seeds {
        match seed.kind() {
            SeedKind::Value(value) => {
                ctx.set_variable(seed.name().to_string(), value.clone());
            }
            SeedKind::Structural => {
                // Asymmetry: a local resource `let cluster = ...` registers
                // `Concrete(String("${cluster}"))` in `ctx.variables`; a
                // seeded structural binding registers `Deferred(BindingRef)`.
                // Dotted refs (`cluster.attr`) go through `is_resource_binding`
                // first and never observe the difference; bare refs read
                // `ctx.variables`, but every current consumer (e.g.
                // `value_as_binding_name`, the eval pipeline) handles both
                // shapes identically. A new consumer that pattern-matches on
                // only one shape would surface this: converge the two then.
                let placeholder_ref = Value::Deferred(DeferredValue::BindingRef {
                    binding: seed.name().to_string(),
                });
                ctx.set_variable(seed.name().to_string(), placeholder_ref);
                let placeholder = Resource::new("_seeded", seed.name());
                ctx.set_resource_binding(seed.name().to_string(), placeholder);
            }
        }
        ctx.seeded_bindings.insert(seed.name().to_string());
    }
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

/// Parse a .crn file and resolve resource references.
///
/// `finalize_provider_configs` is called at the end so deferred
/// `default_tags` (etc.) values are promoted into their typed fields.
/// This works for single-string inputs without `module_call` expansion.
/// Directory-scoped flows (`parse_directory_with_overrides`,
/// `load_configuration_with_config`) finalize **after**
/// `module_resolver::resolve_modules_with_config` so that composition
/// resources from module expansion are visible to the resolver pass.
pub fn parse_and_resolve(input: &str) -> Result<ParsedFile, ParseError> {
    let mut parsed = parse(input, &ProviderContext::default())?;
    resolve_resource_refs(&mut parsed)?;
    resolve_provider_unresolved_attributes(&mut parsed, &ProviderContext::default())?;
    finalize_provider_configs(&mut parsed)?;
    Ok(parsed)
}
