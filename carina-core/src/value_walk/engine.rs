//! One variant declaration dispatches borrowed and owned inputs into typed fields.
use super::*;
use crate::{
    builtins::BuiltinFunction,
    integer_arithmetic::ValidatedIntegerExpr,
    parser::{DeferredIfCondition, DeferredValueIf},
    resource::{AccessExpr, DiagnosticSourceLine, UnavailableReason},
};

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
    ResolvedCall {
        function: M::Input<'v, Arc<UserFunction<B>>>,
        functions: M::Input<'v, Arc<HashMap<String, UserFunction<B>>>>,
        args: M::Input<'v, Vec<FunctionArgument<P, B>>>,
        source_line: M::Input<'v, DiagnosticSourceLine>,
    },
    Access(M::Input<'v, AccessExpr<P, B>>),
    Evidence {
        value: M::Input<'v, Value<P, B>>,
        edge: ExpressionEdge,
    },
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
            Self::ResolvedCall {
                function,
                functions,
                args,
                ..
            } => {
                function_values(M::borrow::<Arc<UserFunction<B>>>(function)).count()
                    + M::borrow(functions)
                        .values()
                        .map(|function| function_values(function).count())
                        .sum::<usize>()
                    + argument_values_len(M::borrow::<Vec<FunctionArgument<P, B>>>(args))
            }
            Self::Access(_) | Self::Evidence { .. } => 1,
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
                Parts::ResolvedCall {
                    function,
                    functions,
                    args,
                    source_line
                }
            ),
            Value::Deferred(DeferredValue::Access(access)) => expr!(Access, Parts::Access(access)),
        }
    }};
}

impl sealed::Mode for Inspect {
    type Input<'v, T: 'v> = &'v T;
    fn dispatch<'v, P: ValueKind + 'v, B: BindingAddress, A: Algebra<Self>>(
        input: Self::Input<'v, Value<P, B>>,
        at: At<'v>,
        algebra: &mut A,
    ) -> Result<<Self as WalkMode>::Product<A::Output, P, B>, WalkError> {
        dispatch_variants!(Inspect, P, B, input, at, algebra)
    }
}
impl sealed::Mode for Transform {
    type Input<'v, T: 'v> = T;
    fn dispatch<'v, P: ValueKind + 'v, B: BindingAddress, A: Algebra<Self>>(
        input: Self::Input<'v, Value<P, B>>,
        at: At<'v>,
        algebra: &mut A,
    ) -> Result<<Self as WalkMode>::Product<A::Output, P, B>, WalkError> {
        dispatch_variants!(Transform, P, B, input, at, algebra)
    }
}
pub(super) fn dispatch<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress, A: Algebra<M>>(
    input: M::Input<'v, Value<P, B>>,
    at: At<'v>,
    algebra: &mut A,
) -> Result<M::Product<A::Output, P, B>, WalkError> {
    M::dispatch(input, at, algebra)
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
pub(super) fn expression_edge<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress>(
    value: M::Input<'v, Value<P, B>>,
    at: At<'v>,
    edge: ExpressionEdge,
) -> ExprChildren<'v, M, P, B> {
    expression(
        Parts::Evidence { value, edge },
        ExpressionKind::Evidence,
        at,
    )
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
    output.push(dispatch::<Inspect, _, _, _>(value, at, algebra)?);
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
fn function_values<B: BindingAddress>(
    function: &UserFunction<B>,
) -> impl Iterator<
    Item = (
        ExpressionEdge,
        &Value<crate::resource::ParserPlaceholder, B>,
    ),
> {
    function
        .params
        .iter()
        .enumerate()
        .filter_map(|(index, param)| {
            param
                .default
                .as_ref()
                .map(|value| (ExpressionEdge::FunctionDefault(index), value))
        })
        .chain(
            function
                .local_lets
                .iter()
                .enumerate()
                .map(|(index, (_, value, _))| (ExpressionEdge::FunctionLocal(index), value)),
        )
        .chain(std::iter::once((
            ExpressionEdge::FunctionBody,
            &function.body.value,
        )))
}
fn inspect_function<'v, B: BindingAddress, A: Algebra<Inspect>>(
    function: &'v UserFunction<B>,
    at: At<'v>,
    algebra: &mut A,
    output: &mut Vec<A::Output>,
) -> Result<(), WalkError> {
    for (edge, value) in function_values(function) {
        inspect_value(value, at.operand(edge), algebra, output)?;
    }
    Ok(())
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
            Self::ResolvedCall {
                function,
                functions,
                args,
                source_line: _,
            } => {
                inspect_function(
                    function,
                    at.operand(ExpressionEdge::FunctionDefinition),
                    algebra,
                    &mut output,
                )?;
                let mut scope: Vec<_> = functions.iter().collect();
                scope.sort_by(|left, right| left.0.cmp(right.0));
                for (index, (_name, function)) in scope.into_iter().enumerate() {
                    inspect_function(
                        function,
                        at.operand(ExpressionEdge::FunctionScope(index)),
                        algebra,
                        &mut output,
                    )?;
                }
                inspect_arguments(args, &at, algebra, &mut output)?;
            }
            Self::Access(access) => inspect_value(
                access.base(),
                at.operand(ExpressionEdge::AccessBase),
                algebra,
                &mut output,
            )?,
            Self::Evidence { value, edge } => {
                inspect_value(value, at.operand(edge), algebra, &mut output)?
            }
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
            // Replacement must not bypass Secret in an auxiliary access base
            // or in the original argument. Inventory it before completing the
            // replacement; inserted values follow leaf-replacement semantics.
            let mut inspector = StructuralInspector {
                locator: None,
                found: None,
            };
            let mut index = 0;
            argument.try_visit_values(&mut |value| {
                dispatch::<Inspect, _, _, _>(
                    value,
                    at.operand(ExpressionEdge::ArgumentValue(index)),
                    &mut inspector,
                )?;
                index += 1;
                Ok::<_, WalkError>(())
            })?;
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
    fn function<
        B: BindingAddress,
        A: BoundTransform<crate::resource::ParserPlaceholder, B> + TransformOutput<Output = O>,
    >(
        &mut self,
        mut function: UserFunction<B>,
        at: At<'_>,
        algebra: &mut A,
    ) -> Result<UserFunction<B>, WalkError> {
        for (index, param) in function.params.iter_mut().enumerate() {
            if let Some(value) = param.default.take() {
                param.default = Some(self.value(
                    value,
                    at.operand(ExpressionEdge::FunctionDefault(index)),
                    algebra,
                )?);
            }
        }
        function.local_lets = function
            .local_lets
            .into_iter()
            .enumerate()
            .map(|(index, (name, value, line))| {
                self.value(
                    value,
                    at.operand(ExpressionEdge::FunctionLocal(index)),
                    algebra,
                )
                .map(|value| (name, value, line))
            })
            .collect::<Result<_, _>>()?;
        function.body.value = self.value(
            function.body.value,
            at.operand(ExpressionEdge::FunctionBody),
            algebra,
        )?;
        Ok(function)
    }
    fn function_candidate<
        B: BindingAddress,
        A: BoundTransform<crate::resource::ParserPlaceholder, B> + TransformOutput<Output = O>,
    >(
        &mut self,
        function: &UserFunction<B>,
        at: At<'_>,
        algebra: &mut A,
    ) -> Result<Option<UserFunction<B>>, WalkError> {
        let mut nested = Self::new();
        let candidate = nested.function(function.clone(), at, algebra)?;
        self.output.extend(nested.output);
        self.changed |= nested.changed;
        Ok(nested.changed.then_some(candidate))
    }
}
/// Transform one typed argument through the same fold used by call operands.
/// Whole-argument replacement still inventories every original Secret operand.
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
                    ),
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
            Self::ResolvedCall {
                function,
                functions,
                args,
                source_line,
            } => {
                let candidate = walked.function_candidate(
                    &function,
                    at.operand(ExpressionEdge::FunctionDefinition),
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
                for (index, (name, function)) in scope.into_iter().enumerate() {
                    if let Some(candidate) = walked.function_candidate(
                        function,
                        at.operand(ExpressionEdge::FunctionScope(index)),
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
            Self::Evidence { value, edge } => walked.value(value, at.operand(edge), algebra)?,
        };
        Ok(RewriteProduct {
            value,
            output: fold(walked.output),
            changed: walked.changed,
        })
    }
}

/// Locating a saved protected position is another algebra over the one fold,
/// never a second representation walker. No payload leaves this resolver.
struct StructuralInspector<'a> {
    locator: Option<&'a ProtectedLocator>,
    found: Option<Scoped<()>>,
}
impl StructuralInspector<'_> {
    fn record(&mut self, at: &At<'_>) {
        let Some(ProtectedLocator(_, anchor, node)) = self.locator else {
            return;
        };
        if at.is_secret() && at.secret_anchor.as_ref() == Some(anchor) && &at.position == node {
            self.found = Some(Scoped::new(at, ()));
        }
    }
}
impl Algebra<Inspect> for StructuralInspector<'_> {
    type Output = ();
    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        value: ScopedScalar<'_, Inspect, P, B>,
    ) -> Step<Inspect, (), P, B> {
        self.record(at);
        value.finish(())
    }
    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        children: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, (), P, B> {
        self.record(at);
        children.inspect(self, |_| ())
    }
    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        children: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, (), P, B> {
        self.record(at);
        children.inspect(self, |_| ())
    }
    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        expression: ExprChildren<'_, Inspect, P, B>,
    ) -> Step<Inspect, (), P, B> {
        self.record(at);
        expression.inspect(self, |_| ())
    }
    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        leaf: ScopedLeaf<'_, Inspect, P, B>,
    ) -> Step<Inspect, (), P, B> {
        self.record(at);
        leaf.finish(())
    }
    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &SecretAt<'_>,
        child: SecretChild<'_, Inspect, P, B>,
    ) -> SecretStep<Inspect, (), P, B> {
        self.record(at.at());
        child.inspect(self)
    }
}
pub(super) fn resolve_protected<P: ValueKind, B: BindingAddress>(
    locator: &ProtectedLocator,
    root: WalkRoot<'_>,
    value: &Value<P, B>,
) -> Result<Scoped<()>, WalkError> {
    let mut resolver = StructuralInspector {
        locator: Some(locator),
        found: None,
    };
    inspect(root, value, &mut resolver)?;
    resolver.found.ok_or(WalkError::ProtectedLocator)
}
