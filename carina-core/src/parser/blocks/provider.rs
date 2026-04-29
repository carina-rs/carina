//! `provider <name> { ... }` block parser and the top-level
//! `require <expr>, "message"` statement parser.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::Rule;
use crate::parser::ast::{ProviderConfig, RequireBlock};
use crate::parser::context::{ParseContext, next_pair};
use crate::parser::error::ParseError;
use crate::parser::expressions::validate_expr::parse_validate_expr;
use crate::parser::parse_expression;
use crate::resource::Value;
use crate::version_constraint::VersionConstraint;
use indexmap::IndexMap;

pub(in crate::parser) fn parse_provider_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ProviderConfig, ParseError> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "provider name", "provider block")?
        .as_str()
        .to_string();

    let mut attributes: IndexMap<String, Value> = IndexMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key = next_pair(&mut attr_inner, "attribute name", "provider block")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut attr_inner, "attribute value", "provider block")?,
                ctx,
            )?;
            attributes.insert(key, value);
        }
    }

    // `shift_remove` keeps the surviving attributes in source order.
    // Extract default_tags from attributes if present
    let default_tags = if let Some(Value::Map(tags)) = attributes.shift_remove("default_tags") {
        tags
    } else {
        IndexMap::new()
    };

    // Extract source from attributes if present
    let source = if let Some(Value::String(s)) = attributes.shift_remove("source") {
        Some(s)
    } else {
        None
    };

    // Extract version from attributes if present
    let version = if let Some(Value::String(v)) = attributes.shift_remove("version") {
        Some(VersionConstraint::parse(&v).map_err(|e| {
            pest::error::Error::new_from_pos(
                pest::error::ErrorVariant::CustomError { message: e },
                pest::Position::from_start(""),
            )
        })?)
    } else {
        None
    };

    // Extract revision from attributes if present
    let revision = if let Some(Value::String(r)) = attributes.shift_remove("revision") {
        Some(r)
    } else {
        None
    };

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
    })
}

/// Parse a require statement: `require <validate_expr>, "error message"`
pub(in crate::parser) fn parse_require_statement(
    pair: pest::iterators::Pair<Rule>,
) -> Result<RequireBlock, ParseError> {
    let mut inner = pair.into_inner();
    let condition_pair = next_pair(&mut inner, "validate_expr", "require statement")?;
    let condition = parse_validate_expr(condition_pair)?;
    let message_pair = next_pair(&mut inner, "string", "require statement")?;
    let raw = message_pair.as_str();
    let error_message = raw[1..raw.len() - 1].to_string();
    Ok(RequireBlock {
        condition,
        error_message,
    })
}
