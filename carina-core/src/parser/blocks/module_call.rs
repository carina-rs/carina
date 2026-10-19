//! Module-call parser: `module_name { arg = value, ... }`.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::Rule;
use crate::parser::ast::{ModuleArgument, ModuleCall, ModuleCallSource, ModuleCallSourceSpan};
use crate::parser::context::{ParseContext, next_pair};
use crate::parser::error::ParseError;
use crate::parser::parse_expression_eval;
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
    let mut argument_value_spans = HashMap::new();
    for arg in inner {
        if arg.as_rule() == Rule::module_call_arg {
            let mut arg_inner = arg.into_inner();
            let key_pair = next_pair(&mut arg_inner, "argument name", "module call argument")?;
            let key_span = key_pair.as_span();
            let (start_line, start_column) = key_span.start_pos().line_col();
            let (end_line, end_column) = key_span.end_pos().line_col();
            let key = key_pair.as_str().to_string();
            let value_pair = next_pair(&mut arg_inner, "argument value", "module call argument")?;
            let value_span = value_pair.as_span();
            let value_line = value_pair.line_col().0;
            let (value_start_line, value_start_column) = value_span.start_pos().line_col();
            let (value_end_line, value_end_column) = value_span.end_pos().line_col();
            let eval = parse_expression_eval(value_pair, ctx)?;
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
            argument_value_spans.insert(
                key.clone(),
                ModuleCallSourceSpan {
                    start_byte: value_span.start(),
                    end_byte: value_span.end(),
                    start_line: value_start_line,
                    start_column: value_start_column,
                    end_line: value_end_line,
                    end_column: value_end_column,
                },
            );
            let argument = match &eval {
                crate::eval_value::EvalValue::StructuralHandle(handle) => {
                    ModuleArgument::from_handle(handle.clone())
                }
                _ => {
                    let access_base = eval.module_argument_access_base();
                    let value = eval
                        .into_value()
                        .map_err(|error| ParseError::from_eval_value(error, value_line))?;
                    match access_base {
                        Some(access_base) => ModuleArgument::with_access_base(value, access_base),
                        None => ModuleArgument::new(value),
                    }
                }
            };
            arguments.insert(key, argument);
        }
    }

    Ok(
        ModuleCall::from_arguments(module_name, None, arguments).with_source(ModuleCallSource {
            file: None,
            call_span: Some(call_span),
            argument_spans,
            argument_value_spans,
            expansion_key: None,
        }),
    )
}
