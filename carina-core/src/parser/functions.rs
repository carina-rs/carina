//! User-defined function parsing, type checking, and evaluation.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ast::{FnParam, PendingFunctionTypeCheck, TypeExpr, UserFunction, UserFunctionBody};
use super::context::{ParseContext, next_pair};
use super::error::{ParseError, ParseWarning};
use super::types::parse_type_expr;
use super::{ParsedFunctionArgument as FunctionArgument, ParsedValue as Value};
use super::{ProviderContext, Rule, is_static_eval, parse_expression};
use crate::eval_value::{BareStructuralHandle, EvalValue, UnexpandedStructuralHandleKind};
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, ParserPlaceholder, PlaceholderKind,
    Value as GenericValue,
};
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
                let mut type_span = None;
                let mut default = None;
                for remaining in param_inner {
                    match remaining.as_rule() {
                        Rule::type_expr => {
                            type_span =
                                Some(super::SourceSpan::from_pest(&remaining, preprocessed_file));
                            param_type = Some(parse_type_expr(
                                remaining,
                                ctx.config,
                                warnings,
                                Some(preprocessed_file),
                            )?);
                        }
                        _ => {
                            // This is the default expression
                            let mut default_ctx = ctx.isolated();
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
                    type_span,
                    default,
                });
            }
        }
        next_pair(&mut inner, "type_expr or fn_body", "fn_def")?
    } else {
        next
    };

    // Parse optional return type annotation (: type_expr)
    let mut return_type_span = None;
    let (return_type, body_pair) = if next_token.as_rule() == Rule::type_expr {
        return_type_span = Some(super::SourceSpan::from_pest(&next_token, preprocessed_file));
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
    let mut body_ctx = ctx.isolated();
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
            Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam {
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
                    Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnLocal {
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
        return_type_span,
        annotation_source_file: Some(ctx.source_file().to_path_buf()),
        local_lets,
        body,
    })
}

enum PreparedUserFunctionCall<'cfg, Source = super::context::AuthoredDiagnosticSource> {
    Ready {
        child_ctx: Box<ParseContext<'cfg, Source>>,
        substitutions: HashMap<String, FunctionArgument>,
    },
    Deferred(Vec<FunctionArgument>),
}

#[derive(Clone, Debug, PartialEq)]
enum FunctionArgumentTypeCheck {
    Complete,
    AwaitingRuntimeValue,
    RecordedProviderDeferral,
    AwaitingProviderSession,
    /// The structural argument is already an explicit handle. Its annotation
    /// still needs provider classification, but a pure body can run while the
    /// pending check retains that handle for loaded validation.
    DeferredHandleAnnotation,
}

/// Prepare a user-defined function call: validate args, build substitutions, and return
/// the child context with all parameters and local lets resolved.
fn prepare_user_function_call<'cfg, Source: Clone>(
    func: &UserFunction,
    args: &[FunctionArgument],
    ctx: &ParseContext<'cfg, Source>,
    call_line: usize,
) -> Result<PreparedUserFunctionCall<'cfg, Source>, ParseError> {
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
    let requires_session = ctx.session.is_none()
        && !ctx.config.custom_types_loaded()
        && func
            .return_type
            .as_ref()
            .is_some_and(annotation_requires_provider_session);
    let mut has_deferred_type_check = requires_session;
    let mut defer_value_call = requires_session;
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
            match check_fn_arg_type(
                fn_name,
                &param.name,
                type_expr,
                &argument,
                ctx,
                call_line,
                None,
            )? {
                FunctionArgumentTypeCheck::Complete => {}
                FunctionArgumentTypeCheck::AwaitingRuntimeValue => {
                    has_deferred_type_check = true;
                    defer_value_call = true;
                }
                FunctionArgumentTypeCheck::RecordedProviderDeferral
                | FunctionArgumentTypeCheck::AwaitingProviderSession => {
                    has_deferred_type_check = true;
                    defer_value_call = true;
                }
                FunctionArgumentTypeCheck::DeferredHandleAnnotation => {
                    has_deferred_type_check = true;
                }
            }
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
            source_column: None,
            source_file: None,
        });
        if defer_value_call {
            return Ok(PreparedUserFunctionCall::Deferred(effective_args));
        }
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
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
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

pub(super) fn contains_module_argument<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
) -> bool {
    contains_module_argument_matching(value, &mut |_| true)
}

pub(crate) fn contains_module_argument_matching<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    predicate: &mut impl FnMut(&str) -> bool,
) -> bool {
    match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred
                .values()
                .into_iter()
                .any(|value| contains_module_argument_matching(value, predicate))
        }
        GenericValue::Deferred(DeferredValue::Placeholder(
            ParserPlaceholder::ModuleArgument {
                name,
                diagnostic: _,
            },
        )) => predicate(name),
        GenericValue::Deferred(DeferredValue::Placeholder(
            ParserPlaceholder::FnParam { name: _ }
            | ParserPlaceholder::FnLocal { name: _ }
            | ParserPlaceholder::Loop(crate::resource::LoopSlot::Key)
            | ParserPlaceholder::Loop(crate::resource::LoopSlot::Index)
            | ParserPlaceholder::Loop(crate::resource::LoopSlot::Value)
            | ParserPlaceholder::EmptyInterpolation { source: _ },
        )) => false,
        GenericValue::Concrete(ConcreteValue::List(items)) => items
            .iter()
            .any(|item| contains_module_argument_matching(item, predicate)),
        GenericValue::Concrete(ConcreteValue::Map(entries)) => {
            entries
                .values()
                .any(|entry| contains_module_argument_matching(entry, predicate))
        }
        GenericValue::Deferred(DeferredValue::FunctionCall {
            name: _,
            args,
            source_line: _,
        })
        | GenericValue::Deferred(DeferredValue::Unavailable {
            name: _,
            args,
            reason: _,
            source_line: _,
        })
        | GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
            function: _,
            functions: _,
            args,
            source_line: _,
        }) => args.iter().any(|argument| {
            argument.pending_parameter().is_some_and(&mut *predicate)
                || argument
                    .value()
                    .is_some_and(|value| contains_module_argument_matching(value, predicate))
                || argument.handle().is_some_and(|handle| {
                    matches!(handle.kind(), UnexpandedStructuralHandleKind::ModuleParameter)
                        && predicate(handle.canonical_binding().as_str())
                })
        }),
        GenericValue::Deferred(DeferredValue::Access(access)) => {
            contains_module_argument_matching(access.base(), predicate)
        }
        GenericValue::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(part, crate::resource::InterpolationPart::Expr(value) if contains_module_argument_matching(value, predicate))
        }),
        GenericValue::Deferred(DeferredValue::Secret(inner)) => {
            contains_module_argument_matching(inner, predicate)
        }
        GenericValue::Deferred(DeferredValue::Coalesce { value, default }) => {
            contains_module_argument_matching(value, predicate)
                || contains_module_argument_matching(default, predicate)
        }
        GenericValue::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_module_argument_matching(expression.lhs(), predicate)
                || contains_module_argument_matching(expression.rhs(), predicate)
        }
        GenericValue::Deferred(
            DeferredValue::ResourceRef { path: _ }
            | DeferredValue::BindingRef { binding: _ }
            | DeferredValue::RedactedSecret { hash: _ }
            | DeferredValue::Unknown(_),
        )
        | GenericValue::Concrete(
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
) -> impl Fn(
    &crate::schema::CompiledAttributeType,
    &GenericValue,
) -> Result<(), crate::schema::TypeError>
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
) -> impl Fn(
    &crate::schema::CompiledAttributeType,
    &GenericValue,
) -> Result<(), crate::schema::TypeError>
+ use<'a> {
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
    declared_type: &crate::schema::CompiledAttributeType,
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
/// Bare built-in identities use their registry declarations. Provider
/// identities must already have resolved schema owners and route through those
/// factories, even if their descriptive identity collides with a built-in.
pub fn validate_custom_type<P: PlaceholderKind, B: BindingAddress>(
    identity: &TypeIdentity,
    value: &GenericValue<P, B>,
    _config: &ProviderContext,
) -> crate::provider::ProviderResult<()> {
    match value {
        GenericValue::Deferred(DeferredValue::ResourceRef { .. }) => return Ok(()),
        GenericValue::Deferred(
            DeferredValue::FunctionCall { .. } | DeferredValue::ResolvedFunctionCall { .. },
        ) => return Ok(()),
        GenericValue::Deferred(DeferredValue::Access(_access)) => return Ok(()),
        GenericValue::Deferred(DeferredValue::Interpolation(_parts)) => return Ok(()),
        GenericValue::Deferred(DeferredValue::Unknown(_reason)) => return Ok(()),
        GenericValue::Concrete(_concrete) => {}
        GenericValue::Deferred(_deferred) => {}
    }

    let declaration = crate::schema::builtin_type_registry()
        .get(identity)
        .ok_or_else(|| {
            crate::provider::ProviderError::invalid_input(format!(
                "unknown custom type `{identity}`; provider-owned annotations must resolve to an owner-qualified schema type"
            ))
        })?;
    validate_builtin_custom_type(declaration, value).map_err(|error| {
        crate::provider::ProviderError::invalid_input_caused_by(
            "Custom type validation failed",
            error,
        )
    })
}

/// Validate a core declaration's scalar shape and constraints, preserving the
/// DSL's textual interpretation of unresolved enum identifiers.
pub(crate) fn validate_builtin_custom_type<P: PlaceholderKind, B: BindingAddress>(
    declaration: &crate::schema::CompiledAttributeType,
    value: &GenericValue<P, B>,
) -> Result<(), crate::schema::TypeError> {
    if let GenericValue::Concrete(ConcreteValue::EnumIdentifier(raw)) = value {
        declaration.validate_identified_scalar_value(&GenericValue::<P, B>::Concrete(
            ConcreteValue::String(raw.as_str().to_string().into()),
        ))
    } else {
        declaration.validate_identified_scalar_value(value)
    }
}

/// Validate an authored provider schema annotation against every owner that
/// declares its semantic identity.
///
/// ```compile_fail
/// use carina_core::parser::{
///     OwnedTypeIdentity, ProviderContext, validate_schema_custom_type,
/// };
/// use carina_core::resource::Value;
///
/// fn check(owned: &OwnedTypeIdentity, value: &Value) {
///     let _ = validate_schema_custom_type(owned, value, &ProviderContext::default());
/// }
/// ```
pub fn validate_schema_custom_type<P: PlaceholderKind, B: BindingAddress>(
    owned: &crate::parser::OwnedTypeIdentity,
    value: &GenericValue<P, B>,
    config: &crate::parser::LoadedProviderContext,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    validate_schema_custom_type_with_context(owned, value, config)
}

pub(crate) fn validate_schema_custom_type_with_context<P: PlaceholderKind, B: BindingAddress>(
    owned: &crate::parser::OwnedTypeIdentity,
    value: &GenericValue<P, B>,
    config: &ProviderContext,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    match value {
        GenericValue::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Interpolation(_)
            | DeferredValue::Secret(_)
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => return Ok(()),
        GenericValue::Concrete(_) | GenericValue::Deferred(_) => {}
    }
    let value: GenericValue = match value {
        GenericValue::Concrete(ConcreteValue::String(value)) => {
            GenericValue::Concrete(ConcreteValue::String(value.clone()))
        }
        GenericValue::Concrete(ConcreteValue::EnumIdentifier(value)) => {
            GenericValue::Concrete(ConcreteValue::EnumIdentifier(value.clone()))
        }
        GenericValue::Concrete(ConcreteValue::CanonicalEnum(value)) => {
            GenericValue::Concrete(ConcreteValue::CanonicalEnum(value.clone()))
        }
        GenericValue::Concrete(ConcreteValue::Int(value)) => {
            GenericValue::Concrete(ConcreteValue::Int(*value))
        }
        GenericValue::Concrete(ConcreteValue::Float(value)) => {
            GenericValue::Concrete(ConcreteValue::Float(*value))
        }
        GenericValue::Concrete(ConcreteValue::Bool(value)) => {
            GenericValue::Concrete(ConcreteValue::Bool(*value))
        }
        GenericValue::Concrete(ConcreteValue::Duration(value)) => {
            GenericValue::Concrete(ConcreteValue::Duration(*value))
        }
        GenericValue::Concrete(ConcreteValue::StringList(value)) => {
            GenericValue::Concrete(ConcreteValue::StringList(value.clone()))
        }
        GenericValue::Concrete(ConcreteValue::List(_) | ConcreteValue::Map(_)) => {
            unreachable!("custom-type validation receives only scalar values")
        }
        GenericValue::Deferred(_) => {
            unreachable!("deferred values returned before provider validation")
        }
    };
    config.custom_type_resolver().validate(
        crate::parser::CustomTypeValidationTarget::Annotation(owned),
        &value,
    )
}

/// Check that a function argument matches the declared parameter type.
fn check_fn_arg_type<Source: Clone>(
    fn_name: &str,
    param_name: &str,
    type_expr: &TypeExpr,
    argument: &FunctionArgument,
    ctx: &ParseContext<'_, Source>,
    call_line: usize,
    site: Option<crate::validation::deferred_types::ProviderTypeCheckSite>,
) -> Result<FunctionArgumentTypeCheck, ParseError> {
    if argument.pending_parameter().is_some() {
        return Ok(FunctionArgumentTypeCheck::AwaitingRuntimeValue);
    }
    // An authored resource handle already proves its resource-type identity.
    // A matching dotted parameter can be checked without querying provider
    // scalar schemas, preserving pure handle calls in static expressions.
    if ctx.session.is_none()
        && !ctx.config.custom_types_loaded()
        && let (TypeExpr::DottedUnresolved(path), Some(handle)) = (type_expr, argument.handle())
        && crate::validation::check_handle_sink(
            handle,
            &crate::validation::ResolvedParameterType::Handle(path.clone()),
        )
        .is_ok()
    {
        return Ok(FunctionArgumentTypeCheck::Complete);
    }
    if ctx.session.is_none()
        && !ctx.config.custom_types_loaded()
        && annotation_requires_provider_session(type_expr)
    {
        return Ok(FunctionArgumentTypeCheck::AwaitingProviderSession);
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
                if let Some(session) = ctx.session {
                    match site {
                        Some(site) => session.record(type_expr, site),
                        None => session.record_type(
                            type_expr,
                            &format!("function '{fn_name}' parameter '{param_name}'"),
                        ),
                    }
                }
                return Ok(if argument.handle().is_some() {
                    FunctionArgumentTypeCheck::DeferredHandleAnnotation
                } else {
                    FunctionArgumentTypeCheck::RecordedProviderDeferral
                });
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

    let provider_context = ctx.typecheck_provider_context();
    let check = crate::validation::typecheck::check_type_expr_value_with_context(
        &type_expr,
        value,
        provider_context,
        crate::validation::typecheck::TypecheckInference::Unavailable,
    );
    let result = finish_function_type_check(check, provider_context, site)
        .map_err(|mismatch| function_argument_type_error(fn_name, param_name, mismatch))?;
    if result == FunctionArgumentTypeCheck::Complete
        && !value.is_fully_resolved()
        && requires_concrete_annotation_check(&type_expr)
    {
        Ok(FunctionArgumentTypeCheck::AwaitingRuntimeValue)
    } else {
        Ok(result)
    }
}

/// Shape-only annotations are checked against the declared source by the
/// binding pass. Refinements/literals also need the eventual concrete value;
/// keep their annotated call instead of erasing that remaining obligation.
fn requires_concrete_annotation_check(expected: &TypeExpr) -> bool {
    match expected {
        TypeExpr::Simple(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::UnloadedSchemaType(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::StringLiteral(_) => true,
        TypeExpr::List(inner) | TypeExpr::Map(inner) => requires_concrete_annotation_check(inner),
        TypeExpr::Union(members) => members.iter().any(requires_concrete_annotation_check),
        TypeExpr::Struct { fields } => fields
            .iter()
            .any(|(_, field)| requires_concrete_annotation_check(field)),
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Ref(_) => false,
    }
}

fn annotation_requires_provider_session(expected: &TypeExpr) -> bool {
    match expected {
        TypeExpr::UnloadedSchemaType(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::DottedUnresolved(_) => true,
        TypeExpr::List(inner) | TypeExpr::Map(inner) => annotation_requires_provider_session(inner),
        TypeExpr::Union(members) => members.iter().any(annotation_requires_provider_session),
        TypeExpr::Struct { fields } => fields
            .iter()
            .any(|(_, field)| annotation_requires_provider_session(field)),
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::Ref(_)
        | TypeExpr::StringLiteral(_) => false,
    }
}

fn finish_function_type_check(
    check: crate::validation::typecheck::TypeExprValueCheck<'_>,
    provider_context: crate::validation::typecheck::TypecheckProviderContext<'_>,
    site: Option<crate::validation::deferred_types::ProviderTypeCheckSite>,
) -> Result<FunctionArgumentTypeCheck, crate::validation::typecheck::TypeExprValueError> {
    use crate::validation::typecheck::TypeExprValueCheck;
    match check {
        TypeExprValueCheck::Assignable => Ok(FunctionArgumentTypeCheck::Complete),
        TypeExprValueCheck::Mismatch(error) => Err(error),
        deferred @ TypeExprValueCheck::Deferred(_)
            if provider_context.requires_provider_schemas() =>
        {
            deferred
                .require_provider_schemas()
                .map(|()| FunctionArgumentTypeCheck::Complete)
        }
        TypeExprValueCheck::Deferred(deferred) => match provider_context.session() {
            Some(session) => {
                let check = TypeExprValueCheck::Deferred(deferred);
                match site {
                    Some(site) => check.record_at(session, site)?,
                    None => check.record_deferred(session)?,
                }
                Ok(FunctionArgumentTypeCheck::RecordedProviderDeferral)
            }
            None => TypeExprValueCheck::Deferred(deferred)
                .require_provider_schemas()
                .map(|()| FunctionArgumentTypeCheck::Complete),
        },
    }
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

pub(super) fn record_deferred_function_type_check<Source: Clone>(
    function: &UserFunction,
    arguments: &[FunctionArgument],
    ctx: &ParseContext<'_, Source>,
    source_line: usize,
    source_column: Option<usize>,
) {
    if function
        .params
        .iter()
        .any(|parameter| parameter.param_type.is_some())
        || function.return_type.is_some()
    {
        ctx.record_pending_function_type_check(PendingFunctionTypeCheck {
            function_name: function.name.clone(),
            arguments: function
                .params
                .iter()
                .enumerate()
                .filter_map(|(index, parameter)| {
                    arguments
                        .get(index)
                        .cloned()
                        .or_else(|| parameter.default.clone().map(FunctionArgument::from_value))
                })
                .collect(),
            source_line,
            source_column,
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

pub(crate) fn evaluate_user_function_eval<Source: Clone>(
    func: &UserFunction,
    args: Vec<EvalValue>,
    ctx: &ParseContext<'_, Source>,
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
    session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ParseError> {
    let ctx = ParseContext::for_evaluation_in_session(session);
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
            &ctx,
            check.source_line,
            Some(
                crate::validation::deferred_types::ProviderTypeCheckSite::at_line(
                    check
                        .source_file
                        .as_deref()
                        .unwrap_or_else(|| std::path::Path::new("<config>")),
                    check.source_line,
                    check.source_column.unwrap_or(1),
                    format!(
                        "function '{}' parameter '{}'",
                        function.name, parameter.name
                    ),
                ),
            ),
        );
        match result {
            Ok(FunctionArgumentTypeCheck::Complete)
            | Ok(FunctionArgumentTypeCheck::AwaitingRuntimeValue)
            | Ok(FunctionArgumentTypeCheck::RecordedProviderDeferral)
            | Ok(FunctionArgumentTypeCheck::DeferredHandleAnnotation) => {}
            Ok(FunctionArgumentTypeCheck::AwaitingProviderSession) => {
                return Err(ParseError::UserFunctionError(
                    "provider annotation check requires its session".into(),
                ));
            }
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
    config: crate::validation::typecheck::TypecheckProviderContext<'_>,
) -> Result<FunctionArgumentTypeCheck, ParseError> {
    let check = crate::validation::typecheck::check_type_expr_value_with_context(
        type_expr,
        value,
        config,
        crate::validation::typecheck::TypecheckInference::Unavailable,
    );
    let result = finish_function_type_check(check, config, None)
        .map_err(|mismatch| function_return_type_error(fn_name, mismatch))?;
    if result == FunctionArgumentTypeCheck::Complete
        && !value.is_fully_resolved()
        && requires_concrete_annotation_check(type_expr)
    {
        Ok(FunctionArgumentTypeCheck::AwaitingRuntimeValue)
    } else {
        Ok(result)
    }
}

fn function_return_type_error(
    fn_name: &str,
    mismatch: crate::validation::typecheck::TypeExprValueError,
) -> ParseError {
    ParseError::UserFunctionError(format!("function '{fn_name}': return value: {mismatch}"))
}

/// Evaluate a user-defined function call by substituting arguments into the body
pub(crate) fn evaluate_user_function<Source: Clone>(
    func: &UserFunction,
    args: &[FunctionArgument],
    ctx: &ParseContext<'_, Source>,
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
        let return_check = check_fn_return_type(
            &func.name,
            return_type,
            &result,
            child_ctx.typecheck_provider_context(),
        )
        .map_err(|error| locate_function_body_error(func, error))?;
        let retain_check = match return_check {
            FunctionArgumentTypeCheck::RecordedProviderDeferral
            | FunctionArgumentTypeCheck::AwaitingRuntimeValue => true,
            FunctionArgumentTypeCheck::Complete
            | FunctionArgumentTypeCheck::DeferredHandleAnnotation => false,
            FunctionArgumentTypeCheck::AwaitingProviderSession => {
                return Err(ParseError::UserFunctionError(
                    "provider return annotation check requires its session".into(),
                ));
            }
        };
        if retain_check {
            ctx.record_pending_function_type_check(PendingFunctionTypeCheck {
                function_name: func.name.clone(),
                arguments: func
                    .params
                    .iter()
                    .filter_map(|parameter| substitutions.get(&parameter.name).cloned())
                    .collect(),
                source_line: func.body.source_line,
                source_column: None,
                source_file: func
                    .annotation_source_file
                    .clone()
                    .or_else(|| func.body.source_file.clone()),
            });
            return Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function: ctx
                    .resolved_user_function(&func.name)
                    .unwrap_or_else(|| std::sync::Arc::new(func.clone())),
                functions: ctx.shared_user_function_scope(),
                args: func
                    .params
                    .iter()
                    .map(|parameter| substitutions[&parameter.name].clone())
                    .collect(),
                source_line: call_line.into(),
            }));
        }
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
    session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
    call_line: usize,
) -> Result<Value, ParseError> {
    let mut context = match session {
        Some(session) => ParseContext::for_evaluation_in_session(session),
        None => ParseContext::for_evaluation(config),
    };
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

type ParserStageValue<B> = GenericValue<ParserPlaceholder, B>;
type ParserStageFunctionArgument<B> = crate::resource::FunctionArgument<ParserPlaceholder, B>;

fn substituted_function_argument<B: BindingAddress>(
    name: &str,
    argument: &ParserStageFunctionArgument<B>,
    position: FunctionSubstitutionPosition,
    source_line: usize,
) -> Result<ParserStageValue<B>, ParseError> {
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

fn substitute_function_call_argument<B: BindingAddress>(
    argument: &ParserStageFunctionArgument<B>,
    substitutions: &HashMap<String, ParserStageFunctionArgument<B>>,
    source_line: usize,
) -> Result<ParserStageFunctionArgument<B>, ParseError> {
    let Some(value) = argument.value() else {
        return Ok(argument.clone());
    };
    if let GenericValue::Deferred(DeferredValue::Placeholder(
        ParserPlaceholder::FnParam { name } | ParserPlaceholder::FnLocal { name },
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
pub(crate) fn substitute_fn_params<B: BindingAddress>(
    value: &ParserStageValue<B>,
    substitutions: &HashMap<String, ParserStageFunctionArgument<B>>,
    source_line: usize,
) -> Result<ParserStageValue<B>, ParseError> {
    substitute_fn_params_with_position(
        value,
        substitutions,
        source_line,
        FunctionSubstitutionPosition::Value,
    )
}

fn substitute_fn_params_with_position<B: BindingAddress>(
    value: &ParserStageValue<B>,
    substitutions: &HashMap<String, ParserStageFunctionArgument<B>>,
    source_line: usize,
    position: FunctionSubstitutionPosition,
) -> Result<ParserStageValue<B>, ParseError> {
    Ok(match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let deferred = deferred
                .try_map(|value| substitute_fn_params(value, substitutions, source_line))?;
            GenericValue::Deferred(DeferredValue::ModuleIf(Box::new(deferred)))
        }
        GenericValue::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam { name }))
        | GenericValue::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnLocal { name })) => {
            match substitutions.get(name) {
                Some(argument) => {
                    substituted_function_argument(name, argument, position, source_line)?
                }
                None => value.clone(),
            }
        }
        GenericValue::Deferred(DeferredValue::ResourceRef { path })
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
        GenericValue::Concrete(ConcreteValue::List(items)) => {
            GenericValue::Concrete(ConcreteValue::List(
                items
                    .iter()
                    .map(|v| substitute_fn_params(v, substitutions, source_line))
                    .collect::<Result<_, _>>()?,
            ))
        }
        GenericValue::Concrete(ConcreteValue::Map(map)) => {
            GenericValue::Concrete(ConcreteValue::Map(
                map.iter()
                    .map(|(k, v)| {
                        Ok((
                            k.clone(),
                            substitute_fn_params(v, substitutions, source_line)?,
                        ))
                    })
                    .collect::<Result<_, ParseError>>()?,
            ))
        }
        GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line: call_line,
        }) => GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
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
        GenericValue::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line: call_line,
        }) => GenericValue::Deferred(DeferredValue::FunctionCall {
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
        GenericValue::Deferred(DeferredValue::Unavailable {
            name,
            args,
            reason,
            source_line: call_line,
        }) => GenericValue::Deferred(DeferredValue::Unavailable {
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
        GenericValue::Deferred(DeferredValue::Access(access)) => access
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
        GenericValue::Deferred(DeferredValue::Interpolation(parts)) => {
            GenericValue::Deferred(DeferredValue::Interpolation(
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
        GenericValue::Deferred(DeferredValue::Secret(inner)) => {
            GenericValue::Deferred(DeferredValue::Secret(Box::new(substitute_fn_params(
                inner,
                substitutions,
                source_line,
            )?)))
        }
        GenericValue::Deferred(DeferredValue::Coalesce { value, default }) => {
            GenericValue::Deferred(DeferredValue::Coalesce {
                value: Box::new(substitute_fn_params(value, substitutions, source_line)?),
                default: Box::new(substitute_fn_params(default, substitutions, source_line)?),
            })
        }
        GenericValue::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = substitute_fn_params(expression.lhs(), substitutions, source_line)?;
            let rhs = substitute_fn_params(expression.rhs(), substitutions, source_line)?;
            let span = expression.operator_span();
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| ParseError::integer_arithmetic(span, source_line, error))?
        }
        GenericValue::Deferred(DeferredValue::Unknown(_))
        | GenericValue::Deferred(DeferredValue::Placeholder(
            ParserPlaceholder::ModuleArgument {
                name: _,
                diagnostic: _,
            }
            | ParserPlaceholder::Loop(_)
            | ParserPlaceholder::EmptyInterpolation { source: _ },
        ))
        | GenericValue::Deferred(DeferredValue::ResourceRef { path: _ })
        | GenericValue::Deferred(DeferredValue::BindingRef { binding: _ })
        | GenericValue::Deferred(DeferredValue::RedactedSecret { hash: _ })
        | GenericValue::Concrete(
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
pub(crate) fn function_argument_as_eval<B: BindingAddress>(
    argument: &crate::resource::FunctionArgument<ParserPlaceholder, B>,
) -> EvalValue<B> {
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

pub(super) fn try_evaluate_fn_value<Source: Clone>(
    value: Value,
    ctx: &ParseContext<'_, Source>,
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
                ctx.session,
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
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => Ok(value),
    }
}
