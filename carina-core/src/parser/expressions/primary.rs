//! Primary expression parser — the bottom of the expression grammar.
//!
//! Handles literals (boolean / number / float / string), namespaced
//! identifiers, list/map constructors, function calls (including eager
//! partial-application handling), variable references with field/index
//! access chains, and value-position `if` expressions.

use crate::eval_value::EvalValue;
use crate::parser::expressions::if_expr::parse_if_value_expr;
use crate::parser::expressions::string_literal::parse_string_value;
use crate::parser::{
    ParseContext, ParseError, Rule, evaluate_user_function, extract_key_string, first_inner,
    is_static_value, next_pair, parse_block_contents, parse_expression, parse_expression_eval,
};
use crate::resource::Value;
use indexmap::IndexMap;

pub(crate) fn parse_primary_eval(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    // For primary, get inner content; otherwise process directly
    let inner = if pair.as_rule() == Rule::primary {
        first_inner(pair, "value", "primary expression")?
    } else {
        pair
    };

    match inner.as_rule() {
        Rule::resource_expr => {
            // Resource expressions cannot be used as attribute values (only valid in top-level let bindings)
            Err(ParseError::InvalidExpression {
                line: 0,
                message: "Resource expressions can only be used in let bindings".to_string(),
            })
        }
        Rule::list => {
            let items: Result<Vec<Value>, ParseError> = inner
                .into_inner()
                .map(|item| parse_expression(item, ctx))
                .collect();
            Ok(EvalValue::from_value(Value::List(items?)))
        }
        Rule::map => {
            let mut map: IndexMap<String, Value> = IndexMap::new();
            let mut nested_blocks: IndexMap<String, Vec<Value>> = IndexMap::new();
            for entry in inner.into_inner() {
                match entry.as_rule() {
                    Rule::map_entry => {
                        let mut entry_inner = entry.into_inner();
                        let key_pair = next_pair(&mut entry_inner, "map key", "map entry")?;
                        let key = extract_key_string(key_pair)?;
                        let value = parse_expression(
                            next_pair(&mut entry_inner, "map value", "map entry")?,
                            ctx,
                        )?;
                        map.insert(key, value);
                    }
                    Rule::nested_block => {
                        let mut block_inner = entry.into_inner();
                        let block_name =
                            next_pair(&mut block_inner, "block name", "nested block in map")?
                                .as_str()
                                .to_string();
                        let block_attrs = parse_block_contents(block_inner, ctx)?;
                        nested_blocks
                            .entry(block_name)
                            .or_default()
                            .push(Value::Map(block_attrs));
                    }
                    _ => {}
                }
            }
            for (name, blocks) in nested_blocks {
                map.insert(name, Value::List(blocks));
            }
            Ok(EvalValue::from_value(Value::Map(map)))
        }
        Rule::namespaced_id => {
            // Namespaced identifier (e.g., aws.Region.ap_northeast_1)
            // or resource reference (e.g., bucket.name)
            // or arguments reference in module context (e.g., arguments.vpc_id)
            let full_str = inner.as_str();
            let parts: Vec<&str> = full_str.split('.').collect();

            if parts.len() == 2 {
                // Two-part identifier: could be resource reference or variable access
                if ctx.get_variable(parts[0]).is_some() && !ctx.is_resource_binding(parts[0]) {
                    // Variable exists but trying to access attribute on non-resource
                    Err(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "'{}' is not a resource, cannot access attribute '{}'",
                            parts[0], parts[1]
                        ),
                    })
                } else if ctx.is_resource_binding(parts[0]) {
                    // Known resource binding: treat as resource reference
                    Ok(EvalValue::from_value(Value::resource_ref(
                        parts[0].to_string(),
                        parts[1].to_string(),
                        vec![],
                    )))
                } else {
                    // Unknown 2-part identifier: could be TypeName.value enum shorthand
                    // Will be resolved during schema validation
                    Ok(EvalValue::from_value(Value::String(format!(
                        "{}.{}",
                        parts[0], parts[1]
                    ))))
                }
            } else if ctx.is_resource_binding(parts[0]) {
                // 3+ part identifier where first part is a resource binding:
                // chained field access (e.g., web.network.vpc_id)
                Ok(EvalValue::from_value(Value::resource_ref(
                    parts[0].to_string(),
                    parts[1].to_string(),
                    parts[2..].iter().map(|s| s.to_string()).collect(),
                )))
            } else {
                // 3+ part identifier is a namespaced type (aws.Region.ap_northeast_1)
                Ok(EvalValue::from_value(Value::String(full_str.to_string())))
            }
        }
        Rule::boolean => {
            let b = inner.as_str() == "true";
            Ok(EvalValue::from_value(Value::Bool(b)))
        }
        Rule::float => {
            let f: f64 = inner
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: inner.line_col().0,
                    message: format!("invalid float literal: {e}"),
                })?;
            Ok(EvalValue::from_value(Value::Float(f)))
        }
        Rule::number => {
            let n: i64 = inner
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: inner.line_col().0,
                    message: format!("integer literal out of range: {e}"),
                })?;
            Ok(EvalValue::from_value(Value::Int(n)))
        }
        Rule::string => parse_string_value(inner, ctx).map(EvalValue::from_value),
        Rule::function_call => {
            let mut fc_inner = inner.into_inner();
            let func_name = next_pair(&mut fc_inner, "function name", "function call")?
                .as_str()
                .to_string();
            let args: Result<Vec<Value>, ParseError> =
                fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
            let args = args?;

            // Check if the name refers to a Closure variable (direct call on closure)
            if let Some(EvalValue::Closure {
                name: fn_name,
                captured_args,
                remaining_arity,
            }) = ctx.get_variable(&func_name)
                && args.iter().all(is_static_value)
            {
                let eval_args: Vec<EvalValue> =
                    args.iter().cloned().map(EvalValue::from_value).collect();
                return crate::builtins::apply_closure_with_config(
                    fn_name,
                    captured_args,
                    *remaining_arity,
                    &eval_args,
                    ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: e,
                });
            }

            // Try to eagerly evaluate user-defined function calls
            if ctx.user_functions.contains_key(&func_name) && args.iter().all(is_static_value) {
                let user_fn = ctx.user_functions.get(&func_name).unwrap().clone();
                return evaluate_user_function(&user_fn, &args, ctx).map(EvalValue::from_value);
            }

            // Eagerly evaluate partial application (fewer args than arity → Closure)
            if let Some(arity) = crate::builtins::builtin_arity(&func_name)
                && args.len() < arity
                && args.iter().all(is_static_value)
            {
                let eval_args: Vec<EvalValue> =
                    args.iter().cloned().map(EvalValue::from_value).collect();
                return crate::builtins::evaluate_builtin_with_config(
                    &func_name, &eval_args, ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("{}(): {}", func_name, e),
                });
            }

            Ok(EvalValue::from_value(Value::FunctionCall {
                name: func_name,
                args,
            }))
        }
        Rule::variable_ref => {
            // variable_ref = { identifier ~ (field_access | index_access)* }
            // field_access = { "." ~ identifier }
            // index_access = { "[" ~ expression ~ "]" }
            let mut parts = inner.into_inner();
            let first_ident = next_pair(&mut parts, "identifier", "variable reference")?.as_str();

            // Collect all access steps (field or index)
            let access_steps: Vec<pest::iterators::Pair<Rule>> = parts.collect();

            if access_steps.is_empty() {
                // Simple variable reference (no access chain)
                match ctx.get_variable(first_ident) {
                    Some(val) => Ok(val.clone()),
                    None => Ok(EvalValue::from_value(Value::String(
                        first_ident.to_string(),
                    ))),
                }
            } else {
                // Build binding_name, attribute_name, and field_path from access steps.
                // Index access (e.g., [0] or ["key"]) composes the binding name.
                // Field access after the binding gives attribute_name and field_path.
                let mut binding_name = first_ident.to_string();
                let mut field_names: Vec<String> = Vec::new();
                let mut in_field_phase = false;

                for step in access_steps {
                    match step.as_rule() {
                        Rule::index_access => {
                            if in_field_phase {
                                // Index access after field access is not yet supported
                                // (e.g., a.b[0] — would need runtime list indexing)
                                return Err(ParseError::InvalidExpression {
                                    line: 0,
                                    message: "index access after field access is not supported"
                                        .to_string(),
                                });
                            }
                            // Parse the index expression
                            let index_expr_pair =
                                first_inner(step, "index expression", "index access")?;
                            let index_value = parse_expression(index_expr_pair, ctx)?;
                            // Compose the binding name: name[0] or name["key"]
                            match &index_value {
                                Value::Int(n) => {
                                    binding_name = format!("{}[{}]", binding_name, n);
                                }
                                Value::String(s) => {
                                    binding_name = format!("{}[\"{}\"]", binding_name, s);
                                }
                                other => {
                                    return Err(ParseError::InvalidExpression {
                                        line: 0,
                                        message: format!(
                                            "index access key must be an integer or string, got {:?}",
                                            other
                                        ),
                                    });
                                }
                            }
                        }
                        Rule::field_access => {
                            in_field_phase = true;
                            let field_ident =
                                first_inner(step, "field identifier", "field access")?;
                            field_names.push(field_ident.as_str().to_string());
                        }
                        _ => {}
                    }
                }

                if field_names.is_empty() {
                    // Index access only, no field access (e.g., subnets[0])
                    // Check if the composed binding name is a known variable
                    match ctx.get_variable(&binding_name) {
                        Some(val) => Ok(val.clone()),
                        None => {
                            // Return as ResourceRef with empty attribute_name
                            // (will be resolved later)
                            Ok(EvalValue::from_value(Value::resource_ref(
                                binding_name,
                                String::new(),
                                vec![],
                            )))
                        }
                    }
                } else {
                    let attribute_name = field_names.remove(0);
                    Ok(EvalValue::from_value(Value::resource_ref(
                        binding_name,
                        attribute_name,
                        field_names,
                    )))
                }
            }
        }
        Rule::if_expr => parse_if_value_expr(inner, ctx).map(EvalValue::from_value),
        Rule::expression => parse_expression_eval(inner, ctx),
        _ => Ok(EvalValue::from_value(Value::String(
            inner.as_str().to_string(),
        ))),
    }
}
