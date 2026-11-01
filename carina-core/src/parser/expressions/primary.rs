//! Primary expression parser — the bottom of the expression grammar.
//!
//! Handles literals (boolean / number / float / string), namespaced
//! identifiers, list/map constructors, function calls (including eager
//! partial-application handling), variable references with field/index
//! access chains, and value-position `if` expressions.

use crate::eval_value::{EvalValue, StructuralHandleMetadata};
use crate::parser::ParsedValue as Value;
use crate::parser::context::{ExpressionMode, ParseBindingRef};
use crate::parser::expressions::if_expr::parse_if_value_expr_with_mode;
use crate::parser::expressions::string_literal::parse_string_value;
use crate::parser::functions::{evaluate_user_function_eval, reject_builtin_handle_arguments};
use crate::parser::{
    ParseContext, ParseError, Rule, extract_key_string, first_inner, is_static_eval, next_pair,
    parse_block_contents_with_mode, parse_expression, parse_expression_eval,
    parse_expression_eval_with_mode,
};
use crate::resource::{
    AccessMissPolicy, AuthoredAccessPath as AccessPath, AuthoredConcreteValue as ConcreteValue,
    AuthoredDeferredValue as DeferredValue, PathSegment, RelativeBindingName, Subscript,
    ValuePathSegmentResult, ValueString, apply_value_path_segment,
};
use indexmap::IndexMap;

/// Decode a duration literal (`75min`, `1h`, `30s`) into integer
/// seconds.
///
/// The pest grammar guarantees `<digits><unit>`; this helper performs
/// the post-parse decode and shares the implementation between the
/// value-position parser ([`parse_primary_eval`]'s `Rule::duration_literal`
/// arm) and the validate-expression parser
/// (`expressions::validate_expr`). On overflow it surfaces a typed
/// parse error rather than silently truncating, since the grammar
/// accepts arbitrary digit runs.
pub(crate) fn parse_duration_secs(src: &str, line: usize) -> Result<u64, ParseError> {
    let unit_start = src
        .find(|c: char| !c.is_ascii_digit())
        .expect("grammar guarantees a non-digit unit suffix");
    let n: u64 = src[..unit_start]
        .parse()
        .map_err(|e| ParseError::InvalidExpression {
            line,
            message: format!("invalid duration integer in {src:?}: {e}"),
        })?;
    let multiplier: u64 = match &src[unit_start..] {
        "s" | "sec" | "second" | "seconds" => 1,
        "m" | "min" | "minute" | "minutes" => 60,
        "h" | "hr" | "hour" | "hours" => 3600,
        // Reachable only if a future grammar change adds a unit family
        // (e.g. `day` / `week`) without updating this match. Surface as
        // a typed parse error rather than a panic so the binary stays
        // up while the developer fills in the multiplier.
        other => {
            return Err(ParseError::InvalidExpression {
                line,
                message: format!("duration unit {other:?} is not supported by this build"),
            });
        }
    };
    n.checked_mul(multiplier)
        .ok_or_else(|| ParseError::InvalidExpression {
            line,
            message: format!("duration {src} overflows u64 seconds"),
        })
}

pub(crate) fn parse_duration_literal(src: &str, line: usize) -> Result<Value, ParseError> {
    let secs = parse_duration_secs(src, line)?;
    Ok(Value::Concrete(ConcreteValue::Duration(
        std::time::Duration::from_secs(secs),
    )))
}

/// Convert an index-expression value into a `Subscript`. Only
/// non-negative integer and string keys are legal subscripts; anything
/// else is a parse error. Negative integers are rejected here rather
/// than at validate time because the DSL has no `[-1]` "from end"
/// semantic — accepting them would let the validator pass and the
/// resolver silently fall back to the unresolved ref.
pub(crate) fn subscript_from_value(value: Value, line: usize) -> Result<Subscript, ParseError> {
    match value {
        Value::Concrete(ConcreteValue::Int(n)) if n < 0 => Err(ParseError::InvalidExpression {
            line,
            message: format!("index access key must be non-negative, got {}", n),
        }),
        Value::Concrete(ConcreteValue::Int(n)) => Ok(Subscript::Int { index: n }),
        Value::Concrete(ConcreteValue::String(s)) => Ok(Subscript::Str { key: s }),
        other => Err(ParseError::InvalidExpression {
            line,
            message: format!(
                "index access key must be an integer or string, got {}",
                crate::ValueDescription::of(&other)
            ),
        }),
    }
}

pub(crate) fn parse_postfix_path_segment(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<PathSegment, ParseError> {
    match pair.as_rule() {
        Rule::index_access => {
            let line = pair.line_col().0;
            let index_expr_pair = first_inner(pair, "index expression", "index access")?;
            let index_value = parse_expression(index_expr_pair, ctx)?;
            subscript_from_value(index_value, line).map(|index| PathSegment::Subscript { index })
        }
        Rule::field_access => {
            let ident = first_inner(pair, "identifier", "field access")?;
            Ok(PathSegment::Field {
                name: ident.as_str().to_string(),
            })
        }
        other => Err(ParseError::InternalError {
            expected: "field or index access".to_string(),
            context: format!("postfix expression contained {other:?}"),
        }),
    }
}

pub(crate) fn apply_postfix_path_segment(
    value: EvalValue,
    step: PathSegment,
    line: usize,
) -> Result<EvalValue, ParseError> {
    let base = match value {
        EvalValue::User(base) => base,
        EvalValue::StructuralHandle(handle) => {
            let binding = handle.canonical_binding_name();
            if let PathSegment::Subscript { index } = &step {
                let binding = match index {
                    Subscript::Int { index } => ValueString::plain(format!("{binding}[{index}]")),
                    Subscript::Str { key } => ValueString::derived_from(
                        crate::utils::map_key_address(binding, key.expose_plaintext()),
                        [key],
                    ),
                };
                return Ok(EvalValue::from_value(Value::Deferred(
                    DeferredValue::BindingRef {
                        binding: RelativeBindingName::new(binding),
                    },
                )));
            }
            handle.access_base_value()
        }
        EvalValue::PendingValue(slot) => return Ok(EvalValue::pending_value(slot)),
        EvalValue::ProvisionalReference { binding } => {
            Value::Deferred(DeferredValue::BindingRef { binding })
        }
        EvalValue::ComputedWithStructuralHandles {
            value: base,
            metadata,
        } => {
            if metadata.is_handle() {
                return Err(ParseError::InvalidExpression {
                    line,
                    message: format!(
                        "cannot apply access {step} to a resource handle inside a computed value"
                    ),
                });
            }
            let selected_metadata = metadata.select(&step);
            return apply_value_path_segment(base, step, line, AccessMissPolicy::Error)
                .and_then(ValuePathSegmentResult::into_value)
                .map(|value| EvalValue::computed(value, selected_metadata))
                .map_err(|error| ParseError::InvalidExpression {
                    line,
                    message: error.to_string(),
                });
        }
        EvalValue::DeferredForValue { binding, slot } => {
            return Ok(EvalValue::from_value(
                crate::resource::parser_deferred_for_access(binding, slot, step, line),
            ));
        }
        EvalValue::Closure {
            name,
            remaining_arity,
            ..
        } => {
            return Err(ParseError::InvalidExpression {
                line,
                message: format!(
                    "cannot apply access {step} to partial application '{name}' \
                     (still needs {remaining_arity} argument(s))"
                ),
            });
        }
    };
    apply_value_path_segment(base, step, line, AccessMissPolicy::Error)
        .and_then(ValuePathSegmentResult::into_value)
        .map(EvalValue::from_value)
        .map_err(|error| ParseError::InvalidExpression {
            line,
            message: error.to_string(),
        })
}

pub(crate) fn apply_postfix_steps<'i>(
    mut value: EvalValue,
    steps: impl IntoIterator<Item = pest::iterators::Pair<'i, Rule>>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    for pair in steps {
        let line = pair.line_col().0;
        let step = parse_postfix_path_segment(pair, ctx)?;
        value = apply_postfix_path_segment(value, step, line)?;
    }
    Ok(value)
}

/// Parse a postfix-expression wrapper (or the identically-shaped restricted
/// `for_iterable` wrapper) and apply its suffixes from left to right.
pub(crate) fn parse_postfix_eval(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    parse_postfix_eval_with_mode(pair, ctx, &ExpressionMode::untyped())
}

pub(crate) fn parse_postfix_eval_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    if !matches!(pair.as_rule(), Rule::postfix_expr | Rule::for_iterable) {
        return parse_primary_eval_with_mode(pair, ctx, mode);
    }

    let context = if pair.as_rule() == Rule::for_iterable {
        "for iterable"
    } else {
        "postfix expression"
    };
    let mut inner = pair.into_inner();
    let base = next_pair(&mut inner, "primary expression", context)?;
    let steps = inner.collect::<Vec<_>>();
    let untyped_mode = (!steps.is_empty()).then(ExpressionMode::untyped);
    // The expected sink describes the result of a postfix expression, not
    // values inside its intermediate base. In particular, `[resource][0].id`
    // must retain the established bare-handle rejection instead of treating
    // the resource as an enum candidate merely because the final expression
    // occurs in a typed attribute.
    let value = parse_primary_eval_with_mode(base, ctx, untyped_mode.as_ref().unwrap_or(mode))?;
    apply_postfix_steps(value, steps, ctx)
}

/// Lower a `namespaced_id` pair to its `EvalValue`, attaching any
/// trailing path segments collected by the caller (the `subscripted_id`
/// arm — a mix of `.field` and `[index]` continuations). String-form
/// enum shorthands like `aws.Region.ap_northeast_1` have no
/// `AccessPath` to host segments — those reject outright.
fn parse_namespaced_id_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    trailing_segments: Vec<PathSegment>,
) -> Result<EvalValue, ParseError> {
    let line = pair.line_col().0;
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

    let as_resource_ref = |trailing_segments: Vec<PathSegment>| {
        let mut segments: Vec<PathSegment> = parts[2..]
            .iter()
            .map(|s| PathSegment::Field {
                name: (*s).to_string(),
            })
            .collect();
        segments.extend(trailing_segments);
        let path = AccessPath::with_segments_at_source(
            parts[0].to_string(),
            parts[1].to_string(),
            segments,
            ctx.source_file(),
            line,
        );
        EvalValue::from_value(Value::Deferred(DeferredValue::ResourceRef { path }))
    };

    let head_binding = ctx.lookup_binding(parts[0]);

    match head_binding {
        ParseBindingRef::Structural(handle) if handle.addressable_binding().is_some() => {
            return Ok(as_resource_ref(trailing_segments));
        }
        ParseBindingRef::Structural(handle) => {
            let mut segments: Vec<PathSegment> = parts[2..]
                .iter()
                .map(|s| PathSegment::Field {
                    name: (*s).to_string(),
                })
                .collect();
            segments.extend(trailing_segments);
            let path = AccessPath::with_segments_at_source(
                handle.canonical_binding_name().to_string(),
                parts[1].to_string(),
                segments,
                ctx.source_file(),
                line,
            );
            return Ok(EvalValue::from_value(Value::Deferred(
                DeferredValue::ResourceRef { path },
            )));
        }
        ParseBindingRef::Value { value: bound, .. } => {
            // A dotted name whose head is an ordinary bound variable is value
            // access, not a resource path. Apply every `.field` and trailing
            // subscript through the same postfix primitive used by
            // `postfix_expr`.
            let mut value = bound
                .clone()
                .with_reference_source(ctx.diagnostic_source_span(&pair));
            for field in &parts[1..] {
                value = apply_postfix_path_segment(
                    value,
                    PathSegment::Field {
                        name: (*field).to_string(),
                    },
                    line,
                )?;
            }
            for segment in trailing_segments {
                value = apply_postfix_path_segment(value, segment, line)?;
            }
            return Ok(value);
        }
        ParseBindingRef::Unbound => {}
    }

    // Trailing segments (`a.b['k']`, `a.b[0]`, `a.b.c[0].d`)
    // unambiguously mean binding access — no enum shorthand uses
    // `[...]` and the parser only generates trailing path segments for
    // `subscripted_id`. When the head is not a resource binding in this
    // file, emit a structured `ResourceRef` and let
    // `check_identifier_scope` flag genuine typos post-merge.
    //
    // Under the directory-aware parse pipeline (#2817), sibling-defined
    // bindings are seeded into `ctx.resource_bindings` so the early
    // return at the top of this function covers the cross-file case
    // directly. This branch retained as a safety net for the truly-
    // undefined identifier path (typos, mid-edit references in the LSP
    // single-file fallback parses) — without it, an unknown subscripted
    // ID would fall through to the enum-shorthand `Value::Concrete(ConcreteValue::String)`
    // fallback below and silently mistype as a string. Originally added
    // for #2435.
    if !trailing_segments.is_empty() {
        return Ok(as_resource_ref(trailing_segments));
    }

    // Dotted IDs that aren't an enum shorthand are binding refs; emit
    // a structured `ResourceRef` and let `check_identifier_scope` flag
    // genuine typos post-merge.
    //
    // Symmetric with the subscript fallback above, and with the same
    // post-#2817 status: the directory-aware parse pipeline seeds every
    // sibling-defined binding into `ctx.resource_bindings`, so the
    // cross-file case is covered by the top-of-function early return.
    // Branch retained as a safety net for typos and LSP single-file
    // fallback parses. Originally added for #2447.
    if crate::utils::NamespacedId::parse(full_str).is_none() {
        return Ok(as_resource_ref(Vec::new()));
    }

    Ok(EvalValue::from_value(Value::Concrete(
        ConcreteValue::enum_identifier(full_str.to_string()),
    )))
}

pub(crate) fn parse_primary_eval(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    parse_primary_eval_with_mode(pair, ctx, &ExpressionMode::untyped())
}

pub(crate) fn parse_primary_eval_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    // For primary, get inner content; otherwise process directly
    let inner = if pair.as_rule() == Rule::primary {
        first_inner(pair, "value", "primary expression")?
    } else {
        pair
    };

    if matches!(
        inner.as_rule(),
        Rule::read_resource_expr
            | Rule::wait_expr
            | Rule::upstream_state_expr
            | Rule::provider_expr
            | Rule::resource_expr
            | Rule::for_expr
            | Rule::if_expr
            | Rule::module_call
    ) && let Some(operand) = mode.integer_operand_context()
    {
        return crate::parser::let_binding::parse_integer_operand_primary(
            inner,
            ctx,
            operand.preprocessed_file,
            operand.source_file,
        );
    }

    match inner.as_rule() {
        Rule::resource_expr => {
            // Resource expressions cannot be used as attribute values (only valid in top-level let bindings)
            Err(ParseError::InvalidExpression {
                line: 0,
                message: "Resource expressions can only be used in let bindings".to_string(),
            })
        }
        Rule::list => {
            let mut items = Vec::new();
            let mut item_metadata = Vec::new();
            let mut pending = None;
            for (index, item) in inner.into_inner().enumerate() {
                let line = item.line_col().0;
                let item_mode = mode.at_list_index(index);
                let eval = parse_expression_eval_with_mode(item, ctx, &item_mode)?;
                if let EvalValue::PendingValue(slot) = eval {
                    pending.get_or_insert(slot);
                    continue;
                }
                let (value, metadata) = eval
                    .into_collection_element()
                    .map_err(|error| ParseError::from_eval_value(error, line))?;
                items.push(value);
                item_metadata.push(metadata);
            }
            if let Some(slot) = pending {
                return Ok(EvalValue::pending_value(slot));
            }
            let metadata = item_metadata
                .iter()
                .any(Option::is_some)
                .then_some(StructuralHandleMetadata::List(item_metadata));
            Ok(EvalValue::computed(
                Value::Concrete(ConcreteValue::List(items)),
                metadata,
            ))
        }
        Rule::map => {
            let mut map: IndexMap<String, Value> = IndexMap::new();
            let mut nested_blocks: IndexMap<String, Vec<Value>> = IndexMap::new();
            let mut authored_nested_blocks: IndexMap<String, usize> = IndexMap::new();
            let mut structural_handles = IndexMap::new();
            let mut pending = None;
            for entry in inner.into_inner() {
                match entry.as_rule() {
                    Rule::map_entry => {
                        let mut entry_inner = entry.into_inner();
                        let key_pair = next_pair(&mut entry_inner, "map key", "map entry")?;
                        let key = extract_key_string(key_pair)?;
                        let value_pair = next_pair(&mut entry_inner, "map value", "map entry")?;
                        let line = value_pair.line_col().0;
                        let entry_mode = mode.at_key(key.clone());
                        let value = parse_expression_eval_with_mode(value_pair, ctx, &entry_mode)?;
                        if let EvalValue::PendingValue(slot) = value {
                            pending.get_or_insert(slot);
                            continue;
                        }
                        let (value, metadata) = value
                            .into_collection_element()
                            .map_err(|error| ParseError::from_eval_value(error, line))?;
                        if let Some(metadata) = metadata {
                            structural_handles.insert(key.clone(), metadata);
                        }
                        map.insert(key, value);
                    }
                    Rule::nested_block => {
                        let mut block_inner = entry.into_inner();
                        let block_name =
                            next_pair(&mut block_inner, "block name", "nested block in map")?
                                .as_str()
                                .to_string();
                        let authored_count = authored_nested_blocks
                            .entry(block_name.clone())
                            .or_default();
                        let block_index = *authored_count;
                        *authored_count += 1;
                        let block_mode = mode
                            .at_struct_field(block_name.clone())
                            .at_list_index(block_index);
                        let block_attrs =
                            parse_block_contents_with_mode(block_inner, ctx, &block_mode)?;
                        let Some(block_attrs) = block_attrs else {
                            pending.get_or_insert_with(|| {
                                mode.pending_slot()
                                    .expect("a pending nested block has a typed sink")
                            });
                            continue;
                        };
                        nested_blocks
                            .entry(block_name)
                            .or_default()
                            .push(Value::Concrete(ConcreteValue::Map(block_attrs)));
                    }
                    _ => {}
                }
            }
            if let Some(slot) = pending {
                return Ok(EvalValue::pending_value(slot));
            }
            for (name, blocks) in nested_blocks {
                map.insert(name, Value::Concrete(ConcreteValue::List(blocks)));
            }
            let metadata = (!structural_handles.is_empty())
                .then_some(StructuralHandleMetadata::Map(structural_handles));
            Ok(EvalValue::computed(
                Value::Concrete(ConcreteValue::Map(map)),
                metadata,
            ))
        }
        Rule::namespaced_id => parse_namespaced_id_value(inner, ctx, Vec::new()),
        Rule::subscripted_id => {
            // `binding.field[idx]`, `binding.field.subfield[idx]…`,
            // `binding.field[idx].subfield…` — the namespaced_id portion
            // behaves like a plain namespaced_id (identifier shorthand
            // or resource ref), and any trailing chain of `.field` /
            // `[idx]` continuations is folded onto the resulting
            // `AccessPath` as an ordered `Vec<PathSegment>` so source
            // order is preserved (carina#3025).
            let mut parts = inner.into_inner();
            let head = next_pair(&mut parts, "namespaced_id", "subscripted_id")?;
            let mut segments: Vec<PathSegment> = Vec::new();
            for pair in parts {
                match pair.as_rule() {
                    Rule::index_access => {
                        let line = pair.line_col().0;
                        let index_expr_pair =
                            first_inner(pair, "index expression", "index access")?;
                        let index_value = parse_expression(index_expr_pair, ctx)?;
                        segments.push(PathSegment::Subscript {
                            index: subscript_from_value(index_value, line)?,
                        });
                    }
                    Rule::field_access => {
                        let ident = first_inner(pair, "identifier", "field access")?;
                        segments.push(PathSegment::Field {
                            name: ident.as_str().to_string(),
                        });
                    }
                    _ => continue,
                }
            }
            parse_namespaced_id_value(head, ctx, segments)
        }
        Rule::boolean => {
            let b = inner.as_str() == "true";
            Ok(EvalValue::from_value(Value::Concrete(ConcreteValue::Bool(
                b,
            ))))
        }
        Rule::float => {
            let f: f64 = inner
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: inner.line_col().0,
                    message: format!("invalid float literal: {e}"),
                })?;
            Ok(EvalValue::from_value(Value::Concrete(
                ConcreteValue::Float(f),
            )))
        }
        Rule::number => {
            let n: i64 = inner
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: inner.line_col().0,
                    message: format!("integer literal out of range: {e}"),
                })?;
            Ok(EvalValue::from_value(Value::Concrete(ConcreteValue::Int(
                n,
            ))))
        }
        Rule::duration_literal => {
            let line = inner.line_col().0;
            let value = parse_duration_literal(inner.as_str(), line)?;
            Ok(EvalValue::from_value(value))
        }
        Rule::string => parse_string_value(inner, ctx).map(EvalValue::from_value),
        Rule::function_call => {
            let (line, column) = inner.line_col();
            let mut fc_inner = inner.into_inner();
            let func_name = next_pair(&mut fc_inner, "function name", "function call")?
                .as_str()
                .to_string();
            let eval_args: Result<Vec<EvalValue>, ParseError> = fc_inner
                .map(|arg| parse_expression_eval(arg, ctx))
                .collect();
            let eval_args = eval_args?;

            // Check if the name refers to a Closure variable (direct call on closure).
            // The shared helper preserves the underlying typed builtin when
            // an argument is deferred.
            if let Some(EvalValue::Closure {
                name: fn_name,
                captured_args,
                remaining_arity,
            }) = ctx.get_variable(&func_name)
            {
                return crate::parser::functions::apply_parser_closure(
                    fn_name,
                    captured_args,
                    *remaining_arity,
                    eval_args,
                    ctx,
                    line,
                );
            }

            let builtin = crate::builtins::BuiltinFunction::from_name(&func_name);
            if builtin.is_some() {
                reject_builtin_handle_arguments(&eval_args, line)?;
            }

            // User functions own the lowering contract for their parameters:
            // Ref-typed arguments retain canonical handle bases even when a
            // different argument keeps the call deferred.
            if let Some(user_fn) = ctx.resolved_user_function(&func_name) {
                if !ctx.seeded_user_functions.contains(&func_name)
                    && eval_args.iter().all(is_static_eval)
                {
                    return evaluate_user_function_eval(&user_fn, eval_args, ctx, line)
                        .map(EvalValue::from_value);
                }
                let args = crate::parser::functions::lower_function_arguments(eval_args, line)?;
                crate::parser::functions::record_deferred_function_type_check(
                    &user_fn, &args, ctx, line,
                );
                return Ok(EvalValue::from_value(Value::Deferred(
                    DeferredValue::ResolvedFunctionCall {
                        function: user_fn,
                        functions: ctx.shared_user_function_scope(),
                        args,
                        source_line: (line).into(),
                    },
                )));
            }

            // Eagerly evaluate partial application (fewer args than arity → Closure)
            if let Some(arity) = crate::builtins::builtin_arity(&func_name)
                && eval_args.len() < arity
                && eval_args.iter().all(is_static_eval)
            {
                return crate::builtins::evaluate_builtin_with_config(
                    &func_name, &eval_args, ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line,
                    message: format!("{}(): {}", func_name, e),
                });
            }

            let args = crate::parser::functions::lower_function_arguments(eval_args, line)?;
            let builtin = builtin.ok_or(ParseError::UnknownFunction {
                name: func_name,
                line,
                column,
            })?;

            Ok(EvalValue::from_value(Value::Deferred(
                DeferredValue::FunctionCall {
                    name: builtin,
                    args,
                    source_line: (line).into(),
                },
            )))
        }
        Rule::variable_ref => {
            // variable_ref = { identifier ~ (field_access | index_access)* }
            // field_access = { "." ~ identifier }
            // index_access = { "[" ~ expression ~ "]" }
            let mut parts = inner.into_inner();
            let first_ident_pair = next_pair(&mut parts, "identifier", "variable reference")?;
            let line = first_ident_pair.line_col().0;
            let first_ident = first_ident_pair.as_str();

            // Collect all access steps (field or index)
            let access_steps: Vec<pest::iterators::Pair<Rule>> = parts.collect();

            if access_steps.is_empty() {
                // Simple variable reference (no access chain)
                if let Some(slot) = mode.record_bare_binding(ctx, first_ident, &first_ident_pair) {
                    return Ok(EvalValue::pending_value(slot));
                }
                //
                // Bare identifiers (`dedicated`, `enabled`) are emitted as
                // `ConcreteValue::EnumIdentifier` short form. The validator
                // performs schema-aware namespace expansion against the
                // enclosing attribute's `Enum` definition; if the
                // attribute isn't an enum, the validator surfaces a clear
                // type mismatch with the user-typed text intact. Binding
                // precedence is already honored by the `get_variable` arm
                // above — we only reach the `None` branch when the parser
                // has no in-scope variable / let-binding for this name.
                match ctx.lookup_binding(first_ident) {
                    ParseBindingRef::Value { value, .. } => Ok(value
                        .clone()
                        .with_reference_source(ctx.diagnostic_source_span(&first_ident_pair))),
                    ParseBindingRef::Structural(handle) => {
                        Ok(EvalValue::structural_handle(handle.clone()))
                    }
                    ParseBindingRef::Unbound => Ok(EvalValue::from_value(Value::Concrete(
                        ConcreteValue::enum_identifier(first_ident.to_string()),
                    ))),
                }
            } else {
                match ctx.lookup_binding(first_ident) {
                    ParseBindingRef::Value { value: bound, .. } => {
                        return apply_postfix_steps(
                            bound.clone().with_reference_source(
                                ctx.diagnostic_source_span(&first_ident_pair),
                            ),
                            access_steps,
                            ctx,
                        );
                    }
                    ParseBindingRef::Structural(handle)
                        if handle.is_parameter_handle() || handle.alias_name().is_some() =>
                    {
                        return apply_postfix_steps(
                            EvalValue::structural_handle(handle.clone()),
                            access_steps,
                            ctx,
                        );
                    }
                    ParseBindingRef::Structural(_) | ParseBindingRef::Unbound => {}
                }
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
                let mut binding_name = ValueString::plain(first_ident);
                let mut field_names: Vec<String> = Vec::new();
                let mut subscripts: Vec<Subscript> = Vec::new();
                let mut in_field_phase = false;

                for step in access_steps {
                    match step.as_rule() {
                        Rule::index_access => {
                            let step_line = step.line_col().0;
                            let index_expr_pair =
                                first_inner(step, "index expression", "index access")?;
                            let index_value = parse_expression(index_expr_pair, ctx)?;
                            let subscript = subscript_from_value(index_value, step_line)?;
                            if in_field_phase {
                                subscripts.push(subscript);
                            } else {
                                // Pre-field index — fold into the
                                // binding name string per the legacy
                                // convention so `for`-iteration
                                // addresses round-trip.
                                binding_name = match subscript {
                                    Subscript::Int { index } => ValueString::derived_from(
                                        format!("{}[{index}]", binding_name.expose_plaintext()),
                                        [&binding_name],
                                    ),
                                    Subscript::Str { key } => {
                                        let address = crate::utils::map_key_address(
                                            binding_name.expose_plaintext(),
                                            key.expose_plaintext(),
                                        );
                                        ValueString::derived_from(address, [&binding_name, &key])
                                    }
                                };
                            }
                        }
                        Rule::field_access => {
                            if !subscripts.is_empty() {
                                return Err(ParseError::InvalidExpression {
                                    line,
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
                    //
                    // No attribute selector → emit `BindingRef`. The
                    // legacy "ResourceRef with empty attribute" shape
                    // was the source of the empty-field diagnostic in
                    // #2847; the new variant makes that representation
                    // unrepresentable.
                    if matches!(
                        ctx.lookup_binding(first_ident),
                        ParseBindingRef::Structural(_)
                    ) {
                        return Ok(EvalValue::from_value(Value::Deferred(
                            DeferredValue::BindingRef {
                                binding: RelativeBindingName::new(binding_name),
                            },
                        )));
                    }
                    match ctx.lookup_binding(binding_name.expose_plaintext()) {
                        ParseBindingRef::Value { value, .. } => Ok(value.clone()),
                        ParseBindingRef::Structural(_) | ParseBindingRef::Unbound
                            if binding_name.is_diagnostic_sensitive() =>
                        {
                            Err(ParseError::InvalidExpression {
                                line,
                                message: "unresolved binding containing a value from env()"
                                    .to_string(),
                            })
                        }
                        ParseBindingRef::Structural(_) | ParseBindingRef::Unbound => Ok(
                            EvalValue::from_value(Value::Deferred(DeferredValue::BindingRef {
                                binding: RelativeBindingName::new(binding_name),
                            })),
                        ),
                    }
                } else {
                    let attribute_name = field_names.remove(0);
                    let path = AccessPath::with_fields_and_subscripts(
                        binding_name.clone(),
                        attribute_name,
                        field_names,
                        subscripts,
                    );
                    Ok(EvalValue::from_value(Value::Deferred(
                        DeferredValue::ResourceRef { path },
                    )))
                }
            }
        }
        Rule::if_expr => parse_if_value_expr_with_mode(inner, ctx, mode),
        Rule::expression => parse_expression_eval_with_mode(inner, ctx, mode),
        Rule::read_resource_expr
        | Rule::wait_expr
        | Rule::upstream_state_expr
        | Rule::provider_expr
        | Rule::for_expr
        | Rule::module_call => Err(ParseError::InvalidExpression {
            line: ctx.original_position(&inner)?.line(),
            message: format!(
                "structural {:?} expression reached ordinary value lowering",
                inner.as_rule()
            ),
        }),
        other => Err(ParseError::InternalError {
            expected: "a supported primary value".to_string(),
            context: format!("primary expression contained {other:?}"),
        }),
    }
}
