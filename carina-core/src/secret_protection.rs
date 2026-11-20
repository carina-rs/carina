//! The one owner of authored-secret projection onto observations.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone, PartialEq)]
pub(crate) struct SecretProtection {
    shape: ProtectionShape,
    source: ProjectionSource,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectionSource {
    Observation,
    BindingPropagation,
    PersistedBinding,
    DispatchRuleInput,
    ResolvedInput,
    Annotations,
}
#[derive(Clone, PartialEq)]
enum ProtectionShape {
    Public,
    Whole(Option<RedactedSecretHash>),
    Pending,
    Map(IndexMap<String, SecretProtection>),
    List(Vec<SecretProtection>),
}
impl std::fmt::Debug for SecretProtection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretProtection(..)")
    }
}
impl SecretProtection {
    pub(crate) fn attributes<P: ValueKind>(
        attributes: &IndexMap<String, Value<P>>,
    ) -> IndexMap<String, Self> {
        attributes
            .iter()
            .map(|(key, value)| (key.clone(), Self::for_output(value)))
            .collect()
    }
    pub(crate) fn is_secret(&self) -> bool {
        !matches!(self.shape, ProtectionShape::Public)
    }
    pub(crate) fn for_output<P: ValueKind>(value: &Value<P>) -> Self {
        Self::from_shape_inner(value, ProjectionSource::Observation)
    }
    // Binding propagation keeps the pre-#4479 positional correspondence.
    // Output projection alone promotes divergent list lengths. Resolved input
    // branch selection is also independent of output policy.
    pub(crate) fn for_dispatch_rule_input<P: ValueKind>(value: &Value<P>) -> Self {
        Self::from_shape_inner(value, ProjectionSource::DispatchRuleInput)
    }
    pub(crate) fn for_binding<P: ValueKind>(value: &Value<P>) -> Self {
        Self::from_shape_inner(value, ProjectionSource::BindingPropagation)
    }
    pub(super) fn from_resolved_shape<P: ValueKind>(value: &Value<P>) -> Self {
        Self::from_shape_inner(value, ProjectionSource::ResolvedInput)
    }
    fn from_annotations<P: ValueKind>(value: &Value<P>) -> Self {
        Self::from_shape_inner(value, ProjectionSource::Annotations)
    }
    fn from_shape_inner<P: ValueKind>(value: &Value<P>, source: ProjectionSource) -> Self {
        let shape = match value {
            Value::Deferred(DeferredValue::Secret(_)) => ProtectionShape::Whole(None),
            Value::Deferred(DeferredValue::Placeholder(leaf))
                if leaf.redacted_secret_hash().is_some() =>
            {
                ProtectionShape::Whole(leaf.redacted_secret_hash().cloned())
            }
            _ if !contains_secret(value) => ProtectionShape::Public,
            Value::Concrete(ConcreteValue::Map(values)) => ProtectionShape::Map(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::from_shape_inner(value, source)))
                    .collect(),
            ),
            Value::Concrete(ConcreteValue::List(values)) => ProtectionShape::List(
                values
                    .iter()
                    .map(|value| Self::from_shape_inner(value, source))
                    .collect(),
            ),
            _ if matches!(
                source,
                ProjectionSource::Annotations | ProjectionSource::DispatchRuleInput
            ) =>
            {
                ProtectionShape::Public
            }
            _ => ProtectionShape::Pending,
        };
        Self { shape, source }
    }
    pub(super) fn from_persisted(value: PersistedView<'_>) -> Self {
        Self::persisted_for(value, ProjectionSource::Observation)
    }
    pub(super) fn for_persisted_binding(value: PersistedView<'_>) -> Self {
        Self::persisted_for(value, ProjectionSource::PersistedBinding)
    }
    fn persisted_for(value: PersistedView<'_>, source: ProjectionSource) -> Self {
        value.decode_value().as_ref().map_or(
            Self {
                shape: ProtectionShape::Public,
                source,
            },
            |value| Self::from_shape_inner(value, source),
        )
    }
    /// Previous hashes supply reusable evidence only; they never add sensitivity
    /// to a freshly authored public value.
    pub(super) fn desired(value: &Value, previous: &Self) -> Self {
        let mut protection = Self::from_annotations(value);
        protection.reuse_previous(previous);
        protection
    }
    fn reuse_previous(&mut self, previous: &Self) {
        match (&mut self.shape, &previous.shape) {
            (ProtectionShape::Whole(hash), ProtectionShape::Whole(previous)) => {
                *hash = previous.clone()
            }
            (ProtectionShape::Map(values), ProtectionShape::Map(previous)) => {
                for (key, value) in values {
                    if let Some(previous) = previous.get(key) {
                        value.reuse_previous(previous);
                    }
                }
            }
            (ProtectionShape::List(values), ProtectionShape::List(previous)) => {
                for (value, previous) in values.iter_mut().zip(previous) {
                    value.reuse_previous(previous);
                }
            }
            _ => {}
        }
    }
    /// Shape correspondence is decided only here. Renderers and hashers supply
    /// terminal operations, without independently matching observed containers.
    fn project<P: ValueKind, Q: ValueKind, E>(
        &self,
        source: &Value<P>,
        whole: &mut impl FnMut(&Value<P>, Option<&RedactedSecretHash>) -> Result<Value<Q>, E>,
        public: &mut impl FnMut(&Value<P>) -> Result<Value<Q>, E>,
    ) -> Result<Value<Q>, E> {
        if matches!(source, Value::Deferred(DeferredValue::Placeholder(leaf)) if leaf.redacted_secret_hash().is_some())
        {
            return public(source);
        }
        let previous = match &self.shape {
            ProtectionShape::Whole(hash) => hash.as_ref(),
            _ => None,
        };
        if matches!(source, Value::Deferred(DeferredValue::Secret(_))) {
            return whole(source, previous);
        }
        match &self.shape {
            ProtectionShape::Public => public(source),
            ProtectionShape::Whole(previous) => whole(source, previous.as_ref()),
            ProtectionShape::Pending
                if self.source == ProjectionSource::ResolvedInput
                    && matches!(source, Value::Concrete(_)) =>
            {
                public(source)
            }
            ProtectionShape::Pending => whole(source, None),
            ProtectionShape::Map(shapes) => {
                let Value::Concrete(ConcreteValue::Map(values)) = source else {
                    return if matches!(
                        self.source,
                        ProjectionSource::PersistedBinding | ProjectionSource::DispatchRuleInput
                    ) {
                        public(source)
                    } else {
                        whole(source, None)
                    };
                };
                Ok(Value::Concrete(ConcreteValue::Map(
                    values
                        .iter()
                        .map(|(key, value)| {
                            let value = match shapes.get(key) {
                                Some(shape) => shape.project(value, whole, public)?,
                                None => public(value)?,
                            };
                            Ok((key.clone(), value))
                        })
                        .collect::<Result<_, E>>()?,
                )))
            }
            ProtectionShape::List(shapes) => {
                let values = match source {
                    Value::Concrete(ConcreteValue::List(values))
                        if values.len() == shapes.len()
                            || self.source != ProjectionSource::Observation =>
                    {
                        values
                            .iter()
                            .enumerate()
                            .map(|(index, value)| match shapes.get(index) {
                                Some(shape) => shape.project(value, whole, public),
                                None => public(value),
                            })
                            .collect::<Result<Vec<_>, E>>()?
                    }
                    Value::Concrete(ConcreteValue::StringList(values))
                        if !matches!(
                            self.source,
                            ProjectionSource::PersistedBinding
                                | ProjectionSource::DispatchRuleInput
                        ) && (values.len() == shapes.len()
                            || self.source != ProjectionSource::Observation) =>
                    {
                        let values = values
                            .iter()
                            .enumerate()
                            .map(|(index, value)| {
                                let value = Value::Concrete(ConcreteValue::String(value.clone()));
                                match shapes.get(index) {
                                    Some(shape) => shape.project(&value, whole, public),
                                    None => public(&value),
                                }
                            })
                            .collect::<Result<Vec<_>, E>>()?;
                        return Ok(rebuild_projected_string_list(values));
                    }
                    _ if matches!(
                        self.source,
                        ProjectionSource::PersistedBinding | ProjectionSource::DispatchRuleInput
                    ) =>
                    {
                        return public(source);
                    }
                    _ => return whole(source, None),
                };
                Ok(Value::Concrete(ConcreteValue::List(values)))
            }
        }
    }
    pub(crate) fn mask<P: ValueKind>(&self, source: &Value<P>) -> Value<P> {
        self.project(
            source,
            &mut |value, _| {
                Ok::<_, std::convert::Infallible>(
                    if matches!(value, Value::Deferred(DeferredValue::Secret(_))) {
                        value.clone()
                    } else {
                        Value::Deferred(DeferredValue::Secret(Box::new(value.clone()).into()))
                    },
                )
            },
            &mut |value| Ok(value.clone()),
        )
        .unwrap_or_else(|never| match never {})
    }
    pub(crate) fn redact<P: crate::resource::StateValueLeaf>(
        &self,
        source: &Value<P>,
    ) -> Result<SnapshotValue, SerializationError> {
        self.project(
            source,
            &mut |value, previous| {
                // Canonical hash material recursively removes Secret metadata,
                // including inner wrappers added by the persisted-shape pass.
                let hash = crate::resource::SecretPayload::from(Box::new(
                    crate::resource::state_value_snapshot(value),
                ))
                .try_project_plaintext(|value| {
                    hash_secret_value(&value.to_value_at("state")?, previous)
                })?;
                Ok(SnapshotValue::redacted(hash))
            },
            &mut |value| redact_snapshot_secrets(&crate::resource::state_value_snapshot(value)),
        )
    }
}

/// An observation bound to its authored and historical secret provenance.
/// Output boundaries accept this product; raw observations stay with comparison
/// and provider execution. No raw state accessor or serialization is provided.
///
/// Only the core batch builder selects per-row desired and persisted authority.
///
/// ```compile_fail,E0624
/// use carina_core::{resource::SnapshotState, value::ProtectedObservation};
/// fn bypass(raw: &SnapshotState) {
///     let _ = ProtectedObservation::from_authority(raw, None, None);
/// }
/// ```
///
/// Per-row historical constructors are unavailable to external callers.
///
/// ```compile_fail,E0599
/// use carina_core::{provider::PersistedAttributes, resource::SnapshotState, value::ProtectedObservation};
/// fn forge(raw: &SnapshotState, persisted: &PersistedAttributes) {
///     let _ = ProtectedObservation::historical(raw, persisted);
/// }
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::{resource::{DataSource, SnapshotState}, value::ProtectedObservation};
/// fn forge(raw: &SnapshotState, source: &DataSource) {
///     let _ = ProtectedObservation::for_data_source(raw, source);
/// }
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::{resource::{ResolvedResource, SnapshotState}, value::ProtectedObservation};
/// fn forge(raw: &SnapshotState, desired: &ResolvedResource) {
///     let _ = ProtectedObservation::for_resource(raw, desired, None);
/// }
/// ```
#[derive(Clone)]
pub struct ProtectedObservation {
    state: crate::resource::SnapshotState,
    protection: HashMap<String, SecretProtection>,
    persisted: Option<crate::provider::PersistedAttributes>,
    nested_write_only: crate::provider::NestedWriteOnlyRecord,
    snapshot: std::sync::Arc<
        std::sync::OnceLock<Result<BTreeMap<String, SnapshotValue>, SerializationError>>,
    >,
}
impl std::fmt::Debug for ProtectedObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtectedObservation")
            .field("id", &self.state.id)
            .finish_non_exhaustive()
    }
}
impl ProtectedObservation {
    /// Persisted markers authorize historical-only observations, including
    /// orphan rows and old delete generations. The authority is mandatory.
    #[cfg(test)]
    pub(crate) fn historical(
        state: &crate::resource::SnapshotState,
        persisted: &crate::provider::PersistedAttributes,
    ) -> Self {
        Self::protect(state, None::<&IndexMap<String, Value>>, Some(persisted))
    }
    /// Only crate-owned, already classified history/fixtures may lack authority.
    pub(crate) fn classified(state: &crate::resource::SnapshotState) -> Self {
        Self::protect(state, None::<&IndexMap<String, Value>>, None)
    }
    /// Bind the exact resource identity and its immutable authored inputs.
    pub(crate) fn for_resource<S>(
        state: &crate::resource::SnapshotState,
        desired: &crate::resource::ResolvedResource<S>,
        persisted: Option<&crate::provider::PersistedAttributes>,
    ) -> Result<Self, SerializationError> {
        if state.id != desired.id {
            return Err(SerializationError::ObservationIdentityMismatch {
                expected: Box::new(desired.id.clone()),
                actual: Box::new(state.id.clone()),
            });
        }
        Ok(Self::protect(state, Some(&desired.attributes), persisted))
    }
    /// Data-source read attributes remain ordinary. Inputs authorize the
    /// identity, without propagating input secrecy onto provider outputs.
    pub(crate) fn for_data_source<S>(
        state: &crate::resource::SnapshotState,
        desired: &crate::resource::DataSource<S>,
    ) -> Result<Self, SerializationError> {
        if state.id != desired.id {
            return Err(SerializationError::ObservationIdentityMismatch {
                expected: Box::new(desired.id.clone()),
                actual: Box::new(state.id.clone()),
            });
        }
        Ok(Self::classified(state))
    }
    fn protect<P: ValueKind>(
        state: &crate::resource::SnapshotState,
        desired: Option<&IndexMap<String, Value<P>>>,
        persisted: Option<&crate::provider::PersistedAttributes>,
    ) -> Self {
        let desired = desired.map(SecretProtection::attributes);
        Self::from_authority(state, desired.as_ref(), persisted)
    }
    pub(crate) fn from_authority(
        state: &crate::resource::SnapshotState,
        desired: Option<&IndexMap<String, SecretProtection>>,
        persisted: Option<&crate::provider::PersistedAttributes>,
    ) -> Self {
        let mut state = state.clone();
        let mut protection = HashMap::new();
        for (key, value) in &mut state.attributes {
            let previous = persisted
                .and_then(|attributes| attributes.encoded_value(key))
                .map(|value| SecretProtection::from_persisted(value.view()));
            if let Some(previous) = &previous {
                *value = previous.mask(value);
            }
            if let Some(shape) = desired.and_then(|attributes| attributes.get(key)) {
                *value = shape.mask(value);
            }
            let mut projected = SecretProtection::from_annotations(value);
            if let Some(previous) = &previous {
                projected.reuse_previous(previous);
            }
            protection.insert(key.clone(), projected);
        }
        Self {
            state,
            protection,
            persisted: persisted.cloned(),
            nested_write_only: crate::provider::NestedWriteOnlyRecord::new(),
            snapshot: std::sync::Arc::default(),
        }
    }
    /// Carry the protection of an earlier observation of this same resource.
    /// Saved-apply comparisons retain their raw operands; rendering receives
    /// only this product, including for newly added attributes.
    pub fn with_history(mut self, history: &Self) -> Result<Self, SerializationError> {
        if self.id() != history.id() {
            return Err(SerializationError::ObservationIdentityMismatch {
                expected: Box::new(history.id().clone()),
                actual: Box::new(self.id().clone()),
            });
        }
        for (key, value) in &mut self.state.attributes {
            if let Some(protection) = history.protection.get(key) {
                *value = protection.mask(value);
            }
            let mut protection = SecretProtection::from_annotations(value);
            if let Some(previous) = self.protection.get(key) {
                protection.reuse_previous(previous);
            }
            if let Some(previous) = history.protection.get(key) {
                protection.reuse_previous(previous);
            }
            self.protection.insert(key.clone(), protection);
        }
        self.snapshot = std::sync::Arc::default();
        Ok(self)
    }
    pub(crate) fn with_nested_write_only(
        mut self,
        nested: Option<&crate::provider::NestedWriteOnlyRecord>,
    ) -> Self {
        self.nested_write_only = nested.cloned().unwrap_or_default();
        self
    }
    pub fn persisted_attributes(&self) -> Option<&crate::provider::PersistedAttributes> {
        self.persisted.as_ref()
    }
    pub fn nested_write_only(&self) -> &crate::provider::NestedWriteOnlyRecord {
        &self.nested_write_only
    }
    pub fn rendered_attribute(&self, key: &str) -> Result<RenderedValue, SerializationError> {
        self.state
            .attributes
            .get(key)
            .map(render_value)
            .ok_or_else(|| SerializationError::MissingObservationAttribute {
                id: Box::new(self.id().clone()),
                attribute: key.to_owned(),
            })
    }
    pub fn id(&self) -> &crate::resource::ResourceId {
        &self.state.id
    }
    pub(crate) fn display_attributes(&self) -> &BTreeMap<String, SnapshotValue> {
        &self.state.attributes
    }
    /// The batch owner shares salts only for identical classified observations.
    /// Different recorded and refreshed views retain their own snapshots.
    pub(super) fn share_snapshot_with(&mut self, previous: &Self) -> bool {
        if self.state.id != previous.state.id
            || self.state.attributes != previous.state.attributes
            || self.protection != previous.protection
        {
            return false;
        }
        self.snapshot = std::sync::Arc::clone(&previous.snapshot);
        true
    }
    /// Hash only the already classified units. Duplicate JSON locations use
    /// this cached result, including its freshly generated promotion salts.
    pub fn snapshot(&self) -> Result<crate::resource::SnapshotState, SerializationError> {
        let attributes = self
            .snapshot
            .get_or_init(|| {
                self.state
                    .attributes
                    .iter()
                    .map(|(key, value)| {
                        self.protection[key]
                            .redact(value)
                            .map(|value| (key.clone(), value))
                    })
                    .collect()
            })
            .clone()?;
        Ok(crate::resource::SnapshotState {
            attributes,
            ..self.state.clone()
        })
    }
}
