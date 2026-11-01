//! The immutable proof that a complete provider input has a wire representation.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use carina_provider_protocol::types as protocol;
use indexmap::IndexMap;

use crate::protocol_convert::{ProtocolConvertError, WirePathSegment, try_core_to_protocol_value};
use crate::provider::{PatchOp, UpdatePatch};
use crate::resource::{
    ConcreteValue, DeferredValue, ProviderModuleSource, ResolvedDataSource, ResolvedResource,
    ResourceId, Value,
};

/// The owner of an input that failed wire preparation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderWireTarget {
    Resource(ResourceId),
    Configuration { provider: String },
}

impl fmt::Display for ProviderWireTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resource(id) => id.fmt(f),
            Self::Configuration { provider } => write!(f, "provider {provider} configuration"),
        }
    }
}

/// A terminal diagnostic: the conversion reason is rendered exactly once.
///
/// Accessors retain the typed reason and path without exposing input values.
#[derive(Debug)]
pub struct ProviderWireError {
    target: Box<ProviderWireTarget>,
    attribute: String,
    path: Vec<WirePathSegment>,
    error: Box<ProtocolConvertError>,
}

impl ProviderWireError {
    fn new(target: &ProviderWireTarget, attribute: &str, error: ProtocolConvertError) -> Self {
        let path = error.value_path();
        Self {
            target: Box::new(target.clone()),
            attribute: attribute.into(),
            path,
            error: Box::new(error),
        }
    }

    pub fn target(&self) -> &ProviderWireTarget {
        &self.target
    }
    pub fn resource(&self) -> Option<&ResourceId> {
        match self.target.as_ref() {
            ProviderWireTarget::Resource(id) => Some(id),
            ProviderWireTarget::Configuration { .. } => None,
        }
    }
    pub fn attribute(&self) -> &str {
        &self.attribute
    }
    pub fn path(&self) -> &[WirePathSegment] {
        &self.path
    }
    pub fn conversion_error(&self) -> &ProtocolConvertError {
        self.error.leaf_error()
    }

    pub fn attribute_path(&self) -> String {
        fn append_key(path: &mut String, key: &str) {
            let mut chars = key.chars();
            let identifier = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            if identifier {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(key);
            } else {
                path.push('[');
                path.push_str(
                    &serde_json::to_string(key).expect("map keys serialize as JSON strings"),
                );
                path.push(']');
            }
        }
        let mut path = String::new();
        append_key(&mut path, &self.attribute);
        for segment in &self.path {
            use std::fmt::Write;
            match segment {
                WirePathSegment::Key(key) => {
                    append_key(&mut path, key);
                }
                WirePathSegment::Index(index) => {
                    let _ = write!(path, "[{index}]");
                }
            }
        }
        path
    }
}

impl fmt::Display for ProviderWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: attribute `{}` cannot cross the provider wire: {}",
            self.target,
            self.attribute_path(),
            self.conversion_error()
        )
    }
}

impl std::error::Error for ProviderWireError {}

/// A projected value with its top-level secret annotation retained for WIT.
#[derive(Clone)]
pub struct WireValue {
    value: protocol::Value,
    secret: bool,
}

impl WireValue {
    pub fn value(&self) -> &protocol::Value {
        &self.value
    }
    pub fn is_secret(&self) -> bool {
        self.secret
    }
}

impl fmt::Debug for WireValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WireValue { .. }")
    }
}

/// Checked attributes. Neither entries nor projected values can be replaced.
#[derive(Debug, Clone)]
pub struct WireAttributes(Arc<BTreeMap<String, WireValue>>);

impl WireAttributes {
    pub fn iter(&self) -> impl Iterator<Item = (&String, &WireValue)> {
        self.0.iter()
    }
    pub fn get(&self, attribute: &str) -> Option<&WireValue> {
        self.0.get(attribute)
    }
}

/// An update operation whose sent value has already passed the wire check.
#[derive(Debug, Clone)]
pub enum WirePatchOp {
    Add { key: String, value: WireValue },
    Replace { key: String, value: WireValue },
    Reassert { key: String, value: WireValue },
    Remove { key: String },
}

mod sealed {
    pub trait Sealed {}
}

/// The closed set of payloads admitted by [`WireReady`].
pub trait ProviderWirePayload: sealed::Sealed {
    type Projection: Clone;
    #[doc(hidden)]
    fn project(&self, target: &ProviderWireTarget) -> Result<Self::Projection, ProviderWireError>;
}

/// Managed and data-source payloads accepted by the shared SDK input view.
pub trait ResourceWirePayload: ProviderWirePayload<Projection = WireAttributes> {
    fn resource_id(&self) -> &ResourceId;
    fn attributes(&self) -> &IndexMap<String, Value>;
}

/// An owned input paired with the exact checked projection used for dispatch.
///
/// The only constructor runs the complete wire projection. There is no mutable
/// access, unchecked conversion, default, or deserialization path.
///
/// ```compile_fail
/// use carina_core::provider::{WireReady, UpdatePatch};
/// fn bypass(raw: UpdatePatch) -> WireReady<UpdatePatch> { raw.into() }
/// ```
///
/// ```compile_fail
/// use carina_core::provider::{WireReady, UpdatePatch};
/// fn forge(raw: UpdatePatch) -> WireReady<UpdatePatch> {
///     WireReady { value: raw, wire: Vec::new() }
/// }
/// ```
///
/// ```compile_fail
/// use carina_core::provider::{WireReady, UpdatePatch};
/// fn mutate(ready: &mut WireReady<UpdatePatch>) {
///     ready.as_inner().ops.clear();
/// }
/// ```
#[derive(Clone)]
pub struct WireReady<T: ProviderWirePayload> {
    value: T,
    wire: T::Projection,
}

impl<T: ProviderWirePayload> WireReady<T> {
    pub(crate) fn try_new(value: T, target: ProviderWireTarget) -> Result<Self, ProviderWireError> {
        let wire = value.project(&target)?;
        Ok(Self { value, wire })
    }
    pub fn as_inner(&self) -> &T {
        &self.value
    }
    pub fn wire(&self) -> &T::Projection {
        &self.wire
    }
}

impl<T: ProviderWirePayload + PartialEq> PartialEq for WireReady<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: ProviderWirePayload> fmt::Debug for WireReady<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WireReady { .. }")
    }
}

// Secret is a transport annotation supported by WIT. Check its complete
// plaintext while retaining the original core tree for provenance and SDK
// opaque-leaf views. Nested annotations are transparent inside WIT JSON.
fn plaintext(value: &Value) -> Value {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => plaintext(inner),
        Value::Concrete(ConcreteValue::List(values)) => {
            Value::Concrete(ConcreteValue::List(values.iter().map(plaintext).collect()))
        }
        Value::Concrete(ConcreteValue::Map(values)) => Value::Concrete(ConcreteValue::Map(
            values
                .iter()
                .map(|(key, value)| (key.clone(), plaintext(value)))
                .collect(),
        )),
        value => value.clone(),
    }
}

fn project_value(
    value: &Value,
    target: &ProviderWireTarget,
    attribute: &str,
) -> Result<WireValue, ProviderWireError> {
    let projected = try_core_to_protocol_value(&plaintext(value))
        .map_err(|error| ProviderWireError::new(target, attribute, error))?;
    Ok(WireValue {
        value: projected,
        secret: matches!(value, Value::Deferred(DeferredValue::Secret(_))),
    })
}

fn project_attributes(
    attributes: &IndexMap<String, Value>,
    target: &ProviderWireTarget,
) -> Result<WireAttributes, ProviderWireError> {
    attributes
        .iter()
        .map(|(key, value)| project_value(value, target, key).map(|value| (key.clone(), value)))
        .collect::<Result<_, _>>()
        .map(|attributes| WireAttributes(Arc::new(attributes)))
}

impl sealed::Sealed for ResolvedResource<ProviderModuleSource> {}
impl ProviderWirePayload for ResolvedResource<ProviderModuleSource> {
    type Projection = WireAttributes;
    fn project(&self, target: &ProviderWireTarget) -> Result<Self::Projection, ProviderWireError> {
        project_attributes(&self.as_resource().attributes, target)
    }
}
impl ResourceWirePayload for ResolvedResource<ProviderModuleSource> {
    fn resource_id(&self) -> &ResourceId {
        &self.as_resource().id
    }
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.as_resource().attributes
    }
}

impl sealed::Sealed for ResolvedDataSource<ProviderModuleSource> {}
impl ProviderWirePayload for ResolvedDataSource<ProviderModuleSource> {
    type Projection = WireAttributes;
    fn project(&self, target: &ProviderWireTarget) -> Result<Self::Projection, ProviderWireError> {
        project_attributes(&self.as_inner().attributes, target)
    }
}
impl ResourceWirePayload for ResolvedDataSource<ProviderModuleSource> {
    fn resource_id(&self) -> &ResourceId {
        &self.as_inner().id
    }
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.as_inner().attributes
    }
}

impl sealed::Sealed for IndexMap<String, Value> {}
impl ProviderWirePayload for IndexMap<String, Value> {
    type Projection = WireAttributes;
    fn project(&self, target: &ProviderWireTarget) -> Result<Self::Projection, ProviderWireError> {
        project_attributes(self, target)
    }
}

impl sealed::Sealed for UpdatePatch {}
impl ProviderWirePayload for UpdatePatch {
    type Projection = Vec<WirePatchOp>;
    fn project(&self, target: &ProviderWireTarget) -> Result<Self::Projection, ProviderWireError> {
        self.ops
            .iter()
            .map(|op| {
                Ok(match op {
                    PatchOp::Add { key, value } => WirePatchOp::Add {
                        key: key.clone(),
                        value: project_value(value, target, key)?,
                    },
                    PatchOp::Replace { key, value } => WirePatchOp::Replace {
                        key: key.clone(),
                        value: project_value(value, target, key)?,
                    },
                    PatchOp::Reassert { key, value } => WirePatchOp::Reassert {
                        key: key.clone(),
                        value: project_value(value, target, key)?,
                    },
                    PatchOp::Remove { key } => WirePatchOp::Remove { key: key.clone() },
                })
            })
            .collect()
    }
}
