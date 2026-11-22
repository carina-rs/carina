//! Parser policy adapter over the merged kernel's typed Transform fold.
//! Concrete parser/module operations keep their rules and raw-node callbacks
//! inside the kernel. Owners supply inventories, never protected-child callbacks.

use super::*;
mod functions;
mod modules;
use crate::{
    integer_arithmetic::ValidatedIntegerExpr,
    parser::ParseError,
    resource::{DiagnosticSourceLine, InsideSecret, ParserPlaceholder},
};
pub(crate) use functions::{substitute_function_call_argument, substitute_function_parameters};
#[cfg(test)]
pub(crate) use modules::select_authored_module_ifs;
pub(crate) use modules::{
    evaluate_expanded_module_functions, select_expanded_module_ifs_with_conditions,
    substitute_module_arguments,
};

#[derive(Clone, Copy)]
pub(crate) enum RewritePosition {
    Value,
    AccessBase,
}

/// Lower a value constructor's operand without classifying a forwarded module
/// parameter as a resource handle. A real structural handle remains invalid.
pub(crate) fn value_argument<P: ValueKind, B: BindingAddress>(
    argument: FunctionArgument<P, B>,
) -> Option<Value<P, B>> {
    if argument.pending_parameter().is_some() {
        Some(argument.walker_value().clone())
    } else {
        argument.into_value()
    }
}

/// Nonrecursive semantic operations private to the kernel. Raw protected
/// children cannot be passed to callbacks implemented by other core modules.
pub(super) trait ParserRewrite<B: BindingAddress> {
    type Error: InsideSecret;

    fn error(&mut self, error: ParseError) -> Self::Error;
    fn source_line(&self) -> usize;
    fn set_source_line(&mut self, _: usize) {}
    fn branches_inherit_position(&self) -> bool {
        false
    }

    fn leaf(
        &mut self,
        _input: &LeafInput<'_, Transform, ParserPlaceholder, B>,
        _: RewritePosition,
    ) -> Result<Option<Value<ParserPlaceholder, B>>, Self::Error> {
        Ok(None)
    }
    fn argument(
        &mut self,
        _: &FunctionArgument<ParserPlaceholder, B>,
        _: crate::resource::DiagnosticSourceLine,
    ) -> Result<Option<FunctionArgument<ParserPlaceholder, B>>, Self::Error> {
        Ok(None)
    }
    fn call(
        &mut self,
        _name: crate::builtins::BuiltinFunction,
        _args: &[FunctionArgument<ParserPlaceholder, B>],
        _source_line: DiagnosticSourceLine,
    ) -> Result<Option<Value<ParserPlaceholder, B>>, Self::Error> {
        Ok(None)
    }
    fn resolved_call(
        &mut self,
        _function: &Arc<crate::parser::UserFunction<B>>,
        _functions: &Arc<indexmap::IndexMap<String, crate::parser::UserFunction<B>>>,
        _args: &[FunctionArgument<ParserPlaceholder, B>],
        _source_line: DiagnosticSourceLine,
    ) -> Result<Option<Value<ParserPlaceholder, B>>, Self::Error> {
        Ok(None)
    }
    fn select(
        &mut self,
        _: ExpressionKind,
        _: &Value<ParserPlaceholder, B>,
        _: Option<(&std::path::Path, usize)>,
    ) -> Result<Option<usize>, Self::Error> {
        Ok(None)
    }
    fn selected_condition(&mut self, _: Option<crate::parser::retained::NodeId>, _: bool) {}
    fn condition_error(&mut self, error: Self::Error, _: &std::path::Path) -> Self::Error {
        error
    }
    fn access(
        &mut self,
        access: crate::resource::AccessExpr<ParserPlaceholder, B>,
        base: Value<ParserPlaceholder, B>,
    ) -> Result<Value<ParserPlaceholder, B>, Self::Error> {
        access
            .rebuild_with_base(base, crate::resource::AccessMissPolicy::Error)
            .map_err(|error| {
                self.error(ParseError::InvalidExpression {
                    line: access.source_line(),
                    message: error.to_string(),
                })
            })
    }
    fn arithmetic(
        &mut self,
        expression: ValidatedIntegerExpr<ParserPlaceholder, B>,
        lhs: Value<ParserPlaceholder, B>,
        rhs: Value<ParserPlaceholder, B>,
    ) -> Result<Value<ParserPlaceholder, B>, Self::Error> {
        expression
            .evaluate_with_operands(lhs, rhs)
            .map_err(|error| {
                self.error(ParseError::integer_arithmetic(
                    expression.operator_span(),
                    self.source_line(),
                    error,
                ))
            })
    }
}

pub(super) fn rewrite_parser_value<B: BindingAddress, R: ParserRewrite<B>>(
    value: &Value<ParserPlaceholder, B>,
    position: RewritePosition,
    policy: &mut R,
) -> Result<Value<ParserPlaceholder, B>, R::Error> {
    let mut algebra = ParserTransform::new(policy, position);
    engine::dispatch_bound(value.clone(), WalkRoot::syntactic().at(), &mut algebra)
        .map(|product| product.value)
        .map_err(|error| algebra.take_error(error))
}

struct ParserTransform<'a, R, E, B: BindingAddress> {
    binding: PhantomData<B>,
    policy: &'a mut R,
    error: Option<E>,
    position: RewritePosition,
    position_start: usize,
    structural_only: usize,
    argument_line: DiagnosticSourceLine,
    replacements: usize,
}

impl<R, E, B: BindingAddress> TransformOutput for ParserTransform<'_, R, E, B> {
    type Output = ();
}

impl<R, E: InsideSecret, B: BindingAddress> ParserTransform<'_, R, E, B> {
    fn record<T>(&mut self, result: Result<T, E>) -> Result<T, WalkError> {
        result.map_err(|error| {
            self.error = Some(error);
            WalkError::Rejected
        })
    }

    fn is_structural_only(&self, at: &At<'_>) -> bool {
        self.structural_only != 0
            || at.expression_edges().iter().any(|edge| {
                matches!(
                    edge,
                    ExpressionEdge::FunctionDefinition | ExpressionEdge::FunctionScope(_)
                )
            })
    }

    fn value_scope<T>(&mut self, at: &At<'_>, run: impl FnOnce(&mut Self) -> T) -> T {
        let previous = (self.position, self.position_start);
        self.position = RewritePosition::Value;
        self.position_start = at.expression_edges().len();
        let result = run(self);
        (self.position, self.position_start) = previous;
        result
    }
}

impl<'a, B: BindingAddress, R: ParserRewrite<B>> ParserTransform<'a, R, R::Error, B> {
    fn new(policy: &'a mut R, position: RewritePosition) -> Self {
        Self {
            policy,
            error: None,
            position,
            position_start: 0,
            structural_only: 0,
            argument_line: DiagnosticSourceLine::synthetic(),
            replacements: 0,
            binding: PhantomData,
        }
    }

    fn position(&self, at: &At<'_>) -> RewritePosition {
        let edges = &at.expression_edges()[self.position_start..];
        let edge = if self.policy.branches_inherit_position() {
            edges
                .iter()
                .rev()
                .find(|edge| !matches!(edge, ExpressionEdge::Then | ExpressionEdge::Else))
        } else {
            edges.last()
        };
        match edge {
            Some(ExpressionEdge::AccessBase) => RewritePosition::AccessBase,
            Some(_) => RewritePosition::Value,
            None => self.position,
        }
    }

    fn take_error(&mut self, error: WalkError) -> R::Error {
        self.error
            .take()
            .unwrap_or_else(|| self.policy.error(ParseError::ValueWalk(error)))
    }

    fn operand(
        &mut self,
        value: Value<ParserPlaceholder, B>,
        at: At<'_>,
    ) -> Result<RewriteProduct<(), ParserPlaceholder, B>, WalkError> {
        engine::dispatch_bound(value, at, self)
    }

    fn retained(
        &mut self,
        value: Value<ParserPlaceholder, B>,
        at: At<'_>,
    ) -> Result<(), WalkError> {
        self.structural_only += 1;
        let result = self.operand(value, at).map(|_| ());
        self.structural_only -= 1;
        result
    }

    fn expression_parts<'v>(
        &mut self,
        parts: Parts<'v, Transform, ParserPlaceholder, B>,
        at: At<'v>,
    ) -> Result<RewriteProduct<(), ParserPlaceholder, B>, WalkError> {
        let (value, changed) =
            match parts {
                Parts::Access(access) => {
                    let base = self.operand(
                        access.base().clone(),
                        at.operand(ExpressionEdge::AccessBase),
                    )?;
                    if base.changed {
                        let result = self.policy.access(access, base.value);
                        (self.record(result)?, true)
                    } else {
                        (Value::Deferred(DeferredValue::Access(access)), false)
                    }
                }
                Parts::Arithmetic(expression) => {
                    let lhs = self.operand(
                        expression.lhs().clone(),
                        at.operand(ExpressionEdge::ArithmeticLeft),
                    )?;
                    let rhs = self.operand(
                        expression.rhs().clone(),
                        at.operand(ExpressionEdge::ArithmeticRight),
                    )?;
                    if lhs.changed || rhs.changed {
                        let result = self.policy.arithmetic(expression, lhs.value, rhs.value);
                        (self.record(result)?, true)
                    } else {
                        (
                            Value::Deferred(DeferredValue::IntegerArithmetic(expression)),
                            false,
                        )
                    }
                }
                Parts::Coalesce { value, default } => {
                    let first = self.operand(*value, at.operand(ExpressionEdge::CoalesceValue))?;
                    let selection =
                        self.policy
                            .select(ExpressionKind::Coalesce, &first.value, None);
                    match self.record(selection)? {
                        Some(0) => {
                            self.retained(*default, at.operand(ExpressionEdge::CoalesceDefault))?;
                            (first.value, true)
                        }
                        Some(1) => {
                            let value = self
                                .operand(*default, at.operand(ExpressionEdge::CoalesceDefault))?;
                            (value.value, true)
                        }
                        Some(_) => return Err(WalkError::Rejected),
                        None => {
                            let fallback = self
                                .operand(*default, at.operand(ExpressionEdge::CoalesceDefault))?;
                            (
                                Value::Deferred(DeferredValue::Coalesce {
                                    value: Box::new(first.value),
                                    default: Box::new(fallback.value),
                                }),
                                first.changed || fallback.changed,
                            )
                        }
                    }
                }
                Parts::ModuleIf(conditional) => {
                    let crate::parser::DeferredValueIf {
                        condition,
                        then_value,
                        else_value,
                    } = *conditional;
                    let crate::parser::DeferredIfCondition {
                        value,
                        source_file,
                        source_line,
                        source_expression,
                        node_id,
                    } = condition;
                    let first = self
                        .operand(value, at.operand(ExpressionEdge::Condition))
                        .inspect_err(|_| {
                            self.error = self
                                .error
                                .take()
                                .map(|error| self.policy.condition_error(error, &source_file));
                        })?;
                    let selection = self.policy.select(
                        ExpressionKind::ModuleIf,
                        &first.value,
                        Some((&source_file, source_line)),
                    );
                    match self.record(selection)? {
                        Some(1) => {
                            self.policy.selected_condition(node_id, true);
                            let value =
                                self.operand(then_value, at.operand(ExpressionEdge::Then))?;
                            self.retained(else_value, at.operand(ExpressionEdge::Else))?;
                            (value.value, true)
                        }
                        Some(2) => {
                            self.policy.selected_condition(node_id, false);
                            self.retained(then_value, at.operand(ExpressionEdge::Then))?;
                            let value =
                                self.operand(else_value, at.operand(ExpressionEdge::Else))?;
                            (value.value, true)
                        }
                        Some(_) => return Err(WalkError::Rejected),
                        None => {
                            let then_value =
                                self.operand(then_value, at.operand(ExpressionEdge::Then))?;
                            let else_value =
                                self.operand(else_value, at.operand(ExpressionEdge::Else))?;
                            let changed = first.changed || then_value.changed || else_value.changed;
                            (Value::Deferred(DeferredValue::ModuleIf(Box::new(
                            crate::parser::DeferredValueIf::new(
                                crate::parser::DeferredIfCondition::new_with_source_expression(
                                    first.value, source_file, source_line, source_expression,
                                ).with_node_id(node_id), then_value.value, else_value.value,
                            ),
                        ))), changed)
                        }
                    }
                }
                parts @ (Parts::List(_)
                | Parts::Map(_)
                | Parts::Interpolation(_)
                | Parts::Call {
                    name: _,
                    args: _,
                    source_line: _,
                }
                | Parts::Unavailable {
                    name: _,
                    args: _,
                    reason: _,
                    source_line: _,
                }
                | Parts::ResolvedCall {
                    function: _,
                    functions: _,
                    args: _,
                    source_line: _,
                }
                | Parts::Evidence { value: _, edge: _ }) => {
                    let replacements = self.replacements;
                    let mut product = parts.transform(at, self, |_| ())?;
                    product.changed |= self.replacements != replacements;
                    return Ok(product);
                }
            };
        Ok(RewriteProduct {
            value,
            output: (),
            changed,
        })
    }
}

impl<B: BindingAddress, R: ParserRewrite<B>> BoundTransform<ParserPlaceholder, B>
    for ParserTransform<'_, R, R::Error, B>
{
    fn scalar(
        &mut self,
        _: &At<'_>,
        value: ScopedScalar<'_, Transform, ParserPlaceholder, B>,
    ) -> Step<Transform, (), ParserPlaceholder, B> {
        value.keep(())
    }

    fn list(
        &mut self,
        at: &At<'_>,
        children: Children<'_, Transform, ParserPlaceholder, B>,
    ) -> Step<Transform, (), ParserPlaceholder, B> {
        self.value_scope(at, |algebra| {
            Step::new(children.parts.transform(children.at, algebra, |_| ()))
        })
    }

    fn map(
        &mut self,
        at: &At<'_>,
        children: Children<'_, Transform, ParserPlaceholder, B>,
    ) -> Step<Transform, (), ParserPlaceholder, B> {
        self.value_scope(at, |algebra| {
            Step::new(children.parts.transform(children.at, algebra, |_| ()))
        })
    }

    fn placeholder(
        &mut self,
        at: &At<'_>,
        leaf: ScopedLeaf<'_, Transform, ParserPlaceholder, B>,
    ) -> Step<Transform, (), ParserPlaceholder, B> {
        if self.is_structural_only(at) {
            return leaf.keep(());
        }
        let result = self.policy.leaf(&leaf.input, self.position(at));
        match self.record(result) {
            Ok(Some(value)) => leaf.replace(value, ()),
            Ok(None) => leaf.keep(()),
            Err(error) => Step::new(Err(error)),
        }
    }

    fn expression(
        &mut self,
        at: &At<'_>,
        expression: ExprChildren<'_, Transform, ParserPlaceholder, B>,
    ) -> Step<Transform, (), ParserPlaceholder, B> {
        if self.is_structural_only(at) {
            return Step::new(expression.children.parts.transform(
                expression.children.at,
                self,
                |_| (),
            ));
        }
        let previous_line = self.policy.source_line();
        let previous_argument_line = self.argument_line;
        let line = match &expression.children.parts {
            Parts::Call {
                name: _,
                args: _,
                source_line,
            }
            | Parts::Unavailable {
                name: _,
                args: _,
                reason: _,
                source_line,
            }
            | Parts::ResolvedCall {
                function: _,
                functions: _,
                args: _,
                source_line,
            } => *source_line,
            Parts::Access(access) => DiagnosticSourceLine::authored(access.source_line()),
            Parts::ModuleIf(conditional) => {
                DiagnosticSourceLine::authored(conditional.condition().source_line())
            }
            Parts::List(_)
            | Parts::Map(_)
            | Parts::Coalesce {
                value: _,
                default: _,
            }
            | Parts::Arithmetic(_)
            | Parts::Interpolation(_)
            | Parts::Evidence { value: _, edge: _ } => DiagnosticSourceLine::synthetic(),
        };
        self.argument_line = line;
        if let Some(line) = line.get() {
            self.policy.set_source_line(line);
        }
        let result = self.expression_parts(expression.children.parts, expression.children.at);
        self.policy.set_source_line(previous_line);
        self.argument_line = previous_argument_line;
        Step::new(result)
    }

    fn secret(
        &mut self,
        at: &SecretAt<'_>,
        child: SecretChild<'_, Transform, ParserPlaceholder, B>,
    ) -> SecretStep<Transform, (), ParserPlaceholder, B> {
        let step = self.value_scope(at.at(), |algebra| child.transform_bound(algebra));
        if step.step.result.is_err() {
            self.error = self.error.take().map(InsideSecret::inside_secret);
        }
        step
    }

    fn call(
        &mut self,
        at: &At<'_>,
        name: crate::builtins::BuiltinFunction,
        args: Vec<FunctionArgument<ParserPlaceholder, B>>,
        source_line: DiagnosticSourceLine,
    ) -> Result<Value<ParserPlaceholder, B>, WalkError> {
        if self.is_structural_only(at) {
            return Ok(Value::Deferred(DeferredValue::FunctionCall {
                name,
                args,
                source_line,
            }));
        }
        let result = self.policy.call(name, &args, source_line);
        match self.record(result)? {
            Some(value) => {
                self.replacements += 1;
                Ok(value)
            }
            None => Ok(Value::Deferred(DeferredValue::FunctionCall {
                name,
                args,
                source_line,
            })),
        }
    }
    fn resolved_call(
        &mut self,
        at: &At<'_>,
        function: Arc<crate::parser::UserFunction<B>>,
        functions: Arc<indexmap::IndexMap<String, crate::parser::UserFunction<B>>>,
        args: Vec<FunctionArgument<ParserPlaceholder, B>>,
        source_line: DiagnosticSourceLine,
    ) -> Result<Value<ParserPlaceholder, B>, WalkError> {
        if self.is_structural_only(at) {
            return Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function,
                functions,
                args,
                source_line,
            }));
        }
        let result = self
            .policy
            .resolved_call(&function, &functions, &args, source_line);
        match self.record(result)? {
            Some(value) => {
                self.replacements += 1;
                Ok(value)
            }
            None => Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function,
                functions,
                args,
                source_line,
            })),
        }
    }

    fn argument(
        &mut self,
        at: &At<'_>,
        argument: &FunctionArgument<ParserPlaceholder, B>,
    ) -> Result<Option<FunctionArgument<ParserPlaceholder, B>>, WalkError> {
        if self.is_structural_only(at) {
            return Ok(None);
        }
        let result = self.policy.argument(argument, self.argument_line);
        self.record(result)
    }
}

/// Apply the authored reference suffix once for both substitution policies.
/// Locations belong to this use site even when a supplied value has its own references.
fn substitute_reference_path<B: BindingAddress>(
    mut value: Value<ParserPlaceholder, B>,
    path: &crate::resource::AccessPath<B>,
    source_line: usize,
) -> Result<Value<ParserPlaceholder, B>, crate::resource::ValueAccessError> {
    for segment in std::iter::once(crate::resource::PathSegment::Field {
        name: path.attribute().to_owned(),
    })
    .chain(path.segments().iter().cloned())
    {
        value = crate::resource::apply_value_path_segment(
            value,
            segment,
            source_line,
            crate::resource::AccessMissPolicy::Error,
        )?
        .into_value()?;
    }
    Ok(match path.reference_locations() {
        Some(locations) => value.with_reference_locations(locations),
        None => value,
    })
}

#[cfg(test)]
mod argument_tests {
    use super::*;
    use crate::resource::{AuthoredValue, RelativeBindingName};

    struct ReplaceArgument;
    impl ParserRewrite<RelativeBindingName> for ReplaceArgument {
        type Error = ParseError;
        fn error(&mut self, error: ParseError) -> ParseError {
            error
        }
        fn source_line(&self) -> usize {
            1
        }
        fn argument(
            &mut self,
            _: &FunctionArgument<ParserPlaceholder, RelativeBindingName>,
            _: DiagnosticSourceLine,
        ) -> Result<Option<FunctionArgument<ParserPlaceholder, RelativeBindingName>>, ParseError>
        {
            Ok(Some(FunctionArgument::from_value(Value::Concrete(
                ConcreteValue::Int(2),
            ))))
        }
    }

    #[test]
    fn argument_replacement_enters_original_secret_before_completing() {
        let value = AuthoredValue::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::length,
            args: vec![FunctionArgument::from_value(
                AuthoredValue::Concrete(ConcreteValue::Int(1)).into_secret(),
            )],
            source_line: DiagnosticSourceLine::authored(1),
        });
        let at = WalkRoot::syntactic().at();
        let encountered = at.encountered_secret.clone();
        let mut policy = ReplaceArgument;
        let mut algebra = ParserTransform::new(&mut policy, RewritePosition::Value);
        assert!(engine::dispatch_bound(value, at, &mut algebra).is_ok());
        assert!(encountered.get());
    }
}
