use super::*;
use crate::{
    binding_index::ResolvedBindings,
    resolver::{ResolutionError, planning_value::evaluate_planned_user_function},
    resource::{DiagnosticSourceLine, InsideSecret, ParserPlaceholder},
    validation::deferred_types::ProviderTypeCheckContext,
};

pub(crate) fn expand_planned_functions<P: ValueKind>(
    value: &Value<P>,
    bindings: &ResolvedBindings<ParserPlaceholder>,
    session: &dyn ProviderTypeCheckContext,
) -> Result<Value<P>, ResolutionError> {
    let mut algebra = PlanningFunctions {
        bindings,
        session,
        error: None,
        expanded: 0,
    };
    let result = engine::dispatch_bound(value.clone(), WalkRoot::syntactic().at(), &mut algebra);
    match result {
        Ok(product) => Ok(product.value),
        Err(WalkError::Rejected) => Err(algebra.error.take().unwrap_or_else(|| {
            ResolutionError::Message("planning expression traversal rejected a value".into())
        })),
        Err(
            error @ (WalkError::ArgumentMetadata
            | WalkError::AccessMetadata
            | WalkError::ArithmeticOperand
            | WalkError::SecretHash
            | WalkError::RedactionStage
            | WalkError::ProtectedLocator
            | WalkError::ReenteredLocator
            | WalkError::UnknownStructField
            | WalkError::ReceiptOrigin),
        ) => Err(ResolutionError::Message(error.to_string())),
    }
}
struct PlanningFunctions<'a> {
    bindings: &'a ResolvedBindings<ParserPlaceholder>,
    session: &'a dyn ProviderTypeCheckContext,
    error: Option<ResolutionError>,
    expanded: usize,
}
impl TransformOutput for PlanningFunctions<'_> {
    type Output = ();
}
impl<P: ValueKind> BoundTransform<P, AbsoluteBindingAddress> for PlanningFunctions<'_> {
    fn scalar(
        &mut self,
        _: &At<'_>,
        value: ScopedScalar<'_, Transform, P, AbsoluteBindingAddress>,
    ) -> Step<Transform, (), P, AbsoluteBindingAddress> {
        value.keep(())
    }
    fn placeholder(
        &mut self,
        _: &At<'_>,
        value: ScopedLeaf<'_, Transform, P, AbsoluteBindingAddress>,
    ) -> Step<Transform, (), P, AbsoluteBindingAddress> {
        value.keep(())
    }
    fn list(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Transform, P, AbsoluteBindingAddress>,
    ) -> Step<Transform, (), P, AbsoluteBindingAddress> {
        Step::new(children.parts.transform(children.at, self, |_| ()))
    }
    fn map(
        &mut self,
        at: &At<'_>,
        children: Children<'_, Transform, P, AbsoluteBindingAddress>,
    ) -> Step<Transform, (), P, AbsoluteBindingAddress> {
        self.list(at, children)
    }
    fn expression(
        &mut self,
        _: &At<'_>,
        expression: ExprChildren<'_, Transform, P, AbsoluteBindingAddress>,
    ) -> Step<Transform, (), P, AbsoluteBindingAddress> {
        let before = self.expanded;
        let result = expression
            .children
            .parts
            .transform(expression.children.at, self, |_| ())
            .map(|mut product| {
                product.changed |= self.expanded != before;
                product
            });
        Step::new(result)
    }
    fn secret(
        &mut self,
        _: &SecretAt<'_>,
        child: SecretChild<'_, Transform, P, AbsoluteBindingAddress>,
    ) -> SecretStep<Transform, (), P, AbsoluteBindingAddress> {
        let result = child.transform_bound(self);
        if result.step.result.is_err() {
            self.error = self.error.take().map(InsideSecret::inside_secret);
        }
        result
    }
    fn resolved_call(
        &mut self,
        at: &At<'_>,
        function: Arc<UserFunction<AbsoluteBindingAddress>>,
        functions: Arc<indexmap::IndexMap<String, UserFunction<AbsoluteBindingAddress>>>,
        args: Vec<FunctionArgument<P>>,
        source_line: DiagnosticSourceLine,
    ) -> Result<Value<P>, WalkError> {
        if at.expression_edges().iter().any(|edge| {
            matches!(
                edge,
                ExpressionEdge::FunctionDefinition | ExpressionEdge::FunctionScope(_)
            )
        }) {
            return Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function,
                functions,
                args,
                source_line,
            }));
        }
        match evaluate_planned_user_function(
            &function,
            &functions,
            &args,
            self.bindings,
            self.session,
            source_line,
        ) {
            Ok(Some(expansion)) => {
                self.expanded += 1;
                Ok(expansion.into_expression())
            }
            Ok(None) => Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function,
                functions,
                args,
                source_line,
            })),
            Err(error) => {
                self.error = Some(error);
                Err(WalkError::Rejected)
            }
        }
    }
}
