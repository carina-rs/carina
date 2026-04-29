//! `import { ... }`, `removed { ... }`, and `moved { ... }` block parsers.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::Rule;
use crate::parser::ast::StateBlock;
use crate::parser::context::first_inner;
use crate::parser::error::ParseError;
use crate::parser::expressions::string_literal::parse_string_literal;
use crate::parser::util::parse_resource_address;
use crate::resource::ResourceId;

/// Parse an import state block
pub(in crate::parser) fn parse_import_state_block(
    pair: pest::iterators::Pair<Rule>,
) -> Result<StateBlock, ParseError> {
    let mut to: Option<ResourceId> = None;
    let mut id: Option<String> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::import_state_attr {
            let inner = first_inner(attr, "import attribute", "import block")?;
            match inner.as_rule() {
                Rule::import_to_attr => {
                    let addr = first_inner(inner, "resource address", "import to")?;
                    to = Some(parse_resource_address(addr)?);
                }
                Rule::import_id_attr => {
                    let str_pair = first_inner(inner, "string", "import id")?;
                    id = Some(parse_string_literal(str_pair)?);
                }
                _ => {}
            }
        }
    }

    let to = to.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "import block requires 'to' attribute".to_string(),
    })?;
    let id = id.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "import block requires 'id' attribute".to_string(),
    })?;

    Ok(StateBlock::Import { to, id })
}

/// Parse a removed block
pub(in crate::parser) fn parse_removed_block(
    pair: pest::iterators::Pair<Rule>,
) -> Result<StateBlock, ParseError> {
    let mut from: Option<ResourceId> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::removed_attr {
            let addr = first_inner(attr, "resource address", "removed from")?;
            from = Some(parse_resource_address(addr)?);
        }
    }

    let from = from.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "removed block requires 'from' attribute".to_string(),
    })?;

    Ok(StateBlock::Removed { from })
}

/// Parse a moved block
pub(in crate::parser) fn parse_moved_block(
    pair: pest::iterators::Pair<Rule>,
) -> Result<StateBlock, ParseError> {
    let mut from: Option<ResourceId> = None;
    let mut to: Option<ResourceId> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::moved_attr {
            let inner = first_inner(attr, "moved attribute", "moved block")?;
            match inner.as_rule() {
                Rule::moved_from_attr => {
                    let addr = first_inner(inner, "resource address", "moved from")?;
                    from = Some(parse_resource_address(addr)?);
                }
                Rule::moved_to_attr => {
                    let addr = first_inner(inner, "resource address", "moved to")?;
                    to = Some(parse_resource_address(addr)?);
                }
                _ => {}
            }
        }
    }

    let from = from.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "moved block requires 'from' attribute".to_string(),
    })?;
    let to = to.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "moved block requires 'to' attribute".to_string(),
    })?;

    Ok(StateBlock::Moved { from, to })
}
