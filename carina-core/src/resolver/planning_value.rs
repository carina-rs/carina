use super::*;
use crate::resource::{FunctionArgument, ParserPlaceholder, ValueKind};
use crate::validation::deferred_types::{ProviderTypeCheckContext, TypeCheckSession};

/// Planning bindings and the loaded declarations used to evaluate saved expressions.
#[derive(Debug, Clone)]
pub struct PlanningEvaluation {
    bindings: std::sync::Arc<ResolvedBindings<ParserPlaceholder>>,
    context: crate::parser::LoadedProviderContext,
}

/// An expression paired with the evaluator's result of expanding its checked calls.
/// Only the evaluator can create this pair.
///
/// ```compile_fail,E0451
/// use carina_core::{resolver::PlannedExpression, resource::Value};
/// fn forge(expression: Value) -> PlannedExpression<carina_core::resource::NoPlaceholder> {
///     PlannedExpression { expression }
/// }
/// ```
#[derive(Debug)]
pub struct PlannedExpression<P: ValueKind> {
    expression: Value<P>,
}
impl<P: ValueKind> PlannedExpression<P> {
    pub(crate) fn into_expression(self) -> Value<P> {
        self.expression
    }
}

impl PlanningEvaluation {
    pub fn new(
        bindings: &ResolvedBindings,
        context: &crate::parser::LoadedProviderContext,
    ) -> Self {
        Self {
            bindings: std::sync::Arc::new(bindings.parser_stage()),
            context: context.clone(),
        }
    }
    pub(crate) fn expression<P: ValueKind>(
        &self,
        source: &Value<P>,
    ) -> Result<PlannedExpression<P>, ResolutionError> {
        let mut diagnostics = Vec::new();
        let result = TypeCheckSession::run(self.context.as_provider_context(), |session| {
            crate::value_walk::expand_planned_functions(source, &self.bindings, session)
        })
        .publish(&mut diagnostics);
        let expression = result?;
        let errors = diagnostics.into_iter().filter_map(|diagnostic| match diagnostic {
            crate::validation::deferred_types::ProviderTypeCheckDiagnostic::Deferred(_warning) => None,
            crate::validation::deferred_types::ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error) => Some(error.into_warning().message),
        }).collect::<Vec<_>>();
        if !errors.is_empty() {
            return Err(ResolutionError::Message(errors.join("\n")));
        }
        Ok(PlannedExpression { expression })
    }
}

pub(crate) struct CheckedExpansion<P: ValueKind> {
    result: crate::function_evaluation::CheckedValue<ParserPlaceholder, AbsoluteBindingAddress>,
    expression: Value<P>,
}
impl<P: ValueKind> CheckedExpansion<P> {
    pub(crate) fn into_expression(self) -> Value<P> {
        let Self { result, expression } = self;
        drop(result);
        expression
    }
}

/// An evaluator-owned pair for the exact source tree that will be admitted.
///
/// ```compile_fail,E0451
/// use carina_core::resolver::PlannedDocumentTree;
/// fn forge<T>(source: &T, expanded: T) -> PlannedDocumentTree<'_, T> {
///     PlannedDocumentTree { source, expanded }
/// }
/// ```
///
/// ```compile_fail,E0616
/// use carina_core::resolver::PlannedDocumentTree;
/// fn substitute<'a,T>(mut checked: PlannedDocumentTree<'a,T>, other: T) -> PlannedDocumentTree<'a,T> {
///     checked.expanded = other;
///     checked
/// }
/// ```
#[derive(Debug)]
pub struct PlannedDocumentTree<'a, T> {
    source: &'a T,
    expanded: T,
}
impl<T> PlannedDocumentTree<'_, T> {
    pub(crate) fn into_expanded(self) -> T {
        self.expanded
    }
    pub fn source(&self) -> &T {
        self.source
    }
}
impl PlanningEvaluation {
    pub(crate) fn prepare<'a, T>(
        &self,
        source: &'a T,
    ) -> Result<PlannedDocumentTree<'a, T>, crate::value::SerializationError>
    where
        T: crate::value::ExpandableDocumentTree + Clone,
    {
        let mut expanded = source.clone();
        expanded.expand(self)?;
        Ok(PlannedDocumentTree { source, expanded })
    }
}

pub(crate) fn evaluate_planned_user_function<P: ValueKind>(
    function: &std::sync::Arc<crate::parser::UserFunction<AbsoluteBindingAddress>>,
    functions: &std::sync::Arc<
        IndexMap<String, crate::parser::UserFunction<AbsoluteBindingAddress>>,
    >,
    args: &[FunctionArgument<P>],
    bindings: &ResolvedBindings<ParserPlaceholder>,
    session: &dyn ProviderTypeCheckContext,
    line: crate::resource::DiagnosticSourceLine,
) -> Result<Option<CheckedExpansion<P>>, ResolutionError> {
    // A historical hash has no concrete value on which to run an annotation.
    // Loop slots remain real parser placeholders and may still discharge a
    // primitive annotation, as in the ordinary runtime evaluator.
    let argument_values = args
        .iter()
        .cloned()
        .map(|argument| {
            argument.try_map_stage(|value| {
                value.try_map_placeholder(&mut |leaf| match leaf.loop_slot() {
                    Some(slot) => Ok(Value::Deferred(DeferredValue::Placeholder(
                        ParserPlaceholder::Loop(slot),
                    ))),
                    None => Err(()),
                })
            })
        })
        .collect::<Result<Vec<_>, _>>();
    let arguments = match argument_values {
        Ok(arguments) => arguments,
        Err(_unavailable) => return Ok(None),
    };
    match evaluate_runtime_user_function(
        function,
        functions,
        &arguments,
        bindings,
        session,
        line.get().unwrap_or(0),
    )
    .map_err(RuntimeFunctionError::into_resolution)?
    {
        FunctionEvaluation::Inline(result) => {
            let (function, mut substitutions) = result.source_for_expansion();
            for (name, expression, source_line) in &function.local_lets {
                let expression = crate::value_walk::substitute_function_parameters(
                    expression,
                    &substitutions,
                    *source_line,
                    crate::value_walk::RewritePosition::Value,
                )?;
                let expanded =
                    crate::value_walk::expand_planned_functions(&expression, bindings, session)?;
                substitutions.insert(name.clone(), FunctionArgument::from_value(expanded));
            }
            let expression = crate::value_walk::substitute_function_parameters(
                &function.body.value,
                &substitutions,
                function.body.source_line,
                crate::value_walk::RewritePosition::Value,
            )?;
            let expanded =
                crate::value_walk::expand_planned_functions(&expression, bindings, session)?;
            let expanded = resolve_ref_value_inner(
                &expanded,
                &ResolvedBindings::default(),
                RuntimeContext::Session(session),
            )?;
            let expression = lower_planned_value(expanded)?;
            Ok(Some(CheckedExpansion { result, expression }))
        }
        FunctionEvaluation::Retain(_call) => Ok(None),
    }
}

fn lower_planned_value<P: ValueKind>(
    value: Value<ParserPlaceholder>,
) -> Result<Value<P>, ResolutionError> {
    value.try_map_placeholder(&mut |placeholder| match placeholder {
        ParserPlaceholder::Loop(slot) => P::from_loop_slot(slot)
            .map(|slot| Value::Deferred(DeferredValue::Placeholder(slot)))
            .map_err(|error| ResolutionError::Message(error.to_string())),
        ParserPlaceholder::FnParam { name }
        | ParserPlaceholder::FnLocal { name }
        | ParserPlaceholder::ModuleArgument { name, .. } => Err(ResolutionError::Message(format!(
            "unsubstituted function binding '{name}'"
        ))),
        ParserPlaceholder::EmptyInterpolation { .. } => Err(ResolutionError::Message(
            "empty function interpolation".into(),
        )),
    })
}

impl PlanningEvaluation {
    pub(crate) fn replay_value<P: crate::resource::PlaceholderKind>(
        &self,
        source: &Value<P>,
    ) -> Result<PlannedExpression<P>, ResolutionError> {
        let lifted =
            crate::resource::FunctionArgument::from_value(source.clone()).into_parser_stage();
        let value = lifted
            .value()
            .ok_or_else(|| ResolutionError::Message("secret candidate is not a value".into()))?;
        let mut diagnostics = Vec::new();
        let result = TypeCheckSession::run(self.context.as_provider_context(), |session| {
            resolve_ref_value_inner(value, &self.bindings, RuntimeContext::Session(session))
        })
        .publish(&mut diagnostics)?;
        let errors=diagnostics.into_iter().filter_map(|diagnostic|match diagnostic {
            crate::validation::deferred_types::ProviderTypeCheckDiagnostic::Deferred(_warning)=>None,
            crate::validation::deferred_types::ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error)=>Some(error.into_warning().message),
        }).collect::<Vec<_>>();
        if !errors.is_empty() {
            return Err(ResolutionError::Message(errors.join("\n")));
        }
        Ok(PlannedExpression {
            expression: lower_planned_value(result)?,
        })
    }
}
