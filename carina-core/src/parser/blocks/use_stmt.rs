//! `use { source = "..." }` block parser (used in `let _ = use { ... }`).
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::Rule;
use crate::parser::ast::UseStatement;
use crate::parser::context::{ParseContext, next_pair};
use crate::parser::error::ParseError;
use crate::parser::parse_expression;
use crate::resource::Value;

/// Parse import expression (RHS of `let name = use { source = "path" }`)
pub(in crate::parser) fn parse_use_expr(
    pair: pest::iterators::Pair<Rule>,
    binding_name: &str,
    ctx: &ParseContext,
) -> Result<UseStatement, ParseError> {
    let span = pair.as_span();
    let line = span.start_pos().line_col().0;
    let mut source: Option<String> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() != Rule::attribute {
            continue;
        }
        let attr_span = attr.as_span();
        let attr_line = attr_span.start_pos().line_col().0;
        let mut attr_inner = attr.into_inner();
        let key = next_pair(&mut attr_inner, "attribute name", "use expression")?
            .as_str()
            .to_string();
        let value_pair = next_pair(&mut attr_inner, "attribute value", "use expression")?;
        if key != "source" {
            return Err(ParseError::InvalidExpression {
                line: attr_line,
                message: format!("`use` block only accepts a `source` attribute, got `{key}`"),
            });
        }
        let value = parse_expression(value_pair, ctx)?;
        let Value::String(path) = value else {
            return Err(ParseError::InvalidExpression {
                line: attr_line,
                message: "`use` block `source` must be a string literal".to_string(),
            });
        };
        if source.is_some() {
            return Err(ParseError::InvalidExpression {
                line: attr_line,
                message: "`use` block has more than one `source` attribute".to_string(),
            });
        }
        source = Some(path);
    }

    let path = source.ok_or_else(|| ParseError::InvalidExpression {
        line,
        message: "`use` block must have a `source` attribute".to_string(),
    })?;

    Ok(UseStatement {
        path,
        alias: binding_name.to_string(),
    })
}
