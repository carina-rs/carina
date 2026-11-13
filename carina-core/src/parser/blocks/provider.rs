//! `provider <name> { ... }` block parser and the top-level
//! `require <expr>, "message"` statement parser.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::ParsedValue as Value;
use crate::parser::Rule;
use crate::parser::context::{ParseContext, next_pair};
use crate::parser::error::ParseError;
use crate::parser::expressions::validate_expr::parse_validate_expr;
use crate::parser::grammar::{ParserBindingName, ProviderKindAttribute, ProviderSyntaxIssue};
use crate::parser::parse_expression;
use crate::parser::util::expression_is_plain_string_literal;
use crate::parser::{ParserProviderConfig as ProviderConfig, ParserRequireBlock as RequireBlock};
use crate::resource::{ConcreteValue, ValueString};
use crate::version_constraint::VersionConstraint;
use indexmap::IndexMap;

fn take_provider_string_attribute(
    attributes: &mut crate::parser::ConfigurationAttributes<Value>,
    attribute_lines: &IndexMap<crate::parser::AttributeKey, usize>,
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

    let known = value
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .ok()
        .and_then(|value| value.into_known().ok());
    known
        .as_ref()
        .and_then(provider_string)
        .map(|value| Some((value, line)))
        .ok_or_else(|| ParseError::InvalidExpression {
            line,
            message: format!("Provider '{provider_name}': '{attribute}' must be a string literal"),
        })
}

fn provider_string(
    value: &crate::resource::knowledge::KnownValue<
        '_,
        crate::resource::ParserPlaceholder,
        crate::resource::RelativeBindingName,
    >,
) -> Option<ValueString> {
    value.visit(
        |value| match value {
            crate::resource::knowledge::KnownView::String(value) => Some(value.clone()),
            _ => None,
        },
        |_| None,
    )
}

pub(in crate::parser) fn parse_provider_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ProviderConfig, ParseError> {
    let mut inner = pair.into_inner();
    let provider = ParserBindingName::from_identifier_pair(next_pair(
        &mut inner,
        "provider name",
        "provider block",
    )?);
    let name = provider.as_str().to_owned();

    let mut attributes = crate::parser::ConfigurationAttributes::new();
    let mut attribute_lines: IndexMap<crate::parser::AttributeKey, usize> = IndexMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let (attr_line, _) = attr_pair.as_span().start_pos().line_col();
            let mut attr_inner = attr_pair.into_inner();
            let key = ctx.authored_key(next_pair(
                &mut attr_inner,
                "attribute name",
                "provider block",
            )?);
            let value_pair = next_pair(&mut attr_inner, "attribute value", "provider block")?;
            // Source classification is syntactic. Version and revision retain
            // evaluated-string support, but non-string results are rejected below.
            if key.matches("source") && !expression_is_plain_string_literal(value_pair.clone()) {
                return Err(ParseError::InvalidExpression {
                    line: attr_line,
                    message: format!("Provider '{name}': '{key}' must be a string literal"),
                });
            }
            let value = parse_expression(value_pair, ctx)?
                .projection()
                .expect("materialized parser expression")
                .clone();
            attribute_lines.insert(key.clone(), attr_line);
            attributes.insert(key, value);
        }
    }

    // `shift_remove` keeps the surviving attributes in source order.
    // Extract default_tags from attributes if present. Non-literal values
    // cannot be peeled at parse time — route them into `unresolved_attributes`
    // for the post-resolver finalize step. Otherwise they would silently
    // fall through to an empty map (#2717).
    let mut unresolved_attributes = crate::parser::ConfigurationAttributes::new();
    let default_tags = match attributes.shift_remove("default_tags") {
        Some(Value::Concrete(ConcreteValue::Map(tags))) => {
            crate::parser::ConfigurationAttributes::from_literal_map(tags)
        }
        Some(other) => {
            unresolved_attributes.insert("default_tags".to_string(), other);
            crate::parser::ConfigurationAttributes::new()
        }
        None => crate::parser::ConfigurationAttributes::new(),
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
        return Err(ProviderSyntaxIssue::VersionAndRevision {
            provider: &provider,
        }
        .into_parse_error());
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
    binding_name: &ParserBindingName,
) -> Result<ProviderConfig, ParseError> {
    let provider = ParserBindingName::from_identifier_pair(next_pair(
        &mut pair.clone().into_inner(),
        "provider name",
        "provider expression",
    )?);
    let mut config = parse_provider_block(pair, ctx)?;

    let kind_level_present = [
        (ProviderKindAttribute::Source, config.source.is_some()),
        (ProviderKindAttribute::Version, config.version.is_some()),
        (ProviderKindAttribute::Revision, config.revision.is_some()),
    ];
    for (field, present) in kind_level_present {
        if present {
            return Err(ProviderSyntaxIssue::NamedKindAttribute {
                field,
                binding: binding_name,
                provider: &provider,
            }
            .into_parse_error());
        }
    }

    config.binding = Some(binding_name.as_str().to_owned());
    Ok(config)
}

/// Parse a require statement: `require <validate_expr>, "error message"`
pub(in crate::parser) fn parse_require_statement(
    pair: pest::iterators::Pair<Rule>,
    ctx: &crate::parser::ParseContext,
    preprocessed_file: &crate::parser::PreprocessedFile,
    _source_file: &std::path::Path,
) -> Result<RequireBlock, ParseError> {
    let mut inner = pair.into_inner();
    let condition_pair = next_pair(&mut inner, "validate_expr", "require statement")?;
    // Constraint diagnostics belong to the offending expression, not to the
    // beginning of a potentially multi-line `require` statement.
    let source_span = crate::parser::SourceSpan::from_pest(&condition_pair, preprocessed_file);
    let location = ctx.declaration_location(&condition_pair);
    let condition_id = ctx
        .authored_node_id(&condition_pair)
        .expect("retained require condition");
    let condition = parse_validate_expr(
        condition_pair,
        ctx,
        crate::parser::ConstraintStringPosition::RequireCondition,
    )?;
    let message_pair = next_pair(&mut inner, "string", "require statement")?;
    let error_message = crate::parser::expressions::validate_expr::parse_constraint_string(
        message_pair,
        ctx,
        crate::parser::ConstraintStringPosition::RequireMessage,
    )?;
    let declaration = RequireBlock::new_in_source(condition, error_message, source_span, location);
    ctx.retain_require(condition_id, declaration.clone());
    Ok(declaration)
}
