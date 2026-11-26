//! Structural value traversal with mandatory Secret re-entry.
//!
//! The kernel works over the existing public representation. Parser/module
//! substitutions and unavailable searches use it; the remaining public resource
//! enums and payload callbacks still permit independent walkers elsewhere.
//!
//! Inspect borrows operands only; internal Transform consumes the input and
//! retains function definitions. Cursors never expose raw child values.
//! Secret has its own required algebra arm and a separately minted step.

#![doc = include_str!("contracts.md")]

#[cfg(test)]
use std::cell::Cell;
use std::{collections::HashMap, marker::PhantomData, rc::Rc, sync::Arc};

use crate::{
    parser::UserFunction,
    resource::{
        AbsoluteBindingAddress, BindingAddress, ConcreteValue, DeferredValue, FunctionArgument,
        InterpolationPart, NoPlaceholder, SecretPayload, Value, ValueKind,
    },
    schema::{
        EffectiveType, LeafPath, LeafPathSegment, RootlessType, Shape, ValidatedConfigSchema,
        ValidatedResourceSchema,
    },
};

mod engine;
pub(crate) use engine::DefinitionMemo;
#[cfg(test)]
pub(crate) use engine::with_function_definition_inspect_count;
use engine::{LeafInput, Parts, ScalarInput, dispatch};
mod bound_transform;
use bound_transform::{BoundTransform, TransformOutput};

mod substitution;
#[cfg(test)]
pub(crate) use substitution::select_authored_module_ifs;
#[cfg(test)]
use substitution::{ParserRewrite, rewrite_parser_value};
pub(crate) use substitution::{
    RewritePosition, evaluate_expanded_module_functions,
    select_expanded_module_ifs_with_conditions, substitute_function_call_argument,
    substitute_function_parameters, substitute_module_arguments, value_argument,
};
mod unavailable;
pub(crate) use unavailable::{
    UnavailableSearchPolicy, contains_unavailable, find_required_unavailable,
};

mod sealed {
    pub trait Mode: Sized {
        type Input<'v, T: 'v>: std::borrow::Borrow<T>;
        type ResolvedCall<'v, P: super::ValueKind + 'v, B: super::BindingAddress>:
            super::engine::CallProjection;
        fn borrow<'s, 'v, T: 'v>(input: &'s Self::Input<'v, T>) -> &'s T {
            std::borrow::Borrow::borrow(input)
        }
        fn resolved_call<'v, P: super::ValueKind + 'v, B: super::BindingAddress>(
            function: Self::Input<'v, super::Arc<super::UserFunction<B>>>,
            functions: Self::Input<
                'v,
                super::Arc<indexmap::IndexMap<String, super::UserFunction<B>>>,
            >,
            args: Self::Input<'v, Vec<super::FunctionArgument<P, B>>>,
            source_line: Self::Input<'v, crate::resource::DiagnosticSourceLine>,
        ) -> Self::ResolvedCall<'v, P, B>;
    }
    pub trait Output {}
    impl Output for () {}
    impl Output for bool {}
    impl Output for usize {}
    impl Output for i64 {}
    impl<T: Output> Output for Option<T> {}
    impl<T: Output> Output for Vec<T> {}
    impl<O: super::WalkOutput, R: super::WalkOutput> Output for super::Evidence<O, R> {}
}

/// A borrowed fold or visit.
pub struct Inspect;
/// An owned rewrite. Re-entry under Secret restores the wrapper automatically.
pub struct Transform;

/// The modes are closed so a consumer cannot redefine Secret completion.
pub trait WalkMode: sealed::Mode {
    #[doc(hidden)]
    type Product<O: WalkOutput, P: ValueKind, B: BindingAddress>;
}
impl WalkMode for Inspect {
    type Product<O: WalkOutput, P: ValueKind, B: BindingAddress> = O;
}
impl WalkMode for Transform {
    type Product<O: WalkOutput, P: ValueKind, B: BindingAddress> = RewriteProduct<O, P, B>;
}

/// Approved summaries. Plaintext strings, values and arbitrary errors cannot
/// be returned through the inspection result channel.
pub trait WalkOutput: sealed::Output {}
impl WalkOutput for () {}
impl WalkOutput for bool {}
impl WalkOutput for usize {}
impl WalkOutput for i64 {}
impl<T: WalkOutput> WalkOutput for Option<T> {}
impl<T: WalkOutput> WalkOutput for Vec<T> {}
impl<O: WalkOutput, R: WalkOutput> WalkOutput for Evidence<O, R> {}

#[derive(Clone, Copy)]
enum FieldNames {
    Canonical,
    Authored,
}

fn find_field_by_canonical_name<'g>(
    ty: &EffectiveType<'g>,
    key: &str,
    budget: &mut crate::schema::ShapeWalkBudget,
) -> Option<crate::schema::EffectiveField<'g>> {
    ty.field_candidates_with_budget(budget)
        .into_iter()
        .find(|field| field.name() == key)
}

fn find_field_by_authored_name<'g>(
    ty: &EffectiveType<'g>,
    key: &str,
    budget: &mut crate::schema::ShapeWalkBudget,
) -> Option<crate::schema::EffectiveField<'g>> {
    ty.find_field_with_budget(key, budget)
}

/// Typed roots use canonical field names. Authored aliases require an explicitly
/// named `*_by_authored_name` constructor, which still emits canonical paths.
#[derive(Clone)]
pub struct WalkRoot<'g> {
    effective: Option<EffectiveType<'g>>,
    field_names: FieldNames,
}
impl<'g> WalkRoot<'g> {
    pub fn syntactic() -> Self {
        Self {
            effective: None,
            field_names: FieldNames::Canonical,
        }
    }
    pub fn attribute(schema: &'g ValidatedResourceSchema, name: &str) -> Option<Self> {
        Some(Self::typed(schema.effective_attribute(name)?.value_type()))
    }
    pub fn typed(effective: EffectiveType<'g>) -> Self {
        Self {
            effective: Some(effective),
            field_names: FieldNames::Canonical,
        }
    }
    pub fn attribute_by_authored_name(
        schema: &'g ValidatedResourceSchema,
        name: &str,
    ) -> Option<Self> {
        Some(Self::typed_by_authored_name(
            schema
                .effective_attribute_by_authored_name(name)?
                .value_type(),
        ))
    }
    pub fn typed_by_authored_name(effective: EffectiveType<'g>) -> Self {
        Self {
            effective: Some(effective),
            field_names: FieldNames::Authored,
        }
    }
    /// Provider/backend configuration retains its validated type cursor.
    pub fn config_attribute(schema: &'g ValidatedConfigSchema, name: &str) -> Option<Self> {
        schema.effective_type(name).map(Self::typed)
    }
    pub fn config_attribute_by_authored_name(
        schema: &'g ValidatedConfigSchema,
        name: &str,
    ) -> Option<Self> {
        schema
            .effective_type(name)
            .map(Self::typed_by_authored_name)
    }
    /// A validated rootless source retains its root and named definitions.
    pub fn rootless(rootless: &'g RootlessType) -> Self {
        Self::typed(rootless.effective_type())
    }
    pub fn rootless_by_authored_name(rootless: &'g RootlessType) -> Self {
        Self::typed_by_authored_name(rootless.effective_type())
    }
    fn at(self) -> At<'g> {
        At {
            effective: self.effective,
            field_names: self.field_names,
            path: LeafPath::from_segments([]),
            expressions: Vec::new(),
            secret_depth: 0,
            position: Vec::new(),
        }
    }
}

/// Own a definition-free declared type lifted by the existing declaration API.
pub struct DeclarationRoot {
    lifted: crate::validation::LiftedType,
}
impl DeclarationRoot {
    pub fn new(declaration: &crate::parser::TypeExpr) -> Option<Self> {
        crate::validation::lift_type_expr(declaration).map(|lifted| Self { lifted })
    }
    pub fn root(&self) -> WalkRoot<'_> {
        static DEFS: std::sync::LazyLock<
            std::collections::BTreeMap<String, crate::schema::CompiledAttributeType>,
        > = std::sync::LazyLock::new(std::collections::BTreeMap::new);
        WalkRoot::typed(EffectiveType::for_value_walk(&self.lifted, &DEFS))
    }
    pub fn root_by_authored_name(&self) -> WalkRoot<'_> {
        let mut root = self.root();
        root.field_names = FieldNames::Authored;
        root
    }
}

/// Kernel-issued position and protection ancestry. Protected paths can contain
/// private map keys, so only public positions expose their path data.
#[derive(Clone)]
pub struct At<'g> {
    effective: Option<EffectiveType<'g>>,
    field_names: FieldNames,
    path: LeafPath,
    expressions: Vec<ExpressionEdge>,
    secret_depth: usize,
    position: Vec<usize>,
}
impl<'g> At<'g> {
    pub fn is_secret(&self) -> bool {
        self.secret_depth != 0
    }
    pub fn public_path(&self) -> Option<&LeafPath> {
        (!self.is_secret()).then_some(&self.path)
    }
    pub fn expression_edges(&self) -> &[ExpressionEdge] {
        &self.expressions
    }
    pub fn effective_type(&self) -> Option<&EffectiveType<'g>> {
        self.effective.as_ref()
    }
    fn list_child(&self, index: usize) -> Self {
        let mut at = self.child(
            LeafPathSegment::ListIndex(index),
            self.effective
                .as_ref()
                .and_then(EffectiveType::list_element),
        );
        at.position.extend([0, index]);
        at
    }
    fn map_child(&self, key: &str, rank: usize) -> Result<Self, WalkError> {
        let mut at = match self.effective.as_ref() {
            Some(ty) if matches!(ty.shape(), Shape::Struct { name: _ }) => {
                let mut budget = crate::schema::ShapeWalkBudget::new(64);
                let field = match self.field_names {
                    FieldNames::Canonical => find_field_by_canonical_name(ty, key, &mut budget),
                    FieldNames::Authored => find_field_by_authored_name(ty, key, &mut budget),
                }
                .ok_or(WalkError::UnknownStructField)?;
                self.child(
                    LeafPathSegment::StructField(field.name().into()),
                    Some(field.value_type()),
                )
            }
            Some(ty) => self.child(LeafPathSegment::MapKey(key.into()), ty.map_value()),
            None => self.child(LeafPathSegment::MapKey(key.into()), None),
        };
        at.position.extend([1, rank]);
        Ok(at)
    }
    fn child(&self, segment: LeafPathSegment, effective: Option<EffectiveType<'g>>) -> Self {
        let mut at = self.clone();
        at.path = LeafPath::from_segments(self.path.segments().iter().cloned().chain([segment]));
        at.effective = effective;
        at
    }
    fn operand(&self, edge: ExpressionEdge) -> Self {
        let mut at = self.clone();
        let [tag, index] = edge.coordinate();
        at.position.extend([2, tag, index]);
        match &edge {
            ExpressionEdge::Then
            | ExpressionEdge::Else
            | ExpressionEdge::CoalesceValue
            | ExpressionEdge::CoalesceDefault => {}
            ExpressionEdge::Condition
            | ExpressionEdge::ArithmeticLeft
            | ExpressionEdge::ArithmeticRight
            | ExpressionEdge::Interpolation(_)
            | ExpressionEdge::Argument(_)
            | ExpressionEdge::ArgumentValue(_)
            | ExpressionEdge::AccessBase
            | ExpressionEdge::FunctionDefinition
            | ExpressionEdge::FunctionScope(_)
            | ExpressionEdge::FunctionDefault(_)
            | ExpressionEdge::FunctionLocal(_)
            | ExpressionEdge::FunctionBody => at.effective = None,
        }
        at.expressions.push(edge);
        at
    }
    fn protected(&self) -> Self {
        let mut at = self.clone();
        at.secret_depth += 1;
        at
    }
    fn secret_child(&self) -> Self {
        let mut at = self.clone();
        at.position.push(3);
        at
    }
}

/// Secret changes scope, without adding a LeafPath segment.
pub struct SecretAt<'g> {
    at: At<'g>,
}
impl<'g> SecretAt<'g> {
    pub fn at(&self) -> &At<'g> {
        &self.at
    }
}

/// Expression positions are separate from the canonical container path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpressionEdge {
    Condition,
    Then,
    Else,
    CoalesceValue,
    CoalesceDefault,
    ArithmeticLeft,
    ArithmeticRight,
    Interpolation(usize),
    Argument(usize),
    ArgumentValue(usize),
    AccessBase,
    FunctionDefinition,
    FunctionScope(usize),
    FunctionDefault(usize),
    FunctionLocal(usize),
    FunctionBody,
}

impl ExpressionEdge {
    // Stable structural tags: append new tags rather than renumbering them.
    // They contain no data from an expression or a map key.
    fn coordinate(&self) -> [usize; 2] {
        match self {
            Self::Condition => [0, 0],
            Self::Then => [1, 0],
            Self::Else => [2, 0],
            Self::CoalesceValue => [3, 0],
            Self::CoalesceDefault => [4, 0],
            Self::ArithmeticLeft => [5, 0],
            Self::ArithmeticRight => [6, 0],
            Self::Interpolation(index) => [7, *index],
            Self::Argument(index) => [8, *index],
            Self::ArgumentValue(index) => [9, *index],
            Self::AccessBase => [10, 0],
            Self::FunctionDefinition => [11, 0],
            Self::FunctionScope(index) => [12, *index],
            Self::FunctionDefault(index) => [13, *index],
            Self::FunctionLocal(index) => [14, *index],
            Self::FunctionBody => [15, 0],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarKind {
    String,
    EnumIdentifier,
    CanonicalEnum,
    Int,
    Float,
    Bool,
    Duration,
    StringList,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafKind {
    ResourceRef,
    BindingRef,
    Unknown,
    Placeholder,
}

/// Nonrecursive reference data for an owner-managed binding worklist.
/// It contains no child value that could bypass the dispatcher.
pub enum ReferenceRef<'a, B: BindingAddress> {
    Resource(&'a crate::resource::AccessPath<B>),
    Binding(&'a B),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpressionKind {
    ModuleIf,
    Coalesce,
    IntegerArithmetic,
    Interpolation,
    FunctionCall,
    Unavailable,
    ResolvedFunctionCall,
    Access,
}

/// Kernel errors carry no user-controlled text or unprotected payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WalkError {
    #[error("value traversal rejected by its algebra")]
    Rejected,
    #[error(
        "the current function-argument API cannot preserve auxiliary access metadata during a rewrite"
    )]
    ArgumentMetadata,
    #[error("the current access API cannot preserve authored loop metadata during this rewrite")]
    AccessMetadata,
    #[error("value rewrite produced an invalid integer-arithmetic operand")]
    ArithmeticOperand,
    #[error("struct field is absent from the selected naming domain")]
    UnknownStructField,
    #[error("receipt is not from the selected argument of this call")]
    ReceiptOrigin,
}
impl crate::resource::InsideSecret for WalkError {
    fn inside_secret(self) -> Self {
        self
    }
}

/// Completion of an ordinary node; only cursor operations mint it.
pub struct Step<
    M: WalkMode,
    O: WalkOutput,
    P: ValueKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    result: Result<M::Product<O, P, B>, WalkError>,
}
impl<M: WalkMode, O: WalkOutput, P: ValueKind, B: BindingAddress> Step<M, O, P, B> {
    fn new(result: Result<M::Product<O, P, B>, WalkError>) -> Self {
        Self { result }
    }
}

impl<O: WalkOutput, P: ValueKind, B: BindingAddress> Step<Inspect, O, P, B> {
    pub fn try_map_output(self, map: impl FnOnce(O) -> Result<O, WalkError>) -> Self {
        Self::new(self.result.and_then(map))
    }
}
impl<O: WalkOutput, P: ValueKind, B: BindingAddress> Step<Transform, O, P, B> {
    pub fn try_map_output(self, map: impl FnOnce(O) -> Result<O, WalkError>) -> Self {
        Self::new(self.result.and_then(|product| {
            Ok(RewriteProduct {
                value: product.value,
                output: map(product.output)?,
                changed: product.changed,
            })
        }))
    }
}

/// Completion of a Secret node. There is no public success constructor,
/// conversion from Step, Default, or successful skip operation.
pub struct SecretStep<
    M: WalkMode,
    O: WalkOutput,
    P: ValueKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    step: Step<M, O, P, B>,
}
impl<O: WalkOutput, P: ValueKind, B: BindingAddress> SecretStep<Inspect, O, P, B> {
    pub fn map(self, map: impl FnOnce(O) -> O) -> Self {
        Self {
            step: Step::new(self.step.result.map(map)),
        }
    }
}

/// Internal completion product. Only kernel cursor operations construct it.
#[doc(hidden)]
pub struct RewriteProduct<O: WalkOutput, P: ValueKind, B: BindingAddress> {
    value: Value<P, B>,
    output: O,
    changed: bool,
}

/// A summary handle with a kernel-issued authored/result position and scope.
/// Payloads use the same restricted result channel as inspection. Owners can
/// keep typed obligations and receipts in separate inventories indexed by these
/// handles, without passing their plaintext through this channel.
#[derive(Clone)]
pub struct Scoped<T: WalkOutput> {
    output: T,
    path: LeafPath,
    expressions: Vec<ExpressionEdge>,
    secret_depth: usize,
    position: Vec<usize>,
}
impl<T: WalkOutput> Scoped<T> {
    fn new(at: &At<'_>, output: T) -> Self {
        Self {
            output,
            path: at.path.clone(),
            expressions: at.expressions.clone(),
            secret_depth: at.secret_depth,
            position: at.position.clone(),
        }
    }
    pub fn output(&self) -> &T {
        &self.output
    }
    pub fn is_secret(&self) -> bool {
        self.secret_depth != 0
    }
    pub fn public_path(&self) -> Option<&LeafPath> {
        (!self.is_secret()).then_some(&self.path)
    }
    pub fn expression_edges(&self) -> &[ExpressionEdge] {
        &self.expressions
    }
}

/// Independent authored obligations and result-path receipts. The kernel
/// scopes and transports handles; their interpretation remains with the owner.
/// In particular, constructing this carrier does not validate a conversion.
#[derive(Clone)]
pub struct Evidence<O: WalkOutput, R: WalkOutput> {
    obligations: Vec<Scoped<O>>,
    receipts: Vec<Scoped<R>>,
}
impl<O: WalkOutput, R: WalkOutput> Evidence<O, R> {
    pub fn empty() -> Self {
        Self {
            obligations: Vec::new(),
            receipts: Vec::new(),
        }
    }
    pub fn at(
        at: &At<'_>,
        obligations: impl IntoIterator<Item = O>,
        receipts: impl IntoIterator<Item = R>,
    ) -> Self {
        Self {
            obligations: obligations
                .into_iter()
                .map(|output| Scoped::new(at, output))
                .collect(),
            receipts: receipts
                .into_iter()
                .map(|output| Scoped::new(at, output))
                .collect(),
        }
    }
    pub fn obligations(&self) -> &[Scoped<O>] {
        &self.obligations
    }
    pub fn receipts(&self) -> &[Scoped<R>] {
        &self.receipts
    }
    /// Move only selected argument receipts to a call's result. Protection is
    /// joined, never cleared. Authored obligations retain their original sites.
    pub fn reroot_receipts(
        mut self,
        call: &At<'_>,
        argument: usize,
        mut select: impl FnMut(&Scoped<R>) -> bool,
    ) -> Result<Self, WalkError> {
        for receipt in &mut self.receipts {
            if select(receipt) {
                let suffix = receipt
                    .expressions
                    .strip_prefix(call.expressions.as_slice());
                let [tag, index] = ExpressionEdge::Argument(argument).coordinate();
                let argument_position: Vec<_> = call
                    .position
                    .iter()
                    .copied()
                    .chain([2, tag, index])
                    .collect();
                if !receipt.position.starts_with(&argument_position)
                    || !receipt.path.segments().starts_with(call.path.segments())
                    || !suffix.is_some_and(|edges| {
                        edges.first() == Some(&ExpressionEdge::Argument(argument))
                    })
                {
                    return Err(WalkError::ReceiptOrigin);
                }
                receipt.path = call.path.clone();
                receipt.position = call.position.clone();
                receipt.expressions = call.expressions.clone();
                receipt.secret_depth = receipt.secret_depth.max(call.secret_depth);
            }
        }
        Ok(self)
    }
    /// Append without rebasing paths or merging authored and result evidence.
    pub fn merge(outputs: impl IntoIterator<Item = Self>) -> Self {
        let mut merged = Self::empty();
        for output in outputs {
            merged.obligations.extend(output.obligations);
            merged.receipts.extend(output.receipts);
        }
        merged
    }
}

/// All six arms are required. Inspect algebras work across value stages and
/// binding phases, with argument-only cursors for resolved calls.
pub trait Algebra<M: WalkMode>: Sized {
    type Output: WalkOutput;
    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        scalar: ScopedScalar<'_, M, P, B>,
    ) -> Step<M, Self::Output, P, B>;
    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        items: Children<'_, M, P, B>,
    ) -> Step<M, Self::Output, P, B>;
    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        entries: Children<'_, M, P, B>,
    ) -> Step<M, Self::Output, P, B>;
    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        expr: ExprChildren<'_, M, P, B>,
    ) -> Step<M, Self::Output, P, B>;
    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        leaf: ScopedLeaf<'_, M, P, B>,
    ) -> Step<M, Self::Output, P, B>;
    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &SecretAt<'_>,
        child: SecretChild<'_, M, P, B>,
    ) -> SecretStep<M, Self::Output, P, B>;
}

pub fn inspect<'v, P: ValueKind, B: BindingAddress, A: Algebra<Inspect>>(
    root: WalkRoot<'v>,
    value: &'v Value<P, B>,
    algebra: &mut A,
) -> Result<A::Output, WalkError> {
    dispatch(value, root.at(), algebra)
}

pub struct ScopedScalar<'v, M: WalkMode, P: ValueKind, B: BindingAddress> {
    input: ScalarInput<'v, M>,
    mode: PhantomData<(P, B)>,
}
impl<M: WalkMode, P: ValueKind, B: BindingAddress> ScopedScalar<'_, M, P, B> {
    pub fn kind(&self) -> ScalarKind {
        self.input.kind()
    }
    pub fn reject<O: WalkOutput>(self) -> Step<M, O, P, B> {
        Step::new(Err(WalkError::Rejected))
    }
}
impl<P: ValueKind, B: BindingAddress> ScopedScalar<'_, Inspect, P, B> {
    pub fn finish<O: WalkOutput>(self, output: O) -> Step<Inspect, O, P, B> {
        Step::new(Ok(output))
    }
}
impl<P: ValueKind, B: BindingAddress> ScopedScalar<'_, Transform, P, B> {
    pub fn keep<O: WalkOutput>(self, output: O) -> Step<Transform, O, P, B> {
        Step::new(Ok(RewriteProduct {
            value: self.input.into_value(),
            output,
            changed: false,
        }))
    }
    pub fn replace<O: WalkOutput>(self, value: Value<P, B>, output: O) -> Step<Transform, O, P, B> {
        Step::new(Ok(RewriteProduct {
            value,
            output,
            changed: true,
        }))
    }
}

pub struct ScopedLeaf<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress> {
    input: LeafInput<'v, M, P, B>,
}
impl<M: WalkMode, P: ValueKind, B: BindingAddress> ScopedLeaf<'_, M, P, B> {
    pub fn kind(&self) -> LeafKind {
        self.input.kind()
    }
    /// Read typed, nonrecursive reference fields into the summary channel.
    pub fn with_reference<O: WalkOutput>(
        &self,
        read: impl FnOnce(Option<ReferenceRef<'_, B>>) -> O,
    ) -> O {
        read(self.input.reference())
    }
    /// Inspect a stage-specific substitution slot without exposing a value.
    pub fn with_placeholder<O: WalkOutput>(&self, read: impl FnOnce(Option<&P>) -> O) -> O {
        read(self.input.placeholder())
    }
    pub fn reject<O: WalkOutput>(self) -> Step<M, O, P, B> {
        Step::new(Err(WalkError::Rejected))
    }
}
impl<P: ValueKind, B: BindingAddress> ScopedLeaf<'_, Inspect, P, B> {
    pub fn finish<O: WalkOutput>(self, output: O) -> Step<Inspect, O, P, B> {
        Step::new(Ok(output))
    }
}
impl<P: ValueKind, B: BindingAddress> ScopedLeaf<'_, Transform, P, B> {
    pub fn keep<O: WalkOutput>(self, output: O) -> Step<Transform, O, P, B> {
        Step::new(Ok(RewriteProduct {
            value: self.input.into_value(),
            output,
            changed: false,
        }))
    }
    pub fn replace<O: WalkOutput>(self, value: Value<P, B>, output: O) -> Step<Transform, O, P, B> {
        Step::new(Ok(RewriteProduct {
            value,
            output,
            changed: true,
        }))
    }
}

pub struct Children<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress> {
    parts: Parts<'v, M, P, B>,
    at: At<'v>,
}
pub struct ExprChildren<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress> {
    children: Children<'v, M, P, B>,
    kind: ExpressionKind,
    at: At<'v>,
}
impl<'v, M: WalkMode, P: ValueKind, B: BindingAddress> ExprChildren<'v, M, P, B> {
    pub fn kind(&self) -> ExpressionKind {
        self.kind
    }
    /// Attach owner-supplied handles at this expression's result position.
    /// Both branch operands remain structural children and are never selected.
    pub fn with_evidence<O: WalkOutput, R: WalkOutput>(
        self,
        obligations: impl IntoIterator<Item = O>,
        receipts: impl IntoIterator<Item = R>,
    ) -> ExpressionWithEvidence<'v, M, P, B, O, R> {
        let evidence = Evidence::at(&self.at, obligations, receipts);
        ExpressionWithEvidence {
            expression: self,
            evidence,
        }
    }
}
impl<M: WalkMode, P: ValueKind, B: BindingAddress> Children<'_, M, P, B> {
    pub fn len(&self) -> usize {
        self.parts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.parts.len() == 0
    }
    pub fn reject<O: WalkOutput>(self) -> Step<M, O, P, B> {
        Step::new(Err(WalkError::Rejected))
    }
}
impl<P: ValueKind, B: BindingAddress> Children<'_, Inspect, P, B> {
    pub fn inspect<A: Algebra<Inspect>>(
        self,
        algebra: &mut A,
        fold: impl FnOnce(Vec<A::Output>) -> A::Output,
    ) -> Step<Inspect, A::Output, P, B> {
        Step::new(self.parts.inspect(self.at, algebra).map(fold))
    }
}
impl<P: ValueKind, B: BindingAddress> ExprChildren<'_, Inspect, P, B> {
    pub fn inspect<A: Algebra<Inspect>>(
        self,
        algebra: &mut A,
        fold: impl FnOnce(Vec<A::Output>) -> A::Output,
    ) -> Step<Inspect, A::Output, P, B> {
        self.children.inspect(algebra, fold)
    }
}
/// An expression cursor paired with separate, scoped evidence channels.
/// Only its operand is recursed into; evidence is transported without evaluation.
pub struct ExpressionWithEvidence<
    'v,
    M: WalkMode,
    P: ValueKind + 'v,
    B: BindingAddress,
    O: WalkOutput,
    R: WalkOutput,
> {
    expression: ExprChildren<'v, M, P, B>,
    evidence: Evidence<O, R>,
}
impl<P: ValueKind, B: BindingAddress, O: WalkOutput, R: WalkOutput>
    ExpressionWithEvidence<'_, Inspect, P, B, O, R>
{
    pub fn inspect<A: Algebra<Inspect, Output = Evidence<O, R>>>(
        self,
        algebra: &mut A,
    ) -> Step<Inspect, Evidence<O, R>, P, B> {
        self.expression.inspect(algebra, |children| {
            Evidence::merge(std::iter::once(self.evidence).chain(children))
        })
    }
}
pub struct SecretChild<'v, M: WalkMode, P: ValueKind + 'v, B: BindingAddress> {
    payload: M::Input<'v, SecretPayload<P, B>>,
    at: At<'v>,
}
impl<M: WalkMode, P: ValueKind, B: BindingAddress> SecretChild<'_, M, P, B> {
    pub fn reject<O: WalkOutput>(self) -> SecretStep<M, O, P, B> {
        SecretStep {
            step: Step::new(Err(WalkError::Rejected)),
        }
    }
}
impl<P: ValueKind, B: BindingAddress> SecretChild<'_, Inspect, P, B> {
    pub fn inspect<A: Algebra<Inspect>>(
        self,
        algebra: &mut A,
    ) -> SecretStep<Inspect, A::Output, P, B> {
        // The payload's crate-private read is confined here. No cursor exposes it.
        SecretStep {
            step: Step::new(dispatch(self.payload.expose_plaintext(), self.at, algebra)),
        }
    }
}
impl<P: ValueKind, B: BindingAddress> SecretChild<'_, Transform, P, B> {
    fn transform_bound<A: BoundTransform<P, B>>(
        self,
        algebra: &mut A,
    ) -> SecretStep<Transform, A::Output, P, B> {
        // SecretPayload currently has no consuming child accessor. Cloning is
        // confined to this adapter until representation ownership moves here.
        let child = self.payload.expose_plaintext().clone();
        SecretStep {
            step: Step::new(
                engine::dispatch_bound(child, self.at, algebra).map(|product| RewriteProduct {
                    value: product.value.into_secret(),
                    output: product.output,
                    changed: product.changed,
                }),
            ),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static FUNCTION_DEFINITION_WALK_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn with_function_definition_walk_count<T>(run: impl FnOnce() -> T) -> (T, usize) {
    FUNCTION_DEFINITION_WALK_COUNT.with(|count| {
        count.set(0);
        let result = run();
        (result, count.replace(0))
    })
}
