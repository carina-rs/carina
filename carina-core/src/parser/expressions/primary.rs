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
use crate::resource::{AccessPath, Subscript, Value};
use indexmap::IndexMap;

/// Convert an index-expression value into a `Subscript`. Only
/// non-negative integer and string keys are legal subscripts; anything
/// else is a parse error. Negative integers are rejected here rather
/// than at validate time because the DSL has no `[-1]` "from end"
/// semantic — accepting them would let the validator pass and the
/// resolver silently fall back to the unresolved ref.
fn subscript_from_value(value: Value) -> Result<Subscript, ParseError> {
    match value {
        Value::Int(n) if n < 0 => Err(ParseError::InvalidExpression {
            line: 0,
            message: format!("index access key must be non-negative, got {}", n),
        }),
        Value::Int(n) => Ok(Subscript::Int { index: n }),
        Value::String(s) => Ok(Subscript::Str { key: s }),
        other => Err(ParseError::InvalidExpression {
            line: 0,
            message: format!(
                "index access key must be an integer or string, got {:?}",
                other
            ),
        }),
    }
}

/// Lower a `namespaced_id` pair to its `EvalValue`, attaching any
/// trailing subscripts collected by the caller (the `subscripted_id`
/// arm). String-form enum shorthands like `aws.Region.ap_northeast_1`
/// have no `AccessPath` to host subscripts — those reject outright.
fn parse_namespaced_id_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    subscripts: Vec<Subscript>,
) -> Result<EvalValue, ParseError> {
    let full_str = pair.as_str();
    let parts: Vec<&str> = full_str.split('.').collect();
    // Pest's `namespaced_id = @{ identifier ~ ("." ~ ...)+ }` rule
    // guarantees ≥2 parts; a future grammar tweak that loosened it
    // would silently start panicking on `parts[1]` below.
    debug_assert!(
        parts.len() >= 2,
        "namespaced_id parsed without a `.` segment: {:?}",
        full_str
    );

    let as_resource_ref = |subscripts: Vec<Subscript>| {
        let path = AccessPath::with_fields_and_subscripts(
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2..].iter().map(|s| s.to_string()).collect(),
            subscripts,
        );
        EvalValue::from_value(Value::ResourceRef { path })
    };

    if ctx.is_resource_binding(parts[0]) {
        return Ok(as_resource_ref(subscripts));
    }

    // Subscripts (`a.b['k']`, `a.b[0]`) unambiguously mean binding
    // access — no enum shorthand uses `[...]`. So when the head is not
    // yet known as a resource binding in this file, emit a structured
    // `ResourceRef` anyway and let cross-file resolution / scope checks
    // either confirm the binding (e.g. an `upstream_state` declared in
    // a sibling `.crn`) or surface an undefined-identifier diagnostic
    // post-merge. Issue #2435.
    if !subscripts.is_empty() {
        return Ok(as_resource_ref(subscripts));
    }

    if parts.len() == 2
        && ctx.get_variable(parts[0]).is_some()
        && !ctx.is_resource_binding(parts[0])
    {
        return Err(ParseError::InvalidExpression {
            line: 0,
            message: format!(
                "'{}' is not a resource, cannot access attribute '{}'",
                parts[0], parts[1]
            ),
        });
    }

    Ok(EvalValue::from_value(Value::String(full_str.to_string())))
}

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
        Rule::namespaced_id => parse_namespaced_id_value(inner, ctx, Vec::new()),
        Rule::subscripted_id => {
            // `binding.field[idx]` / `binding.field.subfield[idx]…` —
            // the namespaced_id portion behaves like a plain
            // namespaced_id (identifier shorthand or resource ref), and
            // any trailing `[idx]` subscripts are folded onto the
            // resulting `AccessPath`.
            let mut parts = inner.into_inner();
            let head = next_pair(&mut parts, "namespaced_id", "subscripted_id")?;
            let mut subscripts: Vec<Subscript> = Vec::new();
            for index_pair in parts {
                if !matches!(index_pair.as_rule(), Rule::index_access) {
                    continue;
                }
                let index_expr_pair = first_inner(index_pair, "index expression", "index access")?;
                let index_value = parse_expression(index_expr_pair, ctx)?;
                subscripts.push(subscript_from_value(index_value)?);
            }
            parse_namespaced_id_value(head, ctx, subscripts)
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
                // Build binding_name, attribute_name, field_path, and
                // post-field subscripts from the access steps. There are
                // two index phases:
                //
                //   * **pre-field** — index access *before* any field
                //     access. The legacy convention folds these into the
                //     binding name string (`subnets[0]`,
                //     `networks.prod`), preserved here unchanged so
                //     `for`-iteration addresses round-trip.
                //   * **post-field** — index access *after* field access
                //     (`orgs.accounts[0]`, `orgs.account.children[0]`).
                //     Captured structurally on `AccessPath::subscripts`
                //     so cross-directory shape checks can tell `[0]`
                //     from `.0` (#2318).
                //
                // The two never interleave: once a field segment is
                // seen, every later index becomes post-field. A second
                // field after a post-field subscript (e.g.
                // `a.b[0].c`) is rejected — runtime list-indexing of
                // arbitrary structs isn't representable today.
                let mut binding_name = first_ident.to_string();
                let mut field_names: Vec<String> = Vec::new();
                let mut subscripts: Vec<Subscript> = Vec::new();
                let mut in_field_phase = false;

                for step in access_steps {
                    match step.as_rule() {
                        Rule::index_access => {
                            let index_expr_pair =
                                first_inner(step, "index expression", "index access")?;
                            let index_value = parse_expression(index_expr_pair, ctx)?;
                            let subscript = subscript_from_value(index_value)?;
                            if in_field_phase {
                                subscripts.push(subscript);
                            } else {
                                // Pre-field index — fold into the
                                // binding name string per the legacy
                                // convention so `for`-iteration
                                // addresses round-trip.
                                binding_name = match subscript {
                                    Subscript::Int { index } => {
                                        format!("{}[{}]", binding_name, index)
                                    }
                                    Subscript::Str { key } => {
                                        crate::utils::map_key_address(&binding_name, &key)
                                    }
                                };
                            }
                        }
                        Rule::field_access => {
                            if !subscripts.is_empty() {
                                return Err(ParseError::InvalidExpression {
                                    line: 0,
                                    message: "field access after index access is not supported"
                                        .to_string(),
                                });
                            }
                            in_field_phase = true;
                            let field_ident =
                                first_inner(step, "field identifier", "field access")?;
                            field_names.push(field_ident.as_str().to_string());
                        }
                        _ => {}
                    }
                }

                if field_names.is_empty() {
                    // No field access: subscripts is empty by
                    // construction (the post-field bucket only fills
                    // after a field). Pre-field subscripts have already
                    // been folded into binding_name above.
                    match ctx.get_variable(&binding_name) {
                        Some(val) => Ok(val.clone()),
                        None => Ok(EvalValue::from_value(Value::resource_ref(
                            binding_name,
                            String::new(),
                            vec![],
                        ))),
                    }
                } else {
                    let attribute_name = field_names.remove(0);
                    let path = AccessPath::with_fields_and_subscripts(
                        binding_name,
                        attribute_name,
                        field_names,
                        subscripts,
                    );
                    Ok(EvalValue::from_value(Value::ResourceRef { path }))
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
