//! `arguments { }`, `attributes { }`, `exports { }` block parsers and the
//! `directives { }` meta-argument extractor.
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
use crate::resource::{ConcreteValue, DeferredValue, Directives, Resource, Value};
use indexmap::IndexMap;

/// Parse arguments block. See `register_argument_binding` for the
/// incremental scoping discipline that lets a later argument's default
/// expression reference earlier arguments (#2393).
pub(in crate::parser) fn parse_arguments_block(
    pair: pest::iterators::Pair<Rule>,
    config: &ProviderContext,
    ctx: &mut ParseContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Vec<ArgumentParameter>, ParseError> {
    let mut arguments = Vec::new();

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
                                        description = Some(s);
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
                                                        parse_string_value(string_pair, ctx)?;
                                                    if let Value::Concrete(ConcreteValue::String(
                                                        s,
                                                    )) = value
                                                    {
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
                } else {
                    // Simple form: the next element is the default expression itself.
                    default = Some(parse_expression(next, ctx)?);
                }
            }

            register_argument_binding(ctx, &name);
            arguments.push(ArgumentParameter {
                name,
                type_expr,
                default,
                description,
                validations,
            });
        }
    }

    Ok(arguments)
}

/// Register an argument name as a lexical binding so subsequent expressions
/// (later argument defaults, resource bodies, etc.) resolve it as a
/// `BindingRef` placeholder rather than a literal string. Without this
/// incremental registration, `${other_arg}` inside a default would have no
/// in-scope binding and degrade to the literal string `"other_arg"` (#2393).
///
/// `BindingRef` (not `ResourceRef`) is correct here: an `arguments {}`
/// declaration introduces a name without an attribute. When a later
/// expression writes `other_arg.attr`, the parser composes a fresh
/// `ResourceRef`. Storing the placeholder as `ResourceRef` with an
/// empty `attribute` would be a type-level lie — the same shape that
/// produced the empty-field diagnostic in #2847.
fn register_argument_binding(ctx: &mut ParseContext, name: &str) {
    let placeholder_ref = Value::Deferred(DeferredValue::BindingRef {
        binding: name.to_string(),
    });
    ctx.set_variable(name.to_string(), placeholder_ref);
    let placeholder = Resource::new("_argument", name);
    ctx.set_resource_binding(name.to_string(), placeholder);
    // The local declaration is the real one for this file; drop the
    // seed mark (if any) so a later real duplicate (`let <name> = ...`
    // in the same file) is still flagged. #2817.
    ctx.unmark_seeded(name);
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

/// Extract Carina-side directives from a resource's attributes.
///
/// The parser parses `directives { ... }` as a nested block, which
/// becomes a List of Maps in attributes. We extract it and convert to
/// `Directives`.
pub(in crate::parser) fn extract_directives(
    attributes: &mut IndexMap<String, Value>,
) -> Result<Directives, ParseError> {
    if let Some(Value::Concrete(ConcreteValue::List(blocks))) =
        attributes.shift_remove("directives")
    {
        // Take the first directives block (there should only be one)
        if let Some(Value::Concrete(ConcreteValue::Map(map))) = blocks.into_iter().next() {
            let force_delete = matches!(
                map.get("force_delete"),
                Some(Value::Concrete(ConcreteValue::Bool(true)))
            );
            let create_before_destroy = matches!(
                map.get("create_before_destroy"),
                Some(Value::Concrete(ConcreteValue::Bool(true)))
            );
            let prevent_destroy = matches!(
                map.get("prevent_destroy"),
                Some(Value::Concrete(ConcreteValue::Bool(true)))
            );
            let depends_on = match map.get("depends_on") {
                None => Vec::new(),
                Some(Value::Concrete(ConcreteValue::List(items))) => {
                    let mut names = Vec::with_capacity(items.len());
                    for item in items {
                        names.push(value_as_binding_name(item, "depends_on: list elements")?);
                    }
                    names
                }
                Some(other) => {
                    return Err(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "directives.depends_on: must be a list of binding identifiers, \
                             got {:?}",
                            other
                        ),
                    });
                }
            };
            let provider_instance = match map.get("provider") {
                None => None,
                Some(value) => Some(value_as_binding_name(value, "provider: value")?),
            };
            return Ok(Directives {
                force_delete,
                create_before_destroy,
                prevent_destroy,
                depends_on,
                provider_instance,
            });
        }
    }
    Ok(Directives::default())
}

/// Interpret a `Value` as a bare binding-name reference. Used by every
/// `directives { ... }` slot whose value must be `<binding>` (currently
/// `depends_on`'s list elements and `provider`).
///
/// `context` is rendered into the error message as the directives slot
/// path (e.g. `"depends_on: list elements"` or `"provider: value"`) so a
/// shared error message template stays readable.
///
/// Bare-string quoted literals like `"role"` and `"us"` are rejected
/// upstream at the pest layer (see `check_directives_depends_on_elements`
/// / `check_directives_provider_value`), because by the time the value
/// reaches this helper a quoted string and a `${...}` indirection are
/// indistinguishable.
fn value_as_binding_name(value: &Value, context: &str) -> Result<String, ParseError> {
    match value {
        Value::Deferred(DeferredValue::BindingRef { binding }) => Ok(binding.clone()),
        Value::Concrete(ConcreteValue::EnumIdentifier(name)) => Ok(name.clone()),
        Value::Concrete(ConcreteValue::String(name)) => {
            let bare = name
                .strip_prefix("${")
                .and_then(|s| s.strip_suffix('}'))
                .unwrap_or(name.as_str());
            Ok(bare.to_string())
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            Err(ParseError::InvalidExpression {
                line: 0,
                message: format!(
                    "directives.{context} must be a bare binding identifier, not an \
                 attribute selector (got `{}`)",
                    path.binding()
                ),
            })
        }
        other => Err(ParseError::InvalidExpression {
            line: 0,
            message: format!(
                "directives.{context} must be a binding identifier, got {:?}",
                other
            ),
        }),
    }
}
