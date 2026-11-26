//! One variant declaration dispatches borrowed and owned inputs into typed fields.
use super::*;
mod definitions;
use crate::{
    builtins::BuiltinFunction,
    integer_arithmetic::ValidatedIntegerExpr,
    parser::{DeferredIfCondition, DeferredValueIf},
    resource::{AccessExpr, DiagnosticSourceLine, UnavailableReason},
};
pub(crate) use definitions::DefinitionMemo;
use definitions::FunctionSource;

#[cfg(test)]
thread_local! {
    static FUNCTION_DEFINITION_INSPECT_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn with_function_definition_inspect_count<T>(run: impl FnOnce() -> T) -> (T, usize) {
    FUNCTION_DEFINITION_INSPECT_COUNT.with(|count| {
        count.set(0);
        let result = run();
        (result, count.replace(0))
    })
}

/// Scalar fields retain their variant in the type carrying their payload. No
/// cursor stores a separate tag plus a raw Value that needs projecting again.
pub(super) enum ScalarInput<'v, M: WalkMode> {
    String(M::Input<'v, crate::resource::ValueString>),
    EnumIdentifier(M::Input<'v, crate::resource::RawEnumIdentifier>),
    CanonicalEnum(M::Input<'v, crate::resource::CanonicalEnumValue>),
    Int(M::Input<'v, i64>),
    Float(M::Input<'v, crate::FiniteF64>),
    Bool(M::Input<'v, bool>),
    Duration(M::Input<'v, std::time::Duration>),
    StringList(M::Input<'v, Vec<crate::resource::ValueString>>),
}
impl<M: WalkMode> ScalarInput<'_, M> {
    pub(super) fn kind(&self) -> ScalarKind {
        match self {
            Self::String(_) => ScalarKind::String,
            Self::EnumIdentifier(_) => ScalarKind::EnumIdentifier,
            Self::CanonicalEnum(_) => ScalarKind::CanonicalEnum,
            Self::Int(_) => ScalarKind::Int,
            Self::Float(_) => ScalarKind::Float,
            Self::Bool(_) => ScalarKind::Bool,
            Self::Duration(_) => ScalarKind::Duration,
            Self::StringList(_) => ScalarKind::StringList,
        }
    }
}
impl ScalarInput<'_, Transform> {
    pub(super) fn into_value<P: ValueKind, B: BindingAddress>(self) -> Value<P, B> {
        Value::Concrete(match self {
            Self::String(value) => ConcreteValue::String(value),
            Self::EnumIdentifier(value) => ConcreteValue::EnumIdentifier(value),
            Self::CanonicalEnum(value) => ConcreteValue::CanonicalEnum(value),
            Self::Int(value) => ConcreteValue::Int(value),
            Self::Float(value) => ConcreteValue::Float(value),
            Self::Bool(value) => ConcreteValue::Bool(value),
            Self::Duration(value) => ConcreteValue::Duration(value),
            Self::StringList(value) => ConcreteValue::StringList(value),
        })
    }
}

pub(super) enum LeafInput<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress> {
    ResourceRef(M::Input<'v, crate::resource::AccessPath<B>>),
    BindingRef(M::Input<'v, B>),
    Unknown(M::Input<'v, crate::resource::ApplyTimeUnknown<B>>),
    Placeholder(M::Input<'v, P>),
}
impl<M: WalkMode, P: ValueKind, B: BindingAddress> LeafInput<'_, M, P, B> {
    pub(super) fn kind(&self) -> LeafKind {
        match self {
            Self::ResourceRef(_) => LeafKind::ResourceRef,
            Self::BindingRef(_) => LeafKind::BindingRef,
            Self::Unknown(_) => LeafKind::Unknown,
            Self::Placeholder(_) => LeafKind::Placeholder,
        }
    }
    pub(super) fn reference(&self) -> Option<ReferenceRef<'_, B>> {
        match self {
            Self::ResourceRef(path) => Some(ReferenceRef::Resource(M::borrow(path))),
            Self::BindingRef(binding) => Some(ReferenceRef::Binding(M::borrow(binding))),
            Self::Unknown(_) | Self::Placeholder(_) => None,
        }
    }
    pub(super) fn placeholder(&self) -> Option<&P> {
        match self {
            Self::Placeholder(slot) => Some(M::borrow(slot)),
            Self::ResourceRef(_) | Self::BindingRef(_) | Self::Unknown(_) => None,
        }
    }
}
impl<P: ValueKind, B: BindingAddress> LeafInput<'_, Transform, P, B> {
    pub(super) fn cloned_value(&self) -> Value<P, B> {
        Value::Deferred(match self {
            Self::ResourceRef(path) => DeferredValue::ResourceRef { path: path.clone() },
            Self::BindingRef(binding) => DeferredValue::BindingRef {
                binding: binding.clone(),
            },
            Self::Unknown(reason) => DeferredValue::Unknown(reason.clone()),
            Self::Placeholder(slot) => DeferredValue::Placeholder(slot.clone()),
        })
    }
    pub(super) fn into_value(self) -> Value<P, B> {
        Value::Deferred(match self {
            Self::ResourceRef(path) => DeferredValue::ResourceRef { path },
            Self::BindingRef(binding) => DeferredValue::BindingRef { binding },
            Self::Unknown(reason) => DeferredValue::Unknown(reason),
            Self::Placeholder(slot) => DeferredValue::Placeholder(slot),
        })
    }
}

/// The sealed mode chooses the call projection once, at dispatch. In particular,
/// Inspect cannot retain either definition Arc or any route into their values.
pub struct InspectCall<'v, P: ValueKind, B: BindingAddress> {
    args: &'v Vec<FunctionArgument<P, B>>,
}

pub struct TransformCall<P: ValueKind, B: BindingAddress> {
    pub(super) function: Arc<UserFunction<B>>,
    pub(super) functions: Arc<indexmap::IndexMap<String, UserFunction<B>>>,
    pub(super) args: Vec<FunctionArgument<P, B>>,
    pub(super) source_line: DiagnosticSourceLine,
}

pub trait CallProjection {
    fn child_len(&self) -> usize;
}
impl<P: ValueKind, B: BindingAddress> CallProjection for InspectCall<'_, P, B> {
    fn child_len(&self) -> usize {
        argument_values_len(self.args)
    }
}
impl<P: ValueKind, B: BindingAddress> CallProjection for TransformCall<P, B> {
    fn child_len(&self) -> usize {
        let definition_len = |function: &UserFunction<B>| {
            function
                .params
                .iter()
                .filter(|param| param.default.is_some())
                .count()
                + function.local_lets.len()
                + 1
        };
        definition_len(&self.function)
            + self.functions.values().map(definition_len).sum::<usize>()
            + argument_values_len(&self.args)
    }
}

/// Mode-specific fields, including fixed-arity expressions. Inspect instantiates
/// these fields with references and has no rebuilding method or closure.
/// Transform owns them and rebuilds directly from typed operation results.
pub(super) enum Parts<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress> {
    List(M::Input<'v, Vec<Value<P, B>>>),
    Map(M::Input<'v, indexmap::IndexMap<String, Value<P, B>>>),
    Coalesce {
        value: M::Input<'v, Box<Value<P, B>>>,
        default: M::Input<'v, Box<Value<P, B>>>,
    },
    ModuleIf(M::Input<'v, Box<DeferredValueIf<P, B>>>),
    Arithmetic(M::Input<'v, ValidatedIntegerExpr<P, B>>),
    Interpolation(M::Input<'v, Vec<InterpolationPart<P, B>>>),
    Call {
        name: M::Input<'v, BuiltinFunction>,
        args: M::Input<'v, Vec<FunctionArgument<P, B>>>,
        source_line: M::Input<'v, DiagnosticSourceLine>,
    },
    Unavailable {
        name: M::Input<'v, BuiltinFunction>,
        args: M::Input<'v, Vec<FunctionArgument<P, B>>>,
        reason: M::Input<'v, UnavailableReason>,
        source_line: M::Input<'v, DiagnosticSourceLine>,
    },
    ResolvedCall(M::ResolvedCall<'v, P, B>),
    Access(M::Input<'v, AccessExpr<P, B>>),
}
impl<M: WalkMode, P: ValueKind, B: BindingAddress> Parts<'_, M, P, B> {
    pub(super) fn len(&self) -> usize {
        match self {
            Self::List(values) => M::borrow(values).len(),
            Self::Map(values) => M::borrow(values).len(),
            Self::Coalesce { .. } | Self::Arithmetic(_) => 2,
            Self::ModuleIf(_) => 3,
            Self::Interpolation(parts) => M::borrow(parts)
                .iter()
                .filter(|part| matches!(part, InterpolationPart::Expr(_)))
                .count(),
            Self::Call { args, .. } | Self::Unavailable { args, .. } => {
                argument_values_len(M::borrow::<Vec<FunctionArgument<P, B>>>(args))
            }
            Self::ResolvedCall(call) => call.child_len(),
            Self::Access(_) => 1,
        }
    }
}

/// The macro contains the sole exhaustive Value/ConcreteValue/DeferredValue
/// variant list. Rust instantiates it at each input type (&Value / Value).
/// Each arm destructures once; downstream operations receive typed fields.
macro_rules! dispatch_variants {
    ($mode:ty, $p:ty, $b:ty, $input:expr, $at:ident, $algebra:ident) => {{
        macro_rules! scalar {
            ($kind:ident, $value:expr) => {
                $algebra
                    .scalar(
                        &$at,
                        ScopedScalar::<$mode, $p, $b> {
                            input: ScalarInput::<$mode>::$kind($value),
                            mode: PhantomData,
                        },
                    )
                    .result
            };
        }
        macro_rules! leaf {
            ($kind:ident, $value:expr) => {
                $algebra
                    .placeholder(
                        &$at,
                        ScopedLeaf {
                            input: LeafInput::<$mode, $p, $b>::$kind($value),
                        },
                    )
                    .result
            };
        }
        macro_rules! expr {
            ($kind:ident, $parts:expr) => {
                $algebra
                    .expression(
                        &$at,
                        expression::<$mode, $p, $b>($parts, ExpressionKind::$kind, $at.clone()),
                    )
                    .result
            };
        }
        match $input {
            Value::Concrete(ConcreteValue::String(value)) => scalar!(String, value),
            Value::Concrete(ConcreteValue::EnumIdentifier(value)) => scalar!(EnumIdentifier, value),
            Value::Concrete(ConcreteValue::CanonicalEnum(value)) => scalar!(CanonicalEnum, value),
            Value::Concrete(ConcreteValue::Int(value)) => scalar!(Int, value),
            Value::Concrete(ConcreteValue::Float(value)) => scalar!(Float, value),
            Value::Concrete(ConcreteValue::Bool(value)) => scalar!(Bool, value),
            Value::Concrete(ConcreteValue::Duration(value)) => scalar!(Duration, value),
            Value::Concrete(ConcreteValue::StringList(value)) => scalar!(StringList, value),
            Value::Concrete(ConcreteValue::List(values)) => {
                $algebra
                    .list(
                        &$at,
                        children::<$mode, $p, $b>(Parts::List(values), $at.clone()),
                    )
                    .result
            }
            Value::Concrete(ConcreteValue::Map(values)) => {
                $algebra
                    .map(
                        &$at,
                        children::<$mode, $p, $b>(Parts::Map(values), $at.clone()),
                    )
                    .result
            }
            Value::Deferred(DeferredValue::ResourceRef { path }) => leaf!(ResourceRef, path),
            Value::Deferred(DeferredValue::BindingRef { binding }) => leaf!(BindingRef, binding),
            Value::Deferred(DeferredValue::Unknown(reason)) => leaf!(Unknown, reason),
            Value::Deferred(DeferredValue::Placeholder(slot)) => leaf!(Placeholder, slot),
            Value::Deferred(DeferredValue::Secret(payload)) => {
                let protected = $at.protected();
                $algebra
                    .secret(
                        &SecretAt {
                            at: protected.clone(),
                        },
                        SecretChild::<$mode, $p, $b> {
                            payload,
                            at: protected.secret_child(),
                        },
                    )
                    .step
                    .result
            }
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                expr!(Coalesce, Parts::Coalesce { value, default })
            }
            Value::Deferred(DeferredValue::ModuleIf(conditional)) => {
                expr!(ModuleIf, Parts::ModuleIf(conditional))
            }
            Value::Deferred(DeferredValue::IntegerArithmetic(arithmetic)) => {
                expr!(IntegerArithmetic, Parts::Arithmetic(arithmetic))
            }
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                expr!(Interpolation, Parts::Interpolation(parts))
            }
            Value::Deferred(DeferredValue::FunctionCall {
                name,
                args,
                source_line,
            }) => expr!(
                FunctionCall,
                Parts::Call {
                    name,
                    args,
                    source_line
                }
            ),
            Value::Deferred(DeferredValue::Unavailable {
                name,
                args,
                reason,
                source_line,
            }) => expr!(
                Unavailable,
                Parts::Unavailable {
                    name,
                    args,
                    reason,
                    source_line
                }
            ),
            Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function,
                functions,
                args,
                source_line,
            }) => expr!(
                ResolvedFunctionCall,
                Parts::ResolvedCall(<$mode as sealed::Mode>::resolved_call(
                    function,
                    functions,
                    args,
                    source_line,
                ))
            ),
            Value::Deferred(DeferredValue::Access(access)) => expr!(Access, Parts::Access(access)),
            Value::Reference(never) => {
                match *<$p as ValueKind>::no_reference_payload::<$b>(&never) {}
            }
            Value::Conversion(never) => {
                match *<$p as ValueKind>::no_conversion_payload::<$b>(&never) {}
            }
        }
    }};
}

impl sealed::Mode for Inspect {
    type Input<'v, T: 'v> = &'v T;
    type ResolvedCall<'v, P: ValueKind + 'v, B: BindingAddress> = InspectCall<'v, P, B>;
    fn resolved_call<'v, P: ValueKind + 'v, B: BindingAddress>(
        _function: &'v Arc<UserFunction<B>>,
        _functions: &'v Arc<indexmap::IndexMap<String, UserFunction<B>>>,
        args: &'v Vec<FunctionArgument<P, B>>,
        _source_line: &'v DiagnosticSourceLine,
    ) -> InspectCall<'v, P, B> {
        InspectCall { args }
    }
}
impl sealed::Mode for Transform {
    type Input<'v, T: 'v> = T;
    type ResolvedCall<'v, P: ValueKind + 'v, B: BindingAddress> = TransformCall<P, B>;
    fn resolved_call<'v, P: ValueKind + 'v, B: BindingAddress>(
        function: Arc<UserFunction<B>>,
        functions: Arc<indexmap::IndexMap<String, UserFunction<B>>>,
        args: Vec<FunctionArgument<P, B>>,
        source_line: DiagnosticSourceLine,
    ) -> TransformCall<P, B> {
        TransformCall {
            function,
            functions,
            args,
            source_line,
        }
    }
}
pub(super) fn dispatch<'v, P: ValueKind + 'v, B: BindingAddress, A: Algebra<Inspect>>(
    input: &'v Value<P, B>,
    at: At<'v>,
    algebra: &mut A,
) -> Result<A::Output, WalkError> {
    #[cfg(test)]
    if at.expression_edges().iter().any(|edge| {
        matches!(
            edge,
            ExpressionEdge::FunctionDefinition
                | ExpressionEdge::FunctionScope(_)
                | ExpressionEdge::FunctionBody
                | ExpressionEdge::FunctionDefault(_)
                | ExpressionEdge::FunctionLocal(_)
        )
    }) {
        FUNCTION_DEFINITION_INSPECT_COUNT.with(|count| count.set(count.get() + 1));
    }
    dispatch_variants!(Inspect, P, B, input, at, algebra)
}

pub(super) fn dispatch_bound<'v, P: ValueKind + 'v, B: BindingAddress, A: BoundTransform<P, B>>(
    input: Value<P, B>,
    at: At<'v>,
    algebra: &mut A,
) -> Result<RewriteProduct<A::Output, P, B>, WalkError> {
    dispatch_variants!(Transform, P, B, input, at, algebra)
}
fn children<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress>(
    parts: Parts<'v, M, P, B>,
    at: At<'v>,
) -> Children<'v, M, P, B> {
    Children { parts, at }
}
fn expression<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress>(
    parts: Parts<'v, M, P, B>,
    kind: ExpressionKind,
    at: At<'v>,
) -> ExprChildren<'v, M, P, B> {
    ExprChildren {
        children: children(parts, at.clone()),
        kind,
        at,
    }
}
fn map_entries<K: AsRef<str>, T>(
    entries: impl IntoIterator<Item = (K, T)>,
) -> impl Iterator<Item = (usize, K, T)> {
    let mut entries: Vec<_> = entries
        .into_iter()
        .enumerate()
        .map(|(order, (key, value))| (order, 0, key, value))
        .collect();
    entries.sort_by(|left, right| left.2.as_ref().cmp(right.2.as_ref()));
    for (rank, entry) in entries.iter_mut().enumerate() {
        entry.1 = rank;
    }
    entries.sort_by_key(|entry| entry.0);
    entries
        .into_iter()
        .map(|(_, rank, key, value)| (rank, key, value))
}

fn inspect_value<'v, P: ValueKind, B: BindingAddress, A: Algebra<Inspect>>(
    value: &'v Value<P, B>,
    at: At<'v>,
    algebra: &mut A,
    output: &mut Vec<A::Output>,
) -> Result<(), WalkError> {
    output.push(dispatch(value, at, algebra)?);
    Ok(())
}
fn inspect_arguments<'v, P: ValueKind, B: BindingAddress, A: Algebra<Inspect>>(
    args: &'v [FunctionArgument<P, B>],
    at: &At<'v>,
    algebra: &mut A,
    output: &mut Vec<A::Output>,
) -> Result<(), WalkError> {
    for (index, argument) in args.iter().enumerate() {
        let at = at.operand(ExpressionEdge::Argument(index));
        let mut value_index = 0;
        argument.try_visit_values(&mut |value| {
            inspect_value(
                value,
                at.operand(ExpressionEdge::ArgumentValue(value_index)),
                algebra,
                output,
            )?;
            value_index += 1;
            Ok(())
        })?;
    }
    Ok(())
}
fn argument_values_len<P: ValueKind, B: BindingAddress>(args: &[FunctionArgument<P, B>]) -> usize {
    let mut count = 0;
    for argument in args {
        match argument.try_visit_values(&mut |_| {
            count += 1;
            Ok::<_, std::convert::Infallible>(())
        }) {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }
    count
}
impl<'v, P: ValueKind, B: BindingAddress> Parts<'v, Inspect, P, B> {
    pub(super) fn inspect<A: Algebra<Inspect>>(
        self,
        at: At<'v>,
        algebra: &mut A,
    ) -> Result<Vec<A::Output>, WalkError> {
        let mut output = Vec::new();
        match self {
            Self::List(values) => {
                for (index, value) in values.iter().enumerate() {
                    inspect_value(value, at.list_child(index), algebra, &mut output)?;
                }
            }
            Self::Map(values) => {
                for (rank, key, value) in
                    map_entries(values.iter().map(|(key, value)| (key.as_str(), value)))
                {
                    inspect_value(value, at.map_child(key, rank)?, algebra, &mut output)?;
                }
            }
            Self::Coalesce { value, default } => {
                inspect_value(
                    value,
                    at.operand(ExpressionEdge::CoalesceValue),
                    algebra,
                    &mut output,
                )?;
                inspect_value(
                    default,
                    at.operand(ExpressionEdge::CoalesceDefault),
                    algebra,
                    &mut output,
                )?;
            }
            Self::ModuleIf(conditional) => {
                inspect_value(
                    conditional.condition().value(),
                    at.operand(ExpressionEdge::Condition),
                    algebra,
                    &mut output,
                )?;
                inspect_value(
                    conditional.then_value(),
                    at.operand(ExpressionEdge::Then),
                    algebra,
                    &mut output,
                )?;
                inspect_value(
                    conditional.else_value(),
                    at.operand(ExpressionEdge::Else),
                    algebra,
                    &mut output,
                )?;
            }
            Self::Arithmetic(expression) => {
                inspect_value(
                    expression.lhs(),
                    at.operand(ExpressionEdge::ArithmeticLeft),
                    algebra,
                    &mut output,
                )?;
                inspect_value(
                    expression.rhs(),
                    at.operand(ExpressionEdge::ArithmeticRight),
                    algebra,
                    &mut output,
                )?;
            }
            Self::Interpolation(parts) => {
                for (index, part) in parts.iter().enumerate() {
                    match part {
                        InterpolationPart::Literal(_) => {}
                        InterpolationPart::Expr(value) => inspect_value(
                            value,
                            at.operand(ExpressionEdge::Interpolation(index)),
                            algebra,
                            &mut output,
                        )?,
                    }
                }
            }
            Self::Call {
                args,
                name: _,
                source_line: _,
            }
            | Self::Unavailable {
                args,
                name: _,
                reason: _,
                source_line: _,
            } => {
                inspect_arguments(args, &at, algebra, &mut output)?;
            }
            Self::ResolvedCall(InspectCall { args }) => {
                inspect_arguments(args, &at, algebra, &mut output)?;
            }
            Self::Access(access) => inspect_value(
                access.base(),
                at.operand(ExpressionEdge::AccessBase),
                algebra,
                &mut output,
            )?,
        }
        Ok(output)
    }
}

/// Each operation has a concrete return type: Value -> Value, argument ->
/// FunctionArgument, function -> UserFunction. Only summaries share a vector.
struct TransformFold<O: WalkOutput> {
    output: Vec<O>,
    changed: bool,
}
impl<O: WalkOutput> TransformFold<O> {
    fn new() -> Self {
        Self {
            output: Vec::new(),
            changed: false,
        }
    }
    fn value<
        P: ValueKind,
        B: BindingAddress,
        A: BoundTransform<P, B> + TransformOutput<Output = O>,
    >(
        &mut self,
        value: Value<P, B>,
        at: At<'_>,
        algebra: &mut A,
    ) -> Result<Value<P, B>, WalkError> {
        let product = dispatch_bound(value, at, algebra)?;
        self.output.push(product.output);
        self.changed |= product.changed;
        Ok(product.value)
    }
    fn argument<
        P: ValueKind,
        B: BindingAddress,
        A: BoundTransform<P, B> + TransformOutput<Output = O>,
    >(
        &mut self,
        argument: FunctionArgument<P, B>,
        at: At<'_>,
        algebra: &mut A,
    ) -> Result<FunctionArgument<P, B>, WalkError> {
        if let Some(replacement) = algebra.argument(&at, &argument)? {
            self.changed = true;
            return Ok(replacement);
        }
        let mut nested = Self::new();
        let mut index = 0;
        let argument = argument.try_map_values(|value| {
            let value = nested.value(
                value,
                at.operand(ExpressionEdge::ArgumentValue(index)),
                algebra,
            )?;
            index += 1;
            Ok::<_, WalkError>(value)
        })?;
        self.output.extend(nested.output);
        self.changed |= nested.changed;
        Ok(argument)
    }
    fn arguments<
        P: ValueKind,
        B: BindingAddress,
        A: BoundTransform<P, B> + TransformOutput<Output = O>,
    >(
        &mut self,
        args: Vec<FunctionArgument<P, B>>,
        at: &At<'_>,
        algebra: &mut A,
    ) -> Result<Vec<FunctionArgument<P, B>>, WalkError> {
        args.into_iter()
            .enumerate()
            .map(|(index, argument)| {
                self.argument(
                    argument,
                    at.operand(ExpressionEdge::Argument(index)),
                    algebra,
                )
            })
            .collect()
    }
}
/// Transform one typed argument through the same fold used by call operands.
/// A whole-argument replacement returns directly, as with leaf replacement.
pub(super) fn transform_argument_bound<
    P: ValueKind,
    B: BindingAddress,
    A: BoundTransform<P, B> + BoundTransform<crate::resource::ParserPlaceholder, B>,
>(
    argument: FunctionArgument<P, B>,
    at: At<'_>,
    algebra: &mut A,
) -> Result<FunctionArgument<P, B>, WalkError> {
    TransformFold::<A::Output>::new().argument(argument, at, algebra)
}

impl<'v, P: ValueKind, B: BindingAddress> Parts<'v, Transform, P, B> {
    pub(super) fn transform<
        A: BoundTransform<P, B> + BoundTransform<crate::resource::ParserPlaceholder, B>,
    >(
        self,
        at: At<'v>,
        algebra: &mut A,
        fold: impl FnOnce(Vec<A::Output>) -> A::Output,
    ) -> Result<RewriteProduct<A::Output, P, B>, WalkError> {
        let mut walked = TransformFold::new();
        let value = match self {
            Self::List(values) => Value::Concrete(ConcreteValue::List(
                values
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| walked.value(value, at.list_child(index), algebra))
                    .collect::<Result<_, _>>()?,
            )),
            Self::Map(values) => Value::Concrete(ConcreteValue::Map(
                map_entries(values)
                    .map(|(rank, key, value)| {
                        walked
                            .value(value, at.map_child(&key, rank)?, algebra)
                            .map(|value| (key, value))
                    })
                    .collect::<Result<_, _>>()?,
            )),
            Self::Coalesce { value, default } => {
                let value =
                    walked.value(*value, at.operand(ExpressionEdge::CoalesceValue), algebra)?;
                let default = walked.value(
                    *default,
                    at.operand(ExpressionEdge::CoalesceDefault),
                    algebra,
                )?;
                Value::Deferred(DeferredValue::Coalesce {
                    value: Box::new(value),
                    default: Box::new(default),
                })
            }
            Self::ModuleIf(conditional) => {
                let DeferredValueIf {
                    condition,
                    then_value,
                    else_value,
                } = *conditional;
                let DeferredIfCondition {
                    value,
                    source_file,
                    source_line,
                    source_expression,
                    node_id,
                } = condition;
                let value = walked.value(value, at.operand(ExpressionEdge::Condition), algebra)?;
                let then_value =
                    walked.value(then_value, at.operand(ExpressionEdge::Then), algebra)?;
                let else_value =
                    walked.value(else_value, at.operand(ExpressionEdge::Else), algebra)?;
                Value::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
                    DeferredIfCondition::new_with_source_expression(
                        value,
                        source_file,
                        source_line,
                        source_expression,
                    )
                    .with_node_id(node_id),
                    then_value,
                    else_value,
                ))))
            }
            Self::Arithmetic(expression) => {
                let (lhs, op, rhs, span) = expression.into_parts();
                let lhs = walked.value(lhs, at.operand(ExpressionEdge::ArithmeticLeft), algebra)?;
                let rhs =
                    walked.value(rhs, at.operand(ExpressionEdge::ArithmeticRight), algebra)?;
                Value::Deferred(DeferredValue::IntegerArithmetic(
                    ValidatedIntegerExpr::from_post_validation(lhs, op, rhs, span)
                        .map_err(|_| WalkError::ArithmeticOperand)?,
                ))
            }
            Self::Interpolation(parts) => {
                let parts = parts
                    .into_iter()
                    .enumerate()
                    .map(|(index, part)| {
                        Ok(match part {
                            InterpolationPart::Literal(literal) => {
                                InterpolationPart::Literal(literal)
                            }
                            InterpolationPart::Expr(value) => {
                                InterpolationPart::Expr(walked.value(
                                    value,
                                    at.operand(ExpressionEdge::Interpolation(index)),
                                    algebra,
                                )?)
                            }
                        })
                    })
                    .collect::<Result<_, WalkError>>()?;
                Value::Deferred(DeferredValue::Interpolation(parts))
            }
            Self::Call {
                name,
                args,
                source_line,
            } => {
                let args = walked.arguments(args, &at, algebra)?;
                algebra.call(&at, name, args, source_line)?
            }
            Self::Unavailable {
                name,
                args,
                reason,
                source_line,
            } => Value::Deferred(DeferredValue::Unavailable {
                name,
                args: walked.arguments(args, &at, algebra)?,
                reason,
                source_line,
            }),
            Self::ResolvedCall(TransformCall {
                function,
                functions,
                args,
                source_line,
            }) => {
                let candidate = walked.function_candidate(
                    FunctionSource::Definition(function.clone()),
                    &at,
                    algebra,
                )?;
                let function = match candidate {
                    Some(candidate) => Arc::new(candidate),
                    None => function,
                };
                // Scope entries are separate stored definitions. Visit each
                // exactly once; allocate a replacement map only after a change.
                let mut scope: Vec<_> = functions.iter().collect();
                scope.sort_by(|left, right| left.0.cmp(right.0));
                let mut updates = Vec::new();
                for (index, (name, _function)) in scope.into_iter().enumerate() {
                    if let Some(candidate) = walked.function_candidate(
                        FunctionSource::Scope {
                            functions: functions.clone(),
                            name: name.clone(),
                            index,
                        },
                        &at,
                        algebra,
                    )? {
                        updates.push((name.clone(), candidate));
                    }
                }
                let functions = if updates.is_empty() {
                    functions
                } else {
                    let mut changed = Arc::unwrap_or_clone(functions);
                    changed.extend(updates);
                    Arc::new(changed)
                };
                let args = walked.arguments(args, &at, algebra)?;
                algebra.resolved_call(&at, function, functions, args, source_line)?
            }
            Self::Access(access) => {
                // Keep the shell without relying on Value's nonreflexive Eq.
                let mut base_walk = TransformFold::new();
                let access = access.try_map_base(|base| {
                    base_walk.value(base, at.operand(ExpressionEdge::AccessBase), algebra)
                })?;
                let result = if !base_walk.changed {
                    Value::Deferred(DeferredValue::Access(access))
                } else {
                    // Evaluable stages delegate to AccessExpr::rebuild_result.
                    // Only an unsuccessful canonical rebuild takes the same
                    // dormant fallback used by parser substitution.
                    match P::rebuild_access(&access, access.base().clone()) {
                        Ok(value) => value,
                        Err(_) => Value::Deferred(DeferredValue::Access(access))
                            .retain_deferred_miss_display_root(),
                    }
                };
                walked.output.extend(base_walk.output);
                walked.changed |= base_walk.changed;
                result
            }
        };
        Ok(RewriteProduct {
            value,
            output: fold(walked.output),
            changed: walked.changed,
        })
    }
}
