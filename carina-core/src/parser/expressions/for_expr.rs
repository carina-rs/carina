//! `for` expression parsing.
//!
//! Each `for` expression expands at parse time into a vector of concrete
//! resources (or module calls) addressed by the loop's index/key. When
//! the iterable depends on an upstream value the expansion is deferred —
//! the parse pass records a [`crate::parser::DeferredForExpression`] so
//! a later pass can finish the expansion once the upstream value
//! resolves.

use crate::eval_value::{EvalValue, StructuralHandleMetadata};
use crate::parser::expressions::primary::parse_postfix_eval;
use crate::parser::{
    DeferredForExpression, DeferredForTarget, ModuleCall, ParseContext, ParseError, ParseWarning,
    PreprocessedFile, ResourceParseTarget, Rule, WarningKind, display_string_literal,
    evaluate_static_value, first_inner, next_pair, parse_expression, parse_module_call,
    parse_read_resource_expr, parse_resource_for_target,
};
use crate::resource::{
    ConcreteValue, DataSource, DeferredValue, ForLoopElement, LegacyLoopAddress, LoopElementOrigin,
    LoopKind, LoopSource, Resource, UnboundLoopTemplate, UnknownReason, Value,
};

/// Binding pattern for a for expression
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ForBinding {
    /// Simple: `for x in ...`
    Simple(String),
    /// Indexed: `for (i, x) in ...`
    Indexed(String, String),
    /// Map: `for k, v in ...`
    Map(String, String),
}

impl ForBinding {
    /// Every binding name introduced by this pattern, in declaration order.
    pub fn names(&self) -> Vec<&str> {
        match self {
            ForBinding::Simple(a) => vec![a.as_str()],
            ForBinding::Indexed(a, b) | ForBinding::Map(a, b) => vec![a.as_str(), b.as_str()],
        }
    }
}

/// Whether `name` appears in `text` as a whole identifier (not a substring
/// of a longer identifier). An identifier is bounded by anything that is
/// not ASCII alphanumeric or `_`.
fn identifier_appears_in(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let name_bytes = name.as_bytes();
    let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    let mut i = 0;
    while i + name_bytes.len() <= bytes.len() {
        if &bytes[i..i + name_bytes.len()] == name_bytes {
            let before_ok = i == 0 || !is_id_char(bytes[i - 1]);
            let after_idx = i + name_bytes.len();
            let after_ok = after_idx == bytes.len() || !is_id_char(bytes[after_idx]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Result of parsing a for expression body: a managed resource, a data
/// source, or a module call.
pub(crate) enum ForBodyResult {
    Resource(Box<Resource>),
    DataSource(Box<DataSource>),
    ModuleCall(ModuleCall),
}

/// Whether a loop body produces user-bound or anonymous resources.
#[derive(Debug, Clone)]
pub(crate) enum LoopParseTarget {
    Bound { binding: String },
    Unbound(UnboundLoopTemplate),
}

fn parse_deferred_resource_template(
    body_pair: pest::iterators::Pair<Rule>,
    binding: &ForBinding,
    ctx: &ParseContext,
    target: &LoopParseTarget,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<Option<Box<Resource>>, ParseError> {
    let mut template_ctx = ctx.child_scope();
    let bind = |ctx: &mut ParseContext, name: &str, value: Value| {
        if name != "_" {
            ctx.set_variable(name.to_string(), value);
        }
    };
    let bind_value = |ctx: &mut ParseContext, name: &str| {
        if name != "_" {
            ctx.set_variable(
                name.to_string(),
                EvalValue::deferred_for_value(name.to_string()),
            );
        }
    };
    match binding {
        ForBinding::Simple(value) => bind_value(&mut template_ctx, value),
        ForBinding::Indexed(index, value) => {
            bind(
                &mut template_ctx,
                index,
                Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex)),
            );
            bind_value(&mut template_ctx, value);
        }
        ForBinding::Map(key, value) => {
            bind(
                &mut template_ctx,
                key,
                Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey)),
            );
            bind_value(&mut template_ctx, value);
        }
    }

    match parse_for_body(
        body_pair,
        &template_ctx,
        target,
        None,
        preprocessed_file,
        source_file,
    ) {
        Err(error @ ParseError::UnboundLoopRead { .. }) => Err(error),
        Ok(ForBodyResult::Resource(resource)) => Ok(Some(resource)),
        Ok(ForBodyResult::DataSource(_) | ForBodyResult::ModuleCall(_)) | Err(_) => Ok(None),
    }
}

impl LoopParseTarget {
    pub(crate) fn bound(binding: impl Into<String>) -> Self {
        Self::Bound {
            binding: binding.into(),
        }
    }

    pub(crate) fn unbound(source: LoopSource, legacy_base: LegacyLoopAddress) -> Self {
        Self::Unbound(UnboundLoopTemplate::new(source, legacy_base))
    }

    pub(crate) fn scheduler_key(&self) -> &str {
        match self {
            Self::Bound { binding } => binding,
            Self::Unbound(template) => template.scheduler_key(),
        }
    }

    pub(crate) fn unbound_read_error(&self, kind: LoopKind) -> Option<ParseError> {
        match self {
            Self::Bound { .. } => None,
            Self::Unbound(template) => template.unbound_read_error(kind),
        }
    }

    fn element_address(&self, element: &ForLoopElement) -> String {
        element.address(self.scheduler_key())
    }

    fn element_origin(&self, element: ForLoopElement) -> Option<LoopElementOrigin> {
        match self {
            Self::Bound { .. } => None,
            Self::Unbound(template) => Some(template.element_origin(element)),
        }
    }

    pub(crate) fn if_origin(&self) -> Option<LoopElementOrigin> {
        match self {
            Self::Bound { .. } => None,
            Self::Unbound(template) => Some(template.if_origin()),
        }
    }

    pub(crate) fn if_resource_target(&self) -> ResourceParseTarget {
        match self {
            Self::Bound { binding } => ResourceParseTarget::Bound {
                binding: binding.clone(),
            },
            Self::Unbound(_) => ResourceParseTarget::Pending {
                origin: self.if_origin(),
            },
        }
    }

    fn resource_target(&self, element: Option<ForLoopElement>) -> ResourceParseTarget {
        match self {
            Self::Bound { .. } => ResourceParseTarget::Bound {
                binding: element.as_ref().map_or_else(
                    || format!("{}[?]", self.scheduler_key()),
                    |element| self.element_address(element),
                ),
            },
            Self::Unbound(_) => ResourceParseTarget::Pending {
                origin: element.and_then(|element| self.element_origin(element)),
            },
        }
    }

    fn deferred_target(&self) -> DeferredForTarget {
        match self {
            Self::Bound { binding } => DeferredForTarget::Bound {
                binding: binding.clone(),
            },
            Self::Unbound(template) => DeferredForTarget::Unbound {
                template: template.clone(),
            },
        }
    }
}

/// Parse a for expression and expand it into individual resources and/or module calls.
///
/// `for x in list { resource_expr }` expands to resources with addresses like
/// `binding[0]`, `binding[1]`, etc.
///
/// Extract a binding name from a for-expression's iterable.
///
/// For `for x in orgs.accounts { ... }`, returns `_accounts`.
/// For non-variable iterables (lists, function calls), falls back to `_for{N}`.
pub(crate) fn extract_for_iterable_name(
    pair: &pest::iterators::Pair<Rule>,
    counter: usize,
) -> String {
    let fallback = format!("_for{}", counter);
    let mut inner = pair.clone().into_inner();
    // Skip for_binding, take for_iterable
    let iterable_pair = inner.nth(1);
    let Some(iterable) = iterable_pair else {
        return fallback;
    };
    // Only extract name from variable_ref iterables
    let first_child = iterable.into_inner().next();
    let Some(child) = first_child else {
        return fallback;
    };
    if child.as_rule() != Rule::variable_ref {
        return fallback;
    }
    // Take the last segment of the dotted path (e.g., "accounts" from "orgs.accounts")
    let text = child.as_str().trim();
    let last_segment = text.rsplit('.').next().unwrap_or(text);
    format!("_{}", last_segment)
}

/// `for k, v in map { resource_expr }` expands to resources with addresses like
/// `binding["key1"]`, `binding["key2"]`, etc.
///
/// When the body is a module call, each iteration produces a module call with
/// a binding name like `binding[0]` or `binding["key"]`.
#[allow(clippy::type_complexity)]
pub(crate) fn parse_for_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    target: LoopParseTarget,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<(Vec<Resource>, Vec<DataSource>, Vec<ModuleCall>), ParseError> {
    let for_line = pair.as_span().start_pos().line_col().0;
    let mut inner = pair.into_inner();

    // Parse the binding pattern
    let binding_pair = next_pair(&mut inner, "for binding", "for expression")?;
    let binding = parse_for_binding(binding_pair)?;

    // Parse the iterable expression
    let iterable_pair = next_pair(&mut inner, "iterable", "for expression")?;
    let ParsedForIterable {
        value: iterable,
        bare_unbound_identifier,
        structural_for_binding,
        provisional_binding,
        structural_handle_metadata,
    } = parse_for_iterable(iterable_pair, ctx)?;

    // Parse the body (we'll re-parse it for each iteration)
    let body_pair = next_pair(&mut inner, "body", "for expression")?;

    // Warn on loop variables never referenced in the body. `_` is a discard
    // marker and is skipped. The check is a scan over the body's raw text
    // for the binding name as a whole identifier — good enough because the
    // grammar already guarantees identifiers appear at token boundaries.
    let body_text = body_pair.as_str();
    for var in binding.names() {
        if var == "_" || identifier_appears_in(body_text, var) {
            continue;
        }
        ctx.warnings.push(ParseWarning {
            file: None,
            line: for_line,
            kind: WarningKind::UnusedForBinding,
            span: None,
            message: format!(
                "for-loop binding '{}' is unused. Rename to '_' to suppress this warning.",
                var
            ),
        });
    }

    let mut resources = Vec::new();
    let mut data_sources = Vec::new();
    let mut module_calls = Vec::new();

    let collect = |result: ForBodyResult,
                   expansion_key: String,
                   resources: &mut Vec<Resource>,
                   data_sources: &mut Vec<DataSource>,
                   module_calls: &mut Vec<ModuleCall>| {
        match result {
            ForBodyResult::Resource(r) => resources.push(*r),
            ForBodyResult::DataSource(d) => data_sources.push(*d),
            ForBodyResult::ModuleCall(mut c) => {
                let unexpanded_id = c.id().clone();
                c.set_expansion_key(expansion_key);
                ctx.remap_binding_use_module_call(&unexpanded_id, c.id());
                module_calls.push(c);
            }
        }
    };

    // Helper: only register non-discard bindings so `_` is never addressable.
    let bind = |c: &mut ParseContext, name: &str, v: Value| {
        if name != "_" {
            c.set_variable(name.to_string(), v);
        }
    };
    let bind_eval = |c: &mut ParseContext, name: &str, v: EvalValue| {
        if name != "_" {
            c.set_variable(name.to_string(), v);
        }
    };

    // Expand based on iterable type
    match (&binding, &iterable) {
        (ForBinding::Simple(var), Value::Concrete(ConcreteValue::List(items))) => {
            for (i, item) in items.iter().enumerate() {
                let element = ForLoopElement::Index(i);
                let expansion_key = target.element_address(&element);
                let mut iter_ctx = ctx.child_scope();
                let metadata = match structural_handle_metadata.as_ref() {
                    Some(StructuralHandleMetadata::List(items)) => items.get(i).cloned().flatten(),
                    Some(
                        StructuralHandleMetadata::Handle { .. } | StructuralHandleMetadata::Map(_),
                    )
                    | None => None,
                };
                bind_eval(
                    &mut iter_ctx,
                    var,
                    EvalValue::computed(item.clone(), metadata),
                );
                let result = parse_for_body(
                    body_pair.clone(),
                    &iter_ctx,
                    &target,
                    Some(element),
                    preprocessed_file,
                    source_file,
                )?;
                collect(
                    result,
                    expansion_key,
                    &mut resources,
                    &mut data_sources,
                    &mut module_calls,
                );
            }
        }
        (ForBinding::Indexed(idx_var, val_var), Value::Concrete(ConcreteValue::List(items))) => {
            for (i, item) in items.iter().enumerate() {
                let element = ForLoopElement::Index(i);
                let expansion_key = target.element_address(&element);
                let mut iter_ctx = ctx.child_scope();
                bind(
                    &mut iter_ctx,
                    idx_var,
                    Value::Concrete(ConcreteValue::Int(i as i64)),
                );
                let metadata = match structural_handle_metadata.as_ref() {
                    Some(StructuralHandleMetadata::List(items)) => items.get(i).cloned().flatten(),
                    Some(
                        StructuralHandleMetadata::Handle { .. } | StructuralHandleMetadata::Map(_),
                    )
                    | None => None,
                };
                bind_eval(
                    &mut iter_ctx,
                    val_var,
                    EvalValue::computed(item.clone(), metadata),
                );
                let result = parse_for_body(
                    body_pair.clone(),
                    &iter_ctx,
                    &target,
                    Some(element),
                    preprocessed_file,
                    source_file,
                )?;
                collect(
                    result,
                    expansion_key,
                    &mut resources,
                    &mut data_sources,
                    &mut module_calls,
                );
            }
        }
        (ForBinding::Map(key_var, val_var), Value::Concrete(ConcreteValue::Map(map))) => {
            // Sort keys for deterministic output
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                let val = &map[key];
                let element = ForLoopElement::Key(key.clone());
                let expansion_key = target.element_address(&element);
                let mut iter_ctx = ctx.child_scope();
                bind(
                    &mut iter_ctx,
                    key_var,
                    Value::Concrete(ConcreteValue::String(key.clone().into())),
                );
                let metadata = match structural_handle_metadata.as_ref() {
                    Some(StructuralHandleMetadata::Map(entries)) => entries.get(key).cloned(),
                    Some(
                        StructuralHandleMetadata::Handle { .. } | StructuralHandleMetadata::List(_),
                    )
                    | None => None,
                };
                bind_eval(
                    &mut iter_ctx,
                    val_var,
                    EvalValue::computed(val.clone(), metadata),
                );
                let result = parse_for_body(
                    body_pair.clone(),
                    &iter_ctx,
                    &target,
                    Some(element),
                    preprocessed_file,
                    source_file,
                )?;
                collect(
                    result,
                    expansion_key,
                    &mut resources,
                    &mut data_sources,
                    &mut module_calls,
                );
            }
        }
        // Any deferred iterable is retained as a complete expression and
        // replayed at plan/apply time. In particular, `ref ?? default` must
        // not collapse to the default before the reference snapshot is known
        // to be complete.
        (_, iterable @ Value::Deferred(_)) => {
            let iterable_surface = crate::value::format_value(iterable);
            // Build the for-expression header string
            let header = match &binding {
                ForBinding::Simple(var) => {
                    format!("for {} in {}", var, iterable_surface)
                }
                ForBinding::Indexed(idx, val) => {
                    format!("for ({}, {}) in {}", idx, val, iterable_surface)
                }
                ForBinding::Map(k, v) => {
                    format!("for {}, {} in {}", k, v, iterable_surface)
                }
            };

            let resource = parse_deferred_resource_template(
                body_pair.clone(),
                &binding,
                ctx,
                &target,
                preprocessed_file,
                source_file,
            )?;
            if let Some(resource) = resource {
                let attrs: Vec<(String, Value)> = resource
                    .attributes
                    .iter()
                    .map(|(k, value)| (k.clone(), value.clone()))
                    .collect();
                ctx.deferred_for_expressions.push(DeferredForExpression {
                    file: None,
                    line: for_line,
                    header,
                    resource_type: if resource.id.provider.is_empty() {
                        resource.id.resource_type.clone()
                    } else {
                        format!("{}.{}", resource.id.provider, resource.id.resource_type)
                    },
                    attributes: attrs,
                    target: target.deferred_target(),
                    iterable: iterable.clone(),
                    binding: binding.clone(),
                    template_resource: *resource,
                });
            }
            // Return empty — the for body produces zero concrete resources
        }
        _ => {
            // Only an identifier token that was syntactically bare *and*
            // unbound may enter the deferred identifier-diagnostic path.
            // A String produced by indexing or a function result is data,
            // even when its contents happen to be identifier-shaped.
            if let Some(s) = bare_unbound_identifier
                .as_ref()
                .or(provisional_binding.as_ref())
            {
                let header = match &binding {
                    ForBinding::Simple(var) => format!("for {} in {}", var, s),
                    ForBinding::Indexed(idx, val) => format!("for ({}, {}) in {}", idx, val, s),
                    ForBinding::Map(k, v) => format!("for {}, {} in {}", k, v, s),
                };
                let resource = parse_deferred_resource_template(
                    body_pair.clone(),
                    &binding,
                    ctx,
                    &target,
                    preprocessed_file,
                    source_file,
                )?;
                if let Some(resource) = resource {
                    let attrs: Vec<(String, Value)> = resource
                        .attributes
                        .iter()
                        .map(|(k, value)| (k.clone(), value.clone()))
                        .collect();
                    ctx.deferred_for_expressions.push(DeferredForExpression {
                        file: None,
                        line: for_line,
                        header,
                        resource_type: if resource.id.provider.is_empty() {
                            resource.id.resource_type.clone()
                        } else {
                            format!("{}.{}", resource.id.provider, resource.id.resource_type)
                        },
                        attributes: attrs,
                        target: target.deferred_target(),
                        iterable: Value::Deferred(DeferredValue::BindingRef { binding: s.clone() }),
                        binding: binding.clone(),
                        template_resource: *resource,
                    });
                }
                return Ok((resources, data_sources, module_calls));
            }
            let iterable_type = if structural_handle_metadata
                .as_ref()
                .is_some_and(StructuralHandleMetadata::is_handle)
            {
                "resource handle inside a computed value".to_string()
            } else if let Some(binding) = structural_for_binding {
                format!("for-binding '{binding}'")
            } else {
                match &iterable {
                    Value::Concrete(ConcreteValue::String(s)) => match s.diagnostic_literal() {
                        Some(s) => {
                            let (truncated, marker) =
                                crate::utils::truncate_chars_for_display(s, 50);
                            format!("string {}{marker}", display_string_literal(&truncated))
                        }
                        None => "string".to_string(),
                    },
                    Value::Concrete(ConcreteValue::EnumIdentifier(s)) => {
                        let (truncated, marker) =
                            crate::utils::truncate_chars_for_display(s.as_str(), 50);
                        format!("identifier `{truncated}`{marker}")
                    }
                    Value::Concrete(ConcreteValue::Int(i)) => format!("int {}", i),
                    Value::Concrete(ConcreteValue::Float(f)) => format!("float {}", f),
                    Value::Concrete(ConcreteValue::Bool(b)) => format!("bool {}", b),
                    Value::Concrete(ConcreteValue::Duration(d)) => {
                        format!("duration {}", crate::value::render_duration(*d))
                    }
                    Value::Deferred(DeferredValue::ResourceRef { path }) => {
                        format!("unresolved reference {}", path.to_dot_string())
                    }
                    Value::Concrete(ConcreteValue::List(_)) => "list".to_string(),
                    Value::Concrete(ConcreteValue::Map(_)) => "map".to_string(),
                    other => crate::ValueDescription::of(other).to_string(),
                }
            };
            let binding_type = match &binding {
                ForBinding::Simple(var) => format!("`for {} in ...`", var),
                ForBinding::Indexed(idx, val) => format!("`for {}, {} in ...`", idx, val),
                ForBinding::Map(k, v) => format!("`for {}, {} in ...`", k, v),
            };
            let expected = match &binding {
                ForBinding::Simple(_) | ForBinding::Indexed(_, _) => "list",
                ForBinding::Map(_, _) => "map",
            };
            return Err(ParseError::InvalidExpression {
                line: for_line,
                message: format!(
                    "{} — iterable is {} (expected {})",
                    binding_type, iterable_type, expected
                ),
            });
        }
    }

    Ok((resources, data_sources, module_calls))
}

/// Parse a for binding pattern.
///
/// Each position accepts either an `identifier` or a `discard_pattern`
/// (`_`). The text of the matched pair — either the identifier name or
/// the literal `_` — is stored as the binding name. A `_` marker is not
/// added to the parse-time scope and is exempt from the unused-binding
/// warning; downstream code checks for `name == "_"` to enforce that.
pub(crate) fn parse_for_binding(
    pair: pest::iterators::Pair<Rule>,
) -> Result<ForBinding, ParseError> {
    let inner = first_inner(pair, "binding pattern", "for binding")?;
    match inner.as_rule() {
        Rule::for_simple_binding => {
            let name = first_inner(inner, "identifier", "simple binding")?
                .as_str()
                .to_string();
            Ok(ForBinding::Simple(name))
        }
        Rule::for_indexed_binding => {
            let mut parts = inner.into_inner();
            let idx = next_pair(&mut parts, "index variable", "indexed binding")?
                .as_str()
                .to_string();
            let val = next_pair(&mut parts, "value variable", "indexed binding")?
                .as_str()
                .to_string();
            Ok(ForBinding::Indexed(idx, val))
        }
        Rule::for_map_binding => {
            let mut parts = inner.into_inner();
            let key = next_pair(&mut parts, "key variable", "map binding")?
                .as_str()
                .to_string();
            let val = next_pair(&mut parts, "value variable", "map binding")?
                .as_str()
                .to_string();
            Ok(ForBinding::Map(key, val))
        }
        _ => Err(ParseError::InternalError {
            expected: "for binding pattern".to_string(),
            context: "for expression".to_string(),
        }),
    }
}

/// Parse the iterable part of a for expression
///
/// When the iterable is a function call with all statically-known arguments,
/// the function is eagerly evaluated at parse time. If any argument depends on
/// a runtime value (e.g. ResourceRef), a clear error is returned.
struct ParsedForIterable {
    value: Value,
    bare_unbound_identifier: Option<String>,
    structural_for_binding: Option<String>,
    provisional_binding: Option<String>,
    structural_handle_metadata: Option<StructuralHandleMetadata>,
}

fn syntactic_bare_unbound_identifier(
    pair: &pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Option<String> {
    let mut iterable_parts = pair.clone().into_inner();
    let base = iterable_parts.next()?;
    if iterable_parts.next().is_some() || base.as_rule() != Rule::variable_ref {
        return None;
    }
    let mut variable_parts = base.into_inner();
    let identifier = variable_parts.next()?;
    if variable_parts.next().is_some() || ctx.contains_binding(identifier.as_str()) {
        return None;
    }
    Some(identifier.as_str().to_string())
}

fn parse_for_iterable(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ParsedForIterable, ParseError> {
    // `for_iterable` is a restricted postfix wrapper whose base is a
    // function call, list, variable reference, or parenthesized expression.
    let line = pair.line_col().0;
    let bare_unbound_identifier = syntactic_bare_unbound_identifier(&pair, ctx);
    let eval = parse_postfix_eval(pair, ctx)?;
    let structural_for_binding = eval
        .structural_handle_ref()
        .filter(|handle| handle.is_for_binding())
        .and_then(|handle| handle.addressable_binding())
        .map(str::to_string);
    let provisional_binding = eval.provisional_reference_name().map(str::to_string);
    let structural_handle_metadata = eval.structural_handle_metadata().cloned();
    let value = eval
        .into_value()
        .map_err(|error| ParseError::from_eval_value(error, line))?;
    let value = crate::parser::functions::try_evaluate_fn_value(value, ctx, line)?;
    Ok(ParsedForIterable {
        value: evaluate_static_value(value, ctx.config)?,
        bare_unbound_identifier,
        structural_for_binding,
        provisional_binding,
        structural_handle_metadata,
    })
}

/// Parse the body of a for expression and produce a single resource or module call
pub(crate) fn parse_for_body(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    target: &LoopParseTarget,
    element: Option<ForLoopElement>,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<ForBodyResult, ParseError> {
    let mut local_ctx = ctx.child_scope();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::for_local_binding => {
                let mut binding_inner = inner.into_inner();
                let name = next_pair(&mut binding_inner, "binding name", "for local binding")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut binding_inner, "binding value", "for local binding")?,
                    &local_ctx,
                )?;
                local_ctx.set_variable(name, value);
            }
            Rule::resource_expr => {
                let resource = parse_resource_for_target(
                    inner,
                    &local_ctx,
                    target.resource_target(element.clone()),
                    preprocessed_file,
                    source_file,
                )?;
                return Ok(ForBodyResult::Resource(Box::new(resource)));
            }
            Rule::read_resource_expr => {
                if let Some(error) = target.unbound_read_error(LoopKind::For) {
                    return Err(error);
                }
                let address = element.as_ref().map_or_else(
                    || format!("{}[?]", target.scheduler_key()),
                    |element| target.element_address(element),
                );
                let data_source = parse_read_resource_expr(
                    inner,
                    &local_ctx,
                    &address,
                    preprocessed_file,
                    source_file,
                )?;
                return Ok(ForBodyResult::DataSource(Box::new(data_source)));
            }
            Rule::module_call => {
                let mut call =
                    parse_module_call(inner, &local_ctx, preprocessed_file, source_file)?;
                call.binding_name = match (target, element.as_ref()) {
                    (LoopParseTarget::Bound { .. }, Some(element)) => {
                        Some(target.element_address(element))
                    }
                    (LoopParseTarget::Bound { .. }, None) => {
                        Some(format!("{}[?]", target.scheduler_key()))
                    }
                    (LoopParseTarget::Unbound(_), _) => None,
                };
                if let Some(element) = element {
                    call.set_loop_origin(target.element_origin(element));
                }
                return Ok(ForBodyResult::ModuleCall(call));
            }
            _ => {}
        }
    }

    Err(ParseError::InternalError {
        expected: "resource expression or module call".to_string(),
        context: "for body".to_string(),
    })
}
