//! Module-call parser: `module_name { arg = value, ... }`.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::Rule;
use crate::parser::ast::{
    AuthoredModuleArgument, ModuleArgument, ModuleCall, ModuleCallId, SourceSpan,
};
use crate::parser::context::{ExpressionMode, ParseContext, StructuralSinkMode, next_pair};
use crate::parser::error::ParseError;
use crate::parser::grammar::PreprocessedFile;
use crate::parser::parse_expression_eval_with_mode;
use std::collections::HashMap;

/// Parse module call
pub(crate) fn parse_module_call(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<ModuleCall, ParseError> {
    let span = pair.as_span();
    let call_span = SourceSpan::from_pest(&pair, preprocessed_file);
    let call_id = ModuleCallId::direct(source_file.to_path_buf(), call_span);
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
            let key_pair = next_pair(&mut arg_inner, "argument name", "module call argument")?;
            let key_source_span = SourceSpan::from_pest(&key_pair, preprocessed_file);
            let key = key_pair.as_str().to_string();
            let value_pair = next_pair(&mut arg_inner, "argument value", "module call argument")?;
            let value_source_span = SourceSpan::from_pest(&value_pair, preprocessed_file);
            let value_line = value_pair.line_col().0;
            let mode = ExpressionMode::module_argument(
                call_id.clone(),
                key.clone(),
                value_source_span,
                source_file,
                preprocessed_file,
                StructuralSinkMode::DirectHandle,
            );
            let eval = parse_expression_eval_with_mode(value_pair, ctx, &mode)?;
            if matches!(eval, crate::eval_value::EvalValue::PendingValue(_)) {
                continue;
            }
            let argument = match &eval {
                crate::eval_value::EvalValue::UnexpandedStructuralHandle(handle) => {
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
            arguments.insert(
                key,
                AuthoredModuleArgument::new(argument, key_source_span, value_source_span),
            );
        }
    }

    Ok(ModuleCall::from_authored_arguments_in_source(
        module_name,
        None,
        call_span,
        arguments,
        source_file.to_path_buf(),
    ))
}
