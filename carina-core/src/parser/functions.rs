//! User-defined function parsing, type checking, and evaluation.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ast::{FnParam, TypeExpr, UserFunction, UserFunctionBody};
use super::context::{ParseContext, next_pair};
use super::error::{ParseError, ParseWarning};
use super::types::parse_type_expr;
use super::util::value_type_name;
use super::{ProviderContext, Rule, parse_expression};
use crate::eval_value::EvalValue;
use crate::resource::{ConcreteValue, DeferredValue, UnknownReason, Value};
use crate::schema::{
    TypeIdentity, validate_http_response_status_code, validate_ipv4_address, validate_ipv4_cidr,
    validate_ipv6_address, validate_ipv6_cidr, validate_redirect_host, validate_redirect_path,
    validate_redirect_port, validate_redirect_protocol, validate_redirect_query,
};
use indexmap::IndexMap;
use std::collections::HashMap;

/// Parse a user-defined function definition
pub(super) fn parse_fn_def(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<UserFunction, ParseError> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "function name", "fn_def")?
        .as_str()
        .to_string();

    // Parse parameters (optional)
    let mut params = Vec::new();
    let next = next_pair(&mut inner, "fn_params or fn_body", "fn_def")?;
    let next_token = if next.as_rule() == Rule::fn_params {
        // Parse parameter list
        for param_pair in next.into_inner() {
            if param_pair.as_rule() == Rule::fn_param {
                let mut param_inner = param_pair.into_inner();
                let param_name = next_pair(&mut param_inner, "parameter name", "fn_param")?
                    .as_str()
                    .to_string();
                // Parse optional type annotation (: type_expr)
                let mut param_type = None;
                let mut default = None;
                for remaining in param_inner {
                    match remaining.as_rule() {
                        Rule::type_expr => {
                            param_type = Some(parse_type_expr(remaining, ctx.config, warnings)?);
                        }
                        _ => {
                            // This is the default expression
                            let default_ctx = ParseContext::new(ctx.config);
                            default = Some(parse_expression(remaining, &default_ctx)?);
                        }
                    }
                }
                // Validate: required params must come before optional params
                if default.is_none() && params.iter().any(|p: &FnParam| p.default.is_some()) {
                    return Err(ParseError::UserFunctionError(format!(
                        "in function '{name}': required parameter '{param_name}' cannot follow optional parameter"
                    )));
                }
                params.push(FnParam {
                    name: param_name,
                    param_type,
                    default,
                });
            }
        }
        next_pair(&mut inner, "type_expr or fn_body", "fn_def")?
    } else {
        next
    };

    // Parse optional return type annotation (: type_expr)
    let (return_type, body_pair) = if next_token.as_rule() == Rule::type_expr {
        let rt = parse_type_expr(next_token, ctx.config, warnings)?;
        let bp = next_pair(&mut inner, "fn_body", "fn_def")?;
        (Some(rt), bp)
    } else {
        (None, next_token)
    };

    // Parse body: fn_local_let* ~ (resource_expr | read_resource_expr | expression)
    let mut local_lets = Vec::new();
    let mut body: Option<UserFunctionBody> = None;

    // Create a context where parameters are registered as variables
    // so that param references in the body are resolved as variable refs
    let mut body_ctx = ParseContext::new(ctx.config);
    for p in &params {
        body_ctx.set_variable(
            p.name.clone(),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::FnParam {
                name: p.name.clone(),
            })),
        );
    }

    for body_inner in body_pair.into_inner() {
        match body_inner.as_rule() {
            Rule::fn_local_let => {
                let mut let_inner = body_inner.into_inner();
                let let_name = next_pair(&mut let_inner, "let name", "fn_local_let")?
                    .as_str()
                    .to_string();
                let let_expr = parse_expression(
                    next_pair(&mut let_inner, "let expression", "fn_local_let")?,
                    &body_ctx,
                )?;
                body_ctx.set_variable(
                    let_name.clone(),
                    Value::Deferred(DeferredValue::Unknown(UnknownReason::FnLocal {
                        name: let_name.clone(),
                    })),
                );
                local_lets.push((let_name, let_expr));
            }
            _ => {
                // This should be the expression (the body)
                body = Some(UserFunctionBody(parse_expression(body_inner, &body_ctx)?));
            }
        }
    }

    let body = body.ok_or_else(|| ParseError::InternalError {
        expected: "body expression".to_string(),
        context: "fn_def".to_string(),
    })?;

    Ok(UserFunction {
        name,
        params,
        return_type,
        local_lets,
        body,
    })
}

enum PreparedUserFunctionCall<'cfg> {
    Ready {
        child_ctx: Box<ParseContext<'cfg>>,
        substitutions: HashMap<String, Value>,
        function_derived: HashMap<String, bool>,
    },
    Deferred(Vec<Value>),
}

/// Prepare a user-defined function call: validate args, build substitutions, and return
/// the child context with all parameters and local lets resolved.
fn prepare_user_function_call<'cfg>(
    func: &UserFunction,
    args: &[Value],
    argument_origins: &[bool],
    ctx: &ParseContext<'cfg>,
) -> Result<PreparedUserFunctionCall<'cfg>, ParseError> {
    let fn_name = &func.name;

    // Check recursion
    if ctx.evaluating_functions.contains(fn_name) {
        return Err(ParseError::RecursiveFunction(fn_name.clone()));
    }

    // Validate argument count
    let required_count = func.params.iter().filter(|p| p.default.is_none()).count();
    let max_count = func.params.len();
    if args.len() < required_count {
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}' expects at least {required_count} argument(s), got {}",
            args.len()
        )));
    }
    if args.len() > max_count {
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}' expects at most {max_count} argument(s), got {}",
            args.len()
        )));
    }

    // Build substitution map: param_name -> value, and type-check annotated params
    let mut substitutions: HashMap<String, Value> = HashMap::new();
    let mut function_derived: HashMap<String, bool> = HashMap::new();
    let mut effective_args = Vec::with_capacity(func.params.len());
    for (i, param) in func.params.iter().enumerate() {
        let (value, authored_origin) = if i < args.len() {
            (
                args[i].clone(),
                argument_origins.get(i).copied().unwrap_or(false),
            )
        } else {
            let value = param.default.clone().unwrap();
            let origin = contains_function_call(&value);
            (value, origin)
        };
        // A direct parser call can reach this seam with a nested builtin still
        // represented as `DeferredValue::FunctionCall`. The shared value
        // checker intentionally defers unknown call results, so evaluate what
        // is knowable first and validate the actual argument value. This also
        // ensures a bad secret argument is blamed on the parameter rather than
        // flowing into the body and being blamed on the return annotation.
        let evaluated = try_evaluate_fn_value(value, ctx)?;
        let value = evaluated.value;
        let derived = authored_origin || evaluated.function_derived;
        // Type-check if the parameter has a type annotation
        if let Some(ref type_expr) = param.param_type {
            check_fn_arg_type(fn_name, &param.name, type_expr, &value, derived, ctx)?;
        }
        substitutions.insert(param.name.clone(), value.clone());
        function_derived.insert(param.name.clone(), derived);
        effective_args.push(value);
    }

    // A sibling- or forward-declared call is not resolvable in this file's
    // parse context. Keep the *outer* call intact so the directory-merged
    // resolver can check the same parameter after it learns the nested
    // callee's result. Evaluating the body now would silently discard an
    // unused deferred argument (`f(g(1))` where `f` returns a constant).
    if effective_args.iter().any(contains_function_call) {
        return Ok(PreparedUserFunctionCall::Deferred(effective_args));
    }

    // Create a child context with recursion tracking
    let mut child_ctx = ctx.clone();
    child_ctx.evaluating_functions.push(fn_name.clone());

    // Evaluate local lets, substituting and resolving each one
    for (let_name, let_expr) in &func.local_lets {
        let derived = is_function_derived(let_expr, &function_derived);
        let substituted = substitute_fn_params(let_expr, &substitutions);
        let evaluated = try_evaluate_fn_value(substituted, &child_ctx)?;
        let derived = derived || evaluated.function_derived;
        child_ctx.set_variable(let_name.clone(), evaluated.value.clone());
        substitutions.insert(let_name.clone(), evaluated.value);
        function_derived.insert(let_name.clone(), derived);
    }

    Ok(PreparedUserFunctionCall::Ready {
        child_ctx: Box::new(child_ctx),
        substitutions,
        function_derived,
    })
}

/// Adapter that turns a [`ProviderContext`] into the
/// [`crate::schema::CustomTypeLookup`] shape consumed by the schema-walk
/// validator. The lookup is keyed on a structured [`TypeIdentity`], so
/// [`validate_custom_type`] runs the registered validator chain against
/// the exact provider-scoped identity — no flat-string normalization.
/// The returned closure may be hoisted out of any per-resource loop —
/// it borrows the context only.
pub fn provider_context_lookup(
    ctx: &ProviderContext,
) -> impl Fn(&TypeIdentity, &Value) -> Result<(), crate::schema::TypeError> + use<'_> {
    move |identity, value| {
        validate_custom_type(identity, value, ctx)
            .map_err(|message| crate::schema::TypeError::ValidationFailed { message })
    }
}

/// Validate a value against a custom type (`Ipv4Cidr`, `Ipv4Address`, …).
/// Returns Ok(()) if the value passes validation or cannot be validated
/// statically (e.g., ResourceRef, FunctionCall, Interpolation are
/// deferred).
///
/// Checks built-in validators first (matched on the identity's `kind`),
/// then falls back to custom validators registered in the
/// [`ProviderContext`], keyed by the full structured [`TypeIdentity`].
pub fn validate_custom_type(
    identity: &TypeIdentity,
    value: &Value,
    config: &ProviderContext,
) -> Result<(), String> {
    // Built-in DSL custom types carry no provider axis — match on `kind`.
    let builtin = identity.provider.is_none() && identity.segments.is_empty();
    fn text(value: &Value) -> Option<&str> {
        value
            .as_concrete()
            .and_then(|concrete| concrete.as_string_like())
    }
    match (builtin.then_some(identity.kind.as_str()), value) {
        (Some("Ipv4Cidr"), value) if text(value).is_some() => {
            validate_ipv4_cidr(text(value).unwrap())
        }
        (Some("Ipv4Address"), value) if text(value).is_some() => {
            validate_ipv4_address(text(value).unwrap())
        }
        (Some("Ipv6Cidr"), value) if text(value).is_some() => {
            validate_ipv6_cidr(text(value).unwrap())
        }
        (Some("Ipv6Address"), value) if text(value).is_some() => {
            validate_ipv6_address(text(value).unwrap())
        }
        (Some("HttpResponseStatusCode"), value) if text(value).is_some() => {
            validate_http_response_status_code(text(value).unwrap())
        }
        (Some("RedirectHost"), value) if text(value).is_some() => {
            validate_redirect_host(text(value).unwrap())
        }
        (Some("RedirectPath"), value) if text(value).is_some() => {
            validate_redirect_path(text(value).unwrap())
        }
        (Some("RedirectPort"), value) if text(value).is_some() => {
            validate_redirect_port(text(value).unwrap())
        }
        (Some("RedirectProtocol"), value) if text(value).is_some() => {
            validate_redirect_protocol(text(value).unwrap())
        }
        (Some("RedirectQuery"), value) if text(value).is_some() => {
            validate_redirect_query(text(value).unwrap())
        }
        (_, Value::Deferred(DeferredValue::ResourceRef { .. })) => Ok(()), // will be resolved later
        (_, Value::Deferred(DeferredValue::FunctionCall { .. })) => Ok(()), // will be resolved later
        (_, Value::Deferred(DeferredValue::Interpolation(_))) => Ok(()), // will be resolved later
        (_, Value::Deferred(DeferredValue::Unknown(_))) => Ok(()), // resolved at upstream apply
        (_, value) if text(value).is_some() => {
            // Both `String` (quoted-literal source) and `EnumIdentifier`
            // (bare or namespaced identifier source) are accepted: this
            // lookup runs from `walk_custom_lookup`, which is reached
            // *after* `expand_enum_shorthand` has normalized the value
            // for `validate_custom`. The strict-shape rejection for
            // quoted literals on namespaced Custom types happens in the
            // LSP / CLI surface code (carina#2986 Phase 4), not here —
            // this fallback validator just needs the textual payload.
            // Check custom validators from config (schema-extracted),
            // keyed by the full structured identity so two providers'
            // same-named types do not collide.
            let s = text(value).unwrap();
            if let Some(validator) = config.validators.get(identity) {
                validator(s)?;
            }
            // Fall back to factory-based validator (e.g., WASM providers)
            if let Some(ref factory_validator) = config.custom_type_validator {
                factory_validator(identity, s)
            } else {
                Ok(())
            }
        }
        (_, value) => Err(format!(
            "expected {}, got {}",
            identity,
            value_type_name(value)
        )),
    }
}

/// Check that a function argument matches the declared parameter type.
fn check_fn_arg_type(
    fn_name: &str,
    param_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
    function_derived: bool,
    ctx: &ParseContext,
) -> Result<(), ParseError> {
    if let Err(mismatch) =
        crate::validation::typecheck::check_type_expr_value(type_expr, value, ctx.config)
    {
        let mismatch = if function_derived {
            mismatch.redact_evaluated_value(type_expr, value)
        } else {
            mismatch
        };
        return Err(function_argument_type_error(fn_name, param_name, mismatch));
    }

    // A parser placeholder string can name a known resource binding. Feed
    // that source handle through the same lifted TypeInSchema relation used by
    // BindingIndex; do not maintain a parser-local resource-type relation.
    if let TypeExpr::Ref(expected_path) = type_expr
        && let Value::Concrete(ConcreteValue::String(value)) = value
        && let Some(ref_name) = value
            .strip_prefix("${")
            .and_then(|value| value.strip_suffix('}'))
        && let Some(resource) = ctx.resource_bindings.get(ref_name)
    {
        let actual_provider = &resource.id.provider;
        let actual_type = &resource.id.resource_type;
        let source = crate::schema::AttributeType::resource_handle(
            actual_provider.clone(),
            actual_type.clone(),
        );
        let sink = crate::validation::lift_type_expr(type_expr)
            .expect("TypeExpr::Ref always lifts to a resource handle");
        if !source
            .in_schema(crate::schema::empty_defs_for_schema_walks())
            .is_assignable_to(crate::schema::TypeInSchema::schemaless(&sink))
        {
            return Err(ParseError::UserFunctionError(format!(
                "function '{fn_name}': parameter '{param_name}' expects resource type '{expected_path}', got {actual_provider}.{actual_type}"
            )));
        }
    }

    Ok(())
}

fn function_argument_type_error(
    fn_name: &str,
    param_name: &str,
    mismatch: crate::validation::typecheck::TypeExprValueError,
) -> ParseError {
    ParseError::UserFunctionError(format!(
        "function '{fn_name}': parameter '{param_name}': {mismatch}"
    ))
}

/// Check that a function's return value matches the declared return type.
fn check_fn_return_type(
    fn_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
    function_derived: bool,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    if let Err(mismatch) =
        crate::validation::typecheck::check_type_expr_value(type_expr, value, config)
    {
        let mismatch = if function_derived {
            mismatch.redact_evaluated_value(type_expr, value)
        } else {
            mismatch
        };
        return Err(function_return_type_error(fn_name, mismatch));
    }
    Ok(())
}

fn function_return_type_error(
    fn_name: &str,
    mismatch: crate::validation::typecheck::TypeExprValueError,
) -> ParseError {
    ParseError::UserFunctionError(format!("function '{fn_name}': return value: {mismatch}"))
}

/// Evaluate a user-defined function call by substituting arguments into the body
pub(crate) fn evaluate_user_function(
    func: &UserFunction,
    args: &[Value],
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let argument_origins = args.iter().map(contains_function_call).collect::<Vec<_>>();
    evaluate_user_function_with_origins(func, args, &argument_origins, ctx)
}

pub(crate) fn evaluate_user_function_with_origins(
    func: &UserFunction,
    args: &[Value],
    argument_origins: &[bool],
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let (child_ctx, substitutions, function_derived) =
        match prepare_user_function_call(func, args, argument_origins, ctx)? {
            PreparedUserFunctionCall::Ready {
                child_ctx,
                substitutions,
                function_derived,
            } => (child_ctx, substitutions, function_derived),
            PreparedUserFunctionCall::Deferred(args) => {
                return Ok(Value::Deferred(DeferredValue::FunctionCall {
                    name: func.name.clone(),
                    args,
                }));
            }
        };

    let UserFunctionBody(body) = &func.body;
    let body_is_function_derived = is_function_derived(body, &function_derived);
    let substituted_body = substitute_fn_params(body, &substitutions);
    let evaluated = try_evaluate_fn_value(substituted_body, &child_ctx)?;
    let result_is_function_derived = body_is_function_derived || evaluated.function_derived;
    let result = evaluated.value.canonicalize();
    // Check return type if annotated
    if let Some(ref return_type) = func.return_type {
        check_fn_return_type(
            &func.name,
            return_type,
            &result,
            result_is_function_derived,
            child_ctx.config,
        )?;
    }
    Ok(result)
}

pub(crate) fn contains_function_call(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::FunctionCall { .. }) => true,
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(contains_function_call),
        Value::Concrete(ConcreteValue::Map(entries)) => {
            entries.values().any(contains_function_call)
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(part, crate::resource::InterpolationPart::Expr(value) if contains_function_call(value))
        }),
        Value::Deferred(DeferredValue::Secret(inner)) => contains_function_call(inner),
        _ => false,
    }
}

fn is_function_derived(value: &Value, bindings: &HashMap<String, bool>) -> bool {
    match value {
        Value::Deferred(DeferredValue::Unknown(UnknownReason::FnParam { name }))
        | Value::Deferred(DeferredValue::Unknown(UnknownReason::FnLocal { name })) => {
            bindings.get(name).copied().unwrap_or(false)
        }
        Value::Deferred(DeferredValue::FunctionCall { .. }) => true,
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(|item| is_function_derived(item, bindings))
        }
        Value::Concrete(ConcreteValue::Map(entries)) => entries
            .values()
            .any(|item| is_function_derived(item, bindings)),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(part, crate::resource::InterpolationPart::Expr(value) if is_function_derived(value, bindings))
        }),
        Value::Deferred(DeferredValue::Secret(inner)) => is_function_derived(inner, bindings),
        _ => false,
    }
}

/// Recursively substitute function parameter placeholders with actual values
fn substitute_fn_params(value: &Value, substitutions: &HashMap<String, Value>) -> Value {
    match value {
        Value::Deferred(DeferredValue::Unknown(UnknownReason::FnParam { name }))
        | Value::Deferred(DeferredValue::Unknown(UnknownReason::FnLocal { name })) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| value.clone()),
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|v| substitute_fn_params(v, substitutions))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_fn_params(v, substitutions)))
                .collect(),
        )),
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            Value::Deferred(DeferredValue::FunctionCall {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|a| substitute_fn_params(a, substitutions))
                    .collect(),
            })
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            Value::Deferred(DeferredValue::Interpolation(
                parts
                    .iter()
                    .map(|p| match p {
                        crate::resource::InterpolationPart::Expr(v) => {
                            crate::resource::InterpolationPart::Expr(substitute_fn_params(
                                v,
                                substitutions,
                            ))
                        }
                        other => other.clone(),
                    })
                    .collect(),
            ))
        }
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(substitute_fn_params(inner, substitutions)),
        )),
        Value::Deferred(DeferredValue::Unknown(_)) => value.clone(),
        other => other.clone(),
    }
}

struct EvaluatedFnValue {
    value: Value,
    function_derived: bool,
}

impl EvaluatedFnValue {
    fn authored(value: Value) -> Self {
        Self {
            value,
            function_derived: false,
        }
    }

    fn derived(value: Value) -> Self {
        Self {
            value,
            function_derived: true,
        }
    }
}

/// Try to evaluate a value (resolve function calls including user-defined ones).
/// The provenance bit is intentionally kept out of [`Value`]: it exists only
/// while crossing a typed evaluator boundary, where diagnostics must avoid
/// rendering generated plaintext as though it were an authored literal.
fn try_evaluate_fn_value(value: Value, ctx: &ParseContext) -> Result<EvaluatedFnValue, ParseError> {
    match value {
        Value::Deferred(DeferredValue::FunctionCall { ref name, ref args }) => {
            // First, recursively evaluate arguments
            let evaluated_args: Result<Vec<EvaluatedFnValue>, ParseError> = args
                .iter()
                .map(|a| try_evaluate_fn_value(a.clone(), ctx))
                .collect();
            let evaluated_args = evaluated_args?;
            let argument_origins = evaluated_args
                .iter()
                .map(|evaluated| evaluated.function_derived)
                .collect::<Vec<_>>();
            let evaluated_args = evaluated_args
                .into_iter()
                .map(|evaluated| evaluated.value)
                .collect::<Vec<_>>();

            // Check if the name refers to a Closure variable
            if let Some(EvalValue::Closure {
                name: fn_name,
                captured_args,
                remaining_arity,
            }) = ctx.get_variable(name)
            {
                let eval_args: Vec<EvalValue> = evaluated_args
                    .iter()
                    .cloned()
                    .map(EvalValue::from_value)
                    .collect();
                let result = crate::builtins::apply_closure_with_config(
                    fn_name,
                    captured_args,
                    *remaining_arity,
                    &eval_args,
                    ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: e,
                })?;
                return result
                    .into_value()
                    .map_err(|leak| ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "applying closure '{}' (still needs {} arg(s)) leaves a closure; \
                         finish the partial application before using the result as data",
                            leak.name, leak.remaining_arity
                        ),
                    })
                    .map(EvaluatedFnValue::derived);
            }

            // Try built-in first (with config for decrypt support)
            let eval_args: Vec<EvalValue> = evaluated_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            match crate::builtins::evaluate_builtin_with_config(name, &eval_args, ctx.config) {
                Ok(result) => result
                    .into_value()
                    .map_err(|leak| ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "{}(): produced a closure '{}' (still needs {} arg(s)); \
                         finish the partial application before using the result as data",
                            name, leak.name, leak.remaining_arity
                        ),
                    })
                    .map(EvaluatedFnValue::derived),
                Err(_builtin_err) => {
                    // Try user-defined function declared in *this* file.
                    if let Some(user_fn) = ctx.user_functions.get(name) {
                        evaluate_user_function_with_origins(
                            user_fn,
                            &evaluated_args,
                            &argument_origins,
                            ctx,
                        )
                        .map(EvaluatedFnValue::derived)
                    } else {
                        // Defer the call — the user-fn may live in a
                        // sibling `.crn` and only become visible after
                        // `merge_parsed_file` (#2444). Truly-undefined
                        // names are caught at the post-merge resolver
                        // pass (`resolve_value_with_config`), which has
                        // the full directory's `user_functions`.
                        //
                        // The directory-aware parse pipeline (#2817)
                        // does not seed `user_functions` into the
                        // per-file `ctx` — only `variables` and
                        // `resource_bindings` are seeded — so this
                        // defer path remains the route by which a
                        // sibling-defined user fn becomes resolvable.
                        Ok(EvaluatedFnValue::authored(Value::Deferred(
                            DeferredValue::FunctionCall {
                                name: name.clone(),
                                args: evaluated_args,
                            },
                        )))
                    }
                }
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let evaluated: Result<Vec<EvaluatedFnValue>, ParseError> = items
                .into_iter()
                .map(|v| try_evaluate_fn_value(v, ctx))
                .collect();
            let evaluated = evaluated?;
            let function_derived = evaluated.iter().any(|value| value.function_derived);
            let values = evaluated.into_iter().map(|value| value.value).collect();
            Ok(EvaluatedFnValue {
                value: Value::Concrete(ConcreteValue::List(values)),
                function_derived,
            })
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let evaluated: Result<IndexMap<String, EvaluatedFnValue>, ParseError> = map
                .into_iter()
                .map(|(k, v)| try_evaluate_fn_value(v, ctx).map(|ev| (k, ev)))
                .collect();
            let evaluated = evaluated?;
            let function_derived = evaluated.values().any(|value| value.function_derived);
            let values = evaluated
                .into_iter()
                .map(|(key, value)| (key, value.value))
                .collect();
            Ok(EvaluatedFnValue {
                value: Value::Concrete(ConcreteValue::Map(values)),
                function_derived,
            })
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let mut function_derived = false;
            let mut evaluated = Vec::with_capacity(parts.len());
            for part in parts {
                match part {
                    crate::resource::InterpolationPart::Expr(value) => {
                        let value = try_evaluate_fn_value(value, ctx)?;
                        function_derived |= value.function_derived;
                        evaluated.push(crate::resource::InterpolationPart::Expr(value.value));
                    }
                    other => evaluated.push(other),
                }
            }
            Ok(EvaluatedFnValue {
                value: Value::Deferred(DeferredValue::Interpolation(evaluated)),
                function_derived,
            })
        }
        other => Ok(EvaluatedFnValue::authored(other)),
    }
}
