//! `provider <name> { ... }` block parser and the top-level
//! `require <expr>, "message"` statement parser.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::ParsedValue as Value;
use crate::parser::Rule;
use crate::parser::context::{ParseContext, next_pair};
use crate::parser::error::ParseError;
use crate::parser::expressions::validate_expr::parse_validate_expr;
use crate::parser::parse_expression;
use crate::parser::util::expression_is_plain_string_literal;
use crate::parser::{ParserProviderConfig as ProviderConfig, ParserRequireBlock as RequireBlock};
use crate::resource::{ConcreteValue, ValueString};
use crate::version_constraint::VersionConstraint;
use indexmap::IndexMap;

fn take_provider_string_attribute(
    attributes: &mut IndexMap<String, Value>,
    attribute_lines: &IndexMap<String, usize>,
    provider_name: &str,
    attribute: &str,
) -> Result<Option<(ValueString, usize)>, ParseError> {
    let Some(value) = attributes.shift_remove(attribute) else {
        return Ok(None);
    };
    let line =
        attribute_lines
            .get(attribute)
            .copied()
            .ok_or_else(|| ParseError::InternalError {
                expected: format!("source line for provider attribute '{attribute}'"),
                context: format!("provider '{provider_name}' block"),
            })?;

    match value {
        Value::Concrete(ConcreteValue::String(value)) => Ok(Some((value, line))),
        Value::Concrete(_) | Value::Deferred(_) => Err(ParseError::InvalidExpression {
            line,
            message: format!("Provider '{provider_name}': '{attribute}' must be a string literal"),
        }),
    }
}

pub(in crate::parser) fn parse_provider_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ProviderConfig, ParseError> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "provider name", "provider block")?
        .as_str()
        .to_string();

    let mut attributes: IndexMap<String, Value> = IndexMap::new();
    let mut attribute_lines: IndexMap<String, usize> = IndexMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let (attr_line, _) = attr_pair.as_span().start_pos().line_col();
            let mut attr_inner = attr_pair.into_inner();
            let key = next_pair(&mut attr_inner, "attribute name", "provider block")?
                .as_str()
                .to_string();
            let value_pair = next_pair(&mut attr_inner, "attribute value", "provider block")?;
            // Source classification is syntactic. Version and revision retain
            // evaluated-string support, but non-string results are rejected below.
            if key == "source" && !expression_is_plain_string_literal(value_pair.clone()) {
                return Err(ParseError::InvalidExpression {
                    line: attr_line,
                    message: format!("Provider '{name}': '{key}' must be a string literal"),
                });
            }
            let value = parse_expression(value_pair, ctx)?;
            attribute_lines.insert(key.clone(), attr_line);
            attributes.insert(key, value);
        }
    }

    // `shift_remove` keeps the surviving attributes in source order.
    // Extract default_tags from attributes if present. Non-literal values
    // cannot be peeled at parse time — route them into `unresolved_attributes`
    // for the post-resolver finalize step. Otherwise they would silently
    // fall through to an empty map (#2717).
    let mut unresolved_attributes: IndexMap<String, Value> = IndexMap::new();
    let default_tags = match attributes.shift_remove("default_tags") {
        Some(Value::Concrete(ConcreteValue::Map(tags))) => tags,
        Some(other) => {
            unresolved_attributes.insert("default_tags".to_string(), other);
            IndexMap::new()
        }
        None => IndexMap::new(),
    };

    // Extract source from attributes if present
    let source =
        match take_provider_string_attribute(&mut attributes, &attribute_lines, &name, "source")? {
            Some((source, line)) => Some(source.into_plaintext().parse().map_err(|error| {
                ParseError::InvalidExpression {
                    line,
                    message: format!("Provider '{name}': {error}"),
                }
            })?),
            None => None,
        };

    // Extract version from attributes if present
    let version = match take_provider_string_attribute(
        &mut attributes,
        &attribute_lines,
        &name,
        "version",
    )? {
        Some((version, line)) => Some(
            VersionConstraint::parse(version.expose_plaintext()).map_err(|error| {
                let message = match version.diagnostic_literal() {
                    Some(_) => format!("Provider '{name}': {error}"),
                    None => {
                        format!("Provider '{name}': Invalid version constraint: value from env()")
                    }
                };
                ParseError::InvalidExpression { line, message }
            })?,
        ),
        None => None,
    };

    // Extract revision from attributes if present
    let revision =
        take_provider_string_attribute(&mut attributes, &attribute_lines, &name, "revision")?
            .map(|(revision, _)| revision.into_plaintext());

    // Validate that version and revision are mutually exclusive
    if version.is_some() && revision.is_some() {
        return Err(ParseError::Syntax(pest::error::Error::new_from_pos(
            pest::error::ErrorVariant::CustomError {
                message: format!(
                    "Provider '{}': 'version' and 'revision' are mutually exclusive",
                    name
                ),
            },
            pest::Position::from_start(""),
        )));
    }

    Ok(ProviderConfig {
        name,
        attributes,
        default_tags,
        source,
        version,
        revision,
        unresolved_attributes,
        binding: None,
    })
}

/// Parse a `provider <kind> { ... }` expression used as the RHS of a
/// `let` binding. `source` / `version` / `revision` describe **how to
/// load** the provider plugin and are properties of the kind, not of
/// an individual instance — declaring them on a named instance would
/// invite divergent plugin versions per instance, so they are rejected
/// here in favour of the single top-level `provider <kind>` block.
pub(in crate::parser) fn parse_provider_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<ProviderConfig, ParseError> {
    let mut config = parse_provider_block(pair, ctx)?;

    let kind_level_present: &[(&str, bool)] = &[
        ("source", config.source.is_some()),
        ("version", config.version.is_some()),
        ("revision", config.revision.is_some()),
    ];
    for (field, present) in kind_level_present {
        if *present {
            return Err(reject_kind_level_attr(field, binding_name, &config.name));
        }
    }

    config.binding = Some(binding_name.to_string());
    Ok(config)
}

fn reject_kind_level_attr(field: &str, binding: &str, kind: &str) -> ParseError {
    ParseError::Syntax(pest::error::Error::new_from_pos(
        pest::error::ErrorVariant::CustomError {
            message: format!(
                "'{field}' is a kind-level attribute and cannot be set on \
                 a named provider instance '{binding}'. Move it to the \
                 top-level `provider {kind}` block."
            ),
        },
        pest::Position::from_start(""),
    ))
}

/// Parse a require statement: `require <validate_expr>, "error message"`
pub(in crate::parser) fn parse_require_statement(
    pair: pest::iterators::Pair<Rule>,
    ctx: &crate::parser::ParseContext,
    preprocessed_file: &crate::parser::PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<RequireBlock, ParseError> {
    let mut inner = pair.into_inner();
    let condition_pair = next_pair(&mut inner, "validate_expr", "require statement")?;
    // Constraint diagnostics belong to the offending expression, not to the
    // beginning of a potentially multi-line `require` statement.
    let source_span = crate::parser::SourceSpan::from_pest(&condition_pair, preprocessed_file);
    let condition = parse_validate_expr(condition_pair, ctx)?;
    let message_pair = next_pair(&mut inner, "string", "require statement")?;
    let raw = message_pair.as_str();
    let error_message = raw[1..raw.len() - 1].to_string();
    Ok(RequireBlock::new_in_source(
        condition,
        error_message,
        source_span,
        source_file.to_path_buf(),
    ))
}
