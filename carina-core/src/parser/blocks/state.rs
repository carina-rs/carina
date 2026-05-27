//! `import { ... }`, `removed { ... }`, and `moved { ... }` block parsers.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::ParseContext;
use crate::parser::Rule;
use crate::parser::ast::{StateBlock, StateBlockAddress};
use crate::parser::context::first_inner;
use crate::parser::error::ParseError;
use crate::parser::expressions::string_literal::parse_string_value;
use crate::parser::util::parse_state_block_address;

/// Parse an import state block
pub(in crate::parser) fn parse_import_state_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<StateBlock, ParseError> {
    let mut to: Option<StateBlockAddress> = None;
    let mut id: Option<crate::resource::Value> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::import_state_attr {
            let inner = first_inner(attr, "import attribute", "import block")?;
            match inner.as_rule() {
                Rule::import_to_attr => {
                    let addr = first_inner(inner, "resource address", "import to")?;
                    to = Some(parse_state_block_address(addr)?);
                }
                Rule::import_id_attr => {
                    // carina#3329: parse via `parse_string_value` (not
                    // `parse_string_literal`) so `"${binding.attr}|..."`
                    // keeps its `${...}` segments as deferred `Expr`
                    // parts. The pre-#3329 path called
                    // `parse_string_literal`, which concatenated only the
                    // raw literal segments and silently dropped every
                    // interpolation — yielding a partially-substituted
                    // string that the plan displayed as if it were the
                    // real cloud identifier.
                    let str_pair = first_inner(inner, "string", "import id")?;
                    id = Some(parse_string_value(str_pair, ctx)?);
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
    let mut from: Option<StateBlockAddress> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::removed_attr {
            let addr = first_inner(attr, "resource address", "removed from")?;
            from = Some(parse_state_block_address(addr)?);
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
    let mut from: Option<StateBlockAddress> = None;
    let mut to: Option<StateBlockAddress> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::moved_attr {
            let inner = first_inner(attr, "moved attribute", "moved block")?;
            match inner.as_rule() {
                Rule::moved_from_attr => {
                    let addr = first_inner(inner, "resource address", "moved from")?;
                    from = Some(parse_state_block_address(addr)?);
                }
                Rule::moved_to_attr => {
                    let addr = first_inner(inner, "resource address", "moved to")?;
                    to = Some(parse_state_block_address(addr)?);
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
