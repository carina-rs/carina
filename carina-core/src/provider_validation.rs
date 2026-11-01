//! Read-only validation inputs without access to provider request payloads.

use std::collections::BTreeMap;
use std::fmt;

use carina_provider_protocol::types::Value as Wire;
use indexmap::IndexMap;

use crate::provider::{ProviderReady, ResourceWirePayload, WireAttributes};
use crate::resource::{ConcreteValue, DeferredValue, ResourceId, Value, ValueString};

/// A validation-only view of checked desired inputs.
///
/// This SDK bridge visits typed validation values; it exposes neither the
/// backing resource nor its core attribute map.
///
/// ```compile_fail,E0599
/// use carina_core::provider::UpdateRequest;
/// fn bypass(request: &UpdateRequest) {
///     let _ = request.validation_inputs().attributes();
/// }
/// ```
pub struct ProviderValidationInputs<'a> {
    id: &'a ResourceId,
    originals: &'a IndexMap<String, Value>,
    wire: &'a WireAttributes,
}

impl<'a> ProviderValidationInputs<'a> {
    pub fn from_provider_ready<T: ResourceWirePayload>(resource: &'a ProviderReady<T>) -> Self {
        Self {
            id: resource.as_inner().resource_id(),
            originals: resource.as_inner().attributes(),
            wire: resource.wire_attributes(),
        }
    }

    pub fn id(&self) -> &ResourceId {
        self.id
    }

    /// Visit attributes in the protocol's canonical key order.
    pub fn visit(&self, mut visit: impl FnMut(&str, ProviderValidationValue<'a>)) {
        for (name, wire) in self.wire.iter() {
            visit(name, validation_value(&self.originals[name], wire.value()));
        }
    }
}

impl fmt::Debug for ProviderValidationInputs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderValidationInputs { .. }")
    }
}

/// A validation value with secret annotations kept opaque.
#[derive(Debug)]
pub enum ProviderValidationValue<'a> {
    String(ValueString),
    Int(i64),
    Float(f64),
    Bool(bool),
    Deferred,
    List(ProviderValidationList<'a>),
    Map(ProviderValidationMap<'a>),
}

/// A list that exposes validation values, never backing core values.
pub struct ProviderValidationList<'a>(ListValues<'a>);

enum ListValues<'a> {
    List(&'a [Value], &'a [Wire]),
    Strings(&'a [ValueString]),
}

impl<'a> ProviderValidationList<'a> {
    pub fn iter(&self) -> impl Iterator<Item = ProviderValidationValue<'a>> + '_ {
        let len = match self.0 {
            ListValues::List(_, values) => values.len(),
            ListValues::Strings(values) => values.len(),
        };
        (0..len).map(|index| match self.0 {
            ListValues::List(originals, values) => {
                validation_value(&originals[index], &values[index])
            }
            ListValues::Strings(originals) => {
                ProviderValidationValue::String(originals[index].clone())
            }
        })
    }
}

impl fmt::Debug for ProviderValidationList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderValidationList { .. }")
    }
}

/// A map that exposes validation values in protocol key order.
pub struct ProviderValidationMap<'a> {
    originals: &'a IndexMap<String, Value>,
    wire: &'a BTreeMap<String, Wire>,
}

impl<'a> ProviderValidationMap<'a> {
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, ProviderValidationValue<'a>)> + '_ {
        let originals = self.originals;
        self.wire
            .iter()
            .map(move |(name, wire)| (name.as_str(), validation_value(&originals[name], wire)))
    }
}

impl fmt::Debug for ProviderValidationMap<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderValidationMap { .. }")
    }
}

// Original values supply provenance, never a second representation policy.
// Keep every core variant explicit so new variants require a decision here.
enum Provenance<'a> {
    Scalar,
    String(&'a ValueString),
    StringList(&'a [ValueString]),
    List(&'a [Value]),
    Map(&'a IndexMap<String, Value>),
    Secret,
}

fn provenance(value: &Value) -> Provenance<'_> {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => Provenance::String(value),
        Value::Concrete(ConcreteValue::StringList(values)) => Provenance::StringList(values),
        Value::Concrete(ConcreteValue::List(values)) => Provenance::List(values),
        Value::Concrete(ConcreteValue::Map(values)) => Provenance::Map(values),
        Value::Concrete(
            ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_),
        ) => Provenance::Scalar,
        Value::Deferred(DeferredValue::Secret(_)) => Provenance::Secret,
        Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Deferred(
            DeferredValue::ModuleIf(_)
            | DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Coalesce { .. }
            | DeferredValue::IntegerArithmetic(_)
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => unreachable!("the wire witness excludes unresolved desired values"),
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
    }
}

fn validation_value<'a>(original: &'a Value, wire: &'a Wire) -> ProviderValidationValue<'a> {
    let original = provenance(original);
    if matches!(original, Provenance::Secret) {
        return ProviderValidationValue::Deferred;
    }
    match wire {
        Wire::Bool(value) => ProviderValidationValue::Bool(*value),
        Wire::Int(value) => ProviderValidationValue::Int(*value),
        Wire::Float(value) => ProviderValidationValue::Float(*value),
        Wire::String(value) => {
            let value = match original {
                Provenance::String(value) => value.clone(),
                Provenance::Scalar => value.clone().into(),
                Provenance::StringList(_)
                | Provenance::List(_)
                | Provenance::Map(_)
                | Provenance::Secret => unreachable!("checked string provenance"),
            };
            ProviderValidationValue::String(value)
        }
        Wire::StringList(_) => {
            let Provenance::StringList(values) = original else {
                unreachable!("checked string-list provenance")
            };
            ProviderValidationValue::List(ProviderValidationList(ListValues::Strings(values)))
        }
        Wire::List(values) => {
            let Provenance::List(originals) = original else {
                unreachable!("checked list provenance")
            };
            ProviderValidationValue::List(ProviderValidationList(ListValues::List(
                originals, values,
            )))
        }
        Wire::Map(wire) => {
            let Provenance::Map(originals) = original else {
                unreachable!("checked map provenance")
            };
            ProviderValidationValue::Map(ProviderValidationMap { originals, wire })
        }
    }
}
