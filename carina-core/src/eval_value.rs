//! Evaluator-internal values.
//!
//! `Value` is the public, user-facing type that flows through `ParsedFile`,
//! `Resource.attributes`, plan display, state serialization, and the LSP.
//! Every variant of `Value` is something a `.crn` author can directly
//! observe in their configuration.
//!
//! `EvalValue` is the strictly internal type used while the evaluator is
//! reducing expressions. In addition to `Closure` for partial application it
//! carries parser-only structural handles until the exact boundary where they
//! become their legacy string transport value. Closures are produced when a
//! function receives fewer arguments than its arity, and are consumed by the
//! next pipe / call that finishes the application.
//!
//! The split is enforced by the type system: a `Closure` cannot appear
//! anywhere a `Value` is expected. The boundary is `EvalValue::into_value`,
//! which returns `Err(ClosureLeak)` if a closure tried to escape the
//! evaluator. Consumers that match on `Value` therefore never need a
//! `Closure` arm — the variant does not exist for them.
//!
//! See #2230 for the design discussion.

use crate::resource::{ConcreteValue, PathSegment, Subscript, Value};
use indexmap::IndexMap;

/// Parser-only representation of a structural binding used as an operand.
///
/// `Value` deliberately keeps the historical provider/state transport spelling
/// (`"${vpc}"`, `"${for:roles}"`, ...).  Keeping that spelling out of the
/// evaluator is what lets postfix access distinguish `(vpc).identifier` from
/// indexing an ordinary string without changing any persisted/runtime bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructuralHandle {
    reference: StructuralReference,
    transport: StructuralTransport,
}

/// Parser-only shape metadata for structural handles nested in a computed
/// value. The runtime [`Value`] keeps the historical sentinel/reference bytes;
/// this parallel *typed* tree exists only until an expression boundary needs
/// to decide whether an access targets data or a resource handle.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StructuralHandleMetadata {
    Handle { access_base: Option<Value> },
    List(Vec<Option<StructuralHandleMetadata>>),
    Map(IndexMap<String, StructuralHandleMetadata>),
}

impl StructuralHandleMetadata {
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
enum StructuralReference {
    Addressable(String),
    Alias(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StructuralTransport {
    /// Bindings carried only by a dedicated parser side-channel were not
    /// entered in main's value table.  A bare use therefore retained the
    /// ordinary identifier value, even though postfix access still treats the
    /// name as an addressable structural operand.
    BareIdentifier(String),
    Binding(String),
    For(String),
    If(String),
    Use(String),
    Module(String),
    /// Main's directory pass represented structural names declared in a
    /// sibling file as a binding reference. Preserve that public Value shape
    /// while retaining typed structural behavior inside the parser.
    SeededReference(String),
    /// Syntax-only directory scope seed used before the first semantic pass.
    /// Its declaration kind is not known yet, so it may stand in for a
    /// deferred `for` iterable. It is never retained after the fixed point.
    Provisional(String),
}

impl StructuralHandle {
    pub(crate) fn binding(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            reference: StructuralReference::Addressable(name.clone()),
            transport: StructuralTransport::Binding(name),
        }
    }

    pub(crate) fn for_binding(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            reference: StructuralReference::Addressable(name.clone()),
            transport: StructuralTransport::For(name),
        }
    }

    pub(crate) fn side_channel(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            reference: StructuralReference::Addressable(name.clone()),
            transport: StructuralTransport::BareIdentifier(name),
        }
    }

    pub(crate) fn if_binding(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            reference: StructuralReference::Addressable(name.clone()),
            transport: StructuralTransport::If(name),
        }
    }

    pub(crate) fn use_binding(binding: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            reference: StructuralReference::Addressable(binding.into()),
            transport: StructuralTransport::Use(path.into()),
        }
    }

    pub(crate) fn module_binding(
        binding: impl Into<String>,
        module_name: impl Into<String>,
    ) -> Self {
        Self {
            reference: StructuralReference::Addressable(binding.into()),
            transport: StructuralTransport::Module(module_name.into()),
        }
    }

    /// Rehydrate parser-only type information at the directory seed seam.
    /// The input is known to be a structural binding value collected from an
    /// earlier parse pass; arbitrary expression values never enter here.
    pub(crate) fn from_seed(binding: &str) -> Self {
        Self {
            reference: StructuralReference::Addressable(binding.to_string()),
            transport: StructuralTransport::SeededReference(binding.to_string()),
        }
    }

    pub(crate) fn provisional(binding: &str) -> Self {
        Self {
            reference: StructuralReference::Addressable(binding.to_string()),
            transport: StructuralTransport::Provisional(binding.to_string()),
        }
    }

    pub(crate) fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.reference = StructuralReference::Alias(alias.into());
        self
    }

    pub(crate) fn addressable_binding(&self) -> Option<&str> {
        match &self.reference {
            StructuralReference::Addressable(binding) => Some(binding),
            StructuralReference::Alias(_) => None,
        }
    }

    pub(crate) fn alias_name(&self) -> Option<&str> {
        match &self.reference {
            StructuralReference::Alias(alias) => Some(alias),
            StructuralReference::Addressable(_) => None,
        }
    }

    pub(crate) fn for_binding_name(&self) -> Option<&str> {
        match &self.transport {
            StructuralTransport::For(binding) => Some(binding),
            _ => None,
        }
    }

    pub(crate) fn provisional_binding_name(&self) -> Option<&str> {
        match &self.transport {
            StructuralTransport::Provisional(binding) => Some(binding),
            _ => None,
        }
    }

    /// Return the canonical reference operand for postfix access without
    /// crossing the legacy bare-handle transport boundary.
    pub(crate) fn access_base_value(&self) -> Option<Value> {
        self.addressable_binding().map(|binding| {
            Value::Deferred(crate::resource::DeferredValue::BindingRef {
                binding: binding.to_string(),
            })
        })
    }

    /// The sole parser lowering seam for structural operands.  Value-backed
    /// handles manufacture the historical sentinel stored in `Value` here;
    /// side-channel-only bindings retain main's bare identifier value.
    pub(crate) fn into_value(self) -> Value {
        let token = match self.transport {
            StructuralTransport::BareIdentifier(name) => {
                return Value::Concrete(ConcreteValue::enum_identifier(name));
            }
            StructuralTransport::SeededReference(binding)
            | StructuralTransport::Provisional(binding) => {
                return Value::Deferred(crate::resource::DeferredValue::BindingRef { binding });
            }
            StructuralTransport::Binding(binding) => binding,
            StructuralTransport::For(binding) => format!("for:{binding}"),
            StructuralTransport::If(binding) => format!("if:{binding}"),
            StructuralTransport::Use(path) => format!("use:{path}"),
            StructuralTransport::Module(module) => format!("module:{module}"),
        };
        Value::Concrete(ConcreteValue::String(format!("${{{token}}}").into()))
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
    StructuralHandle(StructuralHandle),

    /// A computed value whose construction or selection lowered one or more
    /// structural handles. The parallel metadata tree survives parser-local
    /// lets, `for` bindings, and directory seed passes, while the public value
    /// remains the historical sentinel/reference representation.
    ComputedWithStructuralHandles {
        value: Value,
        metadata: StructuralHandleMetadata,
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

/// Returned by `EvalValue::into_value` when a closure would otherwise
/// escape the evaluator. The caller should propagate this as a
/// configuration error: a closure that doesn't get applied means the
/// user wrote a partial application without finishing it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClosureLeak {
    pub name: String,
    pub remaining_arity: usize,
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

    pub(crate) fn structural_handle(handle: StructuralHandle) -> Self {
        Self::StructuralHandle(handle)
    }

    pub(crate) fn computed(value: Value, metadata: Option<StructuralHandleMetadata>) -> Self {
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

    pub(crate) fn structural_handle_ref(&self) -> Option<&StructuralHandle> {
        match self {
            Self::StructuralHandle(handle) => Some(handle),
            _ => None,
        }
    }

    pub(crate) fn structural_handle_metadata(&self) -> Option<&StructuralHandleMetadata> {
        match self {
            Self::ComputedWithStructuralHandles { metadata, .. } => Some(metadata),
            Self::User(_)
            | Self::StructuralHandle(_)
            | Self::DeferredForValue { .. }
            | Self::Closure { .. } => None,
        }
    }

    pub(crate) fn into_collection_element(
        self,
    ) -> Result<(Value, Option<StructuralHandleMetadata>), ClosureLeak> {
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
                let access_base = handle.access_base_value();
                Ok((
                    handle.into_value(),
                    Some(StructuralHandleMetadata::Handle { access_base }),
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
            } => Err(ClosureLeak {
                name,
                remaining_arity,
            }),
        }
    }

    /// Preserve the typed operand needed when a module parameter is later
    /// accessed inside the module. The public argument value still lowers to
    /// the historical sentinel; this side-channel prevents module expansion
    /// from trying to navigate that sentinel as a string.
    pub(crate) fn module_argument_access_base(&self) -> Option<Value> {
        match self {
            Self::StructuralHandle(handle) => handle.access_base_value(),
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

    pub(crate) fn into_structural_handle(self) -> Option<StructuralHandle> {
        match self {
            Self::StructuralHandle(handle) => Some(handle),
            _ => None,
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
    pub(crate) fn into_value(self) -> Result<Value, ClosureLeak> {
        match self {
            Self::User(v) => Ok(v),
            Self::StructuralHandle(handle) => Ok(handle.into_value()),
            Self::ComputedWithStructuralHandles { value, .. } => Ok(value),
            Self::DeferredForValue { .. } => Ok(Value::Deferred(
                crate::resource::DeferredValue::Unknown(crate::resource::UnknownReason::ForValue),
            )),
            Self::Closure {
                name,
                remaining_arity,
                ..
            } => Err(ClosureLeak {
                name,
                remaining_arity,
            }),
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
        assert_eq!(err.name, "join");
        assert_eq!(err.remaining_arity, 2);
    }
}
