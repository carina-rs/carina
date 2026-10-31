//! `arguments { }`, `attributes { }`, `exports { }` block parsers and the
//! `directives { }` meta-argument extractor.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::ParsedValue as Value;
use crate::parser::ParserValidationBlock as ValidationBlock;
use crate::parser::ProviderContext;
use crate::parser::Rule;
use crate::parser::ast::{ArgumentParameter, AttributeParameter, ExportParameter, SourceSpan};
use crate::parser::context::{ParseContext, first_inner, next_pair};
use crate::parser::error::{ParseError, ParseWarning};
use crate::parser::expressions::string_literal::parse_string_value;
use crate::parser::expressions::validate_expr::parse_validate_expr;
use crate::parser::grammar::PreprocessedFile;
use crate::parser::parse_expression;
use crate::parser::types::parse_type_expr;
use crate::resource::{ConcreteValue, DeferredValue};

pub(in crate::parser) struct ParsedArgumentsBlock {
    pub parameters: Vec<ArgumentParameter>,
    pub type_sources: Vec<(String, SourceSpan)>,
}

/// Parse arguments block. See `register_argument_binding` for the
/// incremental scoping discipline that lets a later argument's default
/// expression reference earlier arguments (#2393).
pub(in crate::parser) fn parse_arguments_block(
    pair: pest::iterators::Pair<Rule>,
    config: &ProviderContext,
    ctx: &mut ParseContext,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
    warnings: &mut Vec<ParseWarning>,
) -> Result<ParsedArgumentsBlock, ParseError> {
    let mut arguments = Vec::new();
    let mut type_sources = Vec::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::arguments_param {
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "arguments block")?
                .as_str()
                .to_string();
            let type_pair = next_pair(&mut param_inner, "type expression", "arguments parameter")?;
            let type_span = SourceSpan::from_pest(&type_pair, preprocessed_file);
            let type_expr = parse_type_expr(type_pair, config, warnings, Some(preprocessed_file))?;

            let mut description = None;
            let mut default = None;
            let mut validations = Vec::new();

            if let Some(next) = param_inner.next() {
                if next.as_rule() == Rule::arguments_param_block {
                    for attr in next.into_inner() {
                        if attr.as_rule() == Rule::arguments_param_attr {
                            let inner_attr =
                                first_inner(attr, "attribute", "arguments_param_attr")?;
                            match inner_attr.as_rule() {
                                Rule::arg_description_attr => {
                                    let string_pair =
                                        first_inner(inner_attr, "string", "arg_description_attr")?;
                                    let value = parse_string_value(string_pair, ctx)?;
                                    if let Value::Concrete(ConcreteValue::String(s)) = value {
                                        description = Some(s.into_plaintext());
                                    }
                                }
                                Rule::arg_default_attr => {
                                    let expr_pair =
                                        first_inner(inner_attr, "expression", "arg_default_attr")?;
                                    default = Some(parse_expression(expr_pair, ctx)?);
                                }
                                Rule::arg_validation_block => {
                                    let mut rule = None;
                                    let mut error_msg = None;
                                    for vattr in inner_attr.into_inner() {
                                        if vattr.as_rule() == Rule::validation_block_attr {
                                            let inner_vattr = first_inner(
                                                vattr,
                                                "validation_block_attr",
                                                "validation_block_attr",
                                            )?;
                                            match inner_vattr.as_rule() {
                                                Rule::validation_condition_attr => {
                                                    let validate_pair = first_inner(
                                                        inner_vattr,
                                                        "validate_expr",
                                                        "validation_condition_attr",
                                                    )?;
                                                    let source_span = SourceSpan::from_pest(
                                                        &validate_pair,
                                                        preprocessed_file,
                                                    );
                                                    rule = Some((
                                                        parse_validate_expr(validate_pair, ctx)?,
                                                        source_span,
                                                    ));
                                                }
                                                Rule::validation_error_message_attr => {
                                                    let string_pair = first_inner(
                                                        inner_vattr,
                                                        "string",
                                                        "validation_error_message_attr",
                                                    )?;
                                                    let value =
                                                        parse_string_value(string_pair, ctx)?;
                                                    if let Value::Concrete(ConcreteValue::String(
                                                        s,
                                                    )) = value
                                                    {
                                                        error_msg = Some(s.into_plaintext());
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    if let Some((condition, source_span)) = rule {
                                        validations.push(ValidationBlock::new_in_source(
                                            condition,
                                            error_msg,
                                            source_span,
                                            source_file.to_path_buf(),
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                } else {
                    // Simple form: the next element is the default expression itself.
                    default = Some(parse_expression(next, ctx)?);
                }
            }

            register_argument_binding(ctx, &name, &type_expr);
            type_sources.push((name.clone(), type_span));
            arguments.push(ArgumentParameter {
                name,
                static_type: type_expr,
                default,
                description,
                validations,
            });
        }
    }

    Ok(ParsedArgumentsBlock {
        parameters: arguments,
        type_sources,
    })
}

/// Register an argument name as a lexical binding so subsequent expressions
/// (later argument defaults, resource bodies, etc.) resolve it as a
/// module-argument placeholder rather than a literal string. Without this
/// incremental registration, `${other_arg}` inside a default would have no
/// in-scope binding and degrade to the literal string `"other_arg"` (#2393).
///
/// The caller's argument shape, not the annotation, decides whether this
/// placeholder is substituted as a value or a structural handle. Keeping the
/// body representation neutral avoids guessing before provider schemas are
/// available; expansion later applies access to a value or a canonical handle
/// base according to the typed call-site argument.
fn register_argument_binding(
    ctx: &mut ParseContext,
    name: &str,
    type_expr: &crate::parser::TypeExpr,
) {
    ctx.set_argument_variable(
        name.to_string(),
        Value::Deferred(DeferredValue::Placeholder(
            crate::resource::ParserPlaceholder::ModuleArgument {
                name: name.to_string(),
                source_line: (0).into(),
                diagnostic_name: None,
            },
        )),
        Some(type_expr.clone()),
    );
    // The local declaration is the real one for this file; drop the
    // seed mark (if any) so a later real duplicate (`let <name> = ...`
    // in the same file) is still flagged. #2817.
    ctx.unmark_seeded(name);
}

/// Parse attributes block
pub(in crate::parser) fn parse_attributes_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Vec<AttributeParameter>, ParseError> {
    let mut attribute_params = Vec::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::attributes_param {
            let source_span = SourceSpan::from_pest(&param, preprocessed_file);
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "attributes block")?
                .as_str()
                .to_string();

            // Check whether the next inner pair is a type_expr or an expression
            let next = next_pair(
                &mut param_inner,
                "type or expression",
                "attributes parameter",
            )?;
            let (type_expr, value) = if next.as_rule() == Rule::type_expr {
                // Has explicit type annotation: name: type = expr
                let type_expr = Some(parse_type_expr(
                    next,
                    ctx.config,
                    warnings,
                    Some(preprocessed_file),
                )?);
                let expr = next_pair(&mut param_inner, "value expression", "attributes parameter")?;
                let value = Some(parse_expression(expr, ctx)?);
                (type_expr, value)
            } else {
                // No type annotation: name = expr
                let value = Some(parse_expression(next, ctx)?);
                (None, value)
            };

            attribute_params.push(AttributeParameter::new_in_source(
                name,
                type_expr,
                value,
                source_span,
                source_file.to_path_buf(),
            ));
        }
    }

    Ok(attribute_params)
}

pub(in crate::parser) fn parse_exports_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    preprocessed_file: &PreprocessedFile,
    source_file: &std::path::Path,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Vec<ExportParameter>, ParseError> {
    let mut export_params = Vec::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::exports_param {
            let source_span = SourceSpan::from_pest(&param, preprocessed_file);
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "exports block")?
                .as_str()
                .to_string();

            let next = next_pair(&mut param_inner, "type or expression", "exports parameter")?;
            let (type_expr, value) = if next.as_rule() == Rule::type_expr {
                let type_expr = Some(parse_type_expr(
                    next,
                    ctx.config,
                    warnings,
                    Some(preprocessed_file),
                )?);
                let expr = next_pair(&mut param_inner, "value expression", "exports parameter")?;
                let value = Some(parse_expression(expr, ctx)?);
                (type_expr, value)
            } else {
                let value = Some(parse_expression(next, ctx)?);
                (None, value)
            };

            export_params.push(ExportParameter::new_in_source(
                name,
                type_expr,
                value,
                source_span,
                source_file.to_path_buf(),
            ));
        }
    }

    Ok(export_params)
}
