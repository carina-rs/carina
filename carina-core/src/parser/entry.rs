//! Top-level parser entry points: `parse`, `parse_and_resolve`, and the
//! shared `parse_expression` helpers used by block parsers.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::Rule;
use super::ast::{ParsedFile, ParserBindingMetadata, UpstreamState, UserFunction, WaitBinding};
use super::blocks::attributes::{
    ParsedArgumentsBlock, parse_arguments_block, parse_attributes_block, parse_exports_block,
};
use super::blocks::backend::parse_backend_block;
use super::blocks::module_call::parse_module_call;
use super::blocks::provider::{parse_provider_block, parse_require_statement};
use super::blocks::resource::parse_anonymous_resource;
use super::blocks::state::{parse_import_state_block, parse_moved_block, parse_removed_block};
use super::context::{ExpressionMode, ParseBinding, ParseContext, first_inner};
use super::error::{
    ParseError, ParseWarning, ParseWarningSpan, SINGLE_QUOTED_INTERPOLATION_WARNING_MESSAGE,
    WarningKind,
};
use super::expressions::for_expr::{LoopParseTarget, extract_for_iterable_name, parse_for_expr};
use super::expressions::if_expr::parse_if_expr;
use super::expressions::pipe::{parse_coalesce_expr, parse_coalesce_expr_with_mode};
use super::functions::parse_fn_def;
use super::let_binding::{LetBindingKind, parse_let_binding_extended};
use super::resolve::{
    finalize_provider_configs, resolve_provider_unresolved_attributes, resolve_resource_refs,
};
use crate::eval_value::{
    EvalValue, EvalValueError, ParsedSinkValue, StructuralHandle, StructuralHandleMetadata,
};
use crate::resource::{DataSource, LegacyLoopAddress, LoopKind, Resource, Value};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(crate) struct BindingSeed<'a> {
    name: &'a str,
    kind: SeedKind<'a>,
}

#[derive(Clone, Copy)]
pub(crate) enum SeedKind<'a> {
    Value {
        value: &'a Value,
        structural_handles: Option<&'a StructuralHandleMetadata>,
    },
    ModuleParameter,
    TypedStructural(&'a StructuralHandle),
    Provisional,
    StructuralAlias(&'a StructuralHandle),
}

/// Source span of a top-level declaration keyword.
///
/// Lines and columns are 1-indexed, matching pest's source positions and the
/// parser's other source-span types. `end_column` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopLevelBlockSourceSpan {
    pub start_line: usize,
    pub start_column: usize,
    pub end_column: usize,
}

/// Parser-backed source location of a top-level anonymous resource block.
///
/// The provider/type split mirrors [`crate::resource::ResourceId`]. Collecting
/// this from pest pairs keeps comments, let-bound resources, and resources
/// nested in `for` expressions out of editor diagnostic anchoring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnonymousResourceSourceSpan {
    pub provider: String,
    pub resource_type: String,
    pub span: TopLevelBlockSourceSpan,
}

impl<'a> BindingSeed<'a> {
    pub(crate) fn value(name: &'a str, value: &'a Value) -> Self {
        Self {
            name,
            kind: SeedKind::Value {
                value,
                structural_handles: None,
            },
        }
    }

    pub(crate) fn value_with_structural_handles(
        name: &'a str,
        value: &'a Value,
        structural_handles: &'a StructuralHandleMetadata,
    ) -> Self {
        Self {
            name,
            kind: SeedKind::Value {
                value,
                structural_handles: Some(structural_handles),
            },
        }
    }

    pub(crate) fn typed_structural(name: &'a str, handle: &'a StructuralHandle) -> Self {
        Self {
            name,
            kind: SeedKind::TypedStructural(handle),
        }
    }

    pub(crate) fn provisional(name: &'a str) -> Self {
        Self {
            name,
            kind: SeedKind::Provisional,
        }
    }

    pub(crate) fn module_parameter(name: &'a str) -> Self {
        Self {
            name,
            kind: SeedKind::ModuleParameter,
        }
    }

    pub(crate) fn structural_alias(name: &'a str, handle: &'a StructuralHandle) -> Self {
        Self {
            name,
            kind: SeedKind::StructuralAlias(handle),
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
#[cfg(test)]
pub(crate) fn parse(input: &str, config: &ProviderContext) -> Result<ParsedFile, ParseError> {
    let parsed = parse_with_seeded_bindings(input, config, &[])?;
    super::resolve::reject_cyclic_let_bindings(&parsed)?;
    Ok(parsed)
}

/// Parse one source with a concrete file identity suitable for module resolution.
pub fn parse_file(
    input: &str,
    path: &std::path::Path,
    config: &ProviderContext,
) -> Result<ParsedFile, ParseError> {
    let user_functions = resolve_top_level_user_functions(&[(Some(path), input)], config)?;
    let parsed = parse_with_seeded_bindings_inner(input, config, &[], &user_functions, true, path)?;
    super::resolve::reject_cyclic_let_bindings(&parsed)?;
    Ok(parsed)
}

/// Collect top-level `let` names without evaluating their right-hand sides.
///
/// Directory parsing uses this syntax-only pass to establish lexical scope
/// before its first semantic parse. A sibling reference may itself use
/// postfix access, so evaluating files with an entirely empty scope can fail
/// before the fixed-point pass has a chance to classify the referenced
/// binding as a value or structural declaration.
pub(crate) fn top_level_let_binding_names(input: &str) -> Result<Vec<String>, ParseError> {
    let preprocessed_file = super::preprocess_file(input)?;
    let pairs = preprocessed_file.parse()?;
    let mut names = Vec::new();

    for pair in pairs {
        if pair.as_rule() != Rule::file {
            continue;
        }
        for statement in pair
            .into_inner()
            .filter(|pair| pair.as_rule() == Rule::statement)
        {
            let Some(binding) = statement.into_inner().next() else {
                continue;
            };
            if binding.as_rule() != Rule::let_binding {
                continue;
            }
            let Some(name) = binding.into_inner().next() else {
                continue;
            };
            if name.as_str() != "_" {
                names.push(name.as_str().to_string());
            }
        }
    }

    Ok(names)
}

/// Return keyword spans for state blocks parsed as top-level statements.
///
/// This uses the main DSL grammar rather than a textual keyword scan, so a
/// schema attribute written as a nested block named `moved`, `removed`, or
/// `import` is not included.
pub fn top_level_state_block_spans(
    input: &str,
) -> Result<Vec<TopLevelBlockSourceSpan>, ParseError> {
    top_level_block_spans(input, |statement| match statement.as_rule() {
        Rule::moved_block => Some("moved"),
        Rule::removed_block => Some("removed"),
        Rule::import_state_block => Some("import"),
        _ => None,
    })
}

/// Return keyword spans for backend blocks parsed as top-level statements.
///
/// A schema attribute may use `backend` as a nested block name. Parsing with
/// the main DSL grammar keeps those nested blocks out of the result.
pub fn top_level_backend_block_spans(
    input: &str,
) -> Result<Vec<TopLevelBlockSourceSpan>, ParseError> {
    top_level_block_spans(input, |statement| match statement.as_rule() {
        Rule::backend_block => Some("backend"),
        _ => None,
    })
}

/// Return keyword spans for exports blocks parsed as top-level statements.
///
/// A schema attribute may use `exports` as a nested block name. Parsing with
/// the main DSL grammar keeps those nested blocks out of the result.
pub fn top_level_exports_block_spans(
    input: &str,
) -> Result<Vec<TopLevelBlockSourceSpan>, ParseError> {
    top_level_block_spans(input, |statement| match statement.as_rule() {
        Rule::exports_block => Some("exports"),
        _ => None,
    })
}

/// Return keyword spans for top-level `let` declarations whose right-hand
/// side is parsed as an `upstream_state` expression.
///
/// A resource schema may use `upstream_state` as a nested block name. Matching
/// the grammar rule beneath a top-level `let_binding` excludes those blocks.
pub fn top_level_upstream_state_spans(
    input: &str,
) -> Result<Vec<TopLevelBlockSourceSpan>, ParseError> {
    top_level_block_spans(input, |statement| {
        if statement.as_rule() != Rule::let_binding {
            return None;
        }
        let binding = statement.clone().into_inner().next()?;
        if binding.as_str() == "_" {
            return None;
        }
        pair_contains_rule(statement, Rule::upstream_state_expr).then_some("let")
    })
}

/// Return type-name spans for authored top-level anonymous resources.
pub fn top_level_anonymous_resource_spans(
    input: &str,
) -> Result<Vec<AnonymousResourceSourceSpan>, ParseError> {
    let preprocessed_file = super::preprocess_file(input)?;
    let pairs = preprocessed_file.parse()?;
    let mut spans = Vec::new();

    for pair in pairs {
        if pair.as_rule() != Rule::file {
            continue;
        }
        for statement in pair
            .into_inner()
            .filter(|pair| pair.as_rule() == Rule::statement)
        {
            let block = statement
                .into_inner()
                .next()
                .ok_or_else(|| ParseError::InternalError {
                    expected: "top-level statement body".to_string(),
                    context: "anonymous resource source span collection".to_string(),
                })?;
            if block.as_rule() != Rule::anonymous_resource {
                continue;
            }
            let type_pair = block
                .into_inner()
                .next()
                .ok_or_else(|| ParseError::InternalError {
                    expected: "anonymous resource type".to_string(),
                    context: "anonymous resource source span collection".to_string(),
                })?;
            let full_type = type_pair.as_str();
            let Some((provider, resource_type)) = full_type.split_once('.') else {
                continue;
            };
            let start = preprocessed_file.original_position(type_pair.as_span().start());
            spans.push(AnonymousResourceSourceSpan {
                provider: provider.to_string(),
                resource_type: resource_type.to_string(),
                span: TopLevelBlockSourceSpan {
                    start_line: start.line(),
                    start_column: start.column(),
                    end_column: start.column() + full_type.chars().count(),
                },
            });
        }
    }

    Ok(spans)
}

fn top_level_block_spans(
    input: &str,
    keyword_for_statement: fn(&pest::iterators::Pair<Rule>) -> Option<&'static str>,
) -> Result<Vec<TopLevelBlockSourceSpan>, ParseError> {
    let preprocessed_file = super::preprocess_file(input)?;
    let pairs = preprocessed_file.parse()?;
    let mut spans = Vec::new();

    for pair in pairs {
        if pair.as_rule() != Rule::file {
            continue;
        }
        for statement in pair
            .into_inner()
            .filter(|pair| pair.as_rule() == Rule::statement)
        {
            let block = statement
                .into_inner()
                .next()
                .ok_or_else(|| ParseError::InternalError {
                    expected: "top-level statement body".to_string(),
                    context: "top-level block source span collection".to_string(),
                })?;
            let Some(keyword) = keyword_for_statement(&block) else {
                continue;
            };
            let start = preprocessed_file.original_position(block.as_span().start());
            spans.push(TopLevelBlockSourceSpan {
                start_line: start.line(),
                start_column: start.column(),
                end_column: start.column() + keyword.len(),
            });
        }
    }

    Ok(spans)
}

fn pair_contains_rule(pair: &pest::iterators::Pair<Rule>, expected: Rule) -> bool {
    pair.as_rule() == expected
        || pair
            .clone()
            .into_inner()
            .any(|inner| pair_contains_rule(&inner, expected))
}

/// Parse a .crn file with `seeds` pre-registered as lexical bindings.
///
/// Each seed carries the directory-wide binding name and, for plain
/// value-shaped `let` bindings, the pass-1 value from the merged file.
/// Value seeds are installed as typed value bindings; all other binding
/// kinds are installed as typed structural bindings plus a placeholder
/// `Resource` in `ctx.resource_bindings`. This keeps sibling value access
/// and structural reference paths distinct. Provisional names may temporarily
/// use a `BindingRef` until the directory fixed point learns their declaration
/// kind; confirmed structural handles never lower into `Value`.
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
#[cfg(test)]
pub(crate) fn parse_with_seeded_bindings(
    input: &str,
    config: &ProviderContext,
    seeds: &[BindingSeed<'_>],
) -> Result<ParsedFile, ParseError> {
    let user_functions = top_level_user_functions(input, config)?;
    parse_with_seeded_bindings_inner(
        input,
        config,
        seeds,
        &user_functions,
        true,
        std::path::Path::new("test.crn"),
    )
}

/// Parse only the top-level function declarations from one source file.
/// Directory parsing uses this lightweight prepass to make sibling and
/// forward functions available while parsing structure-producing expressions
/// such as `for n in names(...)`.
#[cfg(test)]
pub(crate) fn top_level_user_functions(
    input: &str,
    config: &ProviderContext,
) -> Result<HashMap<String, UserFunction>, ParseError> {
    resolve_top_level_user_functions(&[(None, input)], config)
}

/// Parse the directory's function declarations as one lexical scope.
///
/// Function bodies are resolved in dependency order. This keeps builtin calls
/// strongly typed while still allowing a body to call a forward declaration or
/// a function declared in a sibling file.
pub(crate) fn top_level_user_functions_in_files(
    files: &[(PathBuf, String)],
    config: &ProviderContext,
) -> Result<HashMap<String, UserFunction>, ParseError> {
    let sources = files
        .iter()
        .map(|(path, input)| (Some(path.as_path()), input.as_str()))
        .collect::<Vec<_>>();
    resolve_top_level_user_functions(&sources, config)
}

fn resolve_top_level_user_functions(
    sources: &[(Option<&Path>, &str)],
    config: &ProviderContext,
) -> Result<HashMap<String, UserFunction>, ParseError> {
    // Preserve the old directory behavior for duplicate names: a later file
    // owns the seed, while two declarations in the same file are an error.
    let mut owners = HashMap::<String, usize>::new();
    for (source_index, (path, input)) in sources.iter().enumerate() {
        let preprocessed_file =
            super::preprocess_file(input).map_err(|error| locate_function_error(error, *path))?;
        let pairs = preprocessed_file
            .parse()
            .map_err(|error| locate_function_error(error, *path))?;
        let mut local_names = HashSet::new();
        for pair in pairs {
            if pair.as_rule() != Rule::file {
                continue;
            }
            for statement in pair
                .into_inner()
                .flat_map(|statement| statement.into_inner())
            {
                if statement.as_rule() != Rule::fn_def {
                    continue;
                }
                let name = statement
                    .clone()
                    .into_inner()
                    .next()
                    .expect("fn_def always starts with an identifier")
                    .as_str()
                    .to_string();
                if !local_names.insert(name.clone()) {
                    return Err(locate_function_error(
                        ParseError::UserFunctionError(
                            "duplicate function definition in one file".to_string(),
                        ),
                        *path,
                    ));
                }
                owners.insert(name, source_index);
            }
        }
    }

    let mut pending = owners.keys().cloned().collect::<HashSet<_>>();
    let mut ctx = ParseContext::new(config);
    let mut functions = HashMap::new();

    while !pending.is_empty() {
        let mut made_progress = false;
        for (source_index, (path, input)) in sources.iter().enumerate() {
            let preprocessed_file = super::preprocess_file(input)
                .map_err(|error| locate_function_error(error, *path))?;
            let pairs = preprocessed_file
                .parse()
                .map_err(|error| locate_function_error(error, *path))?;
            for pair in pairs {
                if pair.as_rule() != Rule::file {
                    continue;
                }
                for statement in pair
                    .into_inner()
                    .flat_map(|statement| statement.into_inner())
                {
                    if statement.as_rule() != Rule::fn_def {
                        continue;
                    }
                    let name = statement
                        .clone()
                        .into_inner()
                        .next()
                        .expect("fn_def always starts with an identifier")
                        .as_str()
                        .to_string();
                    if !pending.contains(&name) || owners.get(&name) != Some(&source_index) {
                        continue;
                    }

                    let mut warnings = Vec::new();
                    ctx.set_preprocessed_file(&preprocessed_file);
                    match parse_fn_def(statement, &ctx, &mut warnings, &preprocessed_file) {
                        Ok(mut function) => {
                            function.body.source_file = path.map(Path::to_path_buf);
                            ctx.insert_user_function(name.clone(), function.clone());
                            // Calls made while parsing another declaration must
                            // remain deferred until the complete table exists.
                            ctx.seeded_user_functions.insert(name.clone());
                            functions.insert(name.clone(), function);
                            pending.remove(&name);
                            made_progress = true;
                        }
                        Err(ParseError::UnknownFunction {
                            name: dependency, ..
                        }) if owners.contains_key(&dependency) => {
                            // The dependency is declared but has not been parsed
                            // yet. A later pass will retry this body.
                        }
                        Err(error) => return Err(locate_function_error(error, *path)),
                    }
                }
            }
        }

        if !made_progress {
            let mut cycle = pending.iter().cloned().collect::<Vec<_>>();
            cycle.sort_unstable();
            let name = cycle
                .into_iter()
                .next()
                .expect("non-empty pending function set");
            let source_index = owners[&name];
            return Err(locate_function_error(
                ParseError::RecursiveFunction(name),
                sources[source_index].0,
            ));
        }
    }

    Ok(functions)
}

fn locate_function_error(error: ParseError, path: Option<&Path>) -> ParseError {
    match path {
        Some(path) => error.in_file(path.to_path_buf()),
        None => error,
    }
}

pub(crate) fn top_level_argument_names(input: &str) -> Result<Vec<String>, ParseError> {
    let preprocessed_file = super::preprocess_file(input)?;
    let pairs = preprocessed_file.parse()?;
    let mut names = Vec::new();
    for pair in pairs {
        if pair.as_rule() != Rule::file {
            continue;
        }
        for statement in pair
            .into_inner()
            .flat_map(|statement| statement.into_inner())
        {
            if statement.as_rule() != Rule::arguments_block {
                continue;
            }
            for parameter in statement.into_inner() {
                if parameter.as_rule() != Rule::arguments_param {
                    continue;
                }
                if let Some(name) = parameter.into_inner().next() {
                    names.push(name.as_str().to_string());
                }
            }
        }
    }
    Ok(names)
}

pub(crate) fn parse_with_directory_context(
    input: &str,
    source_file: &std::path::Path,
    config: &ProviderContext,
    seeds: &[BindingSeed<'_>],
    user_functions: &HashMap<String, UserFunction>,
    bindings_fully_classified: bool,
) -> Result<ParsedFile, ParseError> {
    parse_with_seeded_bindings_inner(
        input,
        config,
        seeds,
        user_functions,
        bindings_fully_classified,
        source_file,
    )
}

fn parse_with_seeded_bindings_inner(
    input: &str,
    config: &ProviderContext,
    seeds: &[BindingSeed<'_>],
    seeded_user_functions: &HashMap<String, UserFunction>,
    bindings_fully_classified: bool,
    source_file: &std::path::Path,
) -> Result<ParsedFile, ParseError> {
    let preprocessed_file = super::preprocess_file(input)?;
    let pairs = preprocessed_file.parse()?;
    // Bootstrap passes use provisional sibling names and exist only to
    // classify declarations. The final pass both emits source warnings and
    // evaluates static conditions against the now-typed seed table.
    let single_quote_warnings = if bindings_fully_classified {
        collect_single_quoted_interpolation_warnings(pairs.clone(), &preprocessed_file)
    } else {
        Vec::new()
    };

    let mut ctx = ParseContext::new(config);
    ctx.set_preprocessed_file(&preprocessed_file);
    ctx.set_source_scope(source_file);
    ctx.defer_provisional_static_conditions = !bindings_fully_classified;
    ctx.warnings.extend(single_quote_warnings);
    seed_bindings(&mut ctx, seeds);
    ctx.extend_user_functions(seeded_user_functions.clone());
    ctx.seeded_user_functions
        .extend(seeded_user_functions.keys().cloned());
    let mut providers = Vec::new();
    let mut resources = Vec::new();
    let mut data_sources: Vec<DataSource> = Vec::new();
    let mut uses = Vec::new();
    let mut module_calls = Vec::new();
    let mut arguments = Vec::new();
    let mut argument_type_sources = Vec::new();
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
                                backend =
                                    Some(parse_backend_block(stmt, &ctx, &preprocessed_file)?);
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
                                let parsed_arguments_block = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_arguments_block(
                                        stmt,
                                        config,
                                        &mut ctx,
                                        &preprocessed_file,
                                        source_file,
                                        &mut warnings,
                                    );
                                    ctx.warnings = warnings;
                                    result?
                                };
                                let ParsedArgumentsBlock {
                                    parameters,
                                    type_sources,
                                } = parsed_arguments_block;
                                arguments.extend(parameters);
                                argument_type_sources.extend(type_sources);
                            }
                            Rule::attributes_block => {
                                let parsed_attribute_params = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_attributes_block(
                                        stmt,
                                        &ctx,
                                        &preprocessed_file,
                                        source_file,
                                        &mut warnings,
                                    );
                                    ctx.warnings = warnings;
                                    result?
                                };
                                attribute_params.extend(parsed_attribute_params);
                            }
                            Rule::exports_block => {
                                let parsed_export_params = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_exports_block(
                                        stmt,
                                        &ctx,
                                        &preprocessed_file,
                                        source_file,
                                        &mut warnings,
                                    );
                                    ctx.warnings = warnings;
                                    result?
                                };
                                export_params.extend(parsed_export_params);
                            }
                            Rule::import_state_block => {
                                state_blocks.push(parse_import_state_block(
                                    stmt,
                                    &ctx,
                                    &preprocessed_file,
                                )?);
                            }
                            Rule::removed_block => {
                                state_blocks.push(parse_removed_block(stmt)?);
                            }
                            Rule::moved_block => {
                                state_blocks.push(parse_moved_block(stmt, &preprocessed_file)?);
                            }
                            Rule::require_statement => {
                                requires.push(parse_require_statement(
                                    stmt,
                                    &ctx,
                                    &preprocessed_file,
                                    source_file,
                                )?);
                            }
                            Rule::for_expr => {
                                let iterable_name =
                                    extract_for_iterable_name(&stmt, anon_for_counter);
                                anon_for_counter += 1;
                                let source = preprocessed_file.loop_source(
                                    &stmt,
                                    LoopKind::For,
                                    source_file,
                                );
                                let target = LoopParseTarget::unbound(
                                    source,
                                    LegacyLoopAddress::new(iterable_name),
                                );
                                let (
                                    expanded_resources,
                                    expanded_data_sources,
                                    expanded_module_calls,
                                ) = parse_for_expr(
                                    stmt,
                                    &mut ctx,
                                    target,
                                    &preprocessed_file,
                                    source_file,
                                )?;
                                resources.extend(expanded_resources);
                                data_sources.extend(expanded_data_sources);
                                module_calls.extend(expanded_module_calls);
                            }
                            Rule::if_expr => {
                                let legacy_name = format!("_if{}", anon_if_counter);
                                anon_if_counter += 1;
                                let source =
                                    preprocessed_file.loop_source(&stmt, LoopKind::If, source_file);
                                let target = LoopParseTarget::unbound(
                                    source,
                                    LegacyLoopAddress::new(legacy_name),
                                );
                                let (
                                    _value,
                                    expanded_resources,
                                    expanded_data_sources,
                                    expanded_module_calls,
                                    _import,
                                    _binding_kind,
                                ) = parse_if_expr(
                                    stmt,
                                    &mut ctx,
                                    target,
                                    &preprocessed_file,
                                    source_file,
                                )?;
                                resources.extend(expanded_resources);
                                data_sources.extend(expanded_data_sources);
                                module_calls.extend(expanded_module_calls);
                            }
                            Rule::fn_def => {
                                let user_fn = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result =
                                        parse_fn_def(stmt, &ctx, &mut warnings, &preprocessed_file);
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
                                if ctx.user_functions.contains_key(&fn_name)
                                    && !ctx.seeded_user_functions.remove(&fn_name)
                                {
                                    return Err(ParseError::UserFunctionError(format!(
                                        "duplicate function definition: '{fn_name}'"
                                    )));
                                }
                                ctx.insert_user_function(fn_name, user_fn);
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
                                    binding_kind,
                                    is_structural,
                                ) = parse_let_binding_extended(
                                    stmt,
                                    &mut ctx,
                                    &preprocessed_file,
                                    source_file,
                                )?;
                                if is_structural {
                                    ctx.structural_bindings.insert(name.clone());
                                }
                                let is_discard = name == "_";
                                let is_upstream_state = ctx.upstream_states.contains_key(&name);
                                let is_wait_binding = ctx.wait_bindings.contains_key(&name);
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
                                        && (ctx.contains_binding(&name)
                                            || ctx.resource_bindings.contains_key(&name))
                                    {
                                        return Err(ParseError::DuplicateBinding { name, line });
                                    }
                                    if shadows_seed {
                                        ctx.unmark_seeded(&name);
                                    }
                                    match binding_kind {
                                        LetBindingKind::Value => {
                                            ctx.set_variable_at_line(name.clone(), value, line);
                                        }
                                        LetBindingKind::Structural => {
                                            let handle = value
                                                .into_structural_handle()
                                                .ok_or_else(|| ParseError::InternalError {
                                                    expected: "parser structural handle"
                                                        .to_string(),
                                                    context: format!(
                                                        "structural let binding {name:?}"
                                                    ),
                                                })?;
                                            ctx.set_structural_binding(name.clone(), handle);
                                        }
                                        LetBindingKind::StructuralAlias => {
                                            let handle = value
                                                .into_structural_handle()
                                                .ok_or_else(|| ParseError::InternalError {
                                                    expected: "parser structural handle"
                                                        .to_string(),
                                                    context: format!(
                                                        "structural alias binding {name:?}"
                                                    ),
                                                })?;
                                            ctx.set_structural_alias(name.clone(), handle);
                                        }
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
                                        let placeholder = Resource::new(
                                            "_data_source",
                                            crate::resource::ResourceIdentity::new(name.clone()),
                                        );
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
                                        let placeholder = Resource::new(
                                            "_module_binding",
                                            crate::resource::ResourceIdentity::new(name.clone()),
                                        );
                                        ctx.set_resource_binding(name.clone(), placeholder);
                                    }
                                }
                                if is_upstream_state && !is_discard {
                                    let placeholder = Resource::new(
                                        "_upstream_state",
                                        crate::resource::ResourceIdentity::new(name.clone()),
                                    );
                                    ctx.set_resource_binding(name.clone(), placeholder);
                                    upstream_states.push(ctx.upstream_states[&name].clone());
                                }
                                if is_wait_binding && !is_discard {
                                    // Register a placeholder resource binding so that
                                    // `<wait-binding>.<attr>` parses as `ResourceRef`.
                                    // Downstream resolution (Phase 4 of #2825) treats
                                    // it as passthrough of the target's snapshot.
                                    let placeholder = Resource::new(
                                        "_wait",
                                        crate::resource::ResourceIdentity::new(name.clone()),
                                    );
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
                                let call =
                                    parse_module_call(stmt, &ctx, &preprocessed_file, source_file)?;
                                module_calls.push(call);
                            }
                            Rule::anonymous_resource => {
                                let resource = parse_anonymous_resource(
                                    stmt,
                                    &ctx,
                                    &preprocessed_file,
                                    source_file,
                                )?;
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
    // closure. Pipe / call paths read from typed value bindings directly via
    // `get_variable`, so the closure was already available where it
    // mattered.
    let mut parser_binding_metadata = ParserBindingMetadata::default();
    for (name, binding) in &ctx.bindings {
        // Directory seeds are context, not declarations owned by this file.
        // In particular a provisional seed means only "this name exists";
        // recording it as local structural metadata would misclassify every
        // forward value binding before the fixed point can resolve its RHS.
        if ctx.is_seeded_binding(name) {
            continue;
        }
        match binding {
            ParseBinding::Structural { handle, .. } => {
                parser_binding_metadata.insert_structural_handle(name.clone(), handle.clone());
                if handle.alias_name().is_some() {
                    parser_binding_metadata.insert_structural_alias(name.clone(), handle.clone());
                }
            }
            ParseBinding::Value {
                value,
                shared_value: _,
                source_line: _,
                declaration: _,
                declared_type: _,
                scope: _,
            } => {
                if let Some(metadata) = value.structural_handle_metadata() {
                    parser_binding_metadata
                        .insert_value_structural_handles(name.clone(), metadata.clone());
                }
            }
        }
    }
    for check in ctx.pending_function_type_checks.borrow().iter().cloned() {
        parser_binding_metadata.insert_pending_function_type_check(check);
    }
    for (name, span) in argument_type_sources {
        parser_binding_metadata.insert_argument_type_source(name, source_file.to_path_buf(), span);
    }
    let seeded_bindings = ctx.seeded_bindings.clone();
    let mut variables = IndexMap::new();
    for (name, binding) in std::mem::take(&mut ctx.bindings) {
        if seeded_bindings.contains(&name) {
            continue;
        }
        let ParseBinding::Value {
            value: eval,
            source_line,
            ..
        } = binding
        else {
            continue;
        };
        match (*eval).into_value() {
            Ok(value) => {
                variables.insert(name, value);
            }
            Err(EvalValueError::Closure(_)) => {
                // Partial applications are evaluator-only bindings. A later
                // pipe/call already consumed them if they were used.
            }
            Err(error @ EvalValueError::BareStructuralHandle(_)) => {
                return Err(ParseError::from_eval_value(error, source_line));
            }
            Err(error @ EvalValueError::PendingValue(_)) => {
                return Err(ParseError::from_eval_value(error, source_line));
            }
        }
    }

    // carina#3181: managed resources and data sources are collected into
    // separate typed `Vec`s from the start — `resources` is managed-only,
    // `data_sources` holds the `read`-keyword resources. The parser never
    // synthesizes composition resources (that is the module expander's job),
    // so `compositions` is empty here.

    let parsed_binding_uses = ctx.take_parsed_binding_uses();
    let mut user_functions = Arc::unwrap_or_clone(ctx.user_functions);
    user_functions.retain(|name, _| !ctx.seeded_user_functions.contains(name));

    let mut parsed = ParsedFile::empty_with_parsed_binding_uses(parsed_binding_uses);
    parsed.providers = providers;
    parsed.resources = resources;
    parsed.data_sources = data_sources;
    // Virtual resources are synthesized by module-call expansion, not the
    // parser; `ParsedFile::default()` leaves compositions empty here.
    parsed.variables = variables;
    parsed.uses = uses;
    parsed.module_calls = module_calls;
    parsed.arguments = arguments;
    parsed.attribute_params = attribute_params;
    parsed.export_params = export_params;
    parsed.backend = backend;
    parsed.state_blocks = state_blocks;
    parsed.user_functions = user_functions;
    parsed.upstream_states = upstream_states;
    parsed.wait_bindings = wait_bindings;
    parsed.requires = requires;
    parsed.structural_bindings = ctx.structural_bindings;
    parsed.parser_binding_metadata = parser_binding_metadata;
    parsed.warnings = ctx.warnings;
    *parsed.parsed_deferred_for_expressions_mut() = ctx.parsed_deferred_for_expressions;
    let binding_scope = crate::binding_index::BindingNameSet::from_parsed(&parsed);
    parsed.set_loop_binding_scope(binding_scope.iter_names().map(str::to_string).collect());
    // The parser never synthesizes composition resources, so its default
    // expansion trace remains empty until module expansion (#3306).
    Ok(parsed)
}

fn collect_single_quoted_interpolation_warnings(
    pairs: pest::iterators::Pairs<'_, Rule>,
    preprocessed_file: &super::PreprocessedFile,
) -> Vec<ParseWarning> {
    let mut warnings = Vec::new();
    for pair in pairs {
        collect_single_quoted_interpolation_warnings_from_pair(
            pair,
            preprocessed_file,
            &mut warnings,
        );
    }
    warnings
}

fn collect_single_quoted_interpolation_warnings_from_pair(
    pair: pest::iterators::Pair<'_, Rule>,
    preprocessed_file: &super::PreprocessedFile,
    warnings: &mut Vec<ParseWarning>,
) {
    if pair.as_rule() == Rule::single_quoted_string
        && let Some(content_pair) = pair.clone().into_inner().next()
    {
        let content = content_pair.as_str();
        for (start, end) in interpolation_like_spans(content) {
            let content_start = content_pair.as_span().start();
            let start = preprocessed_file.original_position(content_start + start);
            let end = preprocessed_file.original_position(content_start + end);
            let span = ParseWarningSpan {
                start_line: start.line(),
                start_column: start.column(),
                end_line: end.line(),
                end_column: end.column(),
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
        collect_single_quoted_interpolation_warnings_from_pair(child, preprocessed_file, warnings);
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

/// Pre-register `seeds` as lexical bindings in `ctx`.
///
/// Plain value seeds mirror a local `let name = <value>` and structural
/// seeds use the distinct [`ParseBinding::Structural`] scope entry. A
/// `_seeded` resource marker is also installed for compatibility with
/// resource-path parsing. Seeded structural bindings retain their typed
/// parser-only handle until a bare value crosses the `Value` boundary.
///
/// Empty seed list is a no-op — single-file callers (the legacy
/// `parse(input, config)` wrapper, parser tests) pay no cost.
fn seed_bindings(ctx: &mut ParseContext<'_>, seeds: &[BindingSeed<'_>]) {
    for seed in seeds {
        match seed.kind() {
            SeedKind::Value {
                value,
                structural_handles,
            } => {
                ctx.set_variable(
                    seed.name().to_string(),
                    EvalValue::computed(value.clone(), structural_handles.cloned()),
                );
            }
            SeedKind::ModuleParameter => {
                ctx.set_variable(
                    seed.name().to_string(),
                    Value::Deferred(crate::resource::DeferredValue::Unknown(
                        crate::resource::UnknownReason::ModuleArgument {
                            name: seed.name().to_string(),
                            source_line: (0).into(),
                            diagnostic_name: None,
                        },
                    )),
                );
            }
            SeedKind::TypedStructural(handle) => {
                ctx.set_structural_binding(seed.name().to_string(), handle.clone());
                let placeholder = Resource::new(
                    "_seeded",
                    crate::resource::ResourceIdentity::new(seed.name().to_string()),
                );
                ctx.set_resource_binding(seed.name().to_string(), placeholder);
            }
            SeedKind::Provisional => {
                ctx.set_variable(
                    seed.name().to_string(),
                    EvalValue::provisional_reference(seed.name()),
                );
                let placeholder = Resource::new(
                    "_seeded",
                    crate::resource::ResourceIdentity::new(seed.name().to_string()),
                );
                // A provisional name is only known to exist; inserting the
                // resource-path placeholder must not upgrade its value binding
                // to a confirmed structural handle.
                ctx.resource_bindings
                    .insert(seed.name().to_string(), placeholder);
            }
            SeedKind::StructuralAlias(handle) => {
                ctx.set_structural_alias(seed.name().to_string(), handle.clone());
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
    let line = pair.line_col().0;
    let eval = parse_expression_eval(pair, ctx)?;
    eval.into_value()
        .map_err(|error| ParseError::from_eval_value(error, line))
}

pub(crate) fn parse_expression_in_sink(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<ParsedSinkValue, ParseError> {
    let line = pair.line_col().0;
    parse_expression_eval_with_mode(pair, ctx, mode)?
        .into_sink_value()
        .map_err(|error| ParseError::from_eval_value(error, line))
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

pub(crate) fn parse_expression_eval_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    let inner = first_inner(pair, "expression body", "expression")?;
    parse_coalesce_expr_with_mode(inner, ctx, mode)
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
pub fn parse_file_and_resolve(
    input: &str,
    path: &std::path::Path,
) -> Result<ParsedFile, ParseError> {
    let mut parsed = parse_file(input, path, &ProviderContext::default())?;
    resolve_resource_refs(&mut parsed)?;
    resolve_provider_unresolved_attributes(&mut parsed, &ProviderContext::default())?;
    finalize_provider_configs(&mut parsed)?;
    Ok(parsed)
}

#[cfg(test)]
pub(crate) fn parse_and_resolve(input: &str) -> Result<ParsedFile, ParseError> {
    parse_file_and_resolve(input, std::path::Path::new("test.crn"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_span_api_preserves_unterminated_heredoc_line() {
        let error = top_level_upstream_state_spans("let x = mock.test.Thing {\n  p = <<EOT\nabc\n")
            .expect_err("an unterminated heredoc must be rejected");

        let ParseError::InvalidExpression { line, .. } = error else {
            panic!("unexpected error: {error:?}");
        };
        assert_eq!(line, 4);
    }
}
