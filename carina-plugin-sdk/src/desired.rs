//! One structural desired-value view for native and WASM normalizers.

use indexmap::IndexMap;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use carina_core::desired::NormalizationBatch;
use carina_core::protocol_convert::{ProtocolConvertError, protocol_to_resource_id};
use carina_core::provider::{
    ProviderError as NativeProviderError, ProviderReady, ProviderResult as NativeProviderResult,
    ResourceWirePayload,
};
use carina_core::resource::{
    ConcreteValue, DeferredValue, Resource, ResourceId, Value, ValueString,
};
use carina_provider_protocol::normalization::{
    NormalizationErrorKind as Kind, NormalizationPathSegment as Segment, NormalizationResource,
    NormalizationValue as Wire,
};

pub use carina_provider_protocol::normalization::NormalizationError as DesiredNormalizationError;

/// A known scalar. Containers and deferred core values cannot be put here.
#[derive(Debug, Clone)]
pub enum DesiredScalar {
    String(ValueString),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl DesiredScalar {
    /// Read a wire string, including an enum's API spelling, for cross-field checks.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value.expose_plaintext()),
            _ => None,
        }
    }
}

/// An opaque, move-only leaf. It exposes neither a slot nor a core value.
///
/// ```compile_fail
/// use carina_plugin_sdk::DeferredLeaf;
/// let _ = DeferredLeaf { slot: 0, owner: todo!() };
/// ```
/// ```compile_fail
/// use carina_plugin_sdk::DeferredLeaf;
/// fn duplicate(leaf: DeferredLeaf) { let _ = leaf.clone(); }
/// ```
pub struct DeferredLeaf {
    slot: u32,
    owner: Arc<()>,
}

impl std::fmt::Debug for DeferredLeaf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeferredLeaf")
    }
}

/// A desired tree node. A secret wrapping a container is still one opaque leaf.
#[derive(Debug)]
pub enum DesiredValue {
    Known(DesiredScalar),
    Deferred(DeferredLeaf),
    List(DesiredList),
    Map(DesiredMap),
}

/// Mutable access never gives out a mutable deferred leaf or backing value.
///
/// ```compile_fail
/// use carina_plugin_sdk::DesiredValueMut;
/// fn edit(value: DesiredValueMut<'_>) {
///     if let DesiredValueMut::Deferred(inner) = value { let _ = inner; }
/// }
/// ```
pub enum DesiredValueMut<'a> {
    Known(&'a mut DesiredScalar),
    Deferred,
    List(&'a mut DesiredList),
    Map(&'a mut DesiredMap),
}

impl DesiredValue {
    fn mutable(&mut self) -> DesiredValueMut<'_> {
        match self {
            Self::Known(value) => DesiredValueMut::Known(value),
            Self::Deferred(_) => DesiredValueMut::Deferred,
            Self::List(value) => DesiredValueMut::List(value),
            Self::Map(value) => DesiredValueMut::Map(value),
        }
    }
}

/// Checked edits cannot overwrite an occupied map key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesiredEditError {
    Occupied { name: String },
    IndexOutOfBounds { index: usize, len: usize },
}
impl std::fmt::Display for DesiredEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Occupied { name } => write!(f, "desired key '{name}' is already present"),
            Self::IndexOutOfBounds { index, len } => {
                write!(f, "desired list index {index} is outside length {len}")
            }
        }
    }
}
impl std::error::Error for DesiredEditError {}

/// A structural map. Providers may replace or drop known entries by replacing
/// the map; any deferred leaves dropped that way cause `MissingHandle` at finish.
#[derive(Debug, Default)]
pub struct DesiredMap {
    entries: IndexMap<String, DesiredValue>,
}
impl DesiredMap {
    /// Construct a map of known defaults using [`Self::insert_absent`].
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self, name: &str) -> Option<&DesiredValue> {
        self.entries.get(name)
    }
    pub fn get_mut(&mut self, name: &str) -> Option<DesiredValueMut<'_>> {
        self.entries.get_mut(name).map(DesiredValue::mutable)
    }
    /// Return the removed value so deferred leaves can be moved within the attribute.
    /// Dropping a removed deferred leaf causes `MissingHandle` at finish.
    pub fn remove(&mut self, key: &str) -> Option<DesiredValue> {
        self.entries.shift_remove(key)
    }
    pub fn insert_absent(
        &mut self,
        name: impl Into<String>,
        value: DesiredValue,
    ) -> Result<(), DesiredEditError> {
        match self.entries.entry(name.into()) {
            indexmap::map::Entry::Vacant(entry) => {
                entry.insert(value);
                Ok(())
            }
            indexmap::map::Entry::Occupied(entry) => Err(DesiredEditError::Occupied {
                name: entry.key().clone(),
            }),
        }
    }
}

/// A structural list. Filtering known scalars always retains opaque leaves.
#[derive(Debug, Default)]
pub struct DesiredList {
    values: Vec<DesiredValue>,
}
impl DesiredList {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, value: DesiredValue) {
        self.values.push(value);
    }
    pub fn iter(&self) -> impl Iterator<Item = &DesiredValue> {
        self.values.iter()
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = DesiredValueMut<'_>> {
        self.values.iter_mut().map(DesiredValue::mutable)
    }
    /// Canonicalize/filter known scalars without deleting deferred elements.
    pub fn retain_known(&mut self, mut keep: impl FnMut(&mut DesiredScalar) -> bool) {
        self.values.retain_mut(|value| match value {
            DesiredValue::Known(value) => keep(value),
            _ => true,
        });
    }
    /// Move an element within this list, retaining its opaque identity.
    pub fn move_within(&mut self, from: usize, to: usize) -> Result<(), DesiredEditError> {
        let len = self.values.len();
        for index in [from, to] {
            if index >= len {
                return Err(DesiredEditError::IndexOutOfBounds { index, len });
            }
        }
        let value = self.values.remove(from);
        self.values.insert(to, value);
        Ok(())
    }
}

impl From<Vec<DesiredValue>> for DesiredList {
    fn from(values: Vec<DesiredValue>) -> Self {
        Self { values }
    }
}

/// A read-only view used by the same cross-field validators on either route.
pub struct DesiredInputs<'a> {
    id: std::borrow::Cow<'a, ResourceId>,
    tree: InputsTree<'a>,
}
enum InputsTree<'a> {
    Borrowed(&'a DesiredMap),
    Owned(DesiredMap),
}
impl DesiredInputs<'_> {
    /// Project a resource that already passed the complete provider wire gate.
    ///
    /// Normalizers obtain their view from their normalization owner instead.
    /// Raw resources cannot bypass provider preparation:
    ///
    /// ```compile_fail
    /// use carina_plugin_sdk::{DesiredInputs, core::resource::Resource};
    /// fn bypass(raw: &Resource) { let _ = DesiredInputs::from_provider_ready(raw); }
    /// ```
    ///
    /// ```compile_fail
    /// use carina_plugin_sdk::{DesiredInputs, core::resource::Resource};
    /// fn bypass(raw: &Resource) { let _ = DesiredInputs::from_core(raw); }
    /// ```
    pub fn from_provider_ready<T: ResourceWirePayload>(resource: &ProviderReady<T>) -> Self {
        let owner = Arc::new(());
        let entries = resource
            .as_inner()
            .attributes()
            .iter()
            .map(|(name, value)| {
                let wire = resource
                    .wire_attributes()
                    .get(name)
                    .expect("checked attribute");
                (name.clone(), checked_value(value, wire.value(), &owner))
            })
            .collect();
        Self {
            id: std::borrow::Cow::Owned(resource.as_inner().resource_id().clone()),
            tree: InputsTree::Owned(DesiredMap { entries }),
        }
    }
    pub fn id(&self) -> &ResourceId {
        &self.id
    }
    pub fn is_resource(&self, provider: &str, resource_type: &str) -> bool {
        self.id.provider == provider && self.id.resource_type == resource_type
    }
    pub fn attribute(&self, name: &str) -> Option<&DesiredValue> {
        match &self.tree {
            InputsTree::Borrowed(tree) => tree.get(name),
            InputsTree::Owned(tree) => tree.get(name),
        }
    }
}

// The original tree supplies provenance and order, never a second scalar
// representation policy. Keep this exhaustive so new core variants require an
// explicit decision before they can enter the SDK view.
enum DesiredProvenance<'a> {
    Scalar,
    String(&'a ValueString),
    StringList(&'a [ValueString]),
    List(&'a [Value]),
    Map(&'a IndexMap<String, Value>),
    Secret,
}

fn desired_provenance(value: &Value) -> DesiredProvenance<'_> {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => DesiredProvenance::String(value),
        Value::Concrete(ConcreteValue::StringList(values)) => DesiredProvenance::StringList(values),
        Value::Concrete(ConcreteValue::List(values)) => DesiredProvenance::List(values),
        Value::Concrete(ConcreteValue::Map(values)) => DesiredProvenance::Map(values),
        Value::Concrete(
            ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_),
        ) => DesiredProvenance::Scalar,
        Value::Deferred(DeferredValue::Secret(_)) => DesiredProvenance::Secret,
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

fn checked_value(original: &Value, wire: &crate::types::Value, owner: &Arc<()>) -> DesiredValue {
    use crate::types::Value as Wire;
    let provenance = desired_provenance(original);
    if matches!(provenance, DesiredProvenance::Secret) {
        return DesiredValue::Deferred(DeferredLeaf {
            slot: 0,
            owner: Arc::clone(owner),
        });
    }
    match wire {
        Wire::Bool(value) => DesiredValue::Known(DesiredScalar::Bool(*value)),
        Wire::Int(value) => DesiredValue::Known(DesiredScalar::Int(*value)),
        Wire::Float(value) => DesiredValue::Known(DesiredScalar::Float(*value)),
        Wire::String(value) => {
            let value = match provenance {
                DesiredProvenance::String(original) => original.clone(),
                DesiredProvenance::Scalar => value.clone().into(),
                DesiredProvenance::StringList(_)
                | DesiredProvenance::List(_)
                | DesiredProvenance::Map(_)
                | DesiredProvenance::Secret => unreachable!("checked string provenance"),
            };
            DesiredValue::Known(DesiredScalar::String(value))
        }
        Wire::StringList(_) => {
            let DesiredProvenance::StringList(originals) = provenance else {
                unreachable!("checked string-list provenance")
            };
            DesiredValue::List(DesiredList {
                values: originals
                    .iter()
                    .map(|value| DesiredValue::Known(DesiredScalar::String(value.clone())))
                    .collect(),
            })
        }
        Wire::List(values) => {
            let DesiredProvenance::List(originals) = provenance else {
                unreachable!("checked list provenance")
            };
            DesiredValue::List(DesiredList {
                values: originals
                    .iter()
                    .zip(values)
                    .map(|(original, wire)| checked_value(original, wire, owner))
                    .collect(),
            })
        }
        Wire::Map(values) => {
            let DesiredProvenance::Map(originals) = provenance else {
                unreachable!("checked map provenance")
            };
            DesiredValue::Map(DesiredMap {
                entries: originals
                    .iter()
                    .map(|(name, original)| {
                        (name.clone(), checked_value(original, &values[name], owner))
                    })
                    .collect(),
            })
        }
    }
}

/// A shared normalizer receives only this view, never a raw resource.
///
/// ```compile_fail
/// use carina_plugin_sdk::DesiredView;
/// fn bypass(view: &mut DesiredView<'_>) { let _ = &mut view.attributes.entries; }
/// ```
pub struct DesiredView<'a> {
    id: &'a ResourceId,
    attributes: &'a mut DesiredMap,
}
impl DesiredView<'_> {
    pub fn id(&self) -> &ResourceId {
        self.id
    }
    pub fn is_resource(&self, provider: &str, resource_type: &str) -> bool {
        self.id.provider == provider && self.id.resource_type == resource_type
    }
    pub fn inputs(&self) -> DesiredInputs<'_> {
        DesiredInputs {
            id: std::borrow::Cow::Borrowed(self.id),
            tree: InputsTree::Borrowed(self.attributes),
        }
    }
    pub fn attribute(&self, name: &str) -> Option<&DesiredValue> {
        self.attributes.get(name)
    }
    pub fn attribute_mut(&mut self, name: &str) -> Option<DesiredValueMut<'_>> {
        self.attributes.get_mut(name)
    }
    /// Return an attribute without discarding its opaque leaves. Deferred leaves
    /// must be reinserted under the same top-level name before finish.
    pub fn remove(&mut self, name: &str) -> Option<DesiredValue> {
        self.attributes.remove(name)
    }
    /// Insert a top-level default without replacing a known or deferred value.
    pub fn insert_absent(
        &mut self,
        name: impl Into<String>,
        value: DesiredValue,
    ) -> Result<(), DesiredEditError> {
        self.attributes.insert_absent(name, value)
    }
}

/// A normalization input received from the guest wire boundary.
///
/// ```compile_fail
/// use carina_plugin_sdk::DesiredResource;
/// fn forge() { let _ = DesiredResource { resource: todo!() }; }
/// ```
/// ```compile_fail
/// use carina_plugin_sdk::{DesiredResource, protocol::normalization::NormalizationResource};
/// fn forge(resource: NormalizationResource) { let _ = DesiredResource::decode(resource); }
/// ```
#[derive(Debug)]
pub struct DesiredResource {
    resource: NormalizationResource,
}
impl DesiredResource {
    #[cfg(any(target_arch = "wasm32", test))]
    pub(crate) fn from_wire(resource: NormalizationResource) -> Self {
        Self { resource }
    }
    pub fn id(&self) -> &crate::types::ResourceId {
        &self.resource.id
    }
    pub fn into_normalization(self) -> Result<DesiredNormalization, DesiredNormalizationError> {
        DesiredNormalization::decode(self.resource)
    }
    /// Finish a no-op normalization while enforcing handle conservation.
    pub fn finish(self) -> Result<NormalizedResource, DesiredNormalizationError> {
        self.into_normalization()?.finish()
    }
}

/// Checked output; only guest glue can access the normalization wire record.
///
/// ```compile_fail
/// use carina_plugin_sdk::NormalizedResource;
/// fn forge() { let _ = NormalizedResource { resource: todo!() }; }
/// ```
#[derive(Debug)]
pub struct NormalizedResource {
    pub(crate) resource: NormalizationResource,
}

/// Owns a guest tree and its received handle inventory until checked finish.
///
/// ```compile_fail
/// use carina_plugin_sdk::DesiredNormalization;
/// fn forge() { let _ = DesiredNormalization { id: todo!(), wire_id: todo!(), attributes: todo!(), expected: todo!(), owner: todo!() }; }
/// ```
pub struct DesiredNormalization {
    id: ResourceId,
    wire_id: crate::types::ResourceId,
    attributes: DesiredMap,
    expected: BTreeMap<u32, ExpectedLeaf>,
    owner: Arc<()>,
}

struct ExpectedLeaf {
    attribute: String,
    path: Vec<Segment>,
}

struct Decoder<'a> {
    id: &'a crate::types::ResourceId,
    owner: Arc<()>,
    expected: BTreeMap<u32, ExpectedLeaf>,
}
impl Decoder<'_> {
    fn value(
        &mut self,
        value: Wire,
        attribute: &str,
        path: &mut Vec<Segment>,
    ) -> Result<DesiredValue, DesiredNormalizationError> {
        let result = match value {
            Wire::Deferred(slot) => {
                if self
                    .expected
                    .insert(
                        slot,
                        ExpectedLeaf {
                            attribute: attribute.into(),
                            path: path.clone(),
                        },
                    )
                    .is_some()
                {
                    return Err(DesiredNormalizationError::new(
                        self.id.clone(),
                        attribute,
                        Kind::DuplicateHandle { slot },
                    )
                    .with_path(path.clone()));
                }
                DesiredValue::Deferred(DeferredLeaf {
                    slot,
                    owner: self.owner.clone(),
                })
            }
            Wire::List(values) => {
                let mut result = Vec::with_capacity(values.len());
                for (index, value) in values.into_iter().enumerate() {
                    path.push(Segment::Index(index));
                    result.push(self.value(value, attribute, path)?);
                    path.pop();
                }
                DesiredValue::List(DesiredList { values: result })
            }
            Wire::StringList(values) => DesiredValue::List(DesiredList {
                values: values
                    .into_iter()
                    .map(|value| DesiredValue::Known(DesiredScalar::String(value.into())))
                    .collect(),
            }),
            Wire::Map(values) => {
                let mut entries = IndexMap::new();
                for (key, value) in values {
                    path.push(Segment::Key(key.clone()));
                    entries.insert(key.clone(), self.value(value, attribute, path)?);
                    path.pop();
                }
                DesiredValue::Map(DesiredMap { entries })
            }
            _ => {
                let scalar = match value {
                    Wire::String(value) => DesiredScalar::String(value.into()),
                    Wire::Int(value) => DesiredScalar::Int(value),
                    Wire::Float(value) => DesiredScalar::Float(value),
                    Wire::Bool(value) => DesiredScalar::Bool(value),
                    _ => unreachable!(),
                };
                DesiredValue::Known(scalar)
            }
        };
        Ok(result)
    }
}

impl DesiredNormalization {
    fn decode(resource: NormalizationResource) -> Result<Self, DesiredNormalizationError> {
        let owner = Arc::new(());
        let mut decoder = Decoder {
            id: &resource.id,
            owner: owner.clone(),
            expected: BTreeMap::new(),
        };
        let mut entries = IndexMap::new();
        for (name, value) in resource.attributes {
            entries.insert(name.clone(), decoder.value(value, &name, &mut Vec::new())?);
        }
        let expected = decoder.expected;
        Ok(Self {
            id: protocol_to_resource_id(&resource.id),
            wire_id: resource.id,
            attributes: DesiredMap { entries },
            expected,
            owner,
        })
    }
    pub fn view(&mut self) -> DesiredView<'_> {
        DesiredView {
            id: &self.id,
            attributes: &mut self.attributes,
        }
    }
    pub fn finish(self) -> Result<NormalizedResource, DesiredNormalizationError> {
        let mut encoder = Encoder {
            id: &self.wire_id,
            owner: &self.owner,
            expected: &self.expected,
            seen: BTreeSet::new(),
        };
        let mut attributes = BTreeMap::new();
        for (name, value) in self.attributes.entries {
            attributes.insert(name.clone(), encoder.value(value, &name, &mut Vec::new())?);
        }
        for (slot, expected) in &self.expected {
            if !encoder.seen.contains(slot) {
                return Err(DesiredNormalizationError::new(
                    self.wire_id.clone(),
                    &expected.attribute,
                    Kind::MissingHandle { slot: *slot },
                )
                .with_path(expected.path.clone()));
            }
        }
        Ok(NormalizedResource {
            resource: NormalizationResource {
                id: self.wire_id,
                attributes,
            },
        })
    }
}

struct Encoder<'a> {
    id: &'a crate::types::ResourceId,
    owner: &'a Arc<()>,
    expected: &'a BTreeMap<u32, ExpectedLeaf>,
    seen: BTreeSet<u32>,
}
impl Encoder<'_> {
    fn value(
        &mut self,
        value: DesiredValue,
        attribute: &str,
        path: &mut Vec<Segment>,
    ) -> Result<Wire, DesiredNormalizationError> {
        let error = |kind| {
            DesiredNormalizationError::new(self.id.clone(), attribute, kind).with_path(path.clone())
        };
        Ok(match value {
            DesiredValue::Deferred(leaf) => {
                if !Arc::ptr_eq(&leaf.owner, self.owner)
                    || self
                        .expected
                        .get(&leaf.slot)
                        .is_some_and(|expected| expected.attribute != attribute)
                {
                    return Err(error(Kind::ForeignHandle { slot: leaf.slot }));
                }
                if !self.expected.contains_key(&leaf.slot) {
                    return Err(error(Kind::UnknownHandle { slot: leaf.slot }));
                }
                if !self.seen.insert(leaf.slot) {
                    return Err(error(Kind::DuplicateHandle { slot: leaf.slot }));
                }
                Wire::Deferred(leaf.slot)
            }
            DesiredValue::Known(value) => match value {
                DesiredScalar::String(value) => Wire::String(value.expose_plaintext().into()),
                DesiredScalar::Int(value) => Wire::Int(value),
                DesiredScalar::Bool(value) => Wire::Bool(value),
                DesiredScalar::Float(value) if value.is_finite() => Wire::Float(value),
                DesiredScalar::Float(_) => {
                    return Err(
                        error(Kind::Conversion).with_source(ProtocolConvertError::NonFiniteFloat)
                    );
                }
            },
            DesiredValue::List(list) => {
                let mut values = Vec::with_capacity(list.values.len());
                for (index, value) in list.values.into_iter().enumerate() {
                    path.push(Segment::Index(index));
                    values.push(self.value(value, attribute, path)?);
                    path.pop();
                }
                Wire::List(values)
            }
            DesiredValue::Map(map) => {
                let mut values = BTreeMap::new();
                for (key, value) in map.entries {
                    path.push(Segment::Key(key.clone()));
                    values.insert(key, self.value(value, attribute, path)?);
                    path.pop();
                }
                Wire::Map(values)
            }
        })
    }
}

/// A staged native normalization. Dropping it or failing finish leaves the target intact.
pub struct NativeDesiredNormalization<'a, S> {
    target: &'a mut Resource<S>,
    batch: NormalizationBatch,
    inner: DesiredNormalization,
}
impl<'a, S> NativeDesiredNormalization<'a, S> {
    pub fn from_core(target: &'a mut Resource<S>) -> NativeProviderResult<Self> {
        let (batch, inner) = decode_native(target)?;
        Ok(Self {
            target,
            batch,
            inner,
        })
    }
    pub fn view(&mut self) -> DesiredView<'_> {
        self.inner.view()
    }
    /// Query the staged normalization input, including its opaque leaves.
    pub fn inputs(&self) -> DesiredInputs<'_> {
        DesiredInputs {
            id: std::borrow::Cow::Borrowed(&self.inner.id),
            tree: InputsTree::Borrowed(&self.inner.attributes),
        }
    }
    pub fn finish(self) -> NativeProviderResult<()> {
        let id = self.target.id.clone();
        let into_error = |error| NativeProviderError::from(error).for_resource(id.clone());
        let normalized = self.inner.finish().map_err(into_error)?;
        let mut attributes = self
            .batch
            .finish(vec![normalized.resource])
            .map_err(into_error)?;
        self.target.attributes = attributes.remove(0);
        Ok(())
    }
}

fn decode_native<S>(
    resource: &Resource<S>,
) -> NativeProviderResult<(NormalizationBatch, DesiredNormalization)> {
    let into_error = |error| NativeProviderError::from(error).for_resource(resource.id.clone());
    let batch = NormalizationBatch::new(std::slice::from_ref(resource)).map_err(into_error)?;
    let mut inner =
        DesiredNormalization::decode(batch.resources()[0].clone()).map_err(into_error)?;
    inner.id = resource.id.clone();
    retain_native_map_metadata(&mut inner.attributes, &resource.attributes);
    Ok((batch, inner))
}

// The normalization wire deliberately carries only plaintext and sorted maps.
// On the native route, keep the original diagnostic provenance and map order
// while retaining the decoder's move-only deferred handles.
fn retain_native_map_metadata(tree: &mut DesiredMap, originals: &IndexMap<String, Value>) {
    let mut entries = std::mem::take(&mut tree.entries);
    for (name, original) in originals {
        let mut value = entries
            .shift_remove(name)
            .expect("decoded original attribute");
        retain_native_value_metadata(&mut value, original);
        tree.entries.insert(name.clone(), value);
    }
}

fn retain_native_value_metadata(tree: &mut DesiredValue, original: &Value) {
    match (tree, original) {
        (
            DesiredValue::Known(DesiredScalar::String(value)),
            Value::Concrete(ConcreteValue::String(original)),
        ) => {
            *value = original.clone();
        }
        (DesiredValue::List(list), Value::Concrete(ConcreteValue::StringList(originals))) => {
            for (value, original) in list.values.iter_mut().zip(originals) {
                if let DesiredValue::Known(DesiredScalar::String(value)) = value {
                    *value = original.clone();
                }
            }
        }
        (DesiredValue::List(list), Value::Concrete(ConcreteValue::List(originals))) => {
            for (value, original) in list.values.iter_mut().zip(originals) {
                retain_native_value_metadata(value, original);
            }
        }
        (DesiredValue::Map(map), Value::Concrete(ConcreteValue::Map(originals))) => {
            retain_native_map_metadata(map, originals);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
