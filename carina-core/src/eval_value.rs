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

use crate::parser::{BindingName, PendingValueSlot, ResourceTypePath};
use crate::resource::{PathSegment, Subscript, Value};
use indexmap::IndexMap;
use std::borrow::Cow;

/// Parser-only representation of a structural binding used as an operand.
///
/// A structural handle is deliberately not part of [`Value`]. It can be
/// consumed only by syntax whose contract explicitly accepts a handle (for
/// example `directives.depends_on` or a function/module argument boundary).
/// The argument shape stays typed independently of its annotation; every
/// ordinary value boundary rejects a bare handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnexpandedStructuralHandle {
    reference: UnexpandedStructuralReference,
    kind: UnexpandedStructuralHandleKind,
    resource_type: Option<ResourceTypePath>,
}

/// Parser/pre-expansion representation of a managed resource capability.
///
/// Unlike [`UnexpandedStructuralHandle`], this type cannot represent module
/// calls, providers, waits, or value parameters. It is the only structural
/// operand accepted by module-output and dependency boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnexpandedResourceHandle {
    reference: UnexpandedStructuralReference,
    resource_type: ResourceTypePath,
    kind: UnexpandedStructuralHandleKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnexpandedStructuralHandleKind {
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

impl UnexpandedStructuralHandleKind {
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
pub(crate) enum UnexpandedStructuralHandleMetadata {
    Handle { access_base: Option<Value> },
    List(Vec<Option<UnexpandedStructuralHandleMetadata>>),
    Map(IndexMap<String, UnexpandedStructuralHandleMetadata>),
}

impl UnexpandedStructuralHandleMetadata {
    pub(crate) fn is_handle(&self) -> bool {
        matches!(self, Self::Handle { .. })
    }

    pub(crate) fn access_base(&self) -> Option<&Value> {
        match self {
            Self::Handle { access_base } => access_base.as_ref(),
            Self::List(_) | Self::Map(_) => None,
        }
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
            (Self::Handle { .. }, _) | (Self::List(_), _) | (Self::Map(_), _) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnexpandedStructuralReference {
    LocalBinding(BindingName),
    ModuleArgument(BindingName),
    ModuleAttribute {
        call: BindingName,
        attribute: String,
    },
    Alias {
        alias: BindingName,
        target: Box<Self>,
    },
}

impl UnexpandedStructuralReference {
    fn canonical_name(&self) -> Cow<'_, str> {
        match self {
            Self::LocalBinding(binding) | Self::ModuleArgument(binding) => {
                Cow::Borrowed(binding.as_str())
            }
            Self::ModuleAttribute { call, attribute } => Cow::Owned(format!("{call}.{attribute}")),
            Self::Alias { target, .. } => target.canonical_name(),
        }
    }

    fn alias_name(&self) -> Option<&str> {
        match self {
            Self::Alias { alias, .. } => Some(alias.as_str()),
            Self::LocalBinding(_) | Self::ModuleArgument(_) | Self::ModuleAttribute { .. } => None,
        }
    }
}

impl UnexpandedStructuralHandle {
    pub(crate) fn new(kind: UnexpandedStructuralHandleKind, name: impl Into<String>) -> Self {
        let resource_type = match &kind {
            UnexpandedStructuralHandleKind::Resource(resource_type) => Some(resource_type.clone()),
            UnexpandedStructuralHandleKind::For
            | UnexpandedStructuralHandleKind::If
            | UnexpandedStructuralHandleKind::Use
            | UnexpandedStructuralHandleKind::Module
            | UnexpandedStructuralHandleKind::Wait
            | UnexpandedStructuralHandleKind::UpstreamState
            | UnexpandedStructuralHandleKind::Provider
            | UnexpandedStructuralHandleKind::ModuleParameter
            | UnexpandedStructuralHandleKind::FunctionParameter => None,
        };
        Self {
            reference: UnexpandedStructuralReference::LocalBinding(name.into().into()),
            kind,
            resource_type,
        }
    }

    pub(crate) fn module_parameter(
        name: impl Into<String>,
        resource_type: ResourceTypePath,
    ) -> Self {
        Self {
            reference: UnexpandedStructuralReference::ModuleArgument(name.into().into()),
            kind: UnexpandedStructuralHandleKind::ModuleParameter,
            resource_type: Some(resource_type),
        }
    }

    pub(crate) fn module_attribute(
        call: impl Into<String>,
        attribute: impl AsRef<str>,
        resource_type: ResourceTypePath,
    ) -> Self {
        let call = call.into();
        Self {
            reference: UnexpandedStructuralReference::ModuleAttribute {
                call: BindingName::from(call),
                attribute: attribute.as_ref().to_string(),
            },
            kind: UnexpandedStructuralHandleKind::Resource(resource_type.clone()),
            resource_type: Some(resource_type),
        }
    }

    pub(crate) fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.reference = UnexpandedStructuralReference::Alias {
            alias: BindingName::from(alias.into()),
            target: Box::new(self.reference),
        };
        self
    }

    pub(crate) fn addressable_binding(&self) -> Option<&str> {
        match &self.reference {
            UnexpandedStructuralReference::LocalBinding(binding)
            | UnexpandedStructuralReference::ModuleArgument(binding) => Some(binding.as_str()),
            UnexpandedStructuralReference::ModuleAttribute { .. }
            | UnexpandedStructuralReference::Alias { .. } => None,
        }
    }

    pub(crate) fn alias_name(&self) -> Option<&str> {
        self.reference.alias_name()
    }

    pub(crate) fn canonical_binding_name(&self) -> Cow<'_, str> {
        self.reference.canonical_name()
    }

    pub(crate) fn kind(&self) -> &UnexpandedStructuralHandleKind {
        &self.kind
    }

    pub(crate) fn resource_handle(&self) -> Option<UnexpandedResourceHandle> {
        Some(UnexpandedResourceHandle {
            reference: self.reference.clone(),
            resource_type: self.resource_type.clone()?,
            kind: self.kind.clone(),
        })
    }

    pub(crate) fn is_for_binding(&self) -> bool {
        self.kind == UnexpandedStructuralHandleKind::For
    }

    pub(crate) fn is_parameter_handle(&self) -> bool {
        matches!(
            &self.kind,
            UnexpandedStructuralHandleKind::ModuleParameter
                | UnexpandedStructuralHandleKind::FunctionParameter
        )
    }

    /// Return the canonical reference operand used only for postfix access.
    pub(crate) fn access_base_value(&self) -> Value {
        let binding = self.canonical_binding_name();
        match &self.kind {
            UnexpandedStructuralHandleKind::ModuleParameter => {
                Value::Deferred(crate::resource::DeferredValue::Unknown(
                    crate::resource::UnknownReason::ModuleArgument {
                        name: binding.into_owned(),
                        source_line: (0).into(),
                        diagnostic_name: None,
                    },
                ))
            }
            UnexpandedStructuralHandleKind::FunctionParameter => Value::Deferred(
                crate::resource::DeferredValue::Unknown(crate::resource::UnknownReason::FnParam {
                    name: binding.into_owned(),
                }),
            ),
            _ => Value::Deferred(crate::resource::DeferredValue::BindingRef {
                binding: binding.into_owned(),
            }),
        }
    }

    pub(crate) fn bare_error(&self) -> BareStructuralHandle {
        let binding = self
            .alias_name()
            .map(Cow::Borrowed)
            .unwrap_or_else(|| self.canonical_binding_name())
            .to_string();
        BareStructuralHandle {
            binding,
            kind: self.kind.clone(),
        }
    }
}

impl UnexpandedResourceHandle {
    pub(crate) fn new(name: impl Into<String>, resource_type: ResourceTypePath) -> Self {
        Self {
            reference: UnexpandedStructuralReference::LocalBinding(name.into().into()),
            kind: UnexpandedStructuralHandleKind::Resource(resource_type.clone()),
            resource_type,
        }
    }

    pub fn resource_type(&self) -> &ResourceTypePath {
        &self.resource_type
    }

    pub(crate) fn canonical_binding_name(&self) -> Cow<'_, str> {
        self.reference.canonical_name()
    }

    pub(crate) fn diagnostic_name(&self) -> Cow<'_, str> {
        self.reference
            .alias_name()
            .map(Cow::Borrowed)
            .unwrap_or_else(|| self.reference.canonical_name())
    }

    pub(crate) fn with_canonical_binding(mut self, binding: impl Into<String>) -> Self {
        self.reference = UnexpandedStructuralReference::LocalBinding(binding.into().into());
        self
    }

    pub(crate) fn into_structural(self) -> UnexpandedStructuralHandle {
        UnexpandedStructuralHandle {
            reference: self.reference,
            kind: self.kind,
            resource_type: Some(self.resource_type),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BareStructuralHandle {
    pub(crate) binding: String,
    pub(crate) kind: UnexpandedStructuralHandleKind,
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
pub(crate) enum EvalValue {
    /// A user-facing value. Anything reachable from `Value` is reachable
    /// here without loss.
    User(Value),

    /// A structural operand that has not crossed the public `Value` boundary.
    UnexpandedStructuralHandle(UnexpandedStructuralHandle),

    /// A typed value sink reserved by the parser because a bare structural
    /// binding occurred somewhere below it. The owning resource attribute or
    /// module argument consumes this marker instead of manufacturing a public
    /// [`Value`] placeholder.
    PendingValue(PendingValueSlot),

    /// A directory-scoped name whose declaration kind is not known yet.
    /// This is deliberately distinct from `UnexpandedStructuralHandle`: the fixed-point
    /// pass may prove that it names either an ordinary value or a structure.
    ProvisionalReference { binding: String },

    /// A computed value whose construction or selection includes one or more
    /// provisional handles. The parallel metadata tree survives parser-local
    /// lets, `for` bindings, and directory seed passes until their kinds are
    /// known.
    ComputedWithUnexpandedStructuralHandles {
        value: Value,
        metadata: UnexpandedStructuralHandleMetadata,
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
        captured_args: Vec<EvalValue>,
        /// How many more arguments the function still needs.
        remaining_arity: usize,
    },
}

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
pub(crate) enum ParsedSinkValue {
    Ready(Value),
    Pending(PendingValueSlot),
}

impl EvalValue {
    /// Wrap a user-facing `Value`. This is the cheap, infallible
    /// direction — every `Value` is a valid `EvalValue`.
    pub(crate) fn from_value(value: Value) -> Self {
        Self::User(value)
    }

    /// Build a closure value from its parts.
    pub(crate) fn closure(
        name: impl Into<String>,
        captured_args: Vec<EvalValue>,
        remaining_arity: usize,
    ) -> Self {
        Self::Closure {
            name: name.into(),
            captured_args,
            remaining_arity,
        }
    }

    pub(crate) fn structural_handle(handle: UnexpandedStructuralHandle) -> Self {
        Self::UnexpandedStructuralHandle(handle)
    }

    pub(crate) fn pending_value(slot: PendingValueSlot) -> Self {
        Self::PendingValue(slot)
    }

    pub(crate) fn provisional_reference(binding: impl Into<String>) -> Self {
        Self::ProvisionalReference {
            binding: binding.into(),
        }
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

    pub(crate) fn computed(
        value: Value,
        metadata: Option<UnexpandedStructuralHandleMetadata>,
    ) -> Self {
        match metadata {
            Some(metadata) => Self::ComputedWithUnexpandedStructuralHandles { value, metadata },
            None => Self::User(value),
        }
    }

    pub(crate) fn deferred_for_value(binding: impl Into<String>) -> Self {
        Self::DeferredForValue {
            binding: binding.into(),
        }
    }

    pub(crate) fn structural_handle_ref(&self) -> Option<&UnexpandedStructuralHandle> {
        match self {
            Self::UnexpandedStructuralHandle(handle) => Some(handle),
            _ => None,
        }
    }

    pub(crate) fn provisional_reference_name(&self) -> Option<&str> {
        match self {
            Self::ProvisionalReference { binding } => Some(binding),
            _ => None,
        }
    }

    pub(crate) fn structural_handle_metadata(&self) -> Option<&UnexpandedStructuralHandleMetadata> {
        match self {
            Self::ComputedWithUnexpandedStructuralHandles { metadata, .. } => Some(metadata),
            Self::User(_)
            | Self::UnexpandedStructuralHandle(_)
            | Self::PendingValue(_)
            | Self::ProvisionalReference { .. }
            | Self::DeferredForValue { .. }
            | Self::Closure { .. } => None,
        }
    }

    pub(crate) fn into_collection_element(
        self,
    ) -> Result<(Value, Option<UnexpandedStructuralHandleMetadata>), EvalValueError> {
        match self {
            Self::User(value) => {
                let metadata = matches!(
                    value,
                    Value::Deferred(crate::resource::DeferredValue::BindingRef { .. })
                )
                .then(|| UnexpandedStructuralHandleMetadata::Handle {
                    access_base: Some(value.clone()),
                });
                Ok((value, metadata))
            }
            Self::UnexpandedStructuralHandle(handle) => {
                Err(EvalValueError::BareStructuralHandle(handle.bare_error()))
            }
            Self::PendingValue(slot) => Err(EvalValueError::PendingValue(Box::new(slot))),
            Self::ProvisionalReference { binding } => {
                let value = Value::Deferred(crate::resource::DeferredValue::BindingRef { binding });
                Ok((
                    value.clone(),
                    Some(UnexpandedStructuralHandleMetadata::Handle {
                        access_base: Some(value),
                    }),
                ))
            }
            Self::ComputedWithUnexpandedStructuralHandles { value, metadata } => {
                Ok((value, Some(metadata)))
            }
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
    pub(crate) fn module_argument_access_base(&self) -> Option<Value> {
        match self {
            Self::UnexpandedStructuralHandle(handle) => Some(handle.access_base_value()),
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
            Self::ComputedWithUnexpandedStructuralHandles { metadata, .. } => {
                metadata.access_base().cloned()
            }
            _ => None,
        }
    }

    pub(crate) fn into_structural_handle(self) -> Option<UnexpandedStructuralHandle> {
        match self {
            Self::UnexpandedStructuralHandle(handle) => Some(handle),
            _ => None,
        }
    }

    pub(crate) fn into_sink_value(self) -> Result<ParsedSinkValue, EvalValueError> {
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
    pub(crate) fn into_value(self) -> Result<Value, EvalValueError> {
        match self {
            Self::User(v) => Ok(v),
            Self::UnexpandedStructuralHandle(handle) => {
                Err(EvalValueError::BareStructuralHandle(handle.bare_error()))
            }
            Self::PendingValue(slot) => Err(EvalValueError::PendingValue(Box::new(slot))),
            Self::ProvisionalReference { binding } => Ok(Value::Deferred(
                crate::resource::DeferredValue::BindingRef { binding },
            )),
            Self::ComputedWithUnexpandedStructuralHandles { value, .. } => Ok(value),
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

impl From<Value> for EvalValue {
    fn from(value: Value) -> Self {
        Self::User(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{ConcreteValue, Value};

    #[test]
    fn user_round_trips() {
        let original = Value::Concrete(ConcreteValue::String("hi".into()));
        let eval = EvalValue::from_value(original.clone());
        assert_eq!(eval.into_value().unwrap(), original);
    }

    #[test]
    fn closure_into_value_returns_leak() {
        let eval = EvalValue::closure("join", vec![], 2);
        let err = eval.into_value().unwrap_err();
        let EvalValueError::Closure(leak) = err else {
            panic!("expected closure error");
        };
        assert_eq!(leak.name, "join");
        assert_eq!(leak.remaining_arity, 2);
    }
}
