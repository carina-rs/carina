//! User-defined function parsing, type checking, and evaluation.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ast::{FnParam, TypeExpr, UserFunction, UserFunctionBody};
use super::context::{ParseContext, next_pair};
use super::error::{ParseError, ParseWarning};
use super::types::parse_type_expr;
use super::util::{snake_to_pascal, value_type_name};
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

/// Prepare a user-defined function call: validate args, build substitutions, and return
/// the child context with all parameters and local lets resolved.
pub(super) fn prepare_user_function_call<'cfg>(
    func: &UserFunction,
    args: &[Value],
    ctx: &ParseContext<'cfg>,
) -> Result<(ParseContext<'cfg>, HashMap<String, Value>), ParseError> {
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
    for (i, param) in func.params.iter().enumerate() {
        let value = if i < args.len() {
            args[i].clone()
        } else {
            param.default.clone().unwrap()
        };
        // Type-check if the parameter has a type annotation
        if let Some(ref type_expr) = param.param_type {
            check_fn_arg_type(fn_name, &param.name, type_expr, &value, ctx)?;
        }
        substitutions.insert(param.name.clone(), value);
    }

    // Create a child context with recursion tracking
    let mut child_ctx = ctx.clone();
    child_ctx.evaluating_functions.push(fn_name.clone());

    // Evaluate local lets, substituting and resolving each one
    for (let_name, let_expr) in &func.local_lets {
        let substituted = substitute_fn_params(let_expr, &substitutions);
        let evaluated = try_evaluate_fn_value(substituted, &child_ctx)?;
        child_ctx.set_variable(let_name.clone(), evaluated.clone());
        substitutions.insert(let_name.clone(), evaluated);
    }

    Ok((child_ctx, substitutions))
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
    ctx: &ParseContext,
) -> Result<(), ParseError> {
    // `Value::Deferred(DeferredValue::Unknown)` resolves at upstream apply; the concrete type
    // is unknowable at parse time. Skip the type check — same convention
    // the schema validator follows.
    if matches!(value, Value::Deferred(DeferredValue::Unknown(_))) {
        return Ok(());
    }
    let type_matches = match type_expr {
        TypeExpr::String => matches!(
            value,
            Value::Concrete(ConcreteValue::String(_))
                | Value::Deferred(DeferredValue::Interpolation(_))
                | Value::Deferred(DeferredValue::ResourceRef { .. })
        ),
        TypeExpr::Int => matches!(value, Value::Concrete(ConcreteValue::Int(_))),
        TypeExpr::Float => matches!(value, Value::Concrete(ConcreteValue::Float(_))),
        TypeExpr::Bool => matches!(value, Value::Concrete(ConcreteValue::Bool(_))),
        TypeExpr::Duration => matches!(value, Value::Concrete(ConcreteValue::Duration(_))),
        TypeExpr::List(_) => matches!(value, Value::Concrete(ConcreteValue::List(_))),
        TypeExpr::Map(_) => matches!(value, Value::Concrete(ConcreteValue::Map(_))),
        // Simple types (cidr, ipv4_address, arn, etc.) are string subtypes at runtime
        TypeExpr::Simple(name) => {
            if !matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ) {
                false
            } else {
                // `Simple` annotations name a built-in DSL custom type
                // (snake-cased, e.g. `ipv4_cidr`); project it onto a
                // bare, provider-agnostic identity.
                let identity = TypeIdentity::bare(snake_to_pascal(name));
                if let Err(e) = validate_custom_type(&identity, value, ctx.config) {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': parameter '{param_name}' type '{name}' validation failed: {e}"
                    )));
                }
                true
            }
        }
        // Resource type refs: check that the argument is a binding of the correct resource type
        TypeExpr::Ref(expected_path) => {
            // The argument is passed as a ResourceRef-like string "${binding_name}"
            // or as a direct ResourceRef. Check if it corresponds to a resource binding
            // of the expected type.
            if let Value::Concrete(ConcreteValue::String(s)) = value
                && let Some(ref_name) = s.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
                && let Some(resource) = ctx.resource_bindings.get(ref_name)
            {
                let actual_provider = &resource.id.provider;
                let actual_type = &resource.id.resource_type;
                if actual_provider != &expected_path.provider
                    || actual_type != &expected_path.resource_type
                {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': parameter '{param_name}' expects resource type '{expected_path}', got {actual_provider}.{actual_type}"
                    )));
                }
            }
            // If not found in bindings, skip validation (forward ref or dynamic)
            true
        }
        // Unresolved dotted types should normally be classified before
        // function type checking. If one reaches here through an early parse
        // path, it still has string runtime shape.
        TypeExpr::DottedUnresolved(_) => matches!(
            value,
            Value::Concrete(ConcreteValue::String(_))
                | Value::Deferred(DeferredValue::Interpolation(_))
                | Value::Deferred(DeferredValue::ResourceRef { .. })
        ),
        // Schema types (awscc.ec2.VpcId, etc.) are string subtypes with provider validators
        TypeExpr::SchemaType {
            provider,
            path,
            type_name,
        } => {
            if !matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ) {
                false
            } else {
                let identity = TypeIdentity::from_schema_type(provider, path, type_name);
                if let Err(e) = validate_custom_type(&identity, value, ctx.config) {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': parameter '{param_name}' type '{type_expr}' validation failed: {e}"
                    )));
                }
                true
            }
        }
        TypeExpr::Struct { .. } => matches!(value, Value::Concrete(ConcreteValue::Map(_))),
        // Closed-set string literal: only the exact string matches.
        // Interpolations and ResourceRefs are rejected here because
        // their runtime expansion is not knowable at parse time.
        // See carina-rs/carina#2611.
        TypeExpr::StringLiteral(expected) => {
            matches!(value, Value::Concrete(ConcreteValue::String(s)) if s == expected)
        }
        // Union: matches if at least one member's structural-shape
        // arm accepts the value. We only inspect literal/string-shape
        // members here — that is what `'dev' | 'prod'`-style unions
        // need today. Mixed unions (`String | Int`) become reachable
        // when the grammar opens up further; this arm is the
        // single point that needs updating then.
        TypeExpr::Union(members) => members.iter().any(|m| match m {
            TypeExpr::StringLiteral(expected) => {
                matches!(value, Value::Concrete(ConcreteValue::String(s)) if s == expected)
            }
            TypeExpr::String => matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ),
            TypeExpr::Simple(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Bool
            | TypeExpr::Duration
            | TypeExpr::List(_)
            | TypeExpr::Map(_)
            | TypeExpr::Struct { .. }
            | TypeExpr::Union(_)
            | TypeExpr::Unknown => false,
        }),
        // Inference sentinel: never matches a concrete value.
        TypeExpr::Unknown => false,
    };
    if !type_matches {
        let actual_type = value_type_name(value);
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}': parameter '{param_name}' expects type '{type_expr}', got {actual_type}"
        )));
    }
    Ok(())
}

/// Check that a function's return value matches the declared return type.
fn check_fn_return_type(
    fn_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    // Same skip as `check_fn_arg_type`: a `Value::Deferred(DeferredValue::Unknown)` propagated
    // through a typed user fn carries no concrete type at parse time.
    if matches!(value, Value::Deferred(DeferredValue::Unknown(_))) {
        return Ok(());
    }
    let type_matches = match type_expr {
        TypeExpr::String => matches!(
            value,
            Value::Concrete(ConcreteValue::String(_))
                | Value::Deferred(DeferredValue::Interpolation(_))
                | Value::Deferred(DeferredValue::ResourceRef { .. })
        ),
        TypeExpr::Int => matches!(value, Value::Concrete(ConcreteValue::Int(_))),
        TypeExpr::Float => matches!(value, Value::Concrete(ConcreteValue::Float(_))),
        TypeExpr::Bool => matches!(value, Value::Concrete(ConcreteValue::Bool(_))),
        TypeExpr::Duration => matches!(value, Value::Concrete(ConcreteValue::Duration(_))),
        TypeExpr::List(_) => matches!(value, Value::Concrete(ConcreteValue::List(_))),
        TypeExpr::Map(_) => matches!(value, Value::Concrete(ConcreteValue::Map(_))),
        // Simple types (cidr, ipv4_address, arn, etc.) — validate the value
        TypeExpr::Simple(name) => {
            if !matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ) {
                false
            } else {
                let identity = TypeIdentity::bare(snake_to_pascal(name));
                if let Err(e) = validate_custom_type(&identity, value, config) {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': return type '{name}' validation failed: {e}"
                    )));
                }
                true
            }
        }
        // Resource type refs are string-shaped for value functions.
        TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_) => matches!(
            value,
            Value::Concrete(ConcreteValue::String(_))
                | Value::Deferred(DeferredValue::Interpolation(_))
                | Value::Deferred(DeferredValue::ResourceRef { .. })
        ),
        // Schema types: validate returned value against the provider validator
        TypeExpr::SchemaType {
            provider,
            path,
            type_name,
        } => {
            if !matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ) {
                false
            } else {
                let identity = TypeIdentity::from_schema_type(provider, path, type_name);
                if let Err(e) = validate_custom_type(&identity, value, config) {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': return type '{type_name}' validation failed: {e}"
                    )));
                }
                true
            }
        }
        TypeExpr::Struct { .. } => matches!(value, Value::Concrete(ConcreteValue::Map(_))),
        // Closed-set string literal: only the exact string matches.
        // See carina-rs/carina#2611.
        TypeExpr::StringLiteral(expected) => {
            matches!(value, Value::Concrete(ConcreteValue::String(s)) if s == expected)
        }
        // Union: matches if at least one member accepts the value.
        // Same shape as in `check_fn_arg_type` — only literal/string
        // members are inspected today.
        TypeExpr::Union(members) => members.iter().any(|m| match m {
            TypeExpr::StringLiteral(expected) => {
                matches!(value, Value::Concrete(ConcreteValue::String(s)) if s == expected)
            }
            TypeExpr::String => matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ),
            TypeExpr::Simple(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Bool
            | TypeExpr::Duration
            | TypeExpr::List(_)
            | TypeExpr::Map(_)
            | TypeExpr::Struct { .. }
            | TypeExpr::Union(_)
            | TypeExpr::Unknown => false,
        }),
        // Inference sentinel: never matches a concrete value.
        TypeExpr::Unknown => false,
    };
    if !type_matches {
        let actual_type = value_type_name(value);
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}': return type '{type_expr}' does not match actual return value of type {actual_type}"
        )));
    }
    Ok(())
}

/// Evaluate a user-defined function call by substituting arguments into the body
pub(crate) fn evaluate_user_function(
    func: &UserFunction,
    args: &[Value],
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let (child_ctx, substitutions) = prepare_user_function_call(func, args, ctx)?;

    let UserFunctionBody(body) = &func.body;
    let substituted_body = substitute_fn_params(body, &substitutions);
    let result = try_evaluate_fn_value(substituted_body, &child_ctx)?.canonicalize();
    // Check return type if annotated
    if let Some(ref return_type) = func.return_type {
        check_fn_return_type(&func.name, return_type, &result, child_ctx.config)?;
    }
    Ok(result)
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

/// Try to evaluate a value (resolve function calls including user-defined ones)
fn try_evaluate_fn_value(value: Value, ctx: &ParseContext) -> Result<Value, ParseError> {
    match value {
        Value::Deferred(DeferredValue::FunctionCall { ref name, ref args }) => {
            // First, recursively evaluate arguments
            let evaluated_args: Result<Vec<Value>, ParseError> = args
                .iter()
                .map(|a| try_evaluate_fn_value(a.clone(), ctx))
                .collect();
            let evaluated_args = evaluated_args?;

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
                    });
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
                    }),
                Err(_builtin_err) => {
                    // Try user-defined function declared in *this* file.
                    if let Some(user_fn) = ctx.user_functions.get(name) {
                        evaluate_user_function(user_fn, &evaluated_args, ctx)
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
                        Ok(Value::Deferred(DeferredValue::FunctionCall {
                            name: name.clone(),
                            args: evaluated_args,
                        }))
                    }
                }
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let evaluated: Result<Vec<Value>, ParseError> = items
                .into_iter()
                .map(|v| try_evaluate_fn_value(v, ctx))
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(evaluated?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let evaluated: Result<IndexMap<String, Value>, ParseError> = map
                .into_iter()
                .map(|(k, v)| try_evaluate_fn_value(v, ctx).map(|ev| (k, ev)))
                .collect();
            Ok(Value::Concrete(ConcreteValue::Map(evaluated?)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let evaluated: Result<Vec<crate::resource::InterpolationPart>, ParseError> = parts
                .into_iter()
                .map(|p| match p {
                    crate::resource::InterpolationPart::Expr(v) => {
                        try_evaluate_fn_value(v, ctx).map(crate::resource::InterpolationPart::Expr)
                    }
                    other => Ok(other),
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(evaluated?)))
        }
        other => Ok(other),
    }
}
