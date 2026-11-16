use crate::parser::{FnParam, TypeExpr, UserFunction};
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, DiagnosticSourceLine, FunctionArgument,
    PlaceholderKind, Value,
};
use crate::validation::deferred_types::ProviderTypeCheckSite;
use crate::validation::typecheck::{TypeExprValueCheck, TypecheckProviderContext};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
struct Discharged<S, V> {
    slot: S,
    value: V,
    provider_recheck: bool,
}
#[derive(Debug)]
struct AwaitingValue<S, V> {
    slot: S,
    value: V,
    reason: AwaitingReason,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AwaitingReason {
    ConcreteValue,
    ProviderSession,
    ProviderSchema,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnnotationStatus {
    Discharged { provider_recheck: bool },
    AwaitingValue(AwaitingReason),
}
#[derive(Debug)]
enum AnnotationCheck<S, V> {
    Discharged(Discharged<S, V>),
    AwaitingValue(AwaitingValue<S, V>),
}
impl<S, V> AnnotationCheck<S, V> {
    fn new(slot: S, value: V, status: AnnotationStatus) -> Self {
        match status {
            AnnotationStatus::Discharged { provider_recheck } => Self::Discharged(Discharged {
                slot,
                value,
                provider_recheck,
            }),
            AnnotationStatus::AwaitingValue(reason) => Self::AwaitingValue(AwaitingValue {
                slot,
                value,
                reason,
            }),
        }
    }
    fn status(&self) -> AnnotationStatus {
        match self {
            Self::Discharged(witness) => AnnotationStatus::Discharged {
                provider_recheck: witness.provider_recheck,
            },
            Self::AwaitingValue(witness) => AnnotationStatus::AwaitingValue(witness.reason),
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum AnnotationError {
    #[error(transparent)]
    Resolution(#[from] crate::static_type::StaticTypeResolutionError),
    #[error(transparent)]
    Handle(#[from] crate::validation::HandleSinkError),
    #[error(transparent)]
    Value(#[from] crate::validation::typecheck::TypeExprValueError),
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum ArgumentCheckError {
    #[error(transparent)]
    Annotation(#[from] AnnotationError),
    #[error("function argument slot {index} is already settled or does not exist")]
    InvalidSlot { index: usize },
}
trait AnnotationValue<P: PlaceholderKind, B: BindingAddress> {
    fn value(&self) -> Option<&Value<P, B>>;
}
impl<P: PlaceholderKind, B: BindingAddress> AnnotationValue<P, B> for FunctionArgument<P, B> {
    fn value(&self) -> Option<&Value<P, B>> {
        self.value()
    }
}
impl<P: PlaceholderKind, B: BindingAddress> AnnotationValue<P, B> for Value<P, B> {
    fn value(&self) -> Option<&Value<P, B>> {
        Some(self)
    }
}

fn check_declared_annotation<P: PlaceholderKind, B: BindingAddress, S, V: AnnotationValue<P, B>>(
    slot: S,
    annotation: Option<&TypeExpr>,
    value: V,
    provider: TypecheckProviderContext<'_>,
    site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
) -> Result<AnnotationCheck<S, V>, crate::validation::typecheck::TypeExprValueError> {
    let discharged = AnnotationStatus::Discharged {
        provider_recheck: false,
    };
    let status = match annotation {
        None => discharged,
        Some(expected) => {
            let known = value.value().and_then(|value| {
                value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .ok()
                    .and_then(|proof| proof.into_classification().into_known().ok())
            });
            if provider.session().is_none()
                && !provider.as_provider_context().custom_types_loaded()
                && annotation_requires_provider_session(expected)
            {
                AnnotationStatus::AwaitingValue(AwaitingReason::ProviderSession)
            } else if let Some(known) = known {
                match provider.check_resolved_constraints(expected, &known) {
                    TypeExprValueCheck::Assignable => discharged,
                    TypeExprValueCheck::Mismatch(error) => return Err(error),
                    TypeExprValueCheck::Deferred(deferred)
                        if provider.requires_provider_schemas() =>
                    {
                        return Err(deferred.into_error());
                    }
                    TypeExprValueCheck::Deferred(deferred) => match provider.session() {
                        Some(session) => {
                            match site() {
                                Some(site) => TypeExprValueCheck::Deferred(deferred)
                                    .record_at(session, site)?,
                                None => TypeExprValueCheck::Deferred(deferred)
                                    .record_deferred(session)?,
                            }
                            AnnotationStatus::AwaitingValue(AwaitingReason::ProviderSchema)
                        }
                        None => return Err(deferred.into_error()),
                    },
                }
            } else if requires_concrete_annotation_check(expected) {
                AnnotationStatus::AwaitingValue(AwaitingReason::ConcreteValue)
            } else {
                discharged
            }
        }
    };
    Ok(AnnotationCheck::new(slot, value, status))
}

fn check_parameter<'a, P: PlaceholderKind, B: BindingAddress>(
    parameter: &'a FnParam<B>,
    argument: FunctionArgument<P, B>,
    provider: TypecheckProviderContext<'_>,
    site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
    context: &str,
) -> Result<AnnotationCheck<&'a FnParam<B>, FunctionArgument<P, B>>, AnnotationError> {
    let Some(annotation) = &parameter.param_type else {
        return Ok(AnnotationCheck::new(
            parameter,
            argument,
            AnnotationStatus::Discharged {
                provider_recheck: false,
            },
        ));
    };
    let catalog = provider.as_provider_context();
    let matching_handle =
        if let (TypeExpr::DottedUnresolved(path), Some(handle)) = (annotation, argument.handle()) {
            crate::validation::check_handle_sink(
                handle,
                &crate::validation::ResolvedParameterType::Handle(path.clone()),
            )
            .is_ok()
        } else {
            false
        };
    if provider.session().is_none() && !catalog.custom_types_loaded() {
        if matching_handle {
            return Ok(AnnotationCheck::new(
                parameter,
                argument,
                AnnotationStatus::Discharged {
                    provider_recheck: false,
                },
            ));
        }
        if annotation_requires_provider_session(annotation) {
            return Ok(AnnotationCheck::new(
                parameter,
                argument,
                AnnotationStatus::AwaitingValue(AwaitingReason::ProviderSession),
            ));
        }
    }
    let Some(resolved_type) =
        crate::validation::resolve_parameter_type_for_check(annotation, catalog)?
    else {
        if let Some(session) = provider.session() {
            match site() {
                Some(site) => session.record(annotation, site),
                None => session.record_type(annotation, context),
            }
        }
        let status = if matching_handle {
            AnnotationStatus::Discharged {
                provider_recheck: true,
            }
        } else {
            AnnotationStatus::AwaitingValue(AwaitingReason::ProviderSchema)
        };
        return Ok(AnnotationCheck::new(parameter, argument, status));
    };
    if let Some(handle) = argument.handle() {
        crate::validation::check_handle_sink(handle, &resolved_type)?;
        return Ok(AnnotationCheck::new(
            parameter,
            argument,
            AnnotationStatus::Discharged {
                provider_recheck: false,
            },
        ));
    }
    check_declared_annotation(
        parameter,
        Some(&resolved_type.as_type_expr()),
        argument,
        provider,
        site,
    )
    .map_err(Into::into)
}

pub(crate) fn check_parameter_annotation<P: PlaceholderKind, B: BindingAddress>(
    parameter: &FnParam<B>,
    argument: FunctionArgument<P, B>,
    provider: TypecheckProviderContext<'_>,
    site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
    context: &str,
) -> Result<AnnotationStatus, AnnotationError> {
    check_parameter(parameter, argument, provider, site, context).map(|check| check.status())
}

#[derive(Debug)]
pub(crate) struct CheckedValue<P: PlaceholderKind, B: BindingAddress>(Value<P, B>);
impl<P: PlaceholderKind, B: BindingAddress> CheckedValue<P, B> {
    pub(crate) fn into_value(self) -> Value<P, B> {
        self.0
    }
}
#[derive(Debug)]
pub(crate) struct RetainedCall<P: PlaceholderKind, B: BindingAddress> {
    function: Arc<UserFunction<B>>,
    functions: Arc<IndexMap<String, UserFunction<B>>>,
    args: Vec<FunctionArgument<P, B>>,
    source_line: DiagnosticSourceLine,
}
impl<P: PlaceholderKind, B: BindingAddress> RetainedCall<P, B> {
    fn new(
        function: Arc<UserFunction<B>>,
        functions: Arc<IndexMap<String, UserFunction<B>>>,
        args: Vec<FunctionArgument<P, B>>,
        source_line: DiagnosticSourceLine,
    ) -> Self {
        Self {
            function,
            functions,
            args,
            source_line,
        }
    }
    pub(crate) fn arguments_for_recheck(
        &self,
    ) -> Vec<FunctionArgument<crate::resource::ParserPlaceholder, B>> {
        self.args
            .iter()
            .cloned()
            .map(FunctionArgument::into_parser_stage)
            .collect()
    }
    pub(crate) fn into_value(self) -> Value<P, B> {
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: self.function,
            functions: self.functions,
            args: self.args,
            source_line: self.source_line,
        })
    }
}
#[derive(Debug)]
pub(crate) enum FunctionEvaluation<P: PlaceholderKind, B: BindingAddress> {
    Inline(CheckedValue<P, B>),
    Retain(RetainedCall<P, B>),
}
impl<P: PlaceholderKind, B: BindingAddress> FunctionEvaluation<P, B> {
    pub(crate) fn into_value(self) -> Value<P, B> {
        match self {
            Self::Inline(value) => value.into_value(),
            Self::Retain(call) => call.into_value(),
        }
    }
}
enum ParameterSettlement<'a, P: PlaceholderKind, B: BindingAddress> {
    Unsettled,
    Checked(Box<AnnotationCheck<&'a FnParam<B>, FunctionArgument<P, B>>>),
    Invalid,
}
pub(crate) struct FunctionCompletion<'a, P: PlaceholderKind, B: BindingAddress> {
    function: &'a UserFunction<B>,
    slots: Vec<ParameterSettlement<'a, P, B>>,
    retained: RetainedCall<P, B>,
}
pub(crate) enum PreparedFunction<'a, P: PlaceholderKind, B: BindingAddress> {
    Ready {
        completion: FunctionBodyCompletion<'a, P, B>,
    },
    Retain(RetainedCall<P, B>),
}
impl<'a, P: PlaceholderKind, B: BindingAddress> FunctionCompletion<'a, P, B> {
    pub(crate) fn new(
        function: &'a Arc<UserFunction<B>>,
        functions: &Arc<IndexMap<String, UserFunction<B>>>,
        args: Vec<FunctionArgument<P, B>>,
        source_line: DiagnosticSourceLine,
    ) -> Self {
        Self {
            function,
            slots: function
                .params
                .iter()
                .map(|_| ParameterSettlement::Unsettled)
                .collect(),
            retained: RetainedCall::new(
                Arc::clone(function),
                Arc::clone(functions),
                args,
                source_line,
            ),
        }
    }
    pub(crate) fn check_argument(
        &mut self,
        index: usize,
        argument: FunctionArgument<P, B>,
        provider: TypecheckProviderContext<'_>,
        site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
        context: &str,
    ) -> Result<AnnotationStatus, ArgumentCheckError> {
        let Some(slot) = self.slots.get_mut(index) else {
            return Err(ArgumentCheckError::InvalidSlot { index });
        };
        match slot {
            ParameterSettlement::Unsettled => {}
            ParameterSettlement::Checked(_) | ParameterSettlement::Invalid => {
                *slot = ParameterSettlement::Invalid;
                return Err(ArgumentCheckError::InvalidSlot { index });
            }
        }
        // Taking the slot also prevents a failed check from being replaced by
        // an unrelated successful check if a caller handles the error.
        *slot = ParameterSettlement::Invalid;
        let checked = check_parameter(
            &self.function.params[index],
            argument,
            provider,
            site,
            context,
        )
        .map_err(ArgumentCheckError::Annotation)?;
        let status = checked.status();
        *slot = ParameterSettlement::Checked(Box::new(checked));
        Ok(status)
    }
    pub(crate) fn retain(self) -> RetainedCall<P, B> {
        self.retained
    }
    pub(crate) fn prepare(mut self) -> PreparedFunction<'a, P, B> {
        let mut arguments = Vec::with_capacity(self.slots.len());
        let mut pending = false;
        for (index, slot) in self.slots.into_iter().enumerate() {
            let argument = match slot {
                ParameterSettlement::Unsettled | ParameterSettlement::Invalid => {
                    return PreparedFunction::Retain(self.retained);
                }
                ParameterSettlement::Checked(check) => match *check {
                    AnnotationCheck::Discharged(witness) => {
                        if !std::ptr::eq(witness.slot, &self.function.params[index]) {
                            return PreparedFunction::Retain(self.retained);
                        }
                        witness.value
                    }
                    AnnotationCheck::AwaitingValue(witness) => {
                        if !std::ptr::eq(witness.slot, &self.function.params[index]) {
                            return PreparedFunction::Retain(self.retained);
                        }
                        pending = true;
                        witness.value
                    }
                },
            };
            pending |= contains_deferred_user_function_call(argument.walker_value());
            arguments.push(argument);
        }
        self.retained.args = arguments.clone();
        if pending {
            PreparedFunction::Retain(self.retained)
        } else {
            let substitutions = self
                .function
                .params
                .iter()
                .zip(arguments)
                .map(|(parameter, argument)| (parameter.name.clone(), argument.into_parser_stage()))
                .collect();
            PreparedFunction::Ready {
                completion: FunctionBodyCompletion {
                    function: self.function,
                    retained: self.retained,
                    surviving_call: false,
                    substitutions,
                    next_local: 0,
                },
            }
        }
    }
}
pub(crate) struct FunctionBodyCompletion<'a, P: PlaceholderKind, B: BindingAddress> {
    function: &'a UserFunction<B>,
    retained: RetainedCall<P, B>,
    surviving_call: bool,
    substitutions: HashMap<String, FunctionArgument<crate::resource::ParserPlaceholder, B>>,
    next_local: usize,
}
impl<'a, P: PlaceholderKind, B: BindingAddress> FunctionBodyCompletion<'a, P, B> {
    pub(crate) fn retain(self) -> RetainedCall<P, B> {
        self.retained
    }
    fn local_expression(
        &self,
        index: usize,
    ) -> Result<(&Value<crate::resource::ParserPlaceholder, B>, usize), crate::parser::ParseError>
    {
        if index != self.next_local {
            return Err(crate::parser::ParseError::UserFunctionError(
                "function local slot is out of order".into(),
            ));
        }
        self.function
            .local_lets
            .get(index)
            .map(|(_, expression, line)| (expression, *line))
            .ok_or_else(|| {
                crate::parser::ParseError::UserFunctionError(
                    "function local slot does not exist".into(),
                )
            })
    }
    pub(crate) fn substitute_local_argument(
        &self,
        index: usize,
    ) -> Result<FunctionArgument<crate::resource::ParserPlaceholder, B>, crate::parser::ParseError>
    {
        let (expression, line) = self.local_expression(index)?;
        crate::value_walk::substitute_function_call_argument(
            &FunctionArgument::from_value(expression.clone()),
            &self.substitutions,
            line,
        )
    }
    pub(crate) fn substitute_local_value(
        &self,
        index: usize,
    ) -> Result<Value<crate::resource::ParserPlaceholder, B>, crate::parser::ParseError> {
        let (expression, line) = self.local_expression(index)?;
        crate::value_walk::substitute_function_parameters(
            expression,
            &self.substitutions,
            line,
            crate::value_walk::RewritePosition::Value,
        )
    }
    pub(crate) fn bind_local(
        &mut self,
        index: usize,
        value: FunctionArgument<crate::resource::ParserPlaceholder, B>,
    ) -> Result<(), crate::parser::ParseError> {
        if index != self.next_local || index >= self.function.local_lets.len() {
            self.surviving_call = true;
            return Err(crate::parser::ParseError::UserFunctionError(
                "function local slot is already settled or out of order".into(),
            ));
        }
        self.surviving_call |= contains_deferred_user_function_call(value.walker_value());
        self.substitutions
            .insert(self.function.local_lets[index].0.clone(), value);
        self.next_local += 1;
        Ok(())
    }
    pub(crate) fn substitute_body(
        &self,
    ) -> Result<Value<crate::resource::ParserPlaceholder, B>, crate::parser::ParseError> {
        if self.next_local != self.function.local_lets.len() {
            return Err(crate::parser::ParseError::UserFunctionError(
                "function locals are not settled".into(),
            ));
        }
        crate::value_walk::substitute_function_parameters(
            &self.function.body.value,
            &self.substitutions,
            self.function.body.source_line,
            crate::value_walk::RewritePosition::Value,
        )
    }
    pub(crate) fn observe(&mut self, value: &Value<P, B>) {
        self.surviving_call |= contains_deferred_user_function_call(value);
    }
    pub(crate) fn finish(
        mut self,
        value: Value<P, B>,
        provider: TypecheckProviderContext<'_>,
        site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
    ) -> Result<FunctionEvaluation<P, B>, crate::validation::typecheck::TypeExprValueError> {
        self.observe(&value);
        self.surviving_call |= self.next_local != self.function.local_lets.len();
        let check = check_declared_annotation(
            self.function,
            self.function.return_type.as_ref(),
            value,
            provider,
            site,
        )?;
        Ok(match check {
            AnnotationCheck::Discharged(witness) => {
                if std::ptr::eq(witness.slot, self.function) && !self.surviving_call {
                    FunctionEvaluation::Inline(CheckedValue(witness.value))
                } else {
                    FunctionEvaluation::Retain(self.retained)
                }
            }
            AnnotationCheck::AwaitingValue(witness) => {
                let _ = witness.slot;
                FunctionEvaluation::Retain(self.retained)
            }
        })
    }
}
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

pub(crate) fn annotation_requires_provider_session(expected: &TypeExpr) -> bool {
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
pub(crate) fn contains_deferred_user_function_call<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
) -> bool {
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
            value.visit(contains_deferred_user_function_call)
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
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ProviderContext, parse_file};
    use crate::resource::{AuthoredValue, ParserPlaceholder, RelativeBindingName};

    fn config() -> ProviderContext {
        ProviderContext::new(crate::EnvironmentSnapshot::empty())
    }
    fn function(source: &str) -> Arc<UserFunction> {
        Arc::new(
            parse_file(source, std::path::Path::new("functions.crn"), &config())
                .unwrap()
                .user_functions
                .shift_remove("g")
                .unwrap(),
        )
    }
    fn completion(
        function: &Arc<UserFunction>,
    ) -> FunctionCompletion<'_, ParserPlaceholder, RelativeBindingName> {
        FunctionCompletion::new(function, &Arc::new(IndexMap::new()), vec![], 1.into())
    }
    fn unknown() -> AuthoredValue {
        AuthoredValue::resource_ref("src", "emitted", vec![])
    }
    fn text(text: &str) -> AuthoredValue {
        Value::Concrete(ConcreteValue::String(text.into()))
    }
    fn body<'a>(
        function: &'a Arc<UserFunction>,
        config: &ProviderContext,
    ) -> FunctionBodyCompletion<'a, ParserPlaceholder, RelativeBindingName> {
        let mut completion = completion(function);
        for index in 0..function.params.len() {
            completion
                .check_argument(
                    index,
                    FunctionArgument::from_value(unknown()),
                    TypecheckProviderContext::for_parser(config),
                    || None,
                    "argument",
                )
                .unwrap();
        }
        match completion.prepare() {
            PreparedFunction::Ready { completion } => completion,
            PreparedFunction::Retain(_) => panic!("primitive arguments must prepare"),
        }
    }
    #[test]
    fn declared_checks_completion_requires_every_declared_slot() {
        let function = function("fn g(x: String): 'allowed' { x }");
        assert!(matches!(
            completion(&function).prepare(),
            PreparedFunction::Retain(_)
        ));
        assert!(matches!(
            body(&function, &config())
                .finish(
                    unknown(),
                    TypecheckProviderContext::for_parser(&config()),
                    || None
                )
                .unwrap(),
            FunctionEvaluation::Retain(_)
        ));
        assert!(matches!(
            body(&function, &config())
                .finish(
                    text("allowed"),
                    TypecheckProviderContext::for_parser(&config()),
                    || None
                )
                .unwrap(),
            FunctionEvaluation::Inline(_)
        ));
    }
    #[test]
    fn declared_checks_equal_labels_and_types_do_not_identify_a_slot() {
        let mut function = function("fn g(x: String): String { x }");
        let duplicate = function.params[0].clone();
        Arc::make_mut(&mut function).params.push(duplicate);
        let mut incomplete = completion(&function);
        incomplete
            .check_argument(
                0,
                FunctionArgument::from_value(unknown()),
                TypecheckProviderContext::for_parser(&config()),
                || None,
                "argument",
            )
            .unwrap();
        assert!(matches!(incomplete.prepare(), PreparedFunction::Retain(_)));
        assert!(matches!(
            body(&function, &config())
                .finish(
                    unknown(),
                    TypecheckProviderContext::for_parser(&config()),
                    || None
                )
                .unwrap(),
            FunctionEvaluation::Inline(_)
        ));
    }
    #[test]
    fn declared_checks_shared_check_requires_values_for_nested_literals() {
        let expected = TypeExpr::List(Box::new(TypeExpr::StringLiteral("allowed".into())));
        let check = check_declared_annotation(
            &expected,
            Some(&expected),
            unknown(),
            TypecheckProviderContext::for_parser(&config()),
            || None,
        )
        .unwrap();
        assert!(matches!(check, AnnotationCheck::AwaitingValue(_)));
        assert!(
            check_declared_annotation(
                &expected,
                Some(&expected),
                AuthoredValue::Concrete(ConcreteValue::List(vec![text("bad-name")])),
                TypecheckProviderContext::for_parser(&config()),
                || None
            )
            .is_err()
        );
    }
    #[test]
    fn declared_checks_matching_handle_stays_discharged_with_deferred_schema() {
        let function = function("fn g(r: test.Thing): String { 'const' }");
        let handle = crate::eval_value::UnexpandedStructuralHandle::new(
            crate::eval_value::UnexpandedStructuralHandleKind::Resource(
                crate::parser::ResourceTypePath::new("test", "Thing"),
            ),
            "src",
        );
        crate::validation::deferred_types::with_session_for_test(&config(), |session| {
            let checked = check_parameter(
                &function.params[0],
                FunctionArgument::from_handle(handle),
                TypecheckProviderContext::for_session(session),
                || None,
                "argument",
            )
            .unwrap();
            assert_eq!(
                checked.status(),
                AnnotationStatus::Discharged {
                    provider_recheck: true
                }
            );
        });
    }
    #[test]
    fn declared_checks_nested_call_scanner_excludes_builtin_itself() {
        let builtin: AuthoredValue = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![FunctionArgument::from_value(unknown())],
            source_line: 1.into(),
        });
        assert!(!contains_deferred_user_function_call(&builtin));
        let call = completion(&function("fn g(x: 'allowed') { x }"))
            .retain()
            .into_value();
        let nested = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![FunctionArgument::from_value(call)],
            source_line: 1.into(),
        });
        assert!(contains_deferred_user_function_call(&nested));
    }
    #[test]
    fn declared_checks_resolved_arguments_replace_pending_inputs() {
        let function = function("fn g(x: String): String { x }");
        let pending = completion(&function).retain().into_value();
        let mut completion = FunctionCompletion::new(
            &function,
            &Arc::new(IndexMap::new()),
            vec![FunctionArgument::from_value(pending)],
            1.into(),
        );
        completion
            .check_argument(
                0,
                FunctionArgument::from_value(unknown()),
                TypecheckProviderContext::for_parser(&config()),
                || None,
                "argument",
            )
            .unwrap();
        let completed = match completion.prepare() {
            PreparedFunction::Ready { completion } => completion,
            PreparedFunction::Retain(_) => panic!("resolved argument"),
        };
        assert!(matches!(
            completed
                .finish(
                    unknown(),
                    TypecheckProviderContext::for_parser(&config()),
                    || None
                )
                .unwrap(),
            FunctionEvaluation::Inline(_)
        ));
    }
    #[test]
    fn checked_return_cannot_authorize_another_value() {
        let function = function("fn g(): 'allowed' { 'allowed' }");
        let checked = check_declared_annotation(
            function.as_ref(),
            function.return_type.as_ref(),
            text("allowed"),
            TypecheckProviderContext::for_parser(&config()),
            || None,
        )
        .unwrap();
        assert!(matches!(checked, AnnotationCheck::Discharged(_)));
        let result = body(&function, &config()).finish(
            text("wrong-value"),
            TypecheckProviderContext::for_parser(&config()),
            || None,
        );
        assert!(
            result.is_err(),
            "completion checks the value it owns, independent of any earlier witness"
        );
    }
    #[test]
    fn mixed_checks_for_one_slot_never_inline() {
        let function = function("fn g(x: 'allowed'): String { x }");
        for first_known in [false, true] {
            let mut completion = completion(&function);
            let (first, second) = if first_known {
                (text("allowed"), unknown())
            } else {
                (unknown(), text("allowed"))
            };
            completion
                .check_argument(
                    0,
                    FunctionArgument::from_value(first),
                    TypecheckProviderContext::for_parser(&config()),
                    || None,
                    "argument",
                )
                .unwrap();
            assert!(matches!(
                completion.check_argument(
                    0,
                    FunctionArgument::from_value(second),
                    TypecheckProviderContext::for_parser(&config()),
                    || None,
                    "argument"
                ),
                Err(ArgumentCheckError::InvalidSlot { index: 0 })
            ));
            assert!(matches!(completion.prepare(), PreparedFunction::Retain(_)));
        }
    }
    #[test]
    fn observed_local_survives_argument_replacement() {
        let function = function("fn g(x: String): String { x }");
        let mut completed = body(&function, &config());
        completed.observe(&completion(&function).retain().into_value());
        // This stage has no argument-replacement or record API.
        assert!(matches!(
            completed
                .finish(
                    unknown(),
                    TypecheckProviderContext::for_parser(&config()),
                    || None
                )
                .unwrap(),
            FunctionEvaluation::Retain(_)
        ));
    }
    #[test]
    fn failed_classification_only_discharges_shape_annotation() {
        let failed: AuthoredValue = Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![],
            reason: crate::resource::UnavailableReason::from_builtin_error(
                "attempted fixture failure".into(),
            ),
            source_line: 2.into(),
        });
        assert!(
            failed
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .unwrap()
                .into_classification()
                .into_known()
                .is_err()
        );
        for (annotation, discharged) in [
            (TypeExpr::String, true),
            (TypeExpr::StringLiteral("allowed".into()), false),
        ] {
            let checked = check_declared_annotation(
                &annotation,
                Some(&annotation),
                failed.clone(),
                TypecheckProviderContext::for_parser(&config()),
                || None,
            )
            .unwrap();
            assert_eq!(
                matches!(checked, AnnotationCheck::Discharged(_)),
                discharged
            );
        }
    }
    #[test]
    fn checked_arguments_cannot_be_replaced_before_body() {
        let function = function("fn g(x: 'allowed') { x }");
        let mut completion = completion(&function);
        completion
            .check_argument(
                0,
                FunctionArgument::from_value(text("allowed")),
                TypecheckProviderContext::for_parser(&config()),
                || None,
                "argument",
            )
            .unwrap();
        let PreparedFunction::Ready { completion } = completion.prepare() else {
            panic!("checked argument");
        };
        let mut arguments = completion.retained.arguments_for_recheck();
        arguments[0] = FunctionArgument::from_value(text("bad"));
        let value = completion.substitute_body().unwrap();
        let result = completion
            .finish(
                value,
                TypecheckProviderContext::for_parser(&config()),
                || None,
            )
            .unwrap()
            .into_value();
        assert_eq!(result, text("allowed"));
    }
    #[test]
    fn body_cannot_inline_without_settling_unused_locals() {
        let function = function(
            "fn checked(x: 'allowed') { x }\nfn g(x: String) { let unused = checked(x)\n 'const' }",
        );
        let completed = body(&function, &config());
        let result = completed
            .finish(
                text("const"),
                TypecheckProviderContext::for_parser(&config()),
                || None,
            )
            .unwrap();
        assert!(matches!(result, FunctionEvaluation::Retain(_)));
    }
    #[test]
    fn retained_call_serialization_states_the_problem_only() {
        let function = function("fn g(x: String): String { x }");
        let call = FunctionCompletion::new(
            &function,
            &Arc::new(IndexMap::new()),
            vec![FunctionArgument::from_value(
                text("private-serialization-argument").into_secret(),
            )],
            1.into(),
        )
        .retain()
        .into_value();
        let message = serde_json::to_string(&call).unwrap_err().to_string();
        assert!(
            !message.contains("private-serialization-argument"),
            "SECRET_LEAK_DETECTED core serialization"
        );

        assert_eq!(
            message,
            "a user-defined function call still needs its declared types checked against values available only during apply. A saved plan cannot carry this call."
        );
        assert!(!message.contains("carina apply"));
    }
}
