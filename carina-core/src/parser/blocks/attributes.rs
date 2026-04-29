//! `arguments { }`, `attributes { }`, `exports { }` block parsers and the
//! `lifecycle { }` meta-argument extractor.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::ProviderContext;
use crate::parser::Rule;
use crate::parser::ast::{ArgumentParameter, AttributeParameter, ExportParameter, ValidationBlock};
use crate::parser::context::{ParseContext, first_inner, next_pair};
use crate::parser::error::{ParseError, ParseWarning};
use crate::parser::expressions::string_literal::parse_string_value;
use crate::parser::expressions::validate_expr::parse_validate_expr;
use crate::parser::parse_expression;
use crate::parser::types::parse_type_expr;
use crate::resource::{LifecycleConfig, Value};
use indexmap::IndexMap;

/// Parse arguments block
pub(in crate::parser) fn parse_arguments_block(
    pair: pest::iterators::Pair<Rule>,
    config: &ProviderContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Vec<ArgumentParameter>, ParseError> {
    let mut arguments = Vec::new();
    let ctx = ParseContext::new(config);

    for param in pair.into_inner() {
        if param.as_rule() == Rule::arguments_param {
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "arguments block")?
                .as_str()
                .to_string();
            let type_expr = parse_type_expr(
                next_pair(&mut param_inner, "type expression", "arguments parameter")?,
                config,
                warnings,
            )?;

            // Check if the next element is a block form or simple default
            if let Some(next) = param_inner.next() {
                if next.as_rule() == Rule::arguments_param_block {
                    // Block form: parse description, default, validate blocks from attrs
                    let mut description = None;
                    let mut default = None;
                    let mut validations = Vec::new();
                    for attr in next.into_inner() {
                        if attr.as_rule() == Rule::arguments_param_attr {
                            let inner_attr =
                                first_inner(attr, "attribute", "arguments_param_attr")?;
                            match inner_attr.as_rule() {
                                Rule::arg_description_attr => {
                                    let string_pair =
                                        first_inner(inner_attr, "string", "arg_description_attr")?;
                                    let value = parse_string_value(string_pair, &ctx)?;
                                    if let Value::String(s) = value {
                                        description = Some(s);
                                    }
                                }
                                Rule::arg_default_attr => {
                                    let expr_pair =
                                        first_inner(inner_attr, "expression", "arg_default_attr")?;
                                    default = Some(parse_expression(expr_pair, &ctx)?);
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
                                                    rule =
                                                        Some(parse_validate_expr(validate_pair)?);
                                                }
                                                Rule::validation_error_message_attr => {
                                                    let string_pair = first_inner(
                                                        inner_vattr,
                                                        "string",
                                                        "validation_error_message_attr",
                                                    )?;
                                                    let value =
                                                        parse_string_value(string_pair, &ctx)?;
                                                    if let Value::String(s) = value {
                                                        error_msg = Some(s);
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    if let Some(condition) = rule {
                                        validations.push(ValidationBlock {
                                            condition,
                                            error_message: error_msg,
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    arguments.push(ArgumentParameter {
                        name,
                        type_expr,
                        default,
                        description,
                        validations,
                    });
                } else {
                    // Simple form: the next element is the default expression
                    let default = Some(parse_expression(next, &ctx)?);
                    arguments.push(ArgumentParameter {
                        name,
                        type_expr,
                        default,
                        description: None,
                        validations: Vec::new(),
                    });
                }
            } else {
                // No default, no block
                arguments.push(ArgumentParameter {
                    name,
                    type_expr,
                    default: None,
                    description: None,
                    validations: Vec::new(),
                });
            }
        }
    }

    Ok(arguments)
}

/// Parse attributes block
pub(in crate::parser) fn parse_attributes_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Vec<AttributeParameter>, ParseError> {
    let mut attribute_params = Vec::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::attributes_param {
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
                let type_expr = Some(parse_type_expr(next, ctx.config, warnings)?);
                let expr = next_pair(&mut param_inner, "value expression", "attributes parameter")?;
                let value = Some(parse_expression(expr, ctx)?);
                (type_expr, value)
            } else {
                // No type annotation: name = expr
                let value = Some(parse_expression(next, ctx)?);
                (None, value)
            };

            attribute_params.push(AttributeParameter {
                name,
                type_expr,
                value,
            });
        }
    }

    Ok(attribute_params)
}

pub(in crate::parser) fn parse_exports_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Vec<ExportParameter>, ParseError> {
    let mut export_params = Vec::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::exports_param {
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "exports block")?
                .as_str()
                .to_string();

            let next = next_pair(&mut param_inner, "type or expression", "exports parameter")?;
            let (type_expr, value) = if next.as_rule() == Rule::type_expr {
                let type_expr = Some(parse_type_expr(next, ctx.config, warnings)?);
                let expr = next_pair(&mut param_inner, "value expression", "exports parameter")?;
                let value = Some(parse_expression(expr, ctx)?);
                (type_expr, value)
            } else {
                let value = Some(parse_expression(next, ctx)?);
                (None, value)
            };

            export_params.push(ExportParameter {
                name,
                type_expr,
                value,
            });
        }
    }

    Ok(export_params)
}

/// Extract lifecycle configuration from a resource's attributes.
///
/// The parser parses `lifecycle { ... }` as a nested block, which becomes
/// a List of Maps in attributes. We extract it and convert to LifecycleConfig.
pub(in crate::parser) fn extract_lifecycle_config(
    attributes: &mut IndexMap<String, Value>,
) -> LifecycleConfig {
    if let Some(Value::List(blocks)) = attributes.shift_remove("lifecycle") {
        // Take the first lifecycle block (there should only be one)
        if let Some(Value::Map(map)) = blocks.into_iter().next() {
            let force_delete = matches!(map.get("force_delete"), Some(Value::Bool(true)));
            let create_before_destroy =
                matches!(map.get("create_before_destroy"), Some(Value::Bool(true)));
            let prevent_destroy = matches!(map.get("prevent_destroy"), Some(Value::Bool(true)));
            return LifecycleConfig {
                force_delete,
                create_before_destroy,
                prevent_destroy,
            };
        }
    }
    LifecycleConfig::default()
}
