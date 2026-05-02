//! `for` expression parsing.
//!
//! Each `for` expression expands at parse time into a vector of concrete
//! resources (or module calls) addressed by the loop's index/key. When
//! the iterable depends on an upstream value the expansion is deferred —
//! the parse pass records a [`crate::parser::DeferredForExpression`] so
//! a later pass can finish the expansion once the upstream value
//! resolves.

use crate::parser::expressions::primary::parse_primary_eval;
use crate::parser::{
    DEFERRED_UPSTREAM_INDEX_PLACEHOLDER, DEFERRED_UPSTREAM_KEY_PLACEHOLDER,
    DEFERRED_UPSTREAM_PLACEHOLDER, DeferredForExpression, ModuleCall, ParseContext, ParseError,
    ParseWarning, Rule, evaluate_static_value, first_inner, next_pair, parse_expression,
    parse_module_call, parse_read_resource_expr, parse_resource_expr,
};
use crate::resource::{Resource, Value};

/// Binding pattern for a for expression
#[derive(Debug, Clone, PartialEq)]
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

/// Whether `s` is shaped like a bare Carina identifier — the first byte is
/// `A-Za-z_` and the rest are `A-Za-z0-9_`. Used to recover from the
/// parser's collapse of unresolved identifiers into `Value::String(s)`
/// when we need to decide whether to render an error as "identifier" vs
/// "string literal". See #2101.
fn is_bare_identifier(s: &str) -> bool {
    let mut bytes = s.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Result of parsing a for expression body: either a resource or a module call
pub(crate) enum ForBodyResult {
    Resource(Box<Resource>),
    ModuleCall(ModuleCall),
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
pub(crate) fn parse_for_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<(Vec<Resource>, Vec<ModuleCall>), ParseError> {
    let for_line = pair.as_span().start_pos().line_col().0;
    let mut inner = pair.into_inner();

    // Parse the binding pattern
    let binding_pair = next_pair(&mut inner, "for binding", "for expression")?;
    let binding = parse_for_binding(binding_pair)?;

    // Parse the iterable expression
    let iterable_pair = next_pair(&mut inner, "iterable", "for expression")?;
    let iterable = parse_for_iterable(iterable_pair, ctx)?;

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
            message: format!(
                "for-loop binding '{}' is unused. Rename to '_' to suppress this warning.",
                var
            ),
        });
    }

    let mut resources = Vec::new();
    let mut module_calls = Vec::new();

    let collect = |result: ForBodyResult,
                   resources: &mut Vec<Resource>,
                   module_calls: &mut Vec<ModuleCall>| {
        match result {
            ForBodyResult::Resource(r) => resources.push(*r),
            ForBodyResult::ModuleCall(c) => module_calls.push(c),
        }
    };

    // Helper: only register non-discard bindings so `_` is never addressable.
    let bind = |c: &mut ParseContext, name: &str, v: Value| {
        if name != "_" {
            c.set_variable(name.to_string(), v);
        }
    };

    // Expand based on iterable type
    match (&binding, &iterable) {
        (ForBinding::Simple(var), Value::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                let address = format!("{}[{}]", binding_name, i);
                let mut iter_ctx = ctx.clone();
                bind(&mut iter_ctx, var, item.clone());
                let result = parse_for_body(body_pair.clone(), &iter_ctx, &address)?;
                collect(result, &mut resources, &mut module_calls);
            }
        }
        (ForBinding::Indexed(idx_var, val_var), Value::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                let address = format!("{}[{}]", binding_name, i);
                let mut iter_ctx = ctx.clone();
                bind(&mut iter_ctx, idx_var, Value::Int(i as i64));
                bind(&mut iter_ctx, val_var, item.clone());
                let result = parse_for_body(body_pair.clone(), &iter_ctx, &address)?;
                collect(result, &mut resources, &mut module_calls);
            }
        }
        (ForBinding::Map(key_var, val_var), Value::Map(map)) => {
            // Sort keys for deterministic output
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                let val = &map[key];
                let address = crate::utils::map_key_address(binding_name, key);
                let mut iter_ctx = ctx.clone();
                bind(&mut iter_ctx, key_var, Value::String(key.clone()));
                bind(&mut iter_ctx, val_var, val.clone());
                let result = parse_for_body(body_pair.clone(), &iter_ctx, &address)?;
                collect(result, &mut resources, &mut module_calls);
            }
        }
        // Unresolved reference — defer expansion to plan/apply when the
        // upstream values are loaded. Field validity is checked statically
        // by `upstream_exports::check_upstream_state_field_references`; for
        // a valid field the deferral is an implementation detail the user
        // doesn't need to hear about at validate time.
        (_, Value::ResourceRef { path }) => {
            // Build the for-expression header string
            let header = match &binding {
                ForBinding::Simple(var) => {
                    format!("for {} in {}", var, path.to_dot_string())
                }
                ForBinding::Indexed(idx, val) => {
                    format!("for ({}, {}) in {}", idx, val, path.to_dot_string())
                }
                ForBinding::Map(k, v) => {
                    format!("for {}, {} in {}", k, v, path.to_dot_string())
                }
            };

            // Try to parse the body once with placeholder values for the loop
            // variable(s) to extract the resource type and attribute template.
            let mut template_ctx = ctx.clone();
            let placeholder = || Value::String(DEFERRED_UPSTREAM_PLACEHOLDER.to_string());
            let bind = |c: &mut ParseContext, name: &str, v: Value| {
                if name != "_" {
                    c.set_variable(name.to_string(), v);
                }
            };
            match &binding {
                ForBinding::Simple(var) => {
                    bind(&mut template_ctx, var, placeholder());
                }
                ForBinding::Indexed(idx, val) => {
                    bind(
                        &mut template_ctx,
                        idx,
                        Value::String(DEFERRED_UPSTREAM_INDEX_PLACEHOLDER.to_string()),
                    );
                    bind(&mut template_ctx, val, placeholder());
                }
                ForBinding::Map(k, v) => {
                    bind(
                        &mut template_ctx,
                        k,
                        Value::String(DEFERRED_UPSTREAM_KEY_PLACEHOLDER.to_string()),
                    );
                    bind(&mut template_ctx, v, placeholder());
                }
            }

            let address = format!("{}[?]", binding_name);
            if let Ok(ForBodyResult::Resource(resource)) =
                parse_for_body(body_pair, &template_ctx, &address)
            {
                let attrs: Vec<(String, Value)> = resource
                    .attributes
                    .iter()
                    .filter(|(k, _)| !k.starts_with('_'))
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
                    binding_name: binding_name.to_string(),
                    iterable_binding: path.binding().to_string(),
                    iterable_attr: path.attribute().to_string(),
                    binding: binding.clone(),
                    template_resource: *resource,
                });
            }
            // Return empty — the for body produces zero concrete resources
        }
        _ => {
            // Special case: the parser collapses bare unresolved identifiers
            // (e.g. `for _ in org { ... }`) into `Value::String("org")` — the
            // same slot a quoted literal uses. Reporting those as
            // `iterable is string "org"` is misleading: the user wrote an
            // identifier, not a literal, and the likely fault is a typo for
            // a known binding. Record them as deferred for-expressions so
            // `check_identifier_scope` (which runs on the merged
            // directory-wide ParsedFile, so cross-file upstream_state /
            // module bindings are visible) can emit a proper
            // UndefinedIdentifier with the did-you-mean machinery from
            // #2038 / #2100. See #2101 / #2138.
            if let Value::String(s) = &iterable
                && is_bare_identifier(s)
            {
                let header = match &binding {
                    ForBinding::Simple(var) => format!("for {} in {}", var, s),
                    ForBinding::Indexed(idx, val) => format!("for ({}, {}) in {}", idx, val, s),
                    ForBinding::Map(k, v) => format!("for {}, {} in {}", k, v, s),
                };
                let mut template_ctx = ctx.clone();
                let placeholder = || Value::String(DEFERRED_UPSTREAM_PLACEHOLDER.to_string());
                let bind = |c: &mut ParseContext, name: &str, v: Value| {
                    if name != "_" {
                        c.set_variable(name.to_string(), v);
                    }
                };
                match &binding {
                    ForBinding::Simple(var) => {
                        bind(&mut template_ctx, var, placeholder());
                    }
                    ForBinding::Indexed(idx, val) => {
                        bind(
                            &mut template_ctx,
                            idx,
                            Value::String(DEFERRED_UPSTREAM_INDEX_PLACEHOLDER.to_string()),
                        );
                        bind(&mut template_ctx, val, placeholder());
                    }
                    ForBinding::Map(k, v) => {
                        bind(
                            &mut template_ctx,
                            k,
                            Value::String(DEFERRED_UPSTREAM_KEY_PLACEHOLDER.to_string()),
                        );
                        bind(&mut template_ctx, v, placeholder());
                    }
                }
                let address = format!("{}[?]", binding_name);
                if let Ok(ForBodyResult::Resource(resource)) =
                    parse_for_body(body_pair, &template_ctx, &address)
                {
                    let attrs: Vec<(String, Value)> = resource
                        .attributes
                        .iter()
                        .filter(|(k, _)| !k.starts_with('_'))
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
                        binding_name: binding_name.to_string(),
                        iterable_binding: s.clone(),
                        iterable_attr: String::new(),
                        binding: binding.clone(),
                        template_resource: *resource,
                    });
                }
                return Ok((resources, module_calls));
            }
            let iterable_type = match &iterable {
                Value::String(s) => {
                    format!("string \"{}\"", if s.len() > 50 { &s[..50] } else { s })
                }
                Value::Int(i) => format!("int {}", i),
                Value::Float(f) => format!("float {}", f),
                Value::Bool(b) => format!("bool {}", b),
                Value::ResourceRef { path } => {
                    format!("unresolved reference {}", path.to_dot_string())
                }
                Value::List(_) => "list".to_string(),
                Value::Map(_) => "map".to_string(),
                other => format!("{:?}", other),
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

    Ok((resources, module_calls))
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
pub(crate) fn parse_for_iterable(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    // for_iterable contains function_call | list | variable_ref | "(" expression ")"
    let inner = first_inner(pair, "iterable expression", "for iterable")?;
    let eval = parse_primary_eval(inner, ctx)?;
    let value = eval
        .into_value()
        .map_err(|leak| ParseError::InvalidExpression {
            line: 0,
            message: format!(
                "for iterable evaluates to a closure '{}' (still needs {} arg(s)); \
             closures cannot be iterated",
                leak.name, leak.remaining_arity
            ),
        })?;
    evaluate_static_value(value, ctx.config)
}

/// Parse the body of a for expression and produce a single resource or module call
pub(crate) fn parse_for_body(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    address: &str,
) -> Result<ForBodyResult, ParseError> {
    let mut local_ctx = ctx.clone();

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
                let resource = parse_resource_expr(inner, &local_ctx, address)?;
                return Ok(ForBodyResult::Resource(Box::new(resource)));
            }
            Rule::read_resource_expr => {
                let resource = parse_read_resource_expr(inner, &local_ctx, address)?;
                return Ok(ForBodyResult::Resource(Box::new(resource)));
            }
            Rule::module_call => {
                let mut call = parse_module_call(inner, &local_ctx)?;
                call.binding_name = Some(address.to_string());
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
