use crate::parser::{FnParam, TypeExpr, UserFunction};
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, DiagnosticSourceLine, FunctionArgument,
    PlaceholderKind, Value, ValueKind,
};
use crate::validation::deferred_types::ProviderTypeCheckSite;
use crate::validation::typecheck::{TypeExprValueCheck, TypecheckProviderContext};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
struct Discharged<V> {
    value: V,
    provider_recheck: bool,
}
#[derive(Debug)]
struct AwaitingValue<V> {
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
enum AnnotationStatus {
    Discharged { provider_recheck: bool },
    AwaitingValue(AwaitingReason),
}
#[derive(Debug)]
enum AnnotationCheck<V> {
    Discharged(Discharged<V>),
    AwaitingValue(AwaitingValue<V>),
}
impl<V> AnnotationCheck<V> {
    fn new(value: V, status: AnnotationStatus) -> Self {
        match status {
            AnnotationStatus::Discharged { provider_recheck } => Self::Discharged(Discharged {
                value,
                provider_recheck,
            }),
            AnnotationStatus::AwaitingValue(reason) => {
                Self::AwaitingValue(AwaitingValue { value, reason })
            }
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

fn check_declared_annotation<P: PlaceholderKind, B: BindingAddress, V: AnnotationValue<P, B>>(
    annotation: Option<&TypeExpr>,
    value: V,
    provider: TypecheckProviderContext<'_>,
    site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
) -> Result<AnnotationCheck<V>, crate::validation::typecheck::TypeExprValueError> {
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
    Ok(AnnotationCheck::new(value, status))
}

fn check_parameter<P: PlaceholderKind, B: BindingAddress>(
    parameter: &FnParam<B>,
    argument: FunctionArgument<P, B>,
    provider: TypecheckProviderContext<'_>,
    site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
    context: &str,
) -> Result<AnnotationCheck<FunctionArgument<P, B>>, AnnotationError> {
    let Some(annotation) = &parameter.param_type else {
        return Ok(AnnotationCheck::new(
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
                argument,
                AnnotationStatus::Discharged {
                    provider_recheck: false,
                },
            ));
        }
        if annotation_requires_provider_session(annotation) {
            return Ok(AnnotationCheck::new(
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
        return Ok(AnnotationCheck::new(argument, status));
    };
    if let Some(handle) = argument.handle() {
        crate::validation::check_handle_sink(handle, &resolved_type)?;
        return Ok(AnnotationCheck::new(
            argument,
            AnnotationStatus::Discharged {
                provider_recheck: false,
            },
        ));
    }
    check_declared_annotation(
        Some(&resolved_type.as_type_expr()),
        argument,
        provider,
        site,
    )
    .map_err(Into::into)
}

pub(crate) struct ParameterAnnotationCheck<P: PlaceholderKind, B: BindingAddress> {
    check: AnnotationCheck<FunctionArgument<P, B>>,
}
impl<P: PlaceholderKind, B: BindingAddress> ParameterAnnotationCheck<P, B> {
    pub(crate) fn requires_provider_session(&self) -> bool {
        self.check.status().requires_provider_session()
    }
}
pub(crate) struct ParameterCheckReport(AnnotationStatus);
impl ParameterCheckReport {
    pub(crate) fn needs_recheck(&self) -> bool {
        self.0.needs_recheck()
    }
}
impl AnnotationStatus {
    fn needs_recheck(self) -> bool {
        match self {
            Self::Discharged { provider_recheck } => provider_recheck,
            Self::AwaitingValue(_) => true,
        }
    }
    fn requires_provider_session(self) -> bool {
        matches!(self, Self::AwaitingValue(AwaitingReason::ProviderSession))
    }
}
pub(crate) fn check_parameter_annotation<P: PlaceholderKind, B: BindingAddress>(
    parameter: &FnParam<B>,
    argument: FunctionArgument<P, B>,
    provider: TypecheckProviderContext<'_>,
    site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
    context: &str,
) -> Result<ParameterAnnotationCheck<P, B>, AnnotationError> {
    check_parameter(parameter, argument, provider, site, context)
        .map(|check| ParameterAnnotationCheck { check })
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
#[derive(Default)]
struct CallObservations {
    surviving_call: bool,
}
impl CallObservations {
    fn observe<P: PlaceholderKind, B: BindingAddress>(&mut self, value: &Value<P, B>) {
        self.surviving_call |= contains_deferred_user_function_call(value);
    }
}
pub(crate) enum ArgumentSource<P: PlaceholderKind, B: BindingAddress> {
    Provided(FunctionArgument<P, B>),
    Default(Value<crate::resource::ParserPlaceholder, B>),
}
#[derive(Debug)]
pub(crate) struct FunctionArityError {
    minimum: usize,
    maximum: usize,
    provided: usize,
}
impl FunctionArityError {
    pub(crate) fn parser_message(&self, name: &str) -> String {
        if self.provided > self.maximum {
            format!(
                "function '{name}' expects at most {} argument(s), got {}",
                self.maximum, self.provided
            )
        } else {
            format!(
                "function '{name}' expects at least {} argument(s), got {}",
                self.minimum, self.provided
            )
        }
    }
    pub(crate) fn runtime_message(&self, name: &str) -> String {
        format!(
            "function '{name}' expects {}..={} argument(s), got {}",
            self.minimum, self.maximum, self.provided
        )
    }
}
pub(crate) struct FunctionCompletion<'a, P: PlaceholderKind, B: BindingAddress> {
    function: &'a UserFunction<B>,
    arguments: std::vec::IntoIter<(&'a FnParam<B>, ArgumentSource<P, B>)>,
    checked_arguments: Vec<FunctionArgument<P, B>>,
    pending_check: bool,
    observations: CallObservations,
    retained: RetainedCall<P, B>,
}
pub(crate) enum ArgumentStep<'a, P: PlaceholderKind, B: BindingAddress> {
    Argument(Box<ArgumentSlot<'a, P, B>>),
    Complete(PreparedFunction<'a, P, B>),
}
pub(crate) struct ArgumentSlot<'a, P: PlaceholderKind, B: BindingAddress> {
    parameter: &'a FnParam<B>,
    argument: ArgumentSource<P, B>,
    cursor: FunctionCompletion<'a, P, B>,
}
pub(crate) enum PreparedFunction<'a, P: PlaceholderKind, B: BindingAddress> {
    Ready {
        completion: FunctionLocals<'a, P, B>,
    },
    Retain(RetainedCall<P, B>),
}
impl<'a, P: PlaceholderKind, B: BindingAddress> FunctionCompletion<'a, P, B> {
    pub(crate) fn new(
        function: &'a Arc<UserFunction<B>>,
        functions: &Arc<IndexMap<String, UserFunction<B>>>,
        args: Vec<FunctionArgument<P, B>>,
        source_line: DiagnosticSourceLine,
    ) -> Result<Self, FunctionArityError> {
        let minimum = function
            .params
            .iter()
            .rposition(|parameter| parameter.default.is_none())
            .map_or(0, |index| index + 1);
        let mismatch = || FunctionArityError {
            minimum,
            maximum: function.params.len(),
            provided: args.len(),
        };
        if args.len() > function.params.len() {
            return Err(mismatch());
        }
        let defaults = function
            .params
            .iter()
            .skip(args.len())
            .map(|parameter| {
                parameter
                    .default
                    .clone()
                    .map(|value| (parameter, ArgumentSource::Default(value)))
                    .ok_or_else(mismatch)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let arguments = function
            .params
            .iter()
            .zip(args.iter().cloned())
            .map(|(parameter, argument)| (parameter, ArgumentSource::Provided(argument)))
            .chain(defaults)
            .collect::<Vec<_>>()
            .into_iter();
        Ok(Self {
            function,
            arguments,
            checked_arguments: Vec::new(),
            pending_check: false,
            observations: CallObservations::default(),
            retained: RetainedCall::new(
                Arc::clone(function),
                Arc::clone(functions),
                args,
                source_line,
            ),
        })
    }
    pub(crate) fn retain(self) -> RetainedCall<P, B> {
        self.retained
    }
    pub(crate) fn next_argument(mut self) -> ArgumentStep<'a, P, B> {
        match self.arguments.next() {
            Some((parameter, argument)) => ArgumentStep::Argument(Box::new(ArgumentSlot {
                parameter,
                argument,
                cursor: self,
            })),
            None => {
                self.retained.args = self.checked_arguments.clone();
                let prepared = if self.pending_check || self.observations.surviving_call {
                    PreparedFunction::Retain(self.retained)
                } else {
                    let substitutions = self
                        .function
                        .params
                        .iter()
                        .zip(self.checked_arguments)
                        .map(|(parameter, argument)| {
                            (parameter.name.clone(), argument.into_parser_stage())
                        })
                        .collect();
                    PreparedFunction::Ready {
                        completion: FunctionLocals {
                            locals: self.function.local_lets.iter(),
                            completion: FunctionBodyCompletion {
                                function: self.function,
                                retained: self.retained,
                                observations: self.observations,
                                substitutions,
                            },
                        },
                    }
                };
                ArgumentStep::Complete(prepared)
            }
        }
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> ArgumentSlot<'a, P, B> {
    pub(crate) fn parameter(&self) -> &'a FnParam<B> {
        self.parameter
    }
    pub(crate) fn argument(&self) -> &ArgumentSource<P, B> {
        &self.argument
    }
    pub(crate) fn check(
        mut self,
        argument: FunctionArgument<P, B>,
        provider: TypecheckProviderContext<'_>,
        site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
        context: &str,
    ) -> Result<(FunctionCompletion<'a, P, B>, ParameterCheckReport), AnnotationError> {
        let check = check_parameter(self.parameter, argument, provider, site, context)?;
        let report = ParameterCheckReport(check.status());
        let argument = match check {
            AnnotationCheck::Discharged(witness) => witness.value,
            AnnotationCheck::AwaitingValue(witness) => {
                self.cursor.pending_check = true;
                witness.value
            }
        };
        self.cursor.observations.observe(argument.walker_value());
        self.cursor.checked_arguments.push(argument);
        Ok((self.cursor, report))
    }
}
pub(crate) struct FunctionLocals<'a, P: PlaceholderKind, B: BindingAddress> {
    locals: std::slice::Iter<'a, (String, Value<crate::resource::ParserPlaceholder, B>, usize)>,
    completion: FunctionBodyCompletion<'a, P, B>,
}
pub(crate) enum LocalStep<'a, P: PlaceholderKind, B: BindingAddress> {
    Local(LocalSlot<'a, P, B>),
    Body(FunctionBodyCompletion<'a, P, B>),
}
pub(crate) struct LocalSlot<'a, P: PlaceholderKind, B: BindingAddress> {
    local: &'a (String, Value<crate::resource::ParserPlaceholder, B>, usize),
    cursor: FunctionLocals<'a, P, B>,
}
impl<'a, P: PlaceholderKind, B: BindingAddress> FunctionLocals<'a, P, B> {
    pub(crate) fn retain(self) -> RetainedCall<P, B> {
        self.completion.retained
    }
    pub(crate) fn next_local(mut self) -> LocalStep<'a, P, B> {
        match self.locals.next() {
            Some(local) => LocalStep::Local(LocalSlot {
                local,
                cursor: self,
            }),
            None => LocalStep::Body(self.completion),
        }
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> LocalSlot<'a, P, B> {
    pub(crate) fn name(&self) -> &str {
        &self.local.0
    }
    pub(crate) fn source_line(&self) -> usize {
        self.local.2
    }
    pub(crate) fn substitute_argument(
        &self,
    ) -> Result<FunctionArgument<crate::resource::ParserPlaceholder, B>, crate::parser::ParseError>
    {
        crate::value_walk::substitute_function_call_argument(
            &FunctionArgument::from_value(self.local.1.clone()),
            &self.cursor.completion.substitutions,
            self.source_line(),
        )
    }
    pub(crate) fn substitute_value(
        &self,
    ) -> Result<Value<crate::resource::ParserPlaceholder, B>, crate::parser::ParseError> {
        crate::value_walk::substitute_function_parameters(
            &self.local.1,
            &self.cursor.completion.substitutions,
            self.source_line(),
            crate::value_walk::RewritePosition::Value,
        )
    }
    pub(crate) fn bind(
        mut self,
        value: FunctionArgument<crate::resource::ParserPlaceholder, B>,
    ) -> FunctionLocals<'a, P, B> {
        self.cursor
            .completion
            .observations
            .observe(value.walker_value());
        self.cursor
            .completion
            .substitutions
            .insert(self.local.0.clone(), value);
        self.cursor
    }
}
pub(crate) struct FunctionBodyCompletion<'a, P: PlaceholderKind, B: BindingAddress> {
    function: &'a UserFunction<B>,
    retained: RetainedCall<P, B>,
    observations: CallObservations,
    substitutions: HashMap<String, FunctionArgument<crate::resource::ParserPlaceholder, B>>,
}
impl<P: PlaceholderKind, B: BindingAddress> FunctionBodyCompletion<'_, P, B> {
    pub(crate) fn substitute_body(
        &self,
    ) -> Result<Value<crate::resource::ParserPlaceholder, B>, crate::parser::ParseError> {
        crate::value_walk::substitute_function_parameters(
            &self.function.body.value,
            &self.substitutions,
            self.function.body.source_line,
            crate::value_walk::RewritePosition::Value,
        )
    }
    pub(crate) fn finish(
        mut self,
        value: Value<P, B>,
        provider: TypecheckProviderContext<'_>,
        site: impl FnOnce() -> Option<ProviderTypeCheckSite>,
    ) -> Result<FunctionEvaluation<P, B>, crate::validation::typecheck::TypeExprValueError> {
        self.observations.observe(&value);
        let check =
            check_declared_annotation(self.function.return_type.as_ref(), value, provider, site)?;
        Ok(match check {
            AnnotationCheck::Discharged(witness) => {
                if self.observations.surviving_call {
                    FunctionEvaluation::Retain(self.retained)
                } else {
                    FunctionEvaluation::Inline(CheckedValue(witness.value))
                }
            }
            AnnotationCheck::AwaitingValue(_) => FunctionEvaluation::Retain(self.retained),
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
pub(crate) fn contains_deferred_user_function_call<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => deferred
            .values()
            .into_iter()
            .any(contains_deferred_user_function_call),
        Value::Deferred(DeferredValue::FunctionCall { name, args, .. }) => {
            crate::builtins::builtin_arity(name).is_none()
                || args
                    .iter()
                    .any(|argument| contains_deferred_user_function_call(argument.walker_value()))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall { .. }) => true,
        Value::Deferred(DeferredValue::Unavailable { args, .. }) => args
            .iter()
            .any(|argument| contains_deferred_user_function_call(argument.walker_value())),
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
        let args = function
            .params
            .iter()
            .map(|_| FunctionArgument::from_value(unknown()))
            .collect();
        FunctionCompletion::new(function, &Arc::new(IndexMap::new()), args, 1.into()).unwrap()
    }
    fn unknown() -> AuthoredValue {
        AuthoredValue::resource_ref("src", "emitted", vec![])
    }
    fn text(text: &str) -> AuthoredValue {
        Value::Concrete(ConcreteValue::String(text.into()))
    }
    fn locals<'a>(
        function: &'a Arc<UserFunction>,
        config: &ProviderContext,
    ) -> FunctionLocals<'a, ParserPlaceholder, RelativeBindingName> {
        let mut cursor = completion(function);
        loop {
            match cursor.next_argument() {
                ArgumentStep::Argument(slot) => {
                    (cursor, _) = slot
                        .check(
                            FunctionArgument::from_value(unknown()),
                            TypecheckProviderContext::for_parser(config),
                            || None,
                            "argument",
                        )
                        .unwrap();
                }
                ArgumentStep::Complete(PreparedFunction::Ready { completion }) => {
                    return completion;
                }
                ArgumentStep::Complete(PreparedFunction::Retain(_)) => {
                    panic!("primitive arguments must prepare")
                }
            }
        }
    }
    fn body<'a>(
        function: &'a Arc<UserFunction>,
        config: &ProviderContext,
    ) -> FunctionBodyCompletion<'a, ParserPlaceholder, RelativeBindingName> {
        match locals(function, config).next_local() {
            LocalStep::Body(completion) => completion,
            LocalStep::Local(_) => panic!("locals require their tokens"),
        }
    }
    #[test]
    fn declared_checks_completion_requires_every_declared_slot() {
        let function = function("fn g(x: String): 'allowed' { x }");
        assert!(matches!(
            completion(&function).next_argument(),
            ArgumentStep::Argument(_)
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
        let ArgumentStep::Argument(slot) = completion(&function).next_argument() else {
            panic!("first slot")
        };
        let (cursor, _) = slot
            .check(
                FunctionArgument::from_value(unknown()),
                TypecheckProviderContext::for_parser(&config()),
                || None,
                "argument",
            )
            .unwrap();
        assert!(matches!(cursor.next_argument(), ArgumentStep::Argument(_)));
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
            Some(&expected),
            unknown(),
            TypecheckProviderContext::for_parser(&config()),
            || None,
        )
        .unwrap();
        assert!(matches!(check, AnnotationCheck::AwaitingValue(_)));
        assert!(
            check_declared_annotation(
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
        let completion = FunctionCompletion::new(
            &function,
            &Arc::new(IndexMap::new()),
            vec![FunctionArgument::from_value(pending)],
            1.into(),
        )
        .unwrap();
        let ArgumentStep::Argument(slot) = completion.next_argument() else {
            panic!("argument")
        };
        let (cursor, _) = slot
            .check(
                FunctionArgument::from_value(unknown()),
                TypecheckProviderContext::for_parser(&config()),
                || None,
                "argument",
            )
            .unwrap();
        let ArgumentStep::Complete(PreparedFunction::Ready { completion }) = cursor.next_argument()
        else {
            panic!("resolved argument")
        };
        let LocalStep::Body(completed) = completion.next_local() else {
            panic!("no locals")
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
    fn mixed_checks_for_distinct_slots_never_inline() {
        let mut function = function("fn g(x: 'allowed'): String { x }");
        let duplicate = function.params[0].clone();
        Arc::make_mut(&mut function).params.push(duplicate);
        for first_known in [false, true] {
            let mut cursor = completion(&function);
            for known in [first_known, !first_known] {
                let ArgumentStep::Argument(slot) = cursor.next_argument() else {
                    panic!("distinct slot")
                };
                (cursor, _) = slot
                    .check(
                        FunctionArgument::from_value(if known {
                            text("allowed")
                        } else {
                            unknown()
                        }),
                        TypecheckProviderContext::for_parser(&config()),
                        || None,
                        "argument",
                    )
                    .unwrap();
            }
            assert!(matches!(
                cursor.next_argument(),
                ArgumentStep::Complete(PreparedFunction::Retain(_))
            ));
        }
    }
    #[test]
    fn observed_local_survives_body_evaluation() {
        let function = function("fn g(x: String): String { let unused = x\nx }");
        let LocalStep::Local(slot) = locals(&function, &config()).next_local() else {
            panic!("local")
        };
        let cursor = slot.bind(FunctionArgument::from_value(
            completion(&function).retain().into_value(),
        ));
        let LocalStep::Body(completed) = cursor.next_local() else {
            panic!("one local")
        };
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
        let ArgumentStep::Argument(slot) = completion(&function).next_argument() else {
            panic!("argument")
        };
        let (cursor, _) = slot
            .check(
                FunctionArgument::from_value(text("allowed")),
                TypecheckProviderContext::for_parser(&config()),
                || None,
                "argument",
            )
            .unwrap();
        let ArgumentStep::Complete(PreparedFunction::Ready { completion }) = cursor.next_argument()
        else {
            panic!("checked argument")
        };
        let LocalStep::Body(completion) = completion.next_local() else {
            panic!("no locals")
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
    fn settling_unused_pending_local_retains_the_call() {
        let function = function(
            "fn checked(x: 'allowed') { x }\nfn g(x: String) { let unused = checked(x)\n 'const' }",
        );
        let LocalStep::Local(slot) = locals(&function, &config()).next_local() else {
            panic!("unused local token")
        };
        let cursor = slot.bind(FunctionArgument::from_value(
            completion(&function).retain().into_value(),
        ));
        let LocalStep::Body(completed) = cursor.next_local() else {
            panic!("one local")
        };
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
    fn argument_cursor_cannot_revisit_an_awaiting_slot() {
        let function = function("fn g(x: 'allowed'): String { x }");
        let ArgumentStep::Argument(slot) = completion(&function).next_argument() else {
            panic!("one parameter");
        };
        let (cursor, report) = slot
            .check(
                FunctionArgument::from_value(unknown()),
                TypecheckProviderContext::for_parser(&config()),
                || None,
                "argument",
            )
            .unwrap();
        assert!(report.needs_recheck());
        assert!(matches!(
            cursor.next_argument(),
            ArgumentStep::Complete(PreparedFunction::Retain(_))
        ));
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
        .unwrap()
        .retain()
        .into_value();
        let message = serde_json::to_string(&call).unwrap_err().to_string();
        assert!(
            !message.contains("private-serialization-argument"),
            "SECRET_LEAK_DETECTED core serialization"
        );

        assert_eq!(
            message,
            "the plan contains an unevaluated call to a user-defined function. A saved plan cannot carry this call."
        );
        assert!(!message.contains("carina apply"));
    }
}

#[cfg(test)]
mod unavailable_call_walk_tests {
    use super::*;
    #[test]
    fn unavailable_arguments_preserve_retained_call_observation() {
        let value = Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![FunctionArgument::from_value(
                crate::value::retained_call_snapshot_tests::retained_call(),
            )],
            reason: crate::resource::UnavailableReason::from_builtin_error("fixture error".into()),
            source_line: 1.into(),
        });
        assert!(contains_deferred_user_function_call(&value));
        let conversion = crate::value::SavedDocumentConversion::new(std::iter::empty()).unwrap();
        assert!(matches!(
            conversion.value(&value.into_secret()),
            Err(crate::value::SerializationError::RetainedUserFunctionCall(
                _
            ))
        ));
    }
}

#[cfg(test)]
mod arity_constructor_tests {
    use super::*;
    #[test]
    fn completion_constructor_rejects_missing_and_extra_arguments() {
        let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            "fn g(x): String { x }",
            std::path::Path::new("functions.crn"),
            &context,
        )
        .unwrap();
        let function = Arc::new(parsed.user_functions["g"].clone());
        for count in [0, 2] {
            let args = (0..count)
                .map(|_| {
                    FunctionArgument::from_value(crate::resource::AuthoredValue::Concrete(
                        ConcreteValue::String("value".into()),
                    ))
                })
                .collect();
            assert!(
                FunctionCompletion::new(&function, &Arc::new(IndexMap::new()), args, 1.into())
                    .is_err()
            );
        }
    }
}
