//! User-defined function parsing, type checking, and evaluation.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ast::{FnParam, PendingFunctionTypeCheck, TypeExpr, UserFunction, UserFunctionBody};
use super::context::{ParseContext, next_pair};
use super::error::{ParseError, ParseWarning};
use super::types::parse_type_expr;
use super::{ProviderContext, Rule, is_static_eval, parse_expression};
use crate::eval_value::{BareStructuralHandle, EvalValue, UnexpandedStructuralHandleKind};
use crate::resource::{ConcreteValue, DeferredValue, FunctionArgument, UnknownReason, Value};
use crate::schema::TypeIdentity;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;

/// Parse a user-defined function definition
pub(super) fn parse_fn_def(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    warnings: &mut Vec<ParseWarning>,
    preprocessed_file: &super::PreprocessedFile,
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
                            param_type = Some(parse_type_expr(
                                remaining,
                                ctx.config,
                                warnings,
                                Some(preprocessed_file),
                            )?);
                        }
                        _ => {
                            // This is the default expression
                            let mut default_ctx = ParseContext::new(ctx.config);
                            default_ctx.set_preprocessed_file(preprocessed_file);
                            default_ctx
                                .replace_shared_user_functions(Arc::clone(&ctx.user_functions));
                            default_ctx
                                .seeded_user_functions
                                .extend(ctx.seeded_user_functions.iter().cloned());
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
        let rt = parse_type_expr(next_token, ctx.config, warnings, Some(preprocessed_file))?;
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
    body_ctx.set_preprocessed_file(preprocessed_file);
    body_ctx.replace_shared_user_functions(Arc::clone(&ctx.user_functions));
    body_ctx
        .seeded_user_functions
        .extend(ctx.seeded_user_functions.iter().cloned());
    for p in &params {
        // An annotation constrains an argument but does not classify the
        // parameter's runtime shape. Structural arguments bind a handle and
        // value arguments bind a value; this neutral placeholder lets the
        // call-site argument decide during substitution.
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
                let source_line = body_ctx.original_position(&body_inner)?.line();
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
                local_lets.push((let_name, let_expr, source_line));
            }
            _ => {
                // This should be the expression (the body)
                let source_line = body_ctx.original_position(&body_inner)?.line();
                body = Some(UserFunctionBody {
                    value: parse_expression(body_inner, &body_ctx)?,
                    source_line,
                    source_file: None,
                });
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
        substitutions: HashMap<String, FunctionArgument>,
    },
    Deferred(Vec<FunctionArgument>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FunctionArgumentTypeCheck {
    Complete,
    DeferredUntilSchemasAreLoaded,
}

/// Prepare a user-defined function call: validate args, build substitutions, and return
/// the child context with all parameters and local lets resolved.
fn prepare_user_function_call<'cfg>(
    func: &UserFunction,
    args: &[FunctionArgument],
    ctx: &ParseContext<'cfg>,
    call_line: usize,
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
    let mut substitutions: HashMap<String, FunctionArgument> = HashMap::new();
    let mut effective_args = Vec::with_capacity(func.params.len());
    let mut has_deferred_type_check = false;
    for (i, param) in func.params.iter().enumerate() {
        let argument = if i < args.len() {
            args[i].clone()
        } else {
            FunctionArgument::from_value(param.default.clone().unwrap())
        };
        // Keep nested calls and module-argument placeholders intact until the
        // directory/module resolver has the complete function table and the
        // caller's actual argument shape. Everything else can be reduced now
        // so the parameter check sees the concrete value.
        let argument = if function_argument_must_remain_deferred(&argument) {
            argument
        } else {
            argument.try_map_value(|value| try_evaluate_fn_value(value.clone(), ctx, call_line))?
        };
        // Type-check if the parameter has a type annotation
        if let Some(ref type_expr) = param.param_type {
            has_deferred_type_check |= matches!(
                check_fn_arg_type(fn_name, &param.name, type_expr, &argument, ctx, call_line,)?,
                FunctionArgumentTypeCheck::DeferredUntilSchemasAreLoaded
            );
        }
        substitutions.insert(param.name.clone(), argument.clone());
        effective_args.push(argument);
    }

    // A sibling- or forward-declared call is not resolvable in this file's
    // parse context. Keep the *outer* call intact so the directory-merged
    // resolver can check the same parameter after it learns the nested
    // callee's result. Evaluating the body now would silently discard an
    // unused deferred argument (`f(g(1))` where `f` returns a constant).
    if effective_args.iter().any(|argument| {
        function_argument_must_remain_deferred(argument)
            || argument.value().is_some_and(contains_function_call)
    }) {
        return Ok(PreparedUserFunctionCall::Deferred(effective_args));
    }

    if has_deferred_type_check {
        ctx.record_pending_function_type_check(PendingFunctionTypeCheck {
            function_name: fn_name.clone(),
            arguments: effective_args.clone(),
            source_line: call_line,
            source_file: None,
        });
    }

    // Create a child context with recursion tracking
    let mut child_ctx = ctx.child_scope();
    child_ctx.evaluating_functions.push(fn_name.clone());

    // Evaluate local lets, substituting and resolving each one
    for (let_name, let_expr, source_line) in &func.local_lets {
        let local = substitute_function_call_argument(
            &FunctionArgument::from_value(let_expr.clone()),
            &substitutions,
            *source_line,
        )
        .map_err(|error| locate_function_body_error(func, error))?
        .try_map_value(|value| {
            try_evaluate_fn_value(value.clone(), &child_ctx, *source_line)
                .map_err(|error| locate_function_body_error(func, error))
        })?;

        let local = match local.handle() {
            Some(handle) => {
                child_ctx.set_structural_alias(let_name.clone(), handle.clone());
                FunctionArgument::from_handle(handle.clone().with_alias(let_name.clone()))
            }
            None => {
                child_ctx.set_variable(
                    let_name.clone(),
                    local.value().expect("checked value function local").clone(),
                );
                local
            }
        };
        substitutions.insert(let_name.clone(), local);
    }

    Ok(PreparedUserFunctionCall::Ready {
        child_ctx: Box::new(child_ctx),
        substitutions,
    })
}

fn function_argument_must_remain_deferred(argument: &FunctionArgument) -> bool {
    if argument.pending_parameter().is_some()
        || function_argument_awaits_module_substitution(argument)
    {
        return true;
    }
    argument.value().is_some_and(|value| {
        contains_deferred_user_function_call(value) || contains_module_argument(value)
    })
}

fn contains_deferred_user_function_call(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => deferred
            .values()
            .into_iter()
            .any(contains_deferred_user_function_call),
        Value::Deferred(DeferredValue::FunctionCall { name, args, .. }) => {
            crate::builtins::builtin_arity(name).is_none()
                || args.iter().any(|argument| {
                    argument
                        .value()
                        .is_some_and(contains_deferred_user_function_call)
                })
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall { .. }) => true,
        Value::Concrete(ConcreteValue::List(values)) => {
            values.iter().any(contains_deferred_user_function_call)
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            values.values().any(contains_deferred_user_function_call)
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(
                part,
                crate::resource::InterpolationPart::Expr(value)
                    if contains_deferred_user_function_call(value)
            )
        }),
        Value::Deferred(DeferredValue::Secret(value)) => {
            contains_deferred_user_function_call(value)
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            contains_deferred_user_function_call(access.base())
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            contains_deferred_user_function_call(value)
                || contains_deferred_user_function_call(default)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_deferred_user_function_call(expression.lhs())
                || contains_deferred_user_function_call(expression.rhs())
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => false,
    }
}

fn function_argument_awaits_module_substitution(argument: &FunctionArgument) -> bool {
    argument.pending_parameter().is_some()
        || argument.handle().is_some_and(|handle| {
            matches!(
                handle.kind(),
                UnexpandedStructuralHandleKind::ModuleParameter
            )
        })
        || argument.value().is_some_and(contains_module_argument)
}

pub(super) fn contains_module_argument(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred.values().into_iter().any(contains_module_argument)
        }
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument { .. })) => true,
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(contains_module_argument),
        Value::Concrete(ConcreteValue::Map(entries)) => {
            entries.values().any(contains_module_argument)
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args.iter().any(|argument| {
            argument.pending_parameter().is_some()
                || argument
                    .value()
                    .is_some_and(contains_module_argument)
                || argument.handle().is_some_and(|handle| {
                    matches!(handle.kind(), UnexpandedStructuralHandleKind::ModuleParameter)
                })
        }),
        Value::Deferred(DeferredValue::Access(access)) => {
            contains_module_argument(access.base())
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(part, crate::resource::InterpolationPart::Expr(value) if contains_module_argument(value))
        }),
        Value::Deferred(DeferredValue::Secret(inner)) => contains_module_argument(inner),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            contains_module_argument(value) || contains_module_argument(default)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_module_argument(expression.lhs())
                || contains_module_argument(expression.rhs())
        }
        Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        )
        | Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        ) => false,
    }
}

/// Internal adapter used only by [`crate::schema::OwnerBoundResourceSchema`]
/// and owner-routing unit tests. Public callers validate through the bound
/// schema so a lookup cannot be paired with a different witness.
pub(crate) fn custom_type_lookup_for_schema<'resolver, 'schema>(
    resolver: &'resolver dyn crate::parser::CustomTypeResolver,
    schema: crate::schema::OwnerBoundResourceSchema<'schema>,
) -> impl Fn(&crate::schema::AttributeType, &Value) -> Result<(), crate::schema::TypeError>
+ use<'resolver, 'schema> {
    let owner = schema.owner();
    move |declared_type, value| {
        validate_identified_declaration(declared_type, |declaration| {
            resolver
                .validate(
                    crate::parser::CustomTypeValidationTarget::Declaration { owner, declaration },
                    value,
                )
                .map_err(crate::schema::TypeError::from)
        })
    }
}

/// Bind a provider-config schema walk directly to the factory already selected
/// for that config. No provider-name comparison or owner lookup is involved.
pub fn custom_type_lookup_for_factory<'a>(
    factory: &'a dyn crate::provider::ProviderFactory,
) -> impl Fn(&crate::schema::AttributeType, &Value) -> Result<(), crate::schema::TypeError> + use<'a>
{
    move |declared_type, value| {
        validate_identified_declaration(declared_type, |declaration| {
            crate::provider::validate_custom_type_declaration_with_factory(
                factory,
                declaration,
                value,
            )
            .map_err(crate::schema::TypeError::from)
        })
    }
}

fn validate_identified_declaration(
    declared_type: &crate::schema::AttributeType,
    validate: impl FnOnce(
        crate::parser::IdentifiedCustomTypeDeclaration<'_>,
    ) -> Result<(), crate::schema::TypeError>,
) -> Result<(), crate::schema::TypeError> {
    let Some(declaration) = crate::parser::IdentifiedCustomTypeDeclaration::new(declared_type)
    else {
        return Ok(());
    };
    validate(declaration)
}

/// Validate a value against a custom type (`Ipv4Cidr`, `Ipv4Address`, …).
/// Returns Ok(()) if the value passes validation or cannot be validated
/// statically (e.g., ResourceRef, FunctionCall, Interpolation are
/// deferred).
///
/// Bare built-in identities use the kind-aware built-in registry. Provider
/// identities must already have resolved schema owners and route through those
/// factories, even if their descriptive identity collides with a built-in.
pub fn validate_custom_type(
    identity: &TypeIdentity,
    value: &Value,
    _config: &ProviderContext,
) -> crate::provider::ProviderResult<()> {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { .. }) => return Ok(()),
        Value::Deferred(
            DeferredValue::FunctionCall { .. } | DeferredValue::ResolvedFunctionCall { .. },
        ) => return Ok(()),
        Value::Deferred(DeferredValue::Access(_access)) => return Ok(()),
        Value::Deferred(DeferredValue::Interpolation(_parts)) => return Ok(()),
        Value::Deferred(DeferredValue::Unknown(_reason)) => return Ok(()),
        Value::Concrete(_concrete) => {}
        Value::Deferred(_deferred) => {}
    }

    use crate::schema::RefinedScalarKind;

    let builtin_kind = match value {
        Value::Concrete(ConcreteValue::String(_) | ConcreteValue::EnumIdentifier(_)) => {
            Some(RefinedScalarKind::String)
        }
        Value::Concrete(ConcreteValue::Int(_)) => {
            if crate::schema::builtin_validator(RefinedScalarKind::Int, identity).is_some() {
                Some(RefinedScalarKind::Int)
            } else {
                Some(RefinedScalarKind::Float)
            }
        }
        Value::Concrete(ConcreteValue::Float(_)) => Some(RefinedScalarKind::Float),
        Value::Concrete(
            ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::List(_)
            | ConcreteValue::StringList(_)
            | ConcreteValue::Map(_),
        )
        | Value::Deferred(_) => None,
    };
    if let Some(kind) = builtin_kind
        && let Some(validator) = crate::schema::builtin_validator(kind, identity)
    {
        let identifier_text;
        let validation_value = match value {
            Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => {
                identifier_text =
                    Value::Concrete(ConcreteValue::String(raw.as_str().to_string().into()));
                &identifier_text
            }
            _ => value,
        };
        return validator(validation_value).map_err(|error| {
            crate::provider::ProviderError::invalid_input_caused_by(
                "Custom type validation failed",
                error,
            )
        });
    }

    Err(crate::provider::ProviderError::invalid_input(format!(
        "unknown custom type `{identity}`; provider-owned annotations must resolve to an owner-qualified schema type"
    )))
}

/// Validate an authored provider schema annotation against every owner that
/// declares its semantic identity.
///
/// ```compile_fail
/// use carina_core::parser::{
///     OwnedTypeIdentity, ProviderContext, validate_schema_custom_type,
/// };
/// use carina_core::resource::{ConcreteValue, Value};
/// use carina_core::schema::TypeIdentity;
///
/// let owned = OwnedTypeIdentity::provider("test", TypeIdentity::bare("Id"));
/// let value = Value::Concrete(ConcreteValue::String("id".into()));
/// let _ = validate_schema_custom_type(&owned, &value, &ProviderContext::default());
/// ```
pub fn validate_schema_custom_type(
    owned: &crate::parser::OwnedTypeIdentity,
    value: &Value,
    config: &crate::parser::LoadedProviderContext,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    validate_schema_custom_type_with_context(owned, value, config)
}

pub(crate) fn validate_schema_custom_type_with_context(
    owned: &crate::parser::OwnedTypeIdentity,
    value: &Value,
    config: &ProviderContext,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    match value {
        Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Interpolation(_)
            | DeferredValue::Secret(_)
            | DeferredValue::Unknown(_),
        ) => return Ok(()),
        Value::Concrete(_) | Value::Deferred(_) => {}
    }
    config.custom_type_resolver().validate(
        crate::parser::CustomTypeValidationTarget::Annotation(owned),
        value,
    )
}

/// Check that a function argument matches the declared parameter type.
fn check_fn_arg_type(
    fn_name: &str,
    param_name: &str,
    type_expr: &TypeExpr,
    argument: &FunctionArgument,
    ctx: &ParseContext,
    call_line: usize,
) -> Result<FunctionArgumentTypeCheck, ParseError> {
    if argument.pending_parameter().is_some() {
        return Ok(FunctionArgumentTypeCheck::DeferredUntilSchemasAreLoaded);
    }
    // Function annotations are parsed before provider schemas are necessarily
    // available, so dotted types intentionally enter the AST unresolved. When
    // this call runs with an enriched context, resolve the annotation at the
    // checking seam so provider custom-type validation receives the same
    // canonical API spelling as module arguments, attributes, and exports.
    let resolved_type =
        match crate::validation::resolve_parameter_type_for_check(type_expr, ctx.config) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                return Ok(FunctionArgumentTypeCheck::DeferredUntilSchemasAreLoaded);
            }
            Err(error) => return Err(ParseError::UserFunctionError(error.to_string())),
        };

    if let Some(handle) = argument.handle() {
        if let Err(error) = crate::validation::check_handle_sink(handle, &resolved_type) {
            if error.is_bare_value_sink() {
                return Err(ParseError::from_eval_value(
                    crate::eval_value::EvalValueError::BareStructuralHandle(handle.bare_error()),
                    call_line,
                ));
            }
            return Err(ParseError::UserFunctionError(format!(
                "function '{fn_name}': parameter '{param_name}': {error}",
            )));
        }
        return Ok(FunctionArgumentTypeCheck::Complete);
    }

    let value = argument.value().expect("non-handle function argument");
    let type_expr = resolved_type.as_type_expr();

    if let Err(mismatch) = crate::validation::typecheck::check_type_expr_value_with_context(
        &type_expr,
        value,
        ctx.config,
        crate::validation::typecheck::TypecheckInference::Unavailable,
    ) {
        return Err(function_argument_type_error(fn_name, param_name, mismatch));
    }

    Ok(FunctionArgumentTypeCheck::Complete)
}

pub(crate) fn lower_function_arguments(
    args: Vec<EvalValue>,
    line: usize,
) -> Result<Vec<FunctionArgument>, ParseError> {
    args.into_iter()
        .map(|argument| match argument {
            EvalValue::UnexpandedStructuralHandle(handle) => {
                Ok(FunctionArgument::from_handle(handle))
            }
            argument => argument
                .into_value()
                .map(FunctionArgument::from_value)
                .map_err(|error| ParseError::from_eval_value(error, line)),
        })
        .collect()
}

pub(super) fn record_deferred_function_type_check(
    function: &UserFunction,
    arguments: &[FunctionArgument],
    ctx: &ParseContext,
    source_line: usize,
) {
    if function
        .params
        .iter()
        .any(|parameter| parameter.param_type.is_some())
    {
        ctx.record_pending_function_type_check(PendingFunctionTypeCheck {
            function_name: function.name.clone(),
            arguments: arguments.to_vec(),
            source_line,
            source_file: None,
        });
    }
}

/// Builtins operate exclusively on values. Reject a structural operand while
/// its typed handle and authored source line are still available; deferring it
/// would lose the owning file and produce a duplicated builtin prefix later.
pub(crate) fn reject_builtin_handle_arguments(
    args: &[EvalValue],
    line: usize,
) -> Result<(), ParseError> {
    if let Some(handle) = args.iter().find_map(EvalValue::structural_handle_ref) {
        return Err(ParseError::from_eval_value(
            crate::eval_value::EvalValueError::BareStructuralHandle(handle.bare_error()),
            line,
        ));
    }
    Ok(())
}

pub(crate) fn evaluate_user_function_eval(
    func: &UserFunction,
    args: Vec<EvalValue>,
    ctx: &ParseContext,
    line: usize,
) -> Result<Value, ParseError> {
    let arguments = lower_function_arguments(args, line)?;
    evaluate_user_function(func, &arguments, ctx, line)
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

pub(crate) fn validate_pending_function_type_check(
    check: &PendingFunctionTypeCheck,
    functions: &HashMap<String, UserFunction>,
    ctx: &ParseContext,
) -> Result<(), ParseError> {
    let Some(function) = functions.get(&check.function_name) else {
        return Ok(());
    };
    for (parameter, argument) in function.params.iter().zip(&check.arguments) {
        if function_argument_awaits_module_substitution(argument) {
            continue;
        }
        let Some(type_expr) = &parameter.param_type else {
            continue;
        };
        let result = check_fn_arg_type(
            &function.name,
            &parameter.name,
            type_expr,
            argument,
            ctx,
            check.source_line,
        );
        match result {
            Ok(FunctionArgumentTypeCheck::Complete)
            | Ok(FunctionArgumentTypeCheck::DeferredUntilSchemasAreLoaded) => {}
            Err(error) => {
                let located = match error {
                    ParseError::InvalidExpression { message, .. }
                    | ParseError::UserFunctionError(message) => ParseError::InvalidExpression {
                        line: check.source_line,
                        message,
                    },
                    other => other,
                };
                return Err(match &check.source_file {
                    Some(path) => located.in_file(path.clone()),
                    None => located,
                });
            }
        }
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
    if let Err(mismatch) = crate::validation::typecheck::check_type_expr_value_with_context(
        type_expr,
        value,
        config,
        crate::validation::typecheck::TypecheckInference::Unavailable,
    ) {
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
    args: &[FunctionArgument],
    ctx: &ParseContext,
    call_line: usize,
) -> Result<Value, ParseError> {
    let prepared =
        prepare_user_function_call(func, args, ctx, call_line).map_err(|error| match error {
            ParseError::UserFunctionError(message) => ParseError::InvalidExpression {
                line: call_line,
                message,
            },
            other => other,
        })?;
    let (child_ctx, substitutions) = match prepared {
        PreparedUserFunctionCall::Ready {
            child_ctx,
            substitutions,
        } => (child_ctx, substitutions),
        PreparedUserFunctionCall::Deferred(args) => {
            return Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function: ctx
                    .resolved_user_function(&func.name)
                    .unwrap_or_else(|| std::sync::Arc::new(func.clone())),
                functions: ctx.shared_user_function_scope(),
                args,
                source_line: (call_line).into(),
            }));
        }
    };

    let substituted_body =
        substitute_fn_params(&func.body.value, &substitutions, func.body.source_line)
            .map_err(|error| locate_function_body_error(func, error))?;
    let result = try_evaluate_fn_value(substituted_body, &child_ctx, func.body.source_line)
        .map_err(|error| locate_function_body_error(func, error))?
        .canonicalize();
    // Check return type if annotated
    if let Some(ref return_type) = func.return_type {
        check_fn_return_type(&func.name, return_type, &result, child_ctx.config)
            .map_err(|error| locate_function_body_error(func, error))?;
    }
    Ok(result)
}

/// Evaluate a call that already owns its resolved lexical definition.
/// Consumers outside the parser use this instead of rebuilding a name lookup
/// table from their current (and potentially different) module scope.
pub(crate) fn evaluate_resolved_user_function(
    function: &UserFunction,
    functions: &Arc<HashMap<String, UserFunction>>,
    arguments: &[FunctionArgument],
    config: &ProviderContext,
    call_line: usize,
) -> Result<Value, ParseError> {
    let mut context = ParseContext::new(config);
    context.replace_shared_user_functions(Arc::clone(functions));
    if !context.user_functions.contains_key(&function.name) {
        context.insert_user_function(function.name.clone(), function.clone());
    }
    evaluate_user_function(function, arguments, &context, call_line)
}

fn locate_function_body_error(func: &UserFunction, error: ParseError) -> ParseError {
    match &func.body.source_file {
        Some(path) => error.in_file(path.clone()),
        None => error,
    }
}

pub fn contains_function_call(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred.values().into_iter().any(contains_function_call)
        }
        Value::Deferred(
            DeferredValue::FunctionCall { .. }
            | DeferredValue::ResolvedFunctionCall { .. },
        ) => true,
        Value::Deferred(DeferredValue::Access(access)) => {
            contains_function_call(access.base())
        }
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(contains_function_call),
        Value::Concrete(ConcreteValue::Map(entries)) => {
            entries.values().any(contains_function_call)
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(part, crate::resource::InterpolationPart::Expr(value) if contains_function_call(value))
        }),
        Value::Deferred(DeferredValue::Secret(inner)) => contains_function_call(inner),
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_function_call(expression.lhs()) || contains_function_call(expression.rhs())
        }
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum FunctionSubstitutionPosition {
    Value,
    AccessBase,
}

fn substituted_function_argument(
    name: &str,
    argument: &FunctionArgument,
    position: FunctionSubstitutionPosition,
    source_line: usize,
) -> Result<Value, ParseError> {
    if let Some(value) = argument.value() {
        return Ok(value.clone());
    }
    match position {
        FunctionSubstitutionPosition::AccessBase => Ok(argument.walker_value().clone()),
        FunctionSubstitutionPosition::Value => Err(ParseError::from_eval_value(
            crate::eval_value::EvalValueError::BareStructuralHandle(BareStructuralHandle {
                binding: name.to_string(),
                kind: UnexpandedStructuralHandleKind::FunctionParameter,
            }),
            source_line,
        )),
    }
}

fn substitute_function_call_argument(
    argument: &FunctionArgument,
    substitutions: &HashMap<String, FunctionArgument>,
    source_line: usize,
) -> Result<FunctionArgument, ParseError> {
    let Some(value) = argument.value() else {
        return Ok(argument.clone());
    };
    if let Value::Deferred(DeferredValue::Unknown(
        UnknownReason::FnParam { name } | UnknownReason::FnLocal { name },
    )) = value
        && let Some(substitution) = substitutions.get(name)
    {
        return Ok(substitution.clone());
    }
    argument.try_map_value(|value| {
        substitute_fn_params_with_position(
            value,
            substitutions,
            source_line,
            FunctionSubstitutionPosition::Value,
        )
    })
}

/// Recursively substitute function parameter placeholders with actual values.
/// A handle is legal only while it is the access base of `param.field` or a
/// typed argument to another deferred function call.
fn substitute_fn_params(
    value: &Value,
    substitutions: &HashMap<String, FunctionArgument>,
    source_line: usize,
) -> Result<Value, ParseError> {
    substitute_fn_params_with_position(
        value,
        substitutions,
        source_line,
        FunctionSubstitutionPosition::Value,
    )
}

fn substitute_fn_params_with_position(
    value: &Value,
    substitutions: &HashMap<String, FunctionArgument>,
    source_line: usize,
    position: FunctionSubstitutionPosition,
) -> Result<Value, ParseError> {
    Ok(match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let deferred = deferred
                .try_map(|value| substitute_fn_params(value, substitutions, source_line))?;
            Value::Deferred(DeferredValue::ModuleIf(Box::new(deferred)))
        }
        Value::Deferred(DeferredValue::Unknown(UnknownReason::FnParam { name }))
        | Value::Deferred(DeferredValue::Unknown(UnknownReason::FnLocal { name })) => {
            match substitutions.get(name) {
                Some(argument) => {
                    substituted_function_argument(name, argument, position, source_line)?
                }
                None => value.clone(),
            }
        }
        Value::Deferred(DeferredValue::ResourceRef { path })
            if substitutions.contains_key(path.binding()) =>
        {
            let Some(base) = substitutions.get(path.binding()) else {
                unreachable!("substitution presence was checked above")
            };
            let mut substituted = crate::resource::apply_value_path_segment(
                base.walker_value().clone(),
                crate::resource::PathSegment::Field {
                    name: path.attribute().to_string(),
                },
                source_line,
                crate::resource::AccessMissPolicy::Error,
            )
            .and_then(crate::resource::ValuePathSegmentResult::into_value)
            .map_err(|error| ParseError::InvalidExpression {
                line: source_line,
                message: error.to_string(),
            })?;
            for segment in path.segments() {
                substituted = crate::resource::apply_value_path_segment(
                    substituted,
                    segment.clone(),
                    source_line,
                    crate::resource::AccessMissPolicy::Error,
                )
                .and_then(crate::resource::ValuePathSegmentResult::into_value)
                .map_err(|error| ParseError::InvalidExpression {
                    line: source_line,
                    message: error.to_string(),
                })?;
            }
            substituted
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|v| substitute_fn_params(v, substitutions, source_line))
                .collect::<Result<_, _>>()?,
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.iter()
                .map(|(k, v)| {
                    Ok((
                        k.clone(),
                        substitute_fn_params(v, substitutions, source_line)?,
                    ))
                })
                .collect::<Result<_, ParseError>>()?,
        )),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line: call_line,
        }) => Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: function.clone(),
            functions: functions.clone(),
            args: args
                .iter()
                .map(|argument| {
                    substitute_function_call_argument(
                        argument,
                        substitutions,
                        call_line.unwrap_or(source_line),
                    )
                })
                .collect::<Result<_, _>>()?,
            source_line: *call_line,
        }),
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line: call_line,
        }) => Value::Deferred(DeferredValue::FunctionCall {
            name: *name,
            args: args
                .iter()
                .map(|argument| {
                    substitute_function_call_argument(
                        argument,
                        substitutions,
                        call_line.unwrap_or(source_line),
                    )
                })
                .collect::<Result<_, _>>()?,
            source_line: *call_line,
        }),
        Value::Deferred(DeferredValue::Unavailable {
            name,
            args,
            reason,
            source_line: call_line,
        }) => Value::Deferred(DeferredValue::Unavailable {
            name: *name,
            args: args
                .iter()
                .map(|argument| {
                    substitute_function_call_argument(
                        argument,
                        substitutions,
                        call_line.unwrap_or(source_line),
                    )
                })
                .collect::<Result<_, _>>()?,
            reason: reason.clone(),
            source_line: *call_line,
        }),
        Value::Deferred(DeferredValue::Access(access)) => access
            .rebuild_with_base(
                substitute_fn_params_with_position(
                    access.base(),
                    substitutions,
                    access.source_line(),
                    FunctionSubstitutionPosition::AccessBase,
                )?,
                crate::resource::AccessMissPolicy::Error,
            )
            .map_err(|error| ParseError::InvalidExpression {
                line: access.source_line(),
                message: error.to_string(),
            })?,
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            Value::Deferred(DeferredValue::Interpolation(
                parts
                    .iter()
                    .map(|p| match p {
                        crate::resource::InterpolationPart::Expr(v) => {
                            substitute_fn_params(v, substitutions, source_line)
                                .map(crate::resource::InterpolationPart::Expr)
                        }
                        other => Ok(other.clone()),
                    })
                    .collect::<Result<_, _>>()?,
            ))
        }
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(substitute_fn_params(inner, substitutions, source_line)?),
        )),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(substitute_fn_params(value, substitutions, source_line)?),
                default: Box::new(substitute_fn_params(default, substitutions, source_line)?),
            })
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = substitute_fn_params(expression.lhs(), substitutions, source_line)?;
            let rhs = substitute_fn_params(expression.rhs(), substitutions, source_line)?;
            let span = expression.operator_span();
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| ParseError::integer_arithmetic(span, source_line, error))?
        }
        Value::Deferred(DeferredValue::Unknown(_))
        | Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
        | Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        ) => value.clone(),
    })
}

/// Try to evaluate a value (resolve function calls including user-defined ones).
pub(crate) fn function_argument_as_eval(argument: &FunctionArgument) -> EvalValue {
    match argument.handle() {
        Some(handle) => EvalValue::structural_handle(handle.clone()),
        None => EvalValue::from_value(argument.walker_value().clone()),
    }
}

/// Apply a parser-local built-in closure without losing typed built-in
/// identity when one of the supplied values is still deferred.
pub(crate) fn apply_parser_closure(
    name: &str,
    captured_args: &[EvalValue],
    remaining_arity: usize,
    new_args: Vec<EvalValue>,
    ctx: &ParseContext,
    line: usize,
) -> Result<EvalValue, ParseError> {
    if new_args.len() > remaining_arity {
        return Err(ParseError::InvalidExpression {
            line,
            message: format!(
                "{}() closure expects {} more argument{}, got {}",
                name,
                remaining_arity,
                if remaining_arity == 1 { "" } else { "s" },
                new_args.len(),
            ),
        });
    }

    if name == "__compose__" {
        if new_args.len() != 1 {
            return Err(ParseError::InvalidExpression {
                line,
                message: format!(
                    "composed function expects exactly 1 argument, got {}",
                    new_args.len(),
                ),
            });
        }
        let mut result = new_args.into_iter().next().expect("one composed argument");
        for function in captured_args {
            let EvalValue::Closure {
                name,
                captured_args,
                remaining_arity,
            } = function
            else {
                return Err(ParseError::InvalidExpression {
                    line,
                    message: "composed function chain contains a non-function value".to_string(),
                });
            };
            result = apply_parser_closure(
                name,
                captured_args,
                *remaining_arity,
                vec![result],
                ctx,
                line,
            )?;
        }
        return Ok(result);
    }

    let mut all_args = captured_args.to_vec();
    all_args.extend(new_args.iter().cloned());
    reject_builtin_handle_arguments(&all_args, line)?;
    if all_args.iter().all(is_static_eval) {
        return crate::builtins::apply_closure_with_config(
            name,
            captured_args,
            remaining_arity,
            &new_args,
            ctx.config,
        )
        .map_err(|message| ParseError::InvalidExpression { line, message });
    }

    let remaining = remaining_arity - new_args.len();
    if remaining > 0 {
        return Ok(EvalValue::closure(name, all_args, remaining));
    }

    let builtin = crate::builtins::BuiltinFunction::from_name(name).ok_or_else(|| {
        ParseError::InvalidExpression {
            line,
            message: format!("Unknown built-in function: {name}"),
        }
    })?;
    let args = lower_function_arguments(all_args, line)?;
    Ok(EvalValue::from_value(Value::Deferred(
        DeferredValue::FunctionCall {
            name: builtin,
            args,
            source_line: line.into(),
        },
    )))
}

pub(super) fn try_evaluate_fn_value(
    value: Value,
    ctx: &ParseContext,
    containing_line: usize,
) -> Result<Value, ParseError> {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let mut deferred = *deferred;
            deferred.condition.value = try_evaluate_fn_value(
                deferred.condition.value,
                ctx,
                deferred.condition.source_line,
            )?;
            deferred.then_value = try_evaluate_fn_value(deferred.then_value, ctx, containing_line)?;
            deferred.else_value = try_evaluate_fn_value(deferred.else_value, ctx, containing_line)?;
            Ok(Value::Deferred(DeferredValue::ModuleIf(Box::new(deferred))))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => {
            let source_line = source_line.unwrap_or(containing_line);
            let evaluated_args = args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| {
                        try_evaluate_fn_value(value.clone(), ctx, source_line)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            evaluate_resolved_user_function(
                &function,
                &functions,
                &evaluated_args,
                ctx.config,
                source_line,
            )
        }
        Value::Deferred(DeferredValue::FunctionCall {
            ref name,
            ref args,
            source_line,
        }) => {
            let source_line = source_line.unwrap_or(containing_line);
            // First, recursively evaluate arguments
            let evaluated_args: Result<Vec<FunctionArgument>, ParseError> = args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| {
                        try_evaluate_fn_value(value.clone(), ctx, source_line)
                    })
                })
                .collect();
            let evaluated_args = evaluated_args?;

            if !evaluated_args
                .iter()
                .all(|argument| argument.value().is_some_and(crate::parser::is_static_value))
            {
                return Ok(Value::Deferred(DeferredValue::FunctionCall {
                    name: *name,
                    args: evaluated_args,
                    source_line: source_line.into(),
                }));
            }

            let eval_args: Vec<EvalValue> = evaluated_args
                .iter()
                .map(function_argument_as_eval)
                .collect();
            crate::builtins::evaluate_builtin_with_config(name.as_str(), &eval_args, ctx.config)
                .and_then(|result| result.into_value().map_err(|error| error.to_string()))
                .map_err(|message| ParseError::InvalidExpression {
                    line: source_line,
                    message,
                })
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let base = try_evaluate_fn_value(access.base().clone(), ctx, access.source_line())?;
            access
                .rebuild_with_base(base, crate::resource::AccessMissPolicy::Error)
                .map_err(|error| ParseError::InvalidExpression {
                    line: access.source_line(),
                    message: error.to_string(),
                })
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let span = expression.operator_span();
            let lhs = try_evaluate_fn_value(
                expression.lhs().clone(),
                ctx,
                span.line().unwrap_or(containing_line),
            )?;
            let rhs = try_evaluate_fn_value(
                expression.rhs().clone(),
                ctx,
                span.line().unwrap_or(containing_line),
            )?;
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| ParseError::integer_arithmetic(span, containing_line, error))
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let evaluated: Result<Vec<Value>, ParseError> = items
                .into_iter()
                .map(|v| try_evaluate_fn_value(v, ctx, containing_line))
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(evaluated?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let evaluated: Result<IndexMap<String, Value>, ParseError> = map
                .into_iter()
                .map(|(k, v)| {
                    try_evaluate_fn_value(v, ctx, containing_line).map(|value| (k, value))
                })
                .collect();
            Ok(Value::Concrete(ConcreteValue::Map(evaluated?)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let mut evaluated = Vec::with_capacity(parts.len());
            for part in parts {
                match part {
                    crate::resource::InterpolationPart::Expr(value) => {
                        let value = try_evaluate_fn_value(value, ctx, containing_line)?;
                        evaluated.push(crate::resource::InterpolationPart::Expr(value));
                    }
                    other => evaluated.push(other),
                }
            }
            Ok(Value::Deferred(DeferredValue::Interpolation(evaluated)))
        }
        Value::Deferred(DeferredValue::Unavailable {
            name,
            reason,
            source_line,
            ..
        }) => Err(ParseError::InvalidExpression {
            line: source_line.unwrap_or(containing_line),
            message: format!("{name}(): {reason}"),
        }),
        value @ Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | value @ Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Coalesce { .. }
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => Ok(value),
    }
}
