//! Module-call parser: `module_name { arg = value, ... }`.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::Rule;
use crate::parser::ast::ModuleCall;
use crate::parser::context::{ParseContext, next_pair};
use crate::parser::error::ParseError;
use crate::parser::parse_expression;
use std::collections::HashMap;

/// Parse module call
pub(crate) fn parse_module_call(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ModuleCall, ParseError> {
    let span = pair.as_span();
    let mut inner = pair.into_inner();
    let module_name = next_pair(&mut inner, "module name", "module call")?
        .as_str()
        .to_string();

    if module_name == "remote_state" {
        return Err(ParseError::InvalidExpression {
            line: span.start_pos().line_col().0,
            message: "`remote_state` has been replaced by `let <binding> = upstream_state { source = \"...\" }`".to_string(),
        });
    }

    let mut arguments = HashMap::new();
    for arg in inner {
        if arg.as_rule() == Rule::module_call_arg {
            let mut arg_inner = arg.into_inner();
            let key = next_pair(&mut arg_inner, "argument name", "module call argument")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut arg_inner, "argument value", "module call argument")?,
                ctx,
            )?;
            arguments.insert(key, value);
        }
    }

    Ok(ModuleCall {
        module_name,
        binding_name: None,
        arguments,
    })
}
