//! User-defined function parsing, type checking, and evaluation.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ast::{FnParam, PendingFunctionTypeCheck, UserFunction, UserFunctionBody};
use super::context::{ParseContext, next_pair};
use super::error::{ParseError, ParseWarning};
use super::types::parse_type_expr;
use super::{ParsedFunctionArgument as FunctionArgument, ParsedValue as Value};
use super::{ProviderContext, Rule, is_static_eval, parse_expression};
use crate::eval_value::{EvalValue, UnexpandedStructuralHandleKind};
use crate::function_evaluation::{
    AnnotationError, ArgumentStep, FunctionBodyCompletion, FunctionCompletion, FunctionEvaluation,
    LocalStep, ParameterAnnotationCheck, PreparedFunction, RetainedCall,
    annotation_requires_provider_session, check_parameter_annotation,
    contains_deferred_user_function_call,
};
use crate::resource::InsideSecret;
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, ParserPlaceholder, PlaceholderKind,
    Value as GenericValue, ValueString,
};
use crate::schema::TypeIdentity;
use indexmap::IndexMap;
use std::sync::Arc;

/// Parse a user-defined function definition
pub(super) fn parse_fn_def(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    warnings: &mut Vec<ParseWarning>,
    preprocessed_file: &super::PreprocessedFile,
) -> Result<UserFunction, ParseError> {
    let type_annotations =
        super::elements::function_type_annotations(&pair, preprocessed_file, ctx.source_file());
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
                            default = Some(
                                parse_expression(remaining, &default_ctx)?
                                    .projection()
                                    .expect("materialized parser expression")
                                    .clone(),
                            );
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
                )?
                .projection()
                .expect("materialized parser expression")
                .clone();
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
                    location: super::DeclarationLocation::from_pair(
                        &body_inner,
                        preprocessed_file,
                        body_ctx.source_file(),
                    ),
                    value: parse_expression(body_inner, &body_ctx)?
                        .projection()
                        .expect("materialized parser expression")
                        .clone(),
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
        type_annotations,
        name,
        params,
        return_type,
        return_type_span,
        annotation_source_file: Some(ctx.source_file().to_path_buf()),
        local_lets,
        body,
    })
}

enum PreparedUserFunctionCall<'function, 'cfg, Source = super::context::AuthoredDiagnosticSource> {
    Ready {
        child_ctx: Box<ParseContext<'cfg, Source>>,
        completion: FunctionBodyCompletion<
            'function,
            ParserPlaceholder,
            crate::resource::RelativeBindingName,
        >,
    },
    Deferred(RetainedCall<ParserPlaceholder, crate::resource::RelativeBindingName>),
}

/// Prepare a user-defined function call: validate args, build substitutions, and return
/// the child context with all parameters and local lets resolved.
fn prepare_user_function_call<'function, 'cfg, Source: super::context::EvaluationSource>(
    func: &'function Arc<UserFunction>,
    args: &[FunctionArgument],
    ctx: &ParseContext<'cfg, Source>,
    call_line: usize,
) -> Result<PreparedUserFunctionCall<'function, 'cfg, Source>, ParseError> {
    let fn_name = &func.name;

    // Check recursion
    if ctx.evaluating_functions.contains(fn_name) {
        return Err(ParseError::RecursiveFunction(fn_name.clone()));
    }

    let mut completion = FunctionCompletion::new(
        func,
        &ctx.shared_user_function_scope(),
        args.to_vec(),
        call_line.into(),
    )
    .map_err(|error| ParseError::UserFunctionError(error.parser_message(fn_name)))?;
    if ctx
        .session
        .is_some_and(|session| session.function_declaration_failed(func))
    {
        return Ok(PreparedUserFunctionCall::Deferred(completion.retain()));
    }

    // Build substitution map: param_name -> value, and type-check annotated params
    let mut effective_args = Vec::with_capacity(func.params.len());
    let requires_session = ctx.session.is_none()
        && !ctx.config.custom_types_loaded()
        && func
            .return_type
            .as_ref()
            .is_some_and(annotation_requires_provider_session);
    let mut has_deferred_type_check = requires_session;
    let prepared = loop {
        let slot = match completion.next_argument() {
            ArgumentStep::Argument(slot) => slot,
            ArgumentStep::Complete(prepared) => break prepared,
        };
        let param = slot.parameter();
        let argument = match slot.argument() {
            crate::function_evaluation::ArgumentSource::Provided(argument) => argument.clone(),
            crate::function_evaluation::ArgumentSource::Default(value) => {
                FunctionArgument::from_value(value.clone())
            }
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
        let (next, report) = slot
            .check(
                argument.clone(),
                ctx.typecheck_provider_context(),
                || None,
                &format!("function '{}' parameter '{}'", func.name, param.name),
            )
            .map_err(|error| {
                function_argument_annotation_error(func, param, &argument, call_line, error)
            })?;
        completion = next;
        has_deferred_type_check |= report.needs_recheck();
        effective_args.push(argument);
    };
    if has_deferred_type_check {
        ctx.record_pending_function_type_check(PendingFunctionTypeCheck {
            location: func.body.location.clone(),
            evaluation: ctx.evaluation_site_at(&func.body.location),
            function_name: fn_name.clone(),
            arguments: effective_args.clone(),
            source_line: call_line,
            source_column: None,
            source_file: None,
        });
    }
    let mut completion = match prepared {
        PreparedFunction::Retain(call) => return Ok(PreparedUserFunctionCall::Deferred(call)),
        PreparedFunction::Ready { completion } => {
            if requires_session
                || effective_args
                    .iter()
                    .any(function_argument_must_remain_deferred)
            {
                return Ok(PreparedUserFunctionCall::Deferred(completion.retain()));
            }
            completion
        }
    };

    // Create a child context with recursion tracking
    let mut child_ctx = ctx.child_scope();
    child_ctx.evaluating_functions.push(fn_name.clone());

    // Evaluate local lets, substituting and resolving each one
    let completion = loop {
        let slot = match completion.next_local() {
            LocalStep::Local(slot) => slot,
            LocalStep::Body(body) => break body,
        };
        let let_name = slot.name().to_owned();
        let source_line = slot.source_line();
        let local = slot
            .substitute_argument()
            .map_err(|error| locate_function_body_error(func, error))?
            .try_map_value(|value| {
                try_evaluate_fn_value(value.clone(), &child_ctx, source_line)
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
        completion = slot.bind(local);
    };

    Ok(PreparedUserFunctionCall::Ready {
        child_ctx: Box::new(child_ctx),
        completion,
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
            inner.visit(|secret_value| { contains_module_argument_matching(secret_value, predicate) })
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

/// Bind owner-routing unit tests to their registered schema descriptor.
#[cfg(test)]
pub(crate) fn custom_type_lookup_for_schema<'resolver, 'schema>(
    resolver: &'resolver dyn crate::parser::CustomTypeResolver,
    schema: crate::schema::OwnerBoundResourceSchema<'schema>,
) -> impl crate::schema::ValidatedCustomTypeLookup + use<'resolver, 'schema> {
    custom_type_lookup_for_owner(resolver, schema.owner())
}

/// Share dispatch and canonicalization policy between bound schemas and
/// admission contracts. The caller derives the owner from its descriptor.
pub(crate) fn custom_type_lookup_for_owner<'resolver, 'owner>(
    resolver: &'resolver dyn crate::parser::CustomTypeResolver,
    owner: &'owner str,
) -> impl crate::schema::ValidatedCustomTypeLookup + use<'resolver, 'owner> {
    struct Lookup<'a, 'b> {
        resolver: &'a dyn crate::parser::CustomTypeResolver,
        owner: &'b str,
    }
    impl crate::schema::ValidatedCustomTypeLookup for Lookup<'_, '_> {
        fn validate(
            &self,
            value: crate::schema::LocallyValidatedCustomType<'_>,
        ) -> Result<(), crate::schema::TypeError> {
            self.resolver
                .validate_validated(self.owner, value)
                .map_err(Into::into)
        }

        fn canonicalization_error(
            &self,
            declaration: crate::parser::IdentifiedCustomTypeDeclaration<'_>,
            error: crate::schema::TypeError,
        ) -> crate::schema::TypeError {
            custom_type_canonicalization_error(self.owner, declaration, error)
        }
    }
    Lookup { resolver, owner }
}

/// Bind a provider-config value validator to its selected factory. Dispatch
/// consumes the proof produced by the combined walk, never a raw value.
pub fn custom_type_lookup_for_factory<'a>(
    factory: &'a dyn crate::provider::ProviderFactory,
    host: &'a crate::provider::ProviderValidationInputs,
) -> impl crate::schema::ValidatedCustomTypeLookup + 'a {
    struct Lookup<'a>(
        &'a dyn crate::provider::ProviderFactory,
        &'a crate::provider::ProviderValidationInputs,
    );
    impl crate::schema::ValidatedCustomTypeLookup for Lookup<'_> {
        fn validate(
            &self,
            value: crate::schema::LocallyValidatedCustomType<'_>,
        ) -> Result<(), crate::schema::TypeError> {
            crate::provider::validate_validated_custom_type_with_factory(self.0, value, self.1)
                .map_err(Into::into)
        }

        fn canonicalization_error(
            &self,
            declaration: crate::parser::IdentifiedCustomTypeDeclaration<'_>,
            error: crate::schema::TypeError,
        ) -> crate::schema::TypeError {
            custom_type_canonicalization_error(self.0.name(), declaration, error)
        }
    }
    Lookup(factory, host)
}

pub(crate) fn custom_type_canonicalization_error(
    owner: &str,
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'_>,
    error: crate::schema::TypeError,
) -> crate::schema::TypeError {
    crate::parser::CustomTypeValidationError::InvalidDeclarationValue {
        owner: owner.to_string(),
        identity: declaration.identity().clone(),
        error: Box::new(error),
    }
    .into()
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
    value: &crate::resource::knowledge::KnownValue<'_, P, B>,
    _config: &ProviderContext,
) -> crate::provider::HostResult<()> {
    let declaration = crate::schema::builtin_type_registry()
        .get(identity)
        .ok_or_else(|| {
            crate::provider::HostError::invalid_input(format!(
                "unknown custom type `{identity}`; provider-owned annotations must resolve to an owner-qualified schema type"
            ))
        })?;
    validate_builtin_custom_type(declaration, value).map_err(|error| {
        crate::provider::ProviderError::invalid_input_caused_by(
            "Custom type validation failed",
            error,
        )
        .with_secrets(
            crate::provider::ProviderValidationInputs::collect(
                identity.provider.as_deref().unwrap_or("builtin"),
                [&value.to_value()],
            )
            .redaction_set(),
        )
    })
}

/// Validate a core declaration's scalar shape and constraints, preserving the
/// DSL's textual interpretation of unresolved enum identifiers.
pub(crate) fn validate_builtin_custom_type<P: PlaceholderKind, B: BindingAddress>(
    declaration: &crate::schema::CompiledAttributeType,
    value: &crate::resource::knowledge::KnownValue<'_, P, B>,
) -> Result<(), crate::schema::TypeError> {
    let raw = value.to_value();
    if let GenericValue::Concrete(ConcreteValue::EnumIdentifier(raw)) = &raw {
        let text: GenericValue =
            GenericValue::Concrete(ConcreteValue::String(raw.as_str().to_owned().into()));
        let known = text
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .and_then(|proof| proof.into_known())
            .map_err(crate::schema::TypeError::from)?;
        declaration.validate_identified_scalar_value(&known)
    } else {
        declaration.validate_identified_scalar_value(value)
    }
}

/// Validate an authored provider schema annotation against every owner that
/// declares its semantic identity.
///
/// ```compile_fail,E0308
/// use carina_core::parser::{
///     LoadedProviderContext, OwnedTypeIdentity, validate_schema_custom_type,
/// };
/// use carina_core::resource::Value;
///
/// fn check(owned: &OwnedTypeIdentity, value: &Value, config: &LoadedProviderContext) {
///     let _ = validate_schema_custom_type(owned, value, config);
/// }
/// ```
pub fn validate_schema_custom_type<P: PlaceholderKind, B: BindingAddress>(
    owned: &crate::parser::OwnedTypeIdentity,
    value: &crate::resource::knowledge::KnownValue<'_, P, B>,
    config: &crate::parser::LoadedProviderContext,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    validate_schema_custom_type_key_with_context(owned, value, config, None)
}

pub(crate) fn validate_schema_custom_type_with_context<P: PlaceholderKind, B: BindingAddress>(
    owned: &crate::parser::ResolvedSchemaType,
    value: &crate::resource::knowledge::KnownValue<'_, P, B>,
    config: &ProviderContext,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    validate_schema_custom_type_key_with_context(owned, value, config, Some(owned))
}

fn validate_schema_custom_type_key_with_context<P: PlaceholderKind, B: BindingAddress>(
    owned: &crate::parser::OwnedTypeIdentity,
    value: &crate::resource::knowledge::KnownValue<'_, P, B>,
    config: &ProviderContext,
    witness: Option<&crate::parser::ResolvedSchemaType>,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    if value.visit(
        |view| {
            matches!(
                view,
                crate::resource::knowledge::KnownView::List(_)
                    | crate::resource::knowledge::KnownView::Map(_)
                    | crate::resource::knowledge::KnownView::StringList(_)
            )
        },
        |_| false,
    ) {
        return owned
            .scalar_kind()
            .identified_type::<crate::schema::Compiled>(owned.identity().clone())
            .validate_identified_scalar_value(value)
            .map_err(
                |error| super::CustomTypeValidationError::InvalidDeclarationValue {
                    owner: owned
                        .provider_names()
                        .next()
                        .expect("nonempty owners")
                        .into(),
                    identity: owned.identity().clone(),
                    error: Box::new(error),
                },
            );
    }
    let raw = value.to_value();
    if value.visit(|_| false, |_| true) {
        return Ok(());
    }
    let value: GenericValue = raw
        .try_map_placeholder(&mut |_| Err::<GenericValue<crate::resource::NoPlaceholder, B>, _>(()))
        .expect("known values contain no substitution slots")
        .map_bindings(&mut |_| unreachable!("known values contain no reference bindings"));
    let known = value
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .and_then(|proof| proof.into_known())
        .map_err(
            |error| super::CustomTypeValidationError::InvalidDeclarationValue {
                owner: owned
                    .provider_names()
                    .next()
                    .expect("nonempty owners")
                    .into(),
                identity: owned.identity().clone(),
                error: Box::new(error.into()),
            },
        )?;
    let projection = witness
        .map(|witness| witness.resolve_projection(config))
        .transpose()?
        .flatten();
    if let Some(projection) = projection.as_ref() {
        if let Some(declaration) = super::IdentifiedCustomTypeDeclaration::new(projection) {
            super::validate_custom_type_value(
                config.custom_type_resolver(),
                super::CustomTypeValidationTarget::Declaration {
                    owner: owned.provider_names().next().expect("nonempty owners"),
                    declaration,
                },
                &known,
            )?;
        } else {
            projection
                .validate_identified_scalar_value(&known)
                .map_err(
                    |error| super::CustomTypeValidationError::InvalidDeclarationValue {
                        owner: owned
                            .provider_names()
                            .next()
                            .expect("nonempty owners")
                            .into(),
                        identity: owned.identity().clone(),
                        error: Box::new(error),
                    },
                )?;
        }
    } else {
        super::validate_custom_type_value(
            config.custom_type_resolver(),
            super::CustomTypeValidationTarget::Annotation(owned),
            &known,
        )?;
    }
    // The resolved witness can cross a catalog boundary (including serde).
    // Retain its constraints even when the current catalog is less restrictive.
    if let Some(witness) = witness {
        let sensitive = value.contains_diagnostic_sensitive_string();
        let invalid_value = |error: crate::schema::TypeError| {
            super::CustomTypeValidationError::InvalidDeclarationValue {
                owner: owned
                    .provider_names()
                    .next()
                    .expect("nonempty owners")
                    .into(),
                identity: owned.identity().clone(),
                error: Box::new(if sensitive {
                    error.inside_secret()
                } else {
                    error
                }),
            }
        };
        let enumeration = match projection.as_ref() {
            Some(declaration)
                if matches!(
                    declaration.raw_shape(),
                    crate::schema::RawShape::Enum { .. }
                ) =>
            {
                Some(declaration.clone())
            }
            Some(_) => None,
            None => config.custom_type_resolver().enum_declaration(owned)?,
        };
        let canonical_text = if let Some(enumeration) = enumeration {
            crate::provider::canonicalize_custom_type_value(&enumeration, &value)
                .map_err(|error| invalid_value(error.into_type_error()))?
                .map(|canonical| canonical.as_str().to_string())
        } else {
            match &value {
                GenericValue::Concrete(ConcreteValue::EnumIdentifier(raw)) => {
                    Some(raw.as_str().to_string())
                }
                GenericValue::Concrete(ConcreteValue::CanonicalEnum(canonical)) => {
                    Some(canonical.api_value().to_string())
                }
                _ => None,
            }
        };
        let scalar = match canonical_text {
            Some(text) => {
                let source = match &value {
                    GenericValue::Concrete(ConcreteValue::String(source)) => Some(source),
                    _ => None,
                };
                let text = ValueString::derived_from(text, source);
                crate::provider::bridge_text_to_typed_value(owned.scalar_kind(), text).map_err(
                    |error| {
                        invalid_value(crate::schema::TypeError::ValidationFailed {
                            message: crate::executor::render_error_chain(&error).into(),
                        })
                    },
                )?
            }
            None => value,
        };
        let scalar = scalar
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .and_then(|proof| proof.into_known())
            .map_err(|error| invalid_value(error.into()))?;
        witness
            .materialize()
            .validate_identified_scalar_value(&scalar)
            .map_err(invalid_value)?;
    }
    Ok(())
}

/// Check that a function argument matches the declared parameter type.
fn check_fn_arg_type<Source: super::context::EvaluationSource>(
    function: &UserFunction,
    parameter: &FnParam,
    argument: &FunctionArgument,
    ctx: &ParseContext<'_, Source>,
    call_line: usize,
    site: Option<crate::validation::deferred_types::ProviderTypeCheckSite>,
) -> Result<
    Option<ParameterAnnotationCheck<ParserPlaceholder, crate::resource::RelativeBindingName>>,
    ParseError,
> {
    if parameter.param_type.is_none() {
        return Ok(None);
    }
    check_parameter_annotation(
        parameter,
        argument.clone(),
        ctx.typecheck_provider_context(),
        || site,
        &format!(
            "function '{}' parameter '{}'",
            function.name, parameter.name
        ),
    )
    .map(Some)
    .map_err(|error| {
        function_argument_annotation_error(function, parameter, argument, call_line, error)
    })
}

fn function_argument_annotation_error(
    function: &UserFunction,
    parameter: &FnParam,
    argument: &FunctionArgument,
    call_line: usize,
    error: AnnotationError,
) -> ParseError {
    match error {
        AnnotationError::Resolution(error) => {
            let name = match &error {
                    crate::static_type::StaticTypeResolutionError::UnknownCustomType { name }
                    | crate::static_type::StaticTypeResolutionError::UnknownCustomTypeWithSuggestion { name, .. }
                    | crate::static_type::StaticTypeResolutionError::ConflictingScalarKinds { name, .. }
                    | crate::static_type::StaticTypeResolutionError::MissingDeclaration { name, .. } => name,
                };
            let error = match parameter.type_span {
                Some(span) => {
                    ParseError::unresolved_declaration_type(name, span, error.to_string())
                }
                None => ParseError::UserFunctionError(error.to_string()),
            };
            if let Some(annotation) = function
                .type_annotations
                .iter()
                .find(|annotation| Some(annotation.location().span()) == parameter.type_span)
            {
                return error.at(annotation.location());
            }
            match &function.annotation_source_file {
                Some(path) => error.in_file(path.clone()),
                None => error,
            }
        }
        AnnotationError::Handle(error) => {
            if error.is_bare_value_sink()
                && let Some(handle) = argument.handle()
            {
                return ParseError::from_eval_value(
                    crate::eval_value::EvalValueError::BareStructuralHandle(handle.bare_error()),
                    call_line,
                );
            }
            ParseError::UserFunctionError(format!(
                "function '{}': parameter '{}': {error}",
                function.name, parameter.name
            ))
        }
        AnnotationError::Value(error) => {
            function_argument_type_error(&function.name, &parameter.name, error)
        }
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

pub(crate) fn construct_builtin_call(
    builtin: crate::builtins::Builtin,
    args: Vec<EvalValue>,
    ctx: &ParseContext<'_, impl super::context::EvaluationSource>,
    line: usize,
) -> Result<EvalValue, ParseError> {
    reject_builtin_handle_arguments(&args, line)
        .map_err(|error| builtin.protect_parse_error(error, line))?;
    let args = lower_function_arguments(args, line)
        .map_err(|error| builtin.protect_parse_error(error, line))?;
    builtin
        .call(args, line.into(), |value| {
            if value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .is_ok_and(|knowledge| knowledge.permits_static_evaluation())
            {
                // Normalize successful authored inputs here. A failed static
                // evaluation stays deferred so the normal strict/permissive
                // resolver policy still owns unavailable values.
                Ok(try_evaluate_fn_value(value.clone(), ctx, line).unwrap_or(value))
            } else {
                Ok(value)
            }
        })
        .map(EvalValue::from_value)
}

pub(super) fn record_deferred_function_type_check<Source: super::context::EvaluationSource>(
    function: &UserFunction,
    arguments: &[FunctionArgument],
    ctx: &ParseContext<'_, Source>,
    source_line: usize,
    source_column: Option<usize>,
    location: super::DeclarationLocation,
) {
    if function
        .params
        .iter()
        .any(|parameter| parameter.param_type.is_some())
        || function.return_type.is_some()
    {
        ctx.record_pending_function_type_check(PendingFunctionTypeCheck {
            evaluation: ctx.evaluation_site_at(&location),
            location,
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

pub(crate) fn evaluate_user_function_eval<Source: super::context::EvaluationSource>(
    func: &Arc<UserFunction>,
    args: Vec<EvalValue>,
    ctx: &ParseContext<'_, Source>,
    line: usize,
) -> Result<FunctionEvaluation<ParserPlaceholder, crate::resource::RelativeBindingName>, ParseError>
{
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
    functions: &IndexMap<String, UserFunction>,
    session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ParseError> {
    let ctx = ParseContext::for_evaluation_in_session(session);
    ctx.resume_evaluation(&check.evaluation)
        .evaluate(|| {
            let Some(function) = functions.get(&check.function_name) else {
                return Ok(());
            };
            if session.function_declaration_failed(function) {
                return Ok(());
            }
            for (parameter, argument) in function.params.iter().zip(&check.arguments) {
                if function_argument_awaits_module_substitution(argument) {
                    continue;
                }
                let result = check_fn_arg_type(
                    function,
                    parameter,
                    argument,
                    &ctx,
                    check.source_line,
                    Some(
                        crate::validation::deferred_types::ProviderTypeCheckSite::new(
                            check.location.clone(),
                            format!(
                                "function '{}' parameter '{}'",
                                function.name, parameter.name
                            ),
                        ),
                    ),
                );
                match result {
                    Ok(None) => {}
                    Ok(Some(check)) => {
                        if check.requires_provider_session() {
                            return Err(ParseError::UserFunctionError(
                                "provider annotation check requires its session".into(),
                            ));
                        }
                    }
                    Err(error) => {
                        let located = match error {
                            ParseError::InvalidExpression { message, .. }
                            | ParseError::UserFunctionError(message) => {
                                ParseError::InvalidExpression {
                                    line: check.source_line,
                                    message,
                                }
                            }
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
        })
        .map_err(Into::into)
}

fn function_return_type_error(
    fn_name: &str,
    mismatch: crate::validation::typecheck::TypeExprValueError,
) -> ParseError {
    ParseError::UserFunctionError(format!("function '{fn_name}': return value: {mismatch}"))
}

/// Evaluate a user-defined function call by substituting arguments into the body
pub(crate) fn evaluate_user_function<Source: super::context::EvaluationSource>(
    func: &Arc<UserFunction>,
    args: &[FunctionArgument],
    ctx: &ParseContext<'_, Source>,
    call_line: usize,
) -> Result<FunctionEvaluation<ParserPlaceholder, crate::resource::RelativeBindingName>, ParseError>
{
    let prepared =
        prepare_user_function_call(func, args, ctx, call_line).map_err(|error| match error {
            ParseError::UserFunctionError(message) => ParseError::InvalidExpression {
                line: call_line,
                message,
            },
            other => other,
        })?;
    let (child_ctx, completion) = match prepared {
        PreparedUserFunctionCall::Ready {
            child_ctx,
            completion,
        } => (child_ctx, completion),
        PreparedUserFunctionCall::Deferred(call) => return Ok(FunctionEvaluation::Retain(call)),
    };
    let substituted_body = completion
        .substitute_body()
        .map_err(|error| locate_function_body_error(func, error))?;
    let result = try_evaluate_fn_value(substituted_body, &child_ctx, func.body.source_line)
        .map_err(|error| locate_function_body_error(func, error))?
        .canonicalize();
    let evaluation = completion
        .finish(result, child_ctx.typecheck_provider_context(), || None)
        .map_err(|error| {
            locate_function_body_error(func, function_return_type_error(&func.name, error))
        })?;
    match evaluation {
        FunctionEvaluation::Inline(value) => Ok(FunctionEvaluation::Inline(value)),
        FunctionEvaluation::Retain(call) => {
            if func.return_type.is_some() {
                ctx.record_pending_function_type_check(PendingFunctionTypeCheck {
                    location: func.body.location.clone(),
                    evaluation: ctx.evaluation_site_at(&func.body.location),
                    function_name: func.name.clone(),
                    arguments: call.arguments_for_recheck(),
                    source_line: func.body.source_line,
                    source_column: None,
                    source_file: func
                        .annotation_source_file
                        .clone()
                        .or_else(|| func.body.source_file.clone()),
                });
            }
            Ok(FunctionEvaluation::Retain(call))
        }
    }
}

/// Evaluate a call that already owns its resolved lexical definition.
/// Consumers outside the parser use this instead of rebuilding a name lookup
/// table from their current (and potentially different) module scope.
pub(crate) fn evaluate_resolved_user_function(
    function: &Arc<UserFunction>,
    functions: &Arc<IndexMap<String, UserFunction>>,
    arguments: &[FunctionArgument],
    config: &ProviderContext,
    session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
    call_line: usize,
) -> Result<FunctionEvaluation<ParserPlaceholder, crate::resource::RelativeBindingName>, ParseError>
{
    let mut context = match session {
        Some(session) => ParseContext::for_evaluation_in_session(session),
        None => ParseContext::for_evaluation(config),
    };
    context.replace_shared_user_functions(Arc::clone(functions));
    if !context.user_functions.contains_key(&function.name) {
        context.insert_user_function(function.name.clone(), function.as_ref().clone());
    }
    evaluate_user_function(function, arguments, &context, call_line)
}

fn locate_function_body_error(func: &UserFunction, error: ParseError) -> ParseError {
    error.at(&func.body.location)
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
        Value::Deferred(DeferredValue::Secret(inner)) => { inner.visit(|secret_value| { contains_function_call(secret_value) }) },
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_function_call(expression.lhs()) || contains_function_call(expression.rhs())
        }
        _ => false,
    }
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
        .map_err(|error| ParseError::from_resolution(error, line));
    }

    let remaining = remaining_arity - new_args.len();
    if remaining > 0 {
        return Ok(EvalValue::closure(name, all_args, remaining));
    }

    let builtin =
        crate::builtins::Builtin::from_name(name).ok_or_else(|| ParseError::InvalidExpression {
            line,
            message: format!("Unknown built-in function: {name}"),
        })?;
    construct_builtin_call(builtin, all_args, ctx, line)
}

pub(super) fn try_evaluate_fn_value<Source: super::context::EvaluationSource>(
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
            .map(FunctionEvaluation::into_value)
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

            if !evaluated_args.iter().all(|argument| {
                argument.value().is_some_and(|value| {
                    (value)
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                        .map(|proof| proof.into_classification())
                        .is_ok_and(|knowledge| knowledge.permits_static_evaluation())
                })
            }) {
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
            crate::builtins::evaluate_builtin_with_config(
                name.as_str(),
                &eval_args,
                ctx.config,
                source_line.into(),
            )
            .and_then(|result| {
                result
                    .into_value()
                    .map_err(|error| crate::resolver::ResolutionError::from(error.to_string()))
            })
            .map_err(|error| ParseError::from_resolution(error, source_line))
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
        Value::Deferred(DeferredValue::Secret(inner)) => inner
            .try_map(|value| try_evaluate_fn_value(value.clone(), ctx, containing_line))
            .map(|inner| Value::Deferred(DeferredValue::Secret(inner))),
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
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => Ok(value),
    }
}

#[cfg(test)]
mod secret_origin_tests {
    use super::*;

    #[test]
    fn known_collection_with_pending_secret_reports_kind_error_without_rebinding() {
        let owned = crate::parser::OwnedTypeIdentity::provider(
            "mock",
            crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
            crate::schema::RefinedScalarKind::String,
        );
        let value = Value::Concrete(ConcreteValue::List(vec![
            Value::resource_ref("private_source", "value", vec![]).into_secret(),
        ]));
        let proof = value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(matches!(
            &proof,
            crate::resource::knowledge::Classification::ShapeKnown(_)
        ));
        assert!(proof.into_known().is_err());
        // A partial collection uses the existing structural validator. The
        // resolved custom-type consumer still accepts KnownValue only.
        let error = owned
            .scalar_kind()
            .identified_type::<crate::schema::Compiled>(owned.identity().clone())
            .validate(&value)
            .unwrap_err();
        assert!(error.to_string().contains("Type mismatch"), "{error}");
        assert!(!error.to_string().contains("private_source"), "{error}");
    }

    #[test]
    fn secret_origin_4275_closure_and_composition_wrap_deferred_arguments() {
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let ctx = ParseContext::new(
            &config,
            std::path::Path::new("test.crn"),
            std::sync::Arc::new(crate::parser::grammar::preprocess_file("").unwrap()),
        );
        let argument = Value::Deferred(DeferredValue::Interpolation(vec![
            crate::resource::InterpolationPart::Literal("private-closure".into()),
            crate::resource::InterpolationPart::Expr(Value::resource_ref(
                "producer",
                "text",
                Vec::new(),
            )),
        ]));
        let secret = EvalValue::closure("secret", Vec::new(), 1);
        let upper = EvalValue::closure("upper", Vec::new(), 1);
        for (name, captured, remaining) in [
            ("secret", Vec::new(), 1),
            ("__compose__", vec![secret, upper], 1),
        ] {
            let value = apply_parser_closure(
                name,
                &captured,
                remaining,
                vec![EvalValue::from_value(argument.clone())],
                &ctx,
                1,
            )
            .unwrap()
            .into_value()
            .unwrap();
            assert!(!format!("{value:?}").contains("private-"), "{value:?}");
        }
    }
}

#[cfg(test)]
mod declared_check_regressions {
    use super::*;
    use crate::parser::TypeExpr;

    fn value(body: &str, argument: &str) -> Value {
        let source = format!(
            "fn checked(x: 'allowed') {{ x }}\n{body}\nlet src = test.Thing {{ name = 'src' }}\ntest.Sink {{ value = {argument} }}\n"
        );
        let parsed = crate::parser::parse_file(
            &source,
            std::path::Path::new("functions.crn"),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let mut context = ParseContext::for_evaluation(&config);
        context.replace_shared_user_functions(Arc::new(parsed.user_functions.clone()));
        try_evaluate_fn_value(
            parsed.iter_all_resources().last().unwrap().attributes()["value"].clone(),
            &context,
            4,
        )
        .unwrap()
    }
    #[test]
    fn declared_checks_parser_annotation_predicate_matrix() {
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            "fn g(x) { x }",
            std::path::Path::new("functions.crn"),
            &config,
        )
        .unwrap();
        for (annotation, pending) in [
            (TypeExpr::String, false),
            (TypeExpr::Bool, false),
            (TypeExpr::Int, false),
            (TypeExpr::Float, false),
            (TypeExpr::Duration, false),
            (TypeExpr::List(Box::new(TypeExpr::String)), false),
            (TypeExpr::Map(Box::new(TypeExpr::Int)), false),
            (
                TypeExpr::Struct {
                    fields: vec![("x".into(), TypeExpr::String)],
                },
                false,
            ),
            (TypeExpr::StringLiteral("allowed".into()), true),
            (
                TypeExpr::List(Box::new(TypeExpr::StringLiteral("allowed".into()))),
                true,
            ),
            (
                TypeExpr::Map(Box::new(TypeExpr::StringLiteral("allowed".into()))),
                true,
            ),
            (
                TypeExpr::Union(vec![
                    TypeExpr::String,
                    TypeExpr::StringLiteral("allowed".into()),
                ]),
                true,
            ),
            (
                TypeExpr::Struct {
                    fields: vec![("x".into(), TypeExpr::StringLiteral("allowed".into()))],
                },
                true,
            ),
        ] {
            for parameter in [false, true] {
                let mut function = parsed.user_functions["g"].clone();
                if parameter {
                    function.params[0].param_type = Some(annotation.clone());
                } else {
                    function.return_type = Some(annotation.clone());
                }
                let context = ParseContext::for_evaluation(&config);
                let result = evaluate_user_function(
                    &Arc::new(function),
                    &[FunctionArgument::from_value(Value::resource_ref(
                        "src",
                        "emitted",
                        vec![],
                    ))],
                    &context,
                    1,
                )
                .unwrap();
                assert_eq!(
                    matches!(result, FunctionEvaluation::Retain(_)),
                    pending,
                    "{annotation:?}, parameter={parameter}: {result:?}"
                );
            }
        }
    }
    #[test]
    fn declared_checks_retention_preserves_explicit_callee_identity() {
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parse = |body: &str| {
            crate::parser::parse_file(
                &format!("fn g(x: 'allowed'): String {{ '{body}' }}"),
                std::path::Path::new("functions.crn"),
                &config,
            )
            .unwrap()
        };
        let function = Arc::new(parse("callee").user_functions.shift_remove("g").unwrap());
        let scope = Arc::new(parse("different lexical definition").user_functions);
        let arguments = [FunctionArgument::from_value(Value::resource_ref(
            "src",
            "emitted",
            vec![],
        ))];
        let result =
            evaluate_resolved_user_function(&function, &scope, &arguments, &config, None, 4)
                .unwrap();
        let retained = match result {
            FunctionEvaluation::Inline(value) => {
                panic!("pending argument inlined: {:?}", value.into_value())
            }
            FunctionEvaluation::Retain(call) => call.into_value(),
        };
        let Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: retained, ..
        }) = retained
        else {
            panic!("retained call");
        };
        assert!(
            Arc::ptr_eq(&function, &retained),
            "retention replaced the explicit callee with a definition sharing its label"
        );
    }
    #[test]
    fn declared_checks_parser_unused_local_retains_outer_call() {
        let result = value(
            "fn outer(x) { let unused = checked(x)\n 'const' }",
            "outer(src.emitted)",
        );
        assert!(
            matches!(result, Value::Deferred(DeferredValue::ResolvedFunctionCall { ref function, .. }) if function.name == "outer"),
            "{result:?}"
        );
    }
    #[test]
    fn declared_checks_parser_ignored_argument_retains_outer_call() {
        let result = value("fn outer(x) { 'const' }", "outer(checked(src.emitted))");
        assert!(
            matches!(result, Value::Deferred(DeferredValue::ResolvedFunctionCall { ref function, .. }) if function.name == "outer"),
            "{result:?}"
        );
    }
    #[test]
    fn declared_checks_parser_untyped_wrapper_retains_call() {
        let result = value("fn outer(x) { checked(x) }", "outer(src.emitted)");
        assert!(
            matches!(result, Value::Deferred(DeferredValue::ResolvedFunctionCall { ref function, .. }) if function.name == "outer"),
            "{result:?}"
        );
    }
    #[test]
    fn declared_checks_parser_primitive_and_builtin_controls_inline() {
        for body in [
            "fn outer(x: String): String { x }",
            "fn outer(x) { upper(x) }",
        ] {
            let result = value(body, "outer(src.emitted)");
            assert!(
                !matches!(
                    result,
                    Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
                ),
                "{result:?}"
            );
        }
    }
}

#[cfg(test)]
mod annotation_regressions {
    use super::*;
    #[test]
    fn matching_unloaded_handle_keeps_loaded_recheck() {
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let mut parsed = crate::parser::parse_file(
            "fn f(r: test.Thing): String { 'allowed' }",
            std::path::Path::new("functions.crn"),
            &config,
        )
        .unwrap();
        let function = Arc::new(parsed.user_functions.shift_remove("f").unwrap());
        let handle = crate::eval_value::UnexpandedStructuralHandle::new(
            UnexpandedStructuralHandleKind::Resource(crate::parser::ResourceTypePath::new(
                "test", "Thing",
            )),
            "db",
        );
        crate::validation::deferred_types::with_session_for_test(&config, |session| {
            let ctx = ParseContext::for_evaluation_in_session(session);
            evaluate_user_function(&function, &[FunctionArgument::from_handle(handle)], &ctx, 2)
                .unwrap();
            assert_eq!(
                ctx.pending_function_type_checks.borrow().len(),
                1,
                "loaded catalog must recheck matching unresolved handles"
            );
        });
    }
    #[test]
    fn failed_argument_is_rejected_before_shape_only_inlining() {
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            "fn f(x: String): String { 'ignored' }",
            std::path::Path::new("functions.crn"),
            &config,
        )
        .unwrap();
        let failed = Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![],
            reason: crate::resource::UnavailableReason::from_builtin_error(
                "attempted fixture failure".into(),
            ),
            source_line: 2.into(),
        });
        let result = evaluate_user_function(
            &Arc::new(parsed.user_functions["f"].clone()),
            &[FunctionArgument::from_value(failed)],
            &ParseContext::for_evaluation(&config),
            2,
        );
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "Invalid expression at line 2: env(): attempted fixture failure"
        );
    }
    #[test]
    fn return_recheck_keeps_original_argument_when_local_shadows() {
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file("let other = test.Thing { name = 'other' }\nfn f(x: 'allowed'): 'allowed' { let x = 'bad'\nother.emitted }", std::path::Path::new("functions.crn"), &config).unwrap();
        let ctx = ParseContext::for_evaluation(&config);
        let _evaluation = evaluate_user_function(
            &Arc::new(parsed.user_functions["f"].clone()),
            &[FunctionArgument::from_value(Value::Concrete(
                ConcreteValue::String("allowed".into()),
            ))],
            &ctx,
            4,
        )
        .unwrap();
        let pending = ctx.pending_function_type_checks.borrow();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].arguments[0].value().unwrap(),
            &Value::Concrete(ConcreteValue::String("allowed".into()))
        );
    }
}
