//! Module-call parser: `module_name { arg = value, ... }`.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::Rule;
use crate::parser::ast::{ModuleCall, ModuleCallSource, ModuleCallSourceSpan};
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
    let (start_line, start_column) = span.start_pos().line_col();
    let (end_line, end_column) = span.end_pos().line_col();
    let call_span = ModuleCallSourceSpan {
        start_byte: span.start(),
        end_byte: span.end(),
        start_line,
        start_column,
        end_line,
        end_column,
    };
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
    let mut argument_spans = HashMap::new();
    for arg in inner {
        if arg.as_rule() == Rule::module_call_arg {
            let mut arg_inner = arg.into_inner();
            let key_pair = next_pair(&mut arg_inner, "argument name", "module call argument")?;
            let key_span = key_pair.as_span();
            let (start_line, start_column) = key_span.start_pos().line_col();
            let (end_line, end_column) = key_span.end_pos().line_col();
            let key = key_pair.as_str().to_string();
            let value = parse_expression(
                next_pair(&mut arg_inner, "argument value", "module call argument")?,
                ctx,
            )?;
            argument_spans.insert(
                key.clone(),
                ModuleCallSourceSpan {
                    start_byte: key_span.start(),
                    end_byte: key_span.end(),
                    start_line,
                    start_column,
                    end_line,
                    end_column,
                },
            );
            arguments.insert(key, value);
        }
    }

    Ok(
        ModuleCall::synthetic(module_name, None, arguments).with_source(ModuleCallSource {
            file: None,
            call_span: Some(call_span),
            argument_spans,
            expansion_key: None,
        }),
    )
}
