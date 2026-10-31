//! Evaluator-internal values.
//!
//! `Value` is the public, user-facing type that flows through `ParsedFile`,
//! `Resource.attributes`, plan display, state serialization, and the LSP.
//! Every variant of `Value` is something a `.crn` author can directly
//! observe in their configuration.
//!
//! `EvalValue` is the strictly internal type used while the evaluator is
//! reducing expressions. In addition to `Closure` for partial application it
//! carries parser-only structural handles so they can never masquerade as
//! user values. Closures are produced when a
//! function receives fewer arguments than its arity, and are consumed by the
//! next pipe / call that finishes the application.
//!
//! The split is enforced by the type system: a `Closure` cannot appear
//! anywhere a `Value` is expected. The boundary is `EvalValue::into_value`,
//! which returns a typed error if a closure or bare structural handle tried to
//! escape the evaluator. Consumers that match on `Value` therefore never need
//! either arm — the variants do not exist for them.
//!
//! See #2230 for the design discussion.

use crate::parser::{PendingValueSlot, ResourceTypePath};
use crate::resource::{
    AbsoluteBindingAddress, BindingAddress, PathSegment, RelativeBindingName, Subscript, Value,
};
use indexmap::IndexMap;

/// Parser-only representation of a structural binding used as an operand.
///
/// A structural handle is deliberately not part of [`Value`]. It can be
/// consumed only by syntax whose contract explicitly accepts a handle (for
/// example `directives.depends_on` or a function/module argument boundary).
/// The argument shape stays typed independently of its annotation; every
/// ordinary value boundary rejects a bare handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructuralHandle<B: BindingAddress = AbsoluteBindingAddress> {
    reference: StructuralReference<B>,
    kind: StructuralHandleKind,
}

pub(crate) type AuthoredStructuralHandle = StructuralHandle<RelativeBindingName>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuralHandleKind {
    Resource(ResourceTypePath),
    For,
    If,
    Use,
    Module,
    Wait,
    UpstreamState,
    Provider,
    ModuleParameter,
    FunctionParameter,
}

impl StructuralHandleKind {
    /// Whether this structural kind denotes an ordering target that the
    /// dependency graph can actually honor.
    pub(crate) fn is_dependency_target(&self) -> bool {
        matches!(
            self,
            Self::Resource(_) | Self::If | Self::Module | Self::ModuleParameter
        )
    }

    pub(crate) fn description(&self) -> &'static str {
        match self {
            Self::Resource(_) => "a resource",
            Self::For => "a for binding",
            Self::If => "an if binding",
            Self::Use => "a module import",
            Self::Module => "a module call",
            Self::Wait => "a wait binding",
            Self::UpstreamState => "an upstream state",
            Self::Provider => "a provider instance",
            Self::ModuleParameter | Self::FunctionParameter => "a resource handle",
        }
    }

    fn rewrite(&self, binding: &str) -> String {
        match self {
            Self::Module => format!("reference one of its outputs, e.g. '{binding}.<output>'"),
            Self::Use => {
                "instantiate the module, then reference an output from the module call".to_string()
            }
            Self::For => {
                format!("reference an instance attribute, e.g. '{binding}[0].<attribute>'")
            }
            Self::Provider => {
                "use it only in 'directives.provider' where a provider handle is expected"
                    .to_string()
            }
            Self::Resource(_)
            | Self::If
            | Self::Wait
            | Self::UpstreamState
            | Self::ModuleParameter
            | Self::FunctionParameter => {
                format!("reference one of its attributes, e.g. '{binding}.<attribute>'")
            }
        }
    }
}

/// Parser-only shape metadata retained while a provisionally typed directory
/// reference flows through a computed expression. The fixed-point pass either
/// resolves it to ordinary value data or rejects it once the referenced name
/// is known to be a structural handle.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StructuralHandleMetadata<B: BindingAddress = AbsoluteBindingAddress> {
    Handle {
        access_base: Option<Value<B>>,
    },
    List(Vec<Option<StructuralHandleMetadata<B>>>),
    Map(IndexMap<String, StructuralHandleMetadata<B>>),
    /// Provisional handles consumed by a computed scalar expression. This
    /// preserves every operand for the directory fixed point without making
    /// the scalar result itself look like a handle alias.
    ExpressionOperands(Vec<StructuralHandleMetadata<B>>),
}

pub(crate) type AuthoredStructuralHandleMetadata = StructuralHandleMetadata<RelativeBindingName>;

impl<B: BindingAddress> StructuralHandleMetadata<B> {
    pub(crate) fn is_handle(&self) -> bool {
        matches!(self, Self::Handle { .. })
    }

    pub(crate) fn access_base(&self) -> Option<&Value<B>> {
        match self {
            Self::Handle { access_base } => access_base.as_ref(),
            Self::List(_) | Self::Map(_) | Self::ExpressionOperands(_) => None,
        }
    }

    pub(crate) fn from_expression_operands(
        operands: impl IntoIterator<Item = Option<Self>>,
    ) -> Option<Self> {
        let mut collected = Vec::new();
        for operand in operands.into_iter().flatten() {
            match operand {
                Self::ExpressionOperands(nested) => collected.extend(nested),
                other => collected.push(other),
            }
        }
        (!collected.is_empty()).then_some(Self::ExpressionOperands(collected))
    }

    pub(crate) fn select(&self, step: &PathSegment) -> Option<Self> {
        match (self, step) {
            (
                Self::List(items),
                PathSegment::Subscript {
                    index: Subscript::Int { index },
                },
            ) => usize::try_from(*index)
                .ok()
                .and_then(|index| items.get(index))
                .cloned()
                .flatten(),
            (Self::Map(entries), PathSegment::Field { name }) => entries.get(name).cloned(),
            (
                Self::Map(entries),
                PathSegment::Subscript {
                    index: Subscript::Str { key },
                },
            ) => entries.get(key.expose_plaintext()).cloned(),
            (Self::Handle { .. }, _)
            | (Self::List(_), _)
            | (Self::Map(_), _)
            | (Self::ExpressionOperands(_), _) => None,
        }
    }

    pub(crate) fn try_visit_values<E>(
        &self,
        visit: &mut impl FnMut(&Value<B>) -> Result<(), E>,
    ) -> Result<(), E> {
        match self {
            Self::Handle { access_base } => {
                if let Some(value) = access_base {
                    visit(value)?;
                }
            }
            Self::List(items) => {
                for item in items.iter().flatten() {
                    item.try_visit_values(visit)?;
                }
            }
            Self::Map(entries) => {
                for item in entries.values() {
                    item.try_visit_values(visit)?;
                }
            }
            Self::ExpressionOperands(operands) => {
                for operand in operands {
                    operand.try_visit_values(visit)?;
                }
            }
        }
        Ok(())
    }
}

impl StructuralHandleMetadata<RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> StructuralHandleMetadata<AbsoluteBindingAddress> {
        match self {
            Self::Handle { access_base } => StructuralHandleMetadata::Handle {
                access_base: access_base.map(|value| value.resolve_bindings(resolve)),
            },
            Self::List(items) => StructuralHandleMetadata::List(
                items
                    .into_iter()
                    .map(|item| item.map(|metadata| metadata.resolve_bindings(resolve)))
                    .collect(),
            ),
            Self::Map(entries) => StructuralHandleMetadata::Map(
                entries
                    .into_iter()
                    .map(|(name, metadata)| (name, metadata.resolve_bindings(resolve)))
                    .collect(),
            ),
            Self::ExpressionOperands(operands) => StructuralHandleMetadata::ExpressionOperands(
                operands
                    .into_iter()
                    .map(|metadata| metadata.resolve_bindings(resolve))
                    .collect(),
            ),
        }
    }
}

impl StructuralHandleMetadata<AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        match self {
            Self::Handle { access_base } => {
                if let Some(access_base) = access_base {
                    access_base.map_absolute_bindings(map);
                }
            }
            Self::List(items) => {
                for item in items.iter_mut().flatten() {
                    item.map_absolute_bindings(map);
                }
            }
            Self::Map(entries) => {
                for metadata in entries.values_mut() {
                    metadata.map_absolute_bindings(map);
                }
            }
            Self::ExpressionOperands(operands) => {
                for metadata in operands {
                    metadata.map_absolute_bindings(map);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StructuralReference<B: BindingAddress> {
    Addressable(B),
    Alias { name: String, target: B },
}

impl<B: BindingAddress> StructuralHandle<B> {
    pub(crate) fn with_alias(mut self, alias: impl Into<String>) -> Self {
        let target = self.canonical_binding().clone();
        self.reference = StructuralReference::Alias {
            name: alias.into(),
            target,
        };
        self
    }

    pub(crate) fn addressable_binding(&self) -> Option<&str> {
        match &self.reference {
            StructuralReference::Addressable(binding) => Some(binding.as_str()),
            StructuralReference::Alias { .. } => None,
        }
    }

    pub(crate) fn alias_name(&self) -> Option<&str> {
        match &self.reference {
            StructuralReference::Alias { name, .. } => Some(name),
            StructuralReference::Addressable(_) => None,
        }
    }

    pub(crate) fn canonical_binding_name(&self) -> &str {
        match &self.reference {
            StructuralReference::Addressable(binding) => binding.as_str(),
            StructuralReference::Alias { target, .. } => target.as_str(),
        }
    }

    pub(crate) fn canonical_binding(&self) -> &B {
        match &self.reference {
            StructuralReference::Addressable(binding) => binding,
            StructuralReference::Alias { target, .. } => target,
        }
    }

    pub(crate) fn kind(&self) -> &StructuralHandleKind {
        &self.kind
    }

    pub(crate) fn is_for_binding(&self) -> bool {
        self.kind == StructuralHandleKind::For
    }

    pub(crate) fn is_parameter_handle(&self) -> bool {
        matches!(
            &self.kind,
            StructuralHandleKind::ModuleParameter | StructuralHandleKind::FunctionParameter
        )
    }

    /// Return the canonical reference operand used only for postfix access.
    pub(crate) fn access_base_value(&self) -> Value<B> {
        let binding = self.canonical_binding_name();
        match &self.kind {
            StructuralHandleKind::ModuleParameter => {
                Value::Deferred(crate::resource::DeferredValue::Unknown(
                    crate::resource::UnknownReason::ModuleArgument {
                        name: binding.to_string(),
                        source_line: (0).into(),
                        diagnostic_name: None,
                    },
                ))
            }
            StructuralHandleKind::FunctionParameter => Value::Deferred(
                crate::resource::DeferredValue::Unknown(crate::resource::UnknownReason::FnParam {
                    name: binding.to_string(),
                }),
            ),
            _ => Value::Deferred(crate::resource::DeferredValue::BindingRef {
                binding: self.canonical_binding().clone(),
            }),
        }
    }

    pub(crate) fn bare_error(&self) -> BareStructuralHandle {
        let binding = self
            .alias_name()
            .unwrap_or_else(|| self.canonical_binding_name())
            .to_string();
        BareStructuralHandle {
            binding,
            kind: self.kind.clone(),
        }
    }

    pub(crate) fn map_bindings<T: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> T,
    ) -> StructuralHandle<T> {
        let Self { reference, kind } = self;
        let reference = match reference {
            StructuralReference::Addressable(binding) => {
                StructuralReference::Addressable(map(binding))
            }
            StructuralReference::Alias { name, target } => StructuralReference::Alias {
                name,
                target: map(target),
            },
        };
        StructuralHandle { reference, kind }
    }
}

impl StructuralHandle<RelativeBindingName> {
    pub(crate) fn new(kind: StructuralHandleKind, name: impl Into<String>) -> Self {
        Self {
            reference: StructuralReference::Addressable(RelativeBindingName::new(name.into())),
            kind,
        }
    }

    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> crate::resource::AbsoluteBindingAddress,
    ) -> StructuralHandle<crate::resource::AbsoluteBindingAddress> {
        self.map_bindings(resolve)
    }
}

impl StructuralHandle<AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        *self = self.clone().map_bindings(&mut |binding| map(&binding));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BareStructuralHandle {
    pub(crate) binding: String,
    pub(crate) kind: StructuralHandleKind,
}

impl std::fmt::Display for BareStructuralHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "'{}' is {}, not a value; {}",
            self.binding,
            self.kind.description(),
            self.kind.rewrite(&self.binding),
        )
    }
}

/// Evaluator-internal value. Carries ordinary `Value`s plus parser-only
/// variants whose type information must survive until a deliberate lowering
/// boundary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EvalValue<B: BindingAddress = AbsoluteBindingAddress> {
    /// A user-facing value. Anything reachable from `Value` is reachable
    /// here without loss.
    User(Value<B>),

    /// A structural operand that has not crossed the public `Value` boundary.
    StructuralHandle(StructuralHandle<B>),

    /// A typed value sink reserved by the parser because a bare structural
    /// binding occurred somewhere below it. The owning resource attribute or
    /// module argument consumes this marker instead of manufacturing a public
    /// [`Value`] placeholder.
    PendingValue(PendingValueSlot),

    /// A directory-scoped name whose declaration kind is not known yet.
    /// This is deliberately distinct from `StructuralHandle`: the fixed-point
    /// pass may prove that it names either an ordinary value or a structure.
    ProvisionalReference { binding: B },

    /// A computed value whose construction or selection includes one or more
    /// provisional handles. The parallel metadata tree survives parser-local
    /// lets, `for` bindings, and directory seed passes until their kinds are
    /// known.
    ComputedWithStructuralHandles {
        value: Value<B>,
        metadata: StructuralHandleMetadata<B>,
    },

    /// Deferred-for loop value before the iterable is available. Keeping the
    /// authored binding name on the evaluator axis lets a postfix chain retain
    /// `item.missing` for plan display without resurrecting the removed
    /// path-carrying `UnknownReason` variant.
    DeferredForValue { binding: String },

    /// A partially applied function. Lives strictly inside the evaluator.
    /// Constructed when `evaluate_builtin` or a user-function call sees
    /// fewer arguments than the function's arity; consumed by the next
    /// pipe or call that finishes the application.
    Closure {
        /// Original function name.
        name: String,
        /// Arguments already supplied, in order.
        captured_args: Vec<EvalValue<B>>,
        /// How many more arguments the function still needs.
        remaining_arity: usize,
    },
}

pub(crate) type AuthoredEvalValue = EvalValue<RelativeBindingName>;

/// Returned by `EvalValue::into_value` when an evaluator-only value would
/// otherwise escape into the public [`Value`] model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClosureLeak {
    pub name: String,
    pub remaining_arity: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EvalValueError {
    Closure(ClosureLeak),
    BareStructuralHandle(BareStructuralHandle),
    PendingValue(Box<PendingValueSlot>),
}

impl std::fmt::Display for EvalValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closure(leak) => write!(
                f,
                "expression evaluates to a closure '{}' (still needs {} arg(s)); finish the \
                 partial application — closures are not valid as data",
                leak.name, leak.remaining_arity,
            ),
            Self::BareStructuralHandle(error) => error.fmt(f),
            Self::PendingValue(_) => {
                f.write_str("a parser pending value escaped its typed attribute or argument sink")
            }
        }
    }
}

/// Result of lowering an expression at a typed resource-attribute or
/// module-argument boundary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParsedSinkValue<B: BindingAddress = AbsoluteBindingAddress> {
    Ready(Value<B>),
    Pending(PendingValueSlot),
}

pub(crate) type AuthoredParsedSinkValue = ParsedSinkValue<RelativeBindingName>;

impl<B: BindingAddress> EvalValue<B> {
    /// Wrap a user-facing `Value`. This is the cheap, infallible
    /// direction — every `Value` is a valid `EvalValue`.
    pub(crate) fn from_value(value: Value<B>) -> Self {
        Self::User(value)
    }

    /// Build a closure value from its parts.
    pub(crate) fn closure(
        name: impl Into<String>,
        captured_args: Vec<EvalValue<B>>,
        remaining_arity: usize,
    ) -> Self {
        Self::Closure {
            name: name.into(),
            captured_args,
            remaining_arity,
        }
    }

    pub(crate) fn structural_handle(handle: StructuralHandle<B>) -> Self {
        Self::StructuralHandle(handle)
    }

    pub(crate) fn pending_value(slot: PendingValueSlot) -> Self {
        Self::PendingValue(slot)
    }

    /// Stamp a neutral module-parameter placeholder with the authored use
    /// site. Other evaluator values pass through unchanged.
    pub(crate) fn with_reference_source_line(mut self, line: usize) -> Self {
        if let Self::User(Value::Deferred(crate::resource::DeferredValue::Unknown(
            crate::resource::UnknownReason::ModuleArgument { source_line, .. },
        ))) = &mut self
        {
            *source_line = line.into();
        }
        self
    }

    pub(crate) fn computed(value: Value<B>, metadata: Option<StructuralHandleMetadata<B>>) -> Self {
        match metadata {
            Some(metadata) => Self::ComputedWithStructuralHandles { value, metadata },
            None => Self::User(value),
        }
    }

    pub(crate) fn deferred_for_value(binding: impl Into<String>) -> Self {
        Self::DeferredForValue {
            binding: binding.into(),
        }
    }

    pub(crate) fn structural_handle_ref(&self) -> Option<&StructuralHandle<B>> {
        match self {
            Self::StructuralHandle(handle) => Some(handle),
            _ => None,
        }
    }

    pub(crate) fn provisional_reference_name(&self) -> Option<&str> {
        match self {
            Self::ProvisionalReference { binding } => Some(binding.as_str()),
            _ => None,
        }
    }

    pub(crate) fn structural_handle_metadata(&self) -> Option<&StructuralHandleMetadata<B>> {
        match self {
            Self::ComputedWithStructuralHandles { metadata, .. } => Some(metadata),
            Self::User(_)
            | Self::StructuralHandle(_)
            | Self::PendingValue(_)
            | Self::ProvisionalReference { .. }
            | Self::DeferredForValue { .. }
            | Self::Closure { .. } => None,
        }
    }

    pub(crate) fn into_collection_element(
        self,
    ) -> Result<(Value<B>, Option<StructuralHandleMetadata<B>>), EvalValueError> {
        match self {
            Self::User(value) => {
                let metadata = matches!(
                    value,
                    Value::Deferred(crate::resource::DeferredValue::BindingRef { .. })
                )
                .then(|| StructuralHandleMetadata::Handle {
                    access_base: Some(value.clone()),
                });
                Ok((value, metadata))
            }
            Self::StructuralHandle(handle) => {
                Err(EvalValueError::BareStructuralHandle(handle.bare_error()))
            }
            Self::PendingValue(slot) => Err(EvalValueError::PendingValue(Box::new(slot))),
            Self::ProvisionalReference { binding } => {
                let value = Value::Deferred(crate::resource::DeferredValue::BindingRef { binding });
                Ok((
                    value.clone(),
                    Some(StructuralHandleMetadata::Handle {
                        access_base: Some(value),
                    }),
                ))
            }
            Self::ComputedWithStructuralHandles { value, metadata } => Ok((value, Some(metadata))),
            Self::DeferredForValue { .. } => Ok((
                Value::Deferred(crate::resource::DeferredValue::Unknown(
                    crate::resource::UnknownReason::ForValue,
                )),
                None,
            )),
            Self::Closure {
                name,
                remaining_arity,
                ..
            } => Err(EvalValueError::Closure(ClosureLeak {
                name,
                remaining_arity,
            })),
        }
    }

    /// Preserve the typed operand needed when a module parameter is later
    /// accessed inside the module.
    pub(crate) fn module_argument_access_base(&self) -> Option<Value<B>> {
        match self {
            Self::StructuralHandle(handle) => Some(handle.access_base_value()),
            Self::PendingValue(_) => None,
            Self::ProvisionalReference { binding } => Some(Value::Deferred(
                crate::resource::DeferredValue::BindingRef {
                    binding: binding.clone(),
                },
            )),
            Self::User(
                value @ Value::Deferred(
                    crate::resource::DeferredValue::BindingRef { .. }
                    | crate::resource::DeferredValue::ResourceRef { .. },
                ),
            ) => Some(value.clone()),
            Self::ComputedWithStructuralHandles { metadata, .. } => metadata.access_base().cloned(),
            _ => None,
        }
    }

    pub(crate) fn into_structural_handle(self) -> Option<StructuralHandle<B>> {
        match self {
            Self::StructuralHandle(handle) => Some(handle),
            _ => None,
        }
    }

    pub(crate) fn into_sink_value(self) -> Result<ParsedSinkValue<B>, EvalValueError> {
        match self {
            Self::PendingValue(slot) => Ok(ParsedSinkValue::Pending(slot)),
            other => other.into_value().map(ParsedSinkValue::Ready),
        }
    }

    /// True when this value is a `Closure`.
    pub(crate) fn is_closure(&self) -> bool {
        matches!(self, Self::Closure { .. })
    }

    /// Lower an `EvalValue` to a user-facing `Value`. This is the
    /// boundary where the evaluator hands a result off to a public
    /// consumer (parser-level let-binding storage, resolver output,
    /// builtin handler input, etc.). If a closure is still here, the
    /// caller wrote a partial application that never completed.
    pub(crate) fn into_value(self) -> Result<Value<B>, EvalValueError> {
        match self {
            Self::User(v) => Ok(v),
            Self::StructuralHandle(handle) => {
                Err(EvalValueError::BareStructuralHandle(handle.bare_error()))
            }
            Self::PendingValue(slot) => Err(EvalValueError::PendingValue(Box::new(slot))),
            Self::ProvisionalReference { binding } => Ok(Value::Deferred(
                crate::resource::DeferredValue::BindingRef { binding },
            )),
            Self::ComputedWithStructuralHandles { value, .. } => Ok(value),
            Self::DeferredForValue { .. } => Ok(Value::Deferred(
                crate::resource::DeferredValue::Unknown(crate::resource::UnknownReason::ForValue),
            )),
            Self::Closure {
                name,
                remaining_arity,
                ..
            } => Err(EvalValueError::Closure(ClosureLeak {
                name,
                remaining_arity,
            })),
        }
    }
}

impl EvalValue<RelativeBindingName> {
    pub(crate) fn provisional_reference(binding: impl Into<String>) -> Self {
        Self::ProvisionalReference {
            binding: RelativeBindingName::new(binding.into()),
        }
    }
}

impl<B: BindingAddress> From<Value<B>> for EvalValue<B> {
    fn from(value: Value<B>) -> Self {
        Self::User(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    #[test]
    fn user_round_trips() {
        let original = Value::Concrete(ConcreteValue::String("hi".into()));
        let eval = EvalValue::from_value(original.clone());
        assert_eq!(eval.into_value().unwrap(), original);
    }

    #[test]
    fn closure_into_value_returns_leak() {
        let eval: EvalValue = EvalValue::closure("join", vec![], 2);
        let err = eval.into_value().unwrap_err();
        let EvalValueError::Closure(leak) = err else {
            panic!("expected closure error");
        };
        assert_eq!(leak.name, "join");
        assert_eq!(leak.remaining_arity, 2);
    }
}
