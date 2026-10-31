//! `backend "<type>" { ... }` and `upstream_state { source = "..." }`
//! parsers.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::ParserBackendConfig as BackendConfig;
use crate::parser::Rule;
use crate::parser::ast::{SourceSpan, UpstreamState};
use crate::parser::context::{ParseContext, first_inner, next_pair};
use crate::parser::error::ParseError;
use crate::parser::expressions::string_literal::parse_string_literal;
use crate::parser::grammar::PreprocessedFile;
use crate::parser::parse_expression;
use crate::parser::util::unwrap_to_primary;
use std::collections::HashMap;

pub(in crate::parser) fn parse_backend_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    preprocessed_file: &PreprocessedFile,
) -> Result<BackendConfig, ParseError> {
    let mut inner = pair.into_inner();
    let backend_type = next_pair(&mut inner, "backend type", "backend block")?
        .as_str()
        .to_string();

    let mut attributes = HashMap::new();
    let mut attribute_spans = HashMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key_pair = next_pair(&mut attr_inner, "attribute name", "backend block")?;
            let key_span = SourceSpan::from_pest(&key_pair, preprocessed_file);
            let key = key_pair.as_str().to_string();
            let value = parse_expression(
                next_pair(&mut attr_inner, "attribute value", "backend block")?,
                ctx,
            )?;
            attribute_spans.insert(key.clone(), key_span);
            attributes.insert(key, value);
        }
    }

    Ok(BackendConfig {
        backend_type,
        attributes,
        attribute_spans,
    })
}

/// Parse an `upstream_state { source = "<dir>" }` expression.
///
/// The binding name comes from the enclosing `let` binding, so it's passed in
/// rather than extracted from the expression itself.
pub(in crate::parser) fn parse_upstream_state_expr(
    pair: pest::iterators::Pair<Rule>,
    binding_name: &str,
) -> Result<UpstreamState, ParseError> {
    let (block_line, _) = pair.as_span().start_pos().line_col();
    let inner = pair.into_inner();

    let mut source: Option<String> = None;
    for attr_pair in inner {
        if attr_pair.as_rule() != Rule::attribute {
            continue;
        }
        let (attr_line, _) = attr_pair.as_span().start_pos().line_col();
        let mut attr_inner = attr_pair.into_inner();
        let key = next_pair(
            &mut attr_inner,
            "attribute name",
            "upstream_state expression",
        )?
        .as_str()
        .to_string();
        let value_pair = next_pair(
            &mut attr_inner,
            "attribute value",
            "upstream_state expression",
        )?;
        match key.as_str() {
            "source" => {
                let value_text = value_pair.as_str().to_string();
                let invalid_source = |without_interpolation: bool| {
                    let requirement = if without_interpolation {
                        "a string literal without interpolation"
                    } else {
                        "a string literal"
                    };
                    ParseError::InvalidExpression {
                        line: attr_line,
                        message: format!(
                            "upstream_state '{}': 'source' must be {}, got: {}",
                            binding_name, requirement, value_text
                        ),
                    }
                };
                let primary = unwrap_to_primary(value_pair).ok_or_else(|| invalid_source(false))?;
                let value = first_inner(primary, "string literal", "upstream_state source")?;
                if value.as_rule() != Rule::string {
                    return Err(invalid_source(false));
                }
                source = Some(parse_string_literal(value).map_err(|error| {
                    let without_interpolation = matches!(
                        error,
                        ParseError::InterpolationNotAllowedInLiteralString { .. }
                    );
                    invalid_source(without_interpolation)
                })?);
            }
            other => {
                return Err(ParseError::InvalidExpression {
                    line: attr_line,
                    message: format!(
                        "unknown attribute '{}' in upstream_state '{}' expression",
                        other, binding_name
                    ),
                });
            }
        }
    }

    let source = source.ok_or_else(|| ParseError::InvalidExpression {
        line: block_line,
        message: format!(
            "upstream_state '{}' requires a 'source' attribute",
            binding_name
        ),
    })?;

    Ok(UpstreamState {
        binding: binding_name.to_string(),
        source: std::path::PathBuf::from(source),
    })
}
