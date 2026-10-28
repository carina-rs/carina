//! Resource - Representing resources and their state

mod enum_value;

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::{self, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::Arc;

use argon2::{Algorithm, Argon2, Params, Version};
use indexmap::IndexMap;
use password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::ValueDescription;
use crate::diff_helpers::{list_element_type, map_entry_subtype, schema_aware_equal};
use crate::effect::MoveAuthority;
use crate::explicit::ExplicitFields;
use crate::parser::display_string_literal;
use crate::provider::{AppliedIdentifier, InFlightCreate};
use crate::schema::{AttributeType, DslMap, ResourceSchema, Shape, ShapeWalkBudget, TypeIdentity};

pub use enum_value::{
    CanonicalEnumValue, EnumValueResolver, RawEnumIdentifier, RawEnumIdentifierParts,
};
pub(crate) use enum_value::{enum_value_comparison_key, enum_values_equal};

#[derive(Clone, Copy)]
pub struct EnumAttr<'a> {
    api_value: &'a str,
    identity: &'a TypeIdentity,
}

impl<'a> EnumAttr<'a> {
    pub fn api_value(&self) -> &'a str {
        self.api_value
    }

    pub fn identity(&self) -> &'a TypeIdentity {
        self.identity
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnumAttrError {
    #[error("resource schema has no attribute `{path}`")]
    SchemaAttributeNotFound { path: String },
    #[error("resource schema has no struct field `{path}`")]
    SchemaFieldNotFound { path: String },
    #[error("resource schema traversal is too deep at `{path}`")]
    SchemaTooDeep { path: String },
    #[error("attribute `{path}` is not enum-typed")]
    NotEnum { path: String },
    #[error("attribute `{path}` is not struct-typed")]
    NotStruct { path: String },
    #[error("struct attribute `{path}` must contain a map value")]
    StructValueNotMap { path: String },
    #[error("enum attribute `{path}` has identity `{actual}`, but the schema expects `{expected}`")]
    EnumIdentityMismatch {
        path: String,
        expected: Box<TypeIdentity>,
        actual: Box<TypeIdentity>,
    },
    #[error("invalid enum attribute `{path}`: {value} does not exactly match a declared API value")]
    NotAMember {
        path: String,
        value: ValueDescription,
    },
    #[error("invalid enum attribute `{path}`: expected a canonical enum or string, got {value}")]
    UnexpectedShape {
        path: String,
        value: ValueDescription,
    },
}

fn validate_enum_api_value<'a>(
    identity: &'a TypeIdentity,
    values: Option<&[String]>,
    value: &Value,
    api_value: &'a str,
    path: &str,
) -> Result<EnumAttr<'a>, EnumAttrError> {
    if values.is_none_or(|values| values.iter().any(|value| value == api_value)) {
        Ok(EnumAttr {
            api_value,
            identity,
        })
    } else {
        Err(EnumAttrError::NotAMember {
            path: path.to_string(),
            value: ValueDescription::of(value),
        })
    }
}

fn enum_attr_for_value<'a>(
    schema: &'a ResourceSchema,
    attr_type: &'a AttributeType,
    value: Option<&'a Value>,
    path: &str,
) -> Result<Option<EnumAttr<'a>>, EnumAttrError> {
    let Shape::Enum {
        identity, values, ..
    } = schema.shape_of(attr_type)
    else {
        return Err(EnumAttrError::NotEnum {
            path: path.to_string(),
        });
    };
    let Some(value) = value else {
        return Ok(None);
    };

    match value {
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) if c.identity() == identity => {
            validate_enum_api_value(identity, values, value, c.api_value(), path).map(Some)
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => {
            Err(EnumAttrError::EnumIdentityMismatch {
                path: path.to_string(),
                expected: Box::new(identity.clone()),
                actual: Box::new(c.identity().clone()),
            })
        }
        Value::Concrete(ConcreteValue::String(s)) => {
            validate_enum_api_value(identity, values, value, s.expose_plaintext(), path).map(Some)
        }
        _ => Err(EnumAttrError::UnexpectedShape {
            path: path.to_string(),
            value: ValueDescription::of(value),
        }),
    }
}

fn enum_attr_type_at_schema_path<'a>(
    schema: &'a ResourceSchema,
    root_attr: &str,
    path: &[&str],
) -> Result<(&'a AttributeType, String), EnumAttrError> {
    let mut attr_type = &schema
        .attributes
        .get(root_attr)
        .ok_or_else(|| EnumAttrError::SchemaAttributeNotFound {
            path: root_attr.to_string(),
        })?
        .attr_type;
    let mut attr_path = root_attr.to_string();
    let mut budget = ShapeWalkBudget::new(64);

    for segment in path {
        while let Shape::List { element_type, .. } = schema.shape_of(attr_type) {
            if !budget.take() {
                return Err(EnumAttrError::SchemaTooDeep { path: attr_path });
            }
            attr_type = element_type;
        }

        if !matches!(schema.shape_of(attr_type), Shape::Struct { .. }) {
            return Err(EnumAttrError::NotStruct { path: attr_path });
        }
        let fields = schema
            .struct_fields_with_budget(attr_type, &mut budget)
            .ok_or_else(|| EnumAttrError::SchemaTooDeep {
                path: attr_path.clone(),
            })?;
        attr_path.push('.');
        attr_path.push_str(segment);
        attr_type = &fields
            .iter()
            .find(|field| field.name == *segment)
            .ok_or_else(|| EnumAttrError::SchemaFieldNotFound {
                path: attr_path.clone(),
            })?
            .field_type;
    }

    Ok((attr_type, attr_path))
}

/// Project a leaf value at an enum-typed resource schema path into its provider
/// API spelling.
///
/// The path starts at `root_attr` and descends through named struct fields.
/// Ordered and unordered list element types are peeled only at intermediate
/// positions before a named segment. Map and union positions are not traversed,
/// and the leaf is validated without peeling, so a list-of-enum leaf is
/// [`EnumAttrError::NotEnum`]. `value` is the leaf value already reached by the
/// caller. `Ok(None)` is returned only when that value is absent.
pub fn enum_attr_at_schema_path<'a>(
    schema: &'a ResourceSchema,
    root_attr: &str,
    path: &[&str],
    value: Option<&'a Value>,
) -> Result<Option<EnumAttr<'a>>, EnumAttrError> {
    let (attr_type, attr_path) = enum_attr_type_at_schema_path(schema, root_attr, path)?;
    enum_attr_for_value(schema, attr_type, value, &attr_path)
}

/// Opaque resource identity.
///
/// Identity is independent from any schema `name` attribute. Empty
/// strings are not valid identities.
///
/// Runtime strings must be validated before they can enter an
/// identity-taking constructor:
///
/// ```compile_fail
/// use carina_core::resource::ResourceId;
///
/// let identity = String::from("runtime-identity");
/// let _ = ResourceId::with_identity("test.Resource", identity);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceIdentity(String);

/// Error returned when a runtime value cannot be used as a resource identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResourceIdentityError {
    #[error("resource identity cannot be empty")]
    Empty,
}

impl ResourceIdentity {
    pub(crate) fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(!s.is_empty(), "resource identity cannot be empty");
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this identity is a resource address below `ancestor`.
    ///
    /// Module expansion encodes ancestry as dot-delimited identity segments.
    /// Keeping the comparison on the identity type prevents display labels or
    /// user attributes from being mistaken for graph identity, and requiring
    /// the delimiter keeps sibling prefixes such as `net` and `network`
    /// distinct.
    pub fn is_address_descendant_of(&self, ancestor: &Self) -> bool {
        self.0
            .strip_prefix(ancestor.as_str())
            .is_some_and(|suffix| suffix.starts_with('.'))
    }
}

impl From<&'static str> for ResourceIdentity {
    fn from(value: &'static str) -> Self {
        Self::new(value)
    }
}

impl TryFrom<String> for ResourceIdentity {
    type Error = ResourceIdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            Err(ResourceIdentityError::Empty)
        } else {
            Ok(Self(value))
        }
    }
}

impl std::fmt::Display for ResourceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ResourceIdentity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ResourceIdentity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_from(s).map_err(serde::de::Error::custom)
    }
}

/// Process-local identity for a resource whose persistent identity has not
/// been assigned yet.
///
/// The token deliberately has no public constructor, string projection,
/// `Display`, or serde implementation. It participates only in equality and
/// hashing while the resource remains in memory.
#[derive(Debug)]
struct PendingTokenMarker {
    _value: u8,
}

#[derive(Clone)]
pub struct PendingToken(Arc<PendingTokenMarker>);

impl PendingToken {
    fn new() -> Self {
        Self(Arc::new(PendingTokenMarker { _value: 0 }))
    }
}

impl PartialEq for PendingToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for PendingToken {}

impl Hash for PendingToken {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0), state);
    }
}

impl fmt::Debug for PendingToken {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("PendingToken")
    }
}

/// Whether a resource identity is still process-local or has been resolved to
/// its persistent value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceIdentityState {
    Pending(PendingToken),
    Resolved(ResourceIdentity),
}

impl ResourceIdentityState {
    fn pending() -> Self {
        Self::Pending(PendingToken::new())
    }
}

impl Default for ResourceIdentityState {
    fn default() -> Self {
        Self::pending()
    }
}

impl Serialize for ResourceIdentityState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Pending(_) => Err(serde::ser::Error::custom(
                "pending resource identity cannot be serialized",
            )),
            Self::Resolved(identity) => identity.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ResourceIdentityState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        ResourceIdentity::deserialize(deserializer).map(Self::Resolved)
    }
}

/// System-assigned key for one deposed generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeposedKey(String);

impl DeposedKey {
    pub fn new_unique() -> Self {
        let uuid = uuid::Uuid::new_v4().simple().to_string();
        Self(uuid[..12].to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeposedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Unique identifier for a resource
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    /// Provider name (e.g., "aws", "awscc")
    pub provider: String,
    /// Resource type (e.g., "s3.Bucket", "ec2.Instance")
    pub resource_type: String,
    /// Resource identity state. Pending tokens are process-local and cannot be
    /// serialized.
    #[serde(default, alias = "name")]
    identity: ResourceIdentityState,
    /// Binding name of the provider instance this resource is routed
    /// to. `None` resolves to the kind's default instance.
    /// Hash/Eq include the field so two resources differing only in
    /// instance compare unequal (different infrastructure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_instance: Option<String>,
}

impl ResourceId {
    /// Decode a resource id received from the provider protocol boundary.
    ///
    /// The wire format represents a pending identity as an empty string. Use
    /// the exhaustive [`ResourceIdentityError`] match here so guest-controlled
    /// input cannot reach an assertion or `expect` path. The protocol record
    /// has no `provider_instance` field, so routing information does not round
    /// trip and the decoded id always uses the default provider instance.
    pub fn from_wire(id: carina_provider_protocol::types::ResourceId) -> Self {
        let carina_provider_protocol::types::ResourceId {
            provider,
            resource_type,
            identity,
        } = id;
        match ResourceIdentity::try_from(identity) {
            Ok(identity) => Self::with_provider_identity(provider, resource_type, identity, None),
            Err(ResourceIdentityError::Empty) => {
                Self::pending_with_provider(provider, resource_type, None)
            }
        }
    }

    /// Encode this resource id for the provider protocol boundary.
    ///
    /// Pending identities are encoded as the protocol's empty-string sentinel.
    /// The protocol record has no `provider_instance` field, so routing
    /// information is intentionally omitted and cannot round trip.
    pub fn to_wire(&self) -> carina_provider_protocol::types::ResourceId {
        carina_provider_protocol::types::ResourceId {
            provider: self.provider.clone(),
            resource_type: self.resource_type.clone(),
            identity: self.identity_str().unwrap_or_default().to_string(),
        }
    }

    pub fn pending(resource_type: impl Into<String>) -> Self {
        Self {
            provider: String::new(),
            resource_type: resource_type.into(),
            identity: ResourceIdentityState::pending(),
            provider_instance: None,
        }
    }

    pub fn with_identity(
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
    ) -> Self {
        Self {
            provider: String::new(),
            resource_type: resource_type.into(),
            identity: ResourceIdentityState::Resolved(identity.into()),
            provider_instance: None,
        }
    }

    /// Construct a `ResourceId` with explicit routing.
    ///
    /// `provider_instance` is the binding name of the routed provider
    /// instance — `None` resolves to the kind's default instance,
    /// `Some(name)` to a `let <name> = provider <kind> { ... }`
    /// declaration. Taking `Option<String>` here (instead of offering
    /// a sibling 3-arg constructor that silently sets `None`) forces
    /// every call site to consciously pick a routing target — the
    /// hidden default that caused carina#3038 is no longer expressible.
    pub fn pending_with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        provider_instance: Option<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
            identity: ResourceIdentityState::pending(),
            provider_instance,
        }
    }

    pub fn with_provider_identity(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
        provider_instance: Option<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
            identity: ResourceIdentityState::Resolved(identity.into()),
            provider_instance,
        }
    }

    pub fn identity_str(&self) -> Option<&str> {
        match &self.identity {
            ResourceIdentityState::Pending(_) => None,
            ResourceIdentityState::Resolved(identity) => Some(identity.as_str()),
        }
    }

    pub fn identity(&self) -> Option<&ResourceIdentity> {
        match &self.identity {
            ResourceIdentityState::Pending(_) => None,
            ResourceIdentityState::Resolved(identity) => Some(identity),
        }
    }

    pub fn identity_state(&self) -> &ResourceIdentityState {
        &self.identity
    }

    pub fn identity_display(&self) -> ResourceIdentityDisplay<'_> {
        ResourceIdentityDisplay(&self.identity)
    }

    pub fn set_identity(&mut self, identity: ResourceIdentity) {
        self.identity = ResourceIdentityState::Resolved(identity);
    }

    fn instantiate_pending(&self) -> Self {
        match &self.identity {
            ResourceIdentityState::Pending(_) => Self {
                provider: self.provider.clone(),
                resource_type: self.resource_type.clone(),
                identity: ResourceIdentityState::pending(),
                provider_instance: self.provider_instance.clone(),
            },
            ResourceIdentityState::Resolved(_) => self.clone(),
        }
    }

    fn instantiate_with_identity(&self, identity: ResourceIdentity) -> Self {
        Self {
            provider: self.provider.clone(),
            resource_type: self.resource_type.clone(),
            identity: ResourceIdentityState::Resolved(identity),
            provider_instance: self.provider_instance.clone(),
        }
    }

    /// Returns the display type including provider prefix if available
    pub fn display_type(&self) -> String {
        if self.provider.is_empty() {
            self.resource_type.clone()
        } else {
            format!("{}.{}", self.provider, self.resource_type)
        }
    }

    /// Borrow this id as a human-facing display value.
    ///
    /// Unlike the default `Display`, which renders the canonical dotted
    /// form (`provider.resource_type.identity`) used as a logical identifier
    /// for hashmap keys, binding fallbacks, and DSL syntax, this wrapper
    /// renders `provider.resource_type` and `identity` separated by a single
    /// space — making the type/address boundary visible to readers
    /// (carina-rs/carina#2572).
    ///
    /// Use this in progress UIs and human-readable plan/apply output;
    /// keep using `Display` for any context that round-trips through
    /// state files, lookup keys, or DSL.
    pub fn human(&self) -> ResourceIdDisplay<'_> {
        ResourceIdDisplay(self)
    }
}

/// Display-only projection of a resource identity state.
pub struct ResourceIdentityDisplay<'a>(&'a ResourceIdentityState);

impl Display for ResourceIdentityDisplay<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.0 {
            ResourceIdentityState::Pending(_) => f.write_str("<pending>"),
            ResourceIdentityState::Resolved(identity) => identity.fmt(f),
        }
    }
}

/// Human-facing display wrapper for [`ResourceId`].
///
/// Construct with [`ResourceId::human`]; renders via `Display` as
/// `provider.resource_type<SPACE>identity` (or `resource_type<SPACE>identity`
/// when the provider segment is empty).
///
/// The wrapper exists as a distinct type, rather than as a second
/// inherent method that returns `String`, so the choice between
/// "human-readable display" and "canonical logical identifier" is
/// visible at every call site instead of being a string-formatting
/// convention. See carina-rs/carina#2572 for context.
pub struct ResourceIdDisplay<'a>(&'a ResourceId);

impl std::fmt::Display for ResourceIdDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id = self.0;
        match id.provider.is_empty() {
            true => write!(f, "{} {}", id.resource_type, id.identity_display()),
            false => write!(
                f,
                "{}.{} {}",
                id.provider,
                id.resource_type,
                id.identity_display()
            ),
        }
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.provider.is_empty() {
            true => write!(f, "{}.{}", self.resource_type, self.identity_display()),
            false => write!(
                f,
                "{}.{}.{}",
                self.provider,
                self.resource_type,
                self.identity_display()
            ),
        }
    }
}

/// A `ResourceId` whose identity has been resolved. Downstream consumers
/// (differ, scheduler, executor, state, provider) hold this type to
/// guarantee at compile time that every resource has an identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "ResourceId", into = "ResourceId")]
pub struct ResolvedResourceId(ResourceId);

impl ResolvedResourceId {
    pub fn with_identity(
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
    ) -> Self {
        Self(ResourceId::with_identity(resource_type, identity))
    }

    pub fn with_provider_identity(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
        provider_instance: Option<String>,
    ) -> Self {
        Self(ResourceId::with_provider_identity(
            provider,
            resource_type,
            identity,
            provider_instance,
        ))
    }

    /// Construct from a `ResourceId`, panicking if identity is pending.
    pub fn new(id: ResourceId) -> Self {
        assert!(
            matches!(id.identity_state(), ResourceIdentityState::Resolved(_)),
            "ResolvedResourceId requires identity"
        );
        Self(id)
    }

    /// Try to construct; returns `None` if identity is pending.
    pub fn try_new(id: ResourceId) -> Option<Self> {
        match id.identity_state() {
            ResourceIdentityState::Pending(_) => None,
            ResourceIdentityState::Resolved(_) => Some(Self(id)),
        }
    }

    /// Borrow the inner `ResourceId`.
    pub fn as_inner(&self) -> &ResourceId {
        &self.0
    }

    /// Consume and return the inner `ResourceId`.
    pub fn into_inner(self) -> ResourceId {
        self.0
    }

    /// The identity string (always present).
    pub fn identity_str(&self) -> &str {
        match self.0.identity_state() {
            ResourceIdentityState::Pending(_) => {
                unreachable!("ResolvedResourceId requires identity")
            }
            ResourceIdentityState::Resolved(identity) => identity.as_str(),
        }
    }

    /// The resolved identity (always present).
    pub fn identity(&self) -> &ResourceIdentity {
        match self.0.identity_state() {
            ResourceIdentityState::Pending(_) => {
                unreachable!("ResolvedResourceId requires identity")
            }
            ResourceIdentityState::Resolved(identity) => identity,
        }
    }

    pub fn display_type(&self) -> String {
        self.0.display_type()
    }

    pub fn human(&self) -> ResourceIdDisplay<'_> {
        self.0.human()
    }

    pub fn identity_display(&self) -> ResourceIdentityDisplay<'_> {
        self.0.identity_display()
    }

    pub fn provider(&self) -> &str {
        &self.0.provider
    }

    pub fn resource_type(&self) -> &str {
        &self.0.resource_type
    }

    pub fn provider_instance(&self) -> Option<&str> {
        self.0.provider_instance.as_deref()
    }

    pub(crate) fn instantiate_with_identity(&self, identity: ResourceIdentity) -> Self {
        Self(self.0.instantiate_with_identity(identity))
    }
}

impl Deref for ResolvedResourceId {
    type Target = ResourceId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Display for ResolvedResourceId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<ResolvedResourceId> for ResourceId {
    fn from(r: ResolvedResourceId) -> Self {
        r.0
    }
}

impl TryFrom<ResourceId> for ResolvedResourceId {
    type Error = String;

    fn try_from(id: ResourceId) -> Result<Self, String> {
        Self::try_new(id).ok_or_else(|| "identity is required".to_string())
    }
}

impl PartialEq<ResourceId> for ResolvedResourceId {
    fn eq(&self, other: &ResourceId) -> bool {
        self.0 == *other
    }
}

impl PartialEq<ResolvedResourceId> for ResourceId {
    fn eq(&self, other: &ResolvedResourceId) -> bool {
        *self == other.0
    }
}

impl AsRef<ResourceId> for ResolvedResourceId {
    fn as_ref(&self) -> &ResourceId {
        self.as_inner()
    }
}

impl Borrow<ResourceId> for ResolvedResourceId {
    fn borrow(&self) -> &ResourceId {
        self.as_inner()
    }
}

/// A [`Resource`] whose identity has been resolved.
///
/// Downstream effect consumers hold this type to guarantee at compile
/// time that the wrapped resource has an identity. The inner field is
/// private so callers cannot bypass the constructor invariant:
///
/// ```compile_fail
/// use carina_core::resource::{ResolvedResource, Resource};
///
/// let resource = Resource::pending("test");
/// let _ = ResolvedResource(resource);
/// ```
///
/// ```compile_fail
/// use carina_core::resource::{ResolvedResource, Resource};
///
/// let resource = Resource::pending("test");
/// let _: ResolvedResource = resource.into();
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Resource", into = "Resource")]
pub struct ResolvedResource(Resource);

/// A resource reached a resolved-only pipeline boundary before its identity
/// was assigned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("resource '{id}' still has a pending identity")]
pub struct PendingResourceIdentityError {
    id: ResourceId,
}

impl ResolvedResource {
    /// Construct a resource from an already-resolved id.
    pub fn from_id(id: ResolvedResourceId) -> Self {
        Self(Resource::from_id(id.into_inner()))
    }

    /// Construct from a [`Resource`], panicking if identity is pending.
    pub fn new(resource: Resource) -> Self {
        assert!(
            matches!(
                resource.id.identity_state(),
                ResourceIdentityState::Resolved(_)
            ),
            "ResolvedResource requires identity"
        );
        Self(resource)
    }

    /// Try to construct; returns `None` if identity is pending.
    pub fn try_new(resource: Resource) -> Option<Self> {
        match resource.id.identity_state() {
            ResourceIdentityState::Pending(_) => None,
            ResourceIdentityState::Resolved(_) => Some(Self(resource)),
        }
    }

    /// Convert a whole resource collection at the pipeline boundary where
    /// identity assignment is complete.
    pub fn collect_resolved(
        resources: impl IntoIterator<Item = Resource>,
    ) -> Result<Vec<Self>, PendingResourceIdentityError> {
        resources
            .into_iter()
            .map(|resource| match resource.id.identity_state() {
                ResourceIdentityState::Pending(_) => {
                    Err(PendingResourceIdentityError { id: resource.id })
                }
                ResourceIdentityState::Resolved(_) => Ok(Self(resource)),
            })
            .collect()
    }

    /// Borrow the inner [`Resource`].
    pub fn as_inner(&self) -> &Resource {
        &self.0
    }

    /// Borrow the underlying resource for provider-boundary consumers.
    pub fn as_resource(&self) -> &Resource {
        self.as_inner()
    }

    /// Consume and return the inner [`Resource`].
    pub fn into_inner(self) -> Resource {
        self.0
    }

    pub fn identity(&self) -> &ResourceIdentity {
        match self.0.id.identity_state() {
            ResourceIdentityState::Pending(_) => {
                unreachable!("ResolvedResource requires identity")
            }
            ResourceIdentityState::Resolved(identity) => identity,
        }
    }

    pub fn identity_str(&self) -> &str {
        self.identity().as_str()
    }

    pub fn resolved_id(&self) -> ResolvedResourceId {
        ResolvedResourceId(self.0.id.clone())
    }

    /// Set an attribute without weakening the resolved-identity invariant.
    pub fn set_attr(&mut self, key: impl Into<String>, value: Value) {
        self.0.set_attr(key, value);
    }
}

impl Deref for ResolvedResource {
    type Target = Resource;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<ResolvedResource> for Resource {
    fn from(resource: ResolvedResource) -> Self {
        resource.0
    }
}

impl TryFrom<Resource> for ResolvedResource {
    type Error = String;

    fn try_from(resource: Resource) -> Result<Self, Self::Error> {
        Self::try_new(resource).ok_or_else(|| "identity is required".to_string())
    }
}

impl AsRef<Resource> for ResolvedResource {
    fn as_ref(&self) -> &Resource {
        self.as_inner()
    }
}

/// A `[index]` subscript appended to an `AccessPath`'s field chain.
///
/// `binding.field[0]` and `binding.field['k']` parse into the same
/// `binding` + `attribute` + `field_path` as `binding.field`, plus a
/// trailing `Subscript` capturing the `[…]` form. Distinguishing
/// integer from string at this layer lets cross-directory shape checks
/// reject `[0]` against a `map(_)` export and `["k"]` against a
/// `list(_)` export with type-aware diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
// `index_kind` (not `kind`) so the tag cannot collide with the
// `PathSegment` discriminator when a `Subscript` is `#[serde(flatten)]`-ed
// into a `PathSegment::Subscript` (carina#3025).
#[serde(tag = "index_kind", rename_all = "snake_case")]
pub enum Subscript {
    /// Integer subscript: `[0]`. Valid against `list(_)` exports.
    Int { index: i64 },
    /// String subscript: `['k']`. Valid against `map(_)` exports.
    Str { key: ValueString },
}

impl Subscript {
    /// Append this subscript to `out` in canonical source form: `[0]` or
    /// `['k']`. String keys use the same literal encoder as `carina fmt`.
    pub fn append_to_dot_string(&self, out: &mut String) {
        use std::fmt::Write as _;
        match self {
            Subscript::Int { index } => {
                let _ = write!(out, "[{}]", index);
            }
            Subscript::Str { key } => match key.diagnostic_literal() {
                Some(key) => {
                    let _ = write!(out, "[{}]", display_string_literal(key));
                }
                None => out.push_str("[value from env()]"),
            },
        }
    }

    /// Append the runtime path spelling. This is reserved for evaluation,
    /// identity, serialization, and provider transport; diagnostics must use
    /// [`Self::append_to_dot_string`] so an `env()`-derived key stays marked.
    pub fn append_to_plaintext_dot_string(&self, out: &mut String) {
        use std::fmt::Write as _;
        match self {
            Subscript::Int { index } => {
                let _ = write!(out, "[{index}]");
            }
            Subscript::Str { key } => {
                let _ = write!(out, "[{:?}]", key.expose_plaintext());
            }
        }
    }
}

/// One step along an [`AccessPath`] past its `binding.attribute` head.
///
/// A path is an ordered sequence of `PathSegment`s; the grammar can
/// produce any mix of `.field` and `[index]` continuations at any
/// depth (e.g. `cert.foo[0].bar[1].baz`). carina#3025.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PathSegment {
    /// A `.field` continuation.
    Field { name: String },
    /// A `[index]` / `['key']` subscript.
    Subscript {
        #[serde(flatten)]
        index: Subscript,
    },
}

impl PathSegment {
    /// Append this segment in DSL source form (`.field`, `[0]`, or
    /// `['key']`). Diagnostic-sensitive string keys stay redacted through
    /// [`Subscript::append_to_dot_string`].
    pub fn append_to_dot_string(&self, out: &mut String) {
        match self {
            Self::Field { name } => {
                out.push('.');
                out.push_str(name);
            }
            Self::Subscript { index } => index.append_to_dot_string(out),
        }
    }

    /// Return this segment in DSL source form.
    pub fn to_dot_string(&self) -> String {
        let mut out = String::new();
        self.append_to_dot_string(&mut out);
        out
    }

    /// Return this segment in runtime source form, exposing a string
    /// subscript only for internal identity and hashing paths.
    pub(crate) fn to_plaintext_dot_string(&self) -> String {
        let mut out = String::new();
        match self {
            Self::Field { name } => {
                out.push('.');
                out.push_str(name);
            }
            Self::Subscript { index } => index.append_to_plaintext_dot_string(&mut out),
        }
        out
    }
}

impl std::fmt::Display for PathSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_dot_string())
    }
}

/// A typed access path representing a `ResourceRef` target.
///
/// The path always carries a binding name and an attribute name; the
/// remainder of the chain is an ordered `Vec<PathSegment>` that
/// captures the source-form mix of `.field` and `[index]`
/// continuations at any depth (carina#3025). Pre-attribute index
/// access (e.g. `binding[0].x`) is still folded into the binding name
/// string by the parser as before.
///
/// The "binding + attribute is mandatory" invariant is enforced by the
/// type system — there is no way to construct an `AccessPath` without
/// both.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccessPath {
    binding: ValueString,
    attribute: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    segments: Vec<PathSegment>,
}

impl AccessPath {
    /// Create an `AccessPath` referring to a top-level attribute of a binding.
    ///
    /// `attribute` must be non-empty. A bare-binding reference (no
    /// `.attr`) is represented by [`Value::BindingRef`], not by an
    /// `AccessPath` with an empty attribute.
    pub fn new(binding: impl Into<ValueString>, attribute: impl Into<String>) -> Self {
        Self::with_segments(binding, attribute, Vec::new())
    }

    /// Create an `AccessPath` with a nested field path (e.g.,
    /// `web.network.vpc_id`). Equivalent to a path whose every segment
    /// after `attribute` is a `Field`.
    ///
    /// `attribute` must be non-empty. See [`AccessPath::new`].
    pub fn with_fields(
        binding: impl Into<ValueString>,
        attribute: impl Into<String>,
        field_path: Vec<String>,
    ) -> Self {
        let segments = field_path
            .into_iter()
            .map(|name| PathSegment::Field { name })
            .collect();
        Self::with_segments(binding, attribute, segments)
    }

    /// Create an `AccessPath` with both a field chain and trailing
    /// `[index]` subscripts. Used by the parser when source contains
    /// `binding.field[idx]` or `binding.field.subfield[idx]…`.
    ///
    /// `attribute` must be non-empty. See [`AccessPath::new`].
    pub fn with_fields_and_subscripts(
        binding: impl Into<ValueString>,
        attribute: impl Into<String>,
        field_path: Vec<String>,
        subscripts: Vec<Subscript>,
    ) -> Self {
        let mut segments: Vec<PathSegment> =
            Vec::with_capacity(field_path.len() + subscripts.len());
        segments.extend(
            field_path
                .into_iter()
                .map(|name| PathSegment::Field { name }),
        );
        segments.extend(
            subscripts
                .into_iter()
                .map(|index| PathSegment::Subscript { index }),
        );
        Self::with_segments(binding, attribute, segments)
    }

    /// Create an `AccessPath` from an explicit segment list — the
    /// canonical form. Used by the parser when source contains any
    /// mix of `.field` and `[index]` continuations
    /// (`cert.foo[0].bar`, `a.b.c[0]["k"].d`, …). carina#3025.
    ///
    /// `attribute` must be non-empty. See [`AccessPath::new`].
    pub fn with_segments(
        binding: impl Into<ValueString>,
        attribute: impl Into<String>,
        segments: Vec<PathSegment>,
    ) -> Self {
        let binding = binding.into();
        let attribute = attribute.into();
        assert!(
            !attribute.is_empty(),
            "AccessPath::with_segments with empty attribute for binding {:?}; \
             use Value::BindingRef instead (#2847)",
            binding
        );
        Self {
            binding,
            attribute,
            segments,
        }
    }

    /// Return this reference path with one more postfix access step.
    ///
    /// Reference-rooted postfix syntax is normalized through this method so
    /// `(resource.attr)[0].field` has exactly the same representation as the
    /// direct `resource.attr[0].field` spelling.
    pub(crate) fn with_appended_segment(mut self, segment: PathSegment) -> Self {
        self.segments.push(segment);
        self
    }

    /// Returns the binding name.
    pub fn binding(&self) -> &str {
        self.binding.expose_plaintext()
    }

    /// Returns the attribute name.
    pub fn attribute(&self) -> &str {
        &self.attribute
    }

    /// Returns the ordered segments past `binding.attribute`. Empty
    /// for a top-level attribute reference. carina#3025.
    pub fn segments(&self) -> &[PathSegment] {
        &self.segments
    }

    /// Returns the leading run of `.field` segments before the first
    /// `[idx]` subscript. For `cert.foo.bar[0].baz` this yields
    /// `["foo", "bar"]`; for `cert.foo[0].bar` it yields `["foo"]`;
    /// for `cert.foo[0]` it yields `["foo"]`; for `cert.foo` it yields
    /// `["foo"]`. Used by cross-directory shape checkers that walk
    /// dot-form access against an `UpstreamExports` type expression.
    pub fn leading_field_path(&self) -> Vec<String> {
        self.segments
            .iter()
            .map_while(|seg| match seg {
                PathSegment::Field { name } => Some(name.clone()),
                PathSegment::Subscript { .. } => None,
            })
            .collect()
    }

    /// Returns the trailing run of `[idx]` subscripts after the leading
    /// `.field` prefix. For `cert.foo.bar[0]["k"]` this yields the two
    /// subscripts; for `cert.foo[0].bar` it yields *empty* (the
    /// chained-access case where a `.field` follows a subscript — the
    /// trailing run terminates because subscripts aren't contiguous at
    /// the end). carina#3025.
    pub fn trailing_subscripts(&self) -> Vec<Subscript> {
        // Walk from the end; stop at the first Field encountered (going
        // backwards). The resulting slice is reversed to restore order.
        let mut out: Vec<Subscript> = Vec::new();
        for seg in self.segments.iter().rev() {
            match seg {
                PathSegment::Subscript { index } => out.push(index.clone()),
                PathSegment::Field { .. } => break,
            }
        }
        out.reverse();
        out
    }

    /// Returns `true` when the path's segment chain is the
    /// "leading fields then trailing subscripts" shape — the
    /// pre-carina#3025 grammar. Chained access (`cert.foo[0].bar`) is
    /// the only shape that fails this check; cross-directory shape
    /// checkers that don't yet understand chained walks can skip those
    /// paths and let resolver-time errors surface instead.
    pub fn is_simple_shape(&self) -> bool {
        // Equivalent to: there is at most one transition from Field
        // to Subscript in the chain.
        let mut seen_subscript = false;
        for seg in &self.segments {
            match seg {
                PathSegment::Field { .. } => {
                    if seen_subscript {
                        return false;
                    }
                }
                PathSegment::Subscript { .. } => {
                    seen_subscript = true;
                }
            }
        }
        true
    }

    /// Returns the path in source-form: `binding.attribute` followed by
    /// `.field…[idx]…` segments as written.
    pub fn to_dot_string(&self) -> String {
        self.render_dot_string(false)
    }

    /// Return the runtime path spelling, including plaintext string
    /// subscripts. Callers must use this only as a data channel, never to
    /// construct an error, warning, or log message.
    pub fn to_plaintext_dot_string(&self) -> String {
        self.render_dot_string(true)
    }

    fn render_dot_string(&self, expose_plaintext: bool) -> String {
        let segment_len: usize = self
            .segments
            .iter()
            .map(|s| match s {
                PathSegment::Field { name } => name.len() + 1,
                PathSegment::Subscript { .. } => 4, // rough: `[xx]`
            })
            .sum();
        let binding = if expose_plaintext {
            self.binding.expose_plaintext()
        } else {
            self.binding
                .diagnostic_literal()
                .unwrap_or("value from env()")
        };
        let mut out = String::with_capacity(binding.len() + self.attribute.len() + 1 + segment_len);
        out.push_str(binding);
        out.push('.');
        out.push_str(&self.attribute);
        for segment in &self.segments {
            match segment {
                PathSegment::Field { name } => {
                    out.push('.');
                    out.push_str(name);
                }
                PathSegment::Subscript { index } => {
                    if expose_plaintext {
                        index.append_to_plaintext_dot_string(&mut out);
                    } else {
                        index.append_to_dot_string(&mut out);
                    }
                }
            }
        }
        out
    }
}

impl std::fmt::Display for AccessPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_dot_string())
    }
}

/// A reference-shaped value, including both attribute access and a bare
/// binding. [`AccessPath`] deliberately requires an attribute, while typed
/// value parameters can be projected as `BindingAccess` during static
/// validation. This enum lets static type resolution cover both without
/// weakening `AccessPath`'s non-empty-attribute invariant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReferencePath {
    Access(AccessPath),
    Binding(String),
    BindingAccess {
        binding: String,
        segments: Vec<PathSegment>,
    },
}

impl ReferencePath {
    pub fn as_ref(&self) -> ReferencePathRef<'_> {
        match self {
            Self::Access(path) => ReferencePathRef::Access(path),
            Self::Binding(binding) => ReferencePathRef::Binding(binding),
            Self::BindingAccess { binding, segments } => {
                ReferencePathRef::BindingAccess { binding, segments }
            }
        }
    }

    pub fn to_dot_string(&self) -> String {
        match self {
            Self::Access(path) => path.to_dot_string(),
            Self::Binding(binding) => binding.clone(),
            Self::BindingAccess { binding, segments } => {
                let mut rendered = binding.clone();
                for segment in segments {
                    segment.append_to_dot_string(&mut rendered);
                }
                rendered
            }
        }
    }

    pub fn to_plaintext_dot_string(&self) -> String {
        match self {
            Self::Access(path) => path.to_plaintext_dot_string(),
            Self::Binding(binding) => binding.clone(),
            Self::BindingAccess { binding, segments } => {
                let mut rendered = binding.clone();
                for segment in segments {
                    rendered.push_str(&segment.to_plaintext_dot_string());
                }
                rendered
            }
        }
    }
}

impl std::fmt::Display for ReferencePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Access(path) => path.fmt(f),
            Self::Binding(binding) => binding.fmt(f),
            Self::BindingAccess { .. } => f.write_str(&self.to_dot_string()),
        }
    }
}

/// Project a parser-only module-argument placeholder and its postfix chain
/// into the static reference path understood by [`crate::binding_index`].
///
/// Module arguments are values, not structural resource references, so their
/// runtime representation deliberately remains `Unknown(ModuleArgument)` plus
/// `Access`. This projection is validation metadata only: it does not weaken
/// the construction invariant that a structural reference-rooted access is
/// canonicalized to `ResourceRef` / `BindingRef`.
pub(crate) fn module_argument_reference_path(value: &Value) -> Option<ReferencePath> {
    fn collect(value: &Value) -> Option<(String, Vec<PathSegment>)> {
        match value {
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument {
                name, ..
            })) => Some((name.clone(), Vec::new())),
            Value::Deferred(DeferredValue::Access(access)) => {
                let (binding, mut segments) = collect(access.base())?;
                segments.push(access.step().clone());
                Some((binding, segments))
            }
            _ => None,
        }
    }

    let (binding, segments) = collect(value)?;
    if segments.is_empty() {
        Some(ReferencePath::Binding(binding))
    } else {
        Some(ReferencePath::BindingAccess { binding, segments })
    }
}

/// Borrowed projection of [`ReferencePath`] used by validation walkers.
#[derive(Debug, Clone, Copy)]
pub enum ReferencePathRef<'a> {
    Access(&'a AccessPath),
    Binding(&'a str),
    BindingAccess {
        binding: &'a str,
        segments: &'a [PathSegment],
    },
}

impl<'a> ReferencePathRef<'a> {
    pub fn binding(self) -> &'a str {
        match self {
            Self::Access(path) => path.binding(),
            Self::Binding(binding) => binding,
            Self::BindingAccess { binding, .. } => binding,
        }
    }

    pub fn to_dot_string(self) -> String {
        match self {
            Self::Access(path) => path.to_dot_string(),
            Self::Binding(binding) => binding.to_string(),
            Self::BindingAccess { binding, segments } => {
                let mut rendered = binding.to_string();
                for segment in segments {
                    segment.append_to_dot_string(&mut rendered);
                }
                rendered
            }
        }
    }

    pub fn to_plaintext_dot_string(self) -> String {
        match self {
            Self::Access(path) => path.to_plaintext_dot_string(),
            Self::Binding(binding) => binding.to_string(),
            Self::BindingAccess { binding, segments } => {
                let mut rendered = binding.to_string();
                for segment in segments {
                    rendered.push_str(&segment.to_plaintext_dot_string());
                }
                rendered
            }
        }
    }

    pub fn to_owned(self) -> ReferencePath {
        match self {
            Self::Access(path) => ReferencePath::Access(path.clone()),
            Self::Binding(binding) => ReferencePath::Binding(binding.to_string()),
            Self::BindingAccess { binding, segments } => ReferencePath::BindingAccess {
                binding: binding.to_string(),
                segments: segments.to_vec(),
            },
        }
    }
}

impl<'a> From<&'a AccessPath> for ReferencePathRef<'a> {
    fn from(path: &'a AccessPath) -> Self {
        Self::Access(path)
    }
}

impl<'a> From<&'a str> for ReferencePathRef<'a> {
    fn from(binding: &'a str) -> Self {
        Self::Binding(binding)
    }
}

impl<'a> From<&'a ReferencePath> for ReferencePathRef<'a> {
    fn from(path: &'a ReferencePath) -> Self {
        path.as_ref()
    }
}

/// Peel `Value::Secret` wrappers, returning the inner value and the
/// number of layers peeled. Pair with [`rewrap_secrets`] so dot-form /
/// subscript access can descend into a secret container and re-tag the
/// leaf (#2439; plan-display redaction depends on the tag). Single
/// source of truth for the secret-tunnel discipline, shared by
/// `resolver::resolve_ref_value` and [`navigate_value_path`].
pub(crate) fn peel_secrets(mut value: Value) -> (Value, usize) {
    let mut depth = 0;
    while let Value::Deferred(DeferredValue::Secret(inner)) = value {
        value = *inner;
        depth += 1;
    }
    (value, depth)
}

/// Re-wrap `value` in `depth` layers of `Value::Secret`. Inverse of
/// [`peel_secrets`].
pub(crate) fn rewrap_secrets(value: Value, depth: usize) -> Value {
    (0..depth).fold(value, |acc, _| {
        Value::Deferred(DeferredValue::Secret(Box::new(acc)))
    })
}

/// Failure to apply one [`PathSegment`] to a value whose shape is already
/// known. Deferred bases are not errors: [`apply_value_path_segment`] retains
/// them as [`DeferredValue::Access`] until resolution makes their shape
/// concrete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueAccessError {
    IndexOutOfRange {
        index: i64,
        len: usize,
    },
    MissingField {
        name: String,
        available: Option<Vec<String>>,
        len: usize,
    },
    MissingKey {
        key: ValueString,
        available: Option<Vec<String>>,
        len: usize,
    },
    WrongShape {
        step: PathSegment,
        actual: &'static str,
    },
}

impl ValueAccessError {
    fn available_suffix(available: &Option<Vec<String>>, len: usize) -> String {
        match available {
            None => format!("; available keys redacted ({len} entries)"),
            Some(keys) if keys.is_empty() => "; available keys: <no keys>".to_string(),
            Some(keys) => format!(
                "; available keys: {}",
                keys.iter()
                    .map(|key| display_string_literal(key))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Render the historical path-oriented resolver diagnostic from
    /// #2435/#2447. Dot and subscript map misses intentionally share this
    /// format, and secret maps expose only their entry count.
    pub(crate) fn format_for_path(&self, path: &str) -> String {
        match self {
            Self::MissingField { available, len, .. } | Self::MissingKey { available, len, .. } => {
                let known = match available {
                    None => format!("<redacted, {len} entries>"),
                    Some(keys) if keys.is_empty() => "<no keys>".to_string(),
                    Some(keys) => keys
                        .iter()
                        .map(|key| display_string_literal(key))
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                format!("{path}: key not found; available keys: {known}")
            }
            _ => format!("{path}: {self}"),
        }
    }
}

impl std::fmt::Display for ValueAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IndexOutOfRange { index, len } => {
                write!(f, "index {index} out of range for list of length {len}")
            }
            Self::MissingField {
                name,
                available,
                len,
            } => write!(
                f,
                "field {} not found in map{}",
                display_string_literal(name),
                Self::available_suffix(available, *len)
            ),
            Self::MissingKey {
                key,
                available,
                len,
            } => {
                let key = key
                    .diagnostic_literal()
                    .map_or_else(|| "value from env()".to_string(), display_string_literal);
                write!(
                    f,
                    "key {key} not found in map{}",
                    Self::available_suffix(available, *len)
                )
            }
            Self::WrongShape { step, actual } => {
                write!(f, "cannot apply access {step} to {actual}")
            }
        }
    }
}

impl std::error::Error for ValueAccessError {}

fn concrete_value_shape(value: &ConcreteValue) -> &'static str {
    match value {
        ConcreteValue::String(_) => "string",
        ConcreteValue::EnumIdentifier(_) | ConcreteValue::CanonicalEnum(_) => "enum",
        ConcreteValue::Int(_) => "integer",
        ConcreteValue::Float(_) => "float",
        ConcreteValue::Bool(_) => "boolean",
        ConcreteValue::Duration(_) => "duration",
        ConcreteValue::List(_) | ConcreteValue::StringList(_) => "list",
        ConcreteValue::Map(_) => "map",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ValueAccessMiss {
    IndexOutOfRange {
        index: i64,
        len: usize,
    },
    MissingField {
        name: String,
        available: Option<Vec<String>>,
        len: usize,
    },
    MissingKey {
        key: ValueString,
        available: Option<Vec<String>>,
        len: usize,
    },
}

impl From<ValueAccessMiss> for ValueAccessError {
    fn from(miss: ValueAccessMiss) -> Self {
        match miss {
            ValueAccessMiss::IndexOutOfRange { index, len } => Self::IndexOutOfRange { index, len },
            ValueAccessMiss::MissingField {
                name,
                available,
                len,
            } => Self::MissingField {
                name,
                available,
                len,
            },
            ValueAccessMiss::MissingKey {
                key,
                available,
                len,
            } => Self::MissingKey {
                key,
                available,
                len,
            },
        }
    }
}

fn checked_list_position(index: i64, len: usize) -> Result<usize, ValueAccessMiss> {
    usize::try_from(index)
        .ok()
        .filter(|position| *position < len)
        .ok_or(ValueAccessMiss::IndexOutOfRange { index, len })
}

/// What a known-shape access should do when its selected field/index is not
/// present. Local binding views can be incomplete until apply, whereas
/// literals and loaded upstream values are final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMissPolicy {
    Error,
    Defer,
}

/// Result of applying one path segment. The distinction lets reference
/// resolution preserve the original `ResourceRef` when navigation itself is
/// undecidable, without guessing from the returned `Value` shape.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ValuePathSegmentResult {
    Resolved(Value),
    Deferred(Value),
    /// The selected element is absent from a concrete container, but the
    /// caller's source policy says that view may be incomplete.  The caller
    /// must retain its original unresolved expression; constructing an
    /// `Access` around this concrete `base` would violate the Access invariant.
    DeferredMiss(ValueAccessError),
}

impl ValuePathSegmentResult {
    pub(crate) fn into_value(self) -> Result<Value, ValueAccessError> {
        match self {
            Self::Resolved(value) | Self::Deferred(value) => Ok(value),
            Self::DeferredMiss(error) => Err(error),
        }
    }
}

fn deferred_access(base: Value, step: PathSegment, source_line: usize) -> Value {
    let display_root = match &base {
        Value::Deferred(DeferredValue::Access(access)) => access.display_root.clone(),
        _ => None,
    };
    Value::Deferred(DeferredValue::Access(AccessExpr {
        base: Box::new(base),
        step,
        source_line,
        display_root,
    }))
}

fn handle_access_miss(
    miss_policy: AccessMissPolicy,
    miss: ValueAccessMiss,
) -> Result<ValuePathSegmentResult, ValueAccessError> {
    let error = ValueAccessError::from(miss);
    match miss_policy {
        AccessMissPolicy::Error => Err(error),
        AccessMissPolicy::Defer => Ok(ValuePathSegmentResult::DeferredMiss(error)),
    }
}

/// Apply one field/index step to a value.
///
/// This is the single semantic primitive used by parsing, runtime resolution,
/// and [`navigate_value_path`]. Concrete containers are navigated immediately;
/// a deferred base is retained as [`DeferredValue::Access`]. Secret wrappers
/// are peeled before navigation and restored around the selected value.
pub(crate) fn apply_value_path_segment(
    base: Value,
    step: PathSegment,
    source_line: usize,
    miss_policy: AccessMissPolicy,
) -> Result<ValuePathSegmentResult, ValueAccessError> {
    let (peeled, secret_depth) = peel_secrets(base);
    let selected = match peeled {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let reference = Value::Deferred(DeferredValue::ResourceRef {
                path: path.with_appended_segment(step),
            });
            return Ok(ValuePathSegmentResult::Deferred(rewrap_secrets(
                reference,
                secret_depth,
            )));
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            let reference = match step {
                PathSegment::Field { name } => Value::Deferred(DeferredValue::ResourceRef {
                    path: AccessPath::new(binding, name),
                }),
                PathSegment::Subscript {
                    index: Subscript::Int { index },
                } => Value::Deferred(DeferredValue::BindingRef {
                    binding: format!("{binding}[{index}]"),
                }),
                PathSegment::Subscript {
                    index: Subscript::Str { key },
                } => Value::Deferred(DeferredValue::BindingRef {
                    binding: crate::utils::map_key_address(&binding, key.expose_plaintext()),
                }),
            };
            return Ok(ValuePathSegmentResult::Deferred(rewrap_secrets(
                reference,
                secret_depth,
            )));
        }
        Value::Deferred(deferred) => {
            let base = rewrap_secrets(Value::Deferred(deferred), secret_depth);
            return Ok(ValuePathSegmentResult::Deferred(deferred_access(
                base,
                step,
                source_line,
            )));
        }
        Value::Concrete(ConcreteValue::List(items)) => match &step {
            PathSegment::Subscript {
                index: Subscript::Int { index },
            } => {
                let position = match checked_list_position(*index, items.len()) {
                    Ok(position) => position,
                    Err(error) => {
                        return handle_access_miss(miss_policy, error);
                    }
                };
                items[position].clone()
            }
            _ => {
                return Err(ValueAccessError::WrongShape {
                    step: step.clone(),
                    actual: "list",
                });
            }
        },
        Value::Concrete(ConcreteValue::StringList(items)) => match &step {
            PathSegment::Subscript {
                index: Subscript::Int { index },
            } => {
                let position = match checked_list_position(*index, items.len()) {
                    Ok(position) => position,
                    Err(error) => {
                        return handle_access_miss(miss_policy, error);
                    }
                };
                Value::Concrete(ConcreteValue::String(items[position].clone()))
            }
            _ => {
                return Err(ValueAccessError::WrongShape {
                    step: step.clone(),
                    actual: "list",
                });
            }
        },
        Value::Concrete(ConcreteValue::Map(map)) => {
            let available = (secret_depth == 0).then(|| map.keys().cloned().collect());
            let len = map.len();
            match &step {
                PathSegment::Field { name } => match map.get(name).cloned() {
                    Some(value) => value,
                    None => {
                        let error = ValueAccessMiss::MissingField {
                            name: name.clone(),
                            available,
                            len,
                        };
                        return handle_access_miss(miss_policy, error);
                    }
                },
                PathSegment::Subscript {
                    index: Subscript::Str { key },
                } => match map.get(key.expose_plaintext()).cloned() {
                    Some(value) => value,
                    None => {
                        let error = ValueAccessMiss::MissingKey {
                            key: key.clone(),
                            available,
                            len,
                        };
                        return handle_access_miss(miss_policy, error);
                    }
                },
                _ => {
                    return Err(ValueAccessError::WrongShape {
                        step: step.clone(),
                        actual: "map",
                    });
                }
            }
        }
        Value::Concrete(concrete) => {
            let actual = concrete_value_shape(&concrete);
            return Err(ValueAccessError::WrongShape {
                step: step.clone(),
                actual,
            });
        }
    };

    Ok(ValuePathSegmentResult::Resolved(rewrap_secrets(
        selected,
        secret_depth,
    )))
}

/// Construct one postfix access through the canonical access seam.
///
/// This convenience wrapper is used by value-tree transformations and tests;
/// it can return a concrete selection or canonical reference just as the
/// parser and resolver do. The raw [`DeferredValue::Access`] constructor is
/// intentionally unavailable outside this module.
pub fn apply_postfix_access(
    base: Value,
    step: PathSegment,
    source_line: usize,
    miss_policy: AccessMissPolicy,
) -> Result<Value, ValueAccessError> {
    apply_value_path_segment(base, step, source_line, miss_policy)
        .and_then(ValuePathSegmentResult::into_value)
}

/// Navigate a value tree along an [`AccessPath`]'s `attribute` then
/// ordered `segments`, returning the value at the leaf.
///
/// This is the single source of truth for "follow
/// `attribute.field[subscript]…` into a concrete `Value`" used by
/// loop-variable field-access resolution (carina#3136). The element a
/// `for` loop binds its value variable to is a plain concrete tree
/// (`Map`/`List`/scalar); `root` is that element, and the path's
/// `binding` (the loop variable name) has already been matched by the
/// caller, so navigation starts at `attribute`.
///
/// Semantics mirror the segment walk in
/// [`crate::resolver::resolve_ref_value`] exactly — `Map`+`Field`,
/// `List`+`Subscript::Int`, `Map`+`Subscript::Str` — including
/// `Secret` peel/rewrap so a secret leaf keeps its tag. Any missing
/// hop, shape mismatch (a field expected where the value is a scalar),
/// or out-of-range index yields `None`. A hop whose base is still deferred
/// also yields `None`; callers must not mistake a retained
/// [`DeferredValue::Access`] for a concrete navigation result. A
/// loop-variable path that does not navigate is "not resolvable here", never
/// a panic or a wrong value.
pub fn navigate_value_path(root: &Value, path: &AccessPath) -> Option<Value> {
    // The `attribute` head is the first hop: `root` is the loop
    // element, `root[attribute]` is the first navigated value.
    let (peeled_root, root_secret) = peel_secrets(root.clone());
    let Value::Concrete(ConcreteValue::Map(root_map)) = &peeled_root else {
        return None;
    };
    let mut current = rewrap_secrets(root_map.get(path.attribute())?.clone(), root_secret);

    for segment in path.segments() {
        current =
            match apply_value_path_segment(current, segment.clone(), 0, AccessMissPolicy::Error)
                .ok()?
            {
                ValuePathSegmentResult::Resolved(value) => value,
                ValuePathSegmentResult::Deferred(_) | ValuePathSegmentResult::DeferredMiss(_) => {
                    return None;
                }
            };
    }

    let (peeled, _) = peel_secrets(current.clone());
    matches!(peeled, Value::Concrete(_)).then_some(current)
}

/// Serde adapter that maps `std::time::Duration` ↔ integer seconds.
///
/// Without this, the default `Duration` serde impl emits
/// `{ "secs": N, "nanos": N }`, but the project's design decision is to
/// store durations as a plain integer at every JSON boundary
/// (state file, plan file, WIT plugin contract — see
/// `notes/specs/2026-05-10-duration-design.md`). The variant carries
/// `#[serde(with = "duration_secs")]` so this module is used uniformly
/// on serialise and deserialise.
pub(crate) mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

/// Attribute value of a resource.
///
/// Phase 5 of [RFC #2972](https://github.com/carina-rs/carina/issues/2972):
/// physically split into the concrete / deferred axis. Every
/// pattern site explicitly acknowledges which axis it handles, so
/// "deferred Value leaked into concrete-only path" bugs are
/// structurally unrepresentable workspace-wide (not just inside
/// `validate_*`).
///
/// The serde representation uses `#[serde(untagged)]` so existing
/// state files (in the pre-Phase-5 flat shape) round-trip via the
/// inner enums' externally-tagged defaults — exactly the legacy
/// JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Concrete(ConcreteValue),
    Deferred(DeferredValue),
}

/// A runtime string together with provenance used exclusively to make
/// diagnostic rendering safe.
///
/// The provenance bit is deliberately absent from equality, hashing, and the
/// serde representation. An `env()` result therefore behaves exactly like an
/// authored string in evaluation, plans, state, diffs, and provider payloads,
/// while diagnostic code cannot accidentally format its plaintext. There is
/// deliberately no `Display` implementation; `Debug` is redacted, and code
/// that genuinely needs the bytes at an execution or serialization boundary
/// must opt in through [`Self::expose_plaintext`].
#[derive(Clone)]
pub struct ValueString {
    plaintext: String,
    diagnostic_sensitive: bool,
}

impl ValueString {
    /// Construct an ordinary authored/runtime string.
    pub fn plain(plaintext: impl Into<String>) -> Self {
        Self {
            plaintext: plaintext.into(),
            diagnostic_sensitive: false,
        }
    }

    /// Construct a string whose contents must never be included in a
    /// diagnostic. Kept crate-private so only trusted value producers (`env()`
    /// and the builtin dispatcher while computing through `Secret`) can attach
    /// the provenance mark.
    pub(crate) fn diagnostic_sensitive(plaintext: impl Into<String>) -> Self {
        Self {
            plaintext: plaintext.into(),
            diagnostic_sensitive: true,
        }
    }

    /// Expose the runtime bytes for evaluation, plan/state serialization, or
    /// provider transport. Do not use this method to build diagnostics.
    pub fn expose_plaintext(&self) -> &str {
        &self.plaintext
    }

    /// Consume this value at an evaluation/serialization boundary.
    pub fn into_plaintext(self) -> String {
        self.plaintext
    }

    pub fn is_diagnostic_sensitive(&self) -> bool {
        self.diagnostic_sensitive
    }

    /// Return a diagnostic-safe authored literal, or `None` when only the
    /// runtime type may be rendered.
    pub fn diagnostic_literal(&self) -> Option<&str> {
        (!self.diagnostic_sensitive).then_some(self.plaintext.as_str())
    }

    /// Build a derived string and inherit diagnostic sensitivity from every
    /// string input that contributed to it.
    pub(crate) fn derived_from<'a>(
        plaintext: impl Into<String>,
        sources: impl IntoIterator<Item = &'a Self>,
    ) -> Self {
        Self {
            plaintext: plaintext.into(),
            diagnostic_sensitive: sources
                .into_iter()
                .any(|source| source.diagnostic_sensitive),
        }
    }

    pub(crate) fn push_derived(&mut self, other: &Self) {
        self.plaintext.push_str(other.expose_plaintext());
        self.diagnostic_sensitive |= other.diagnostic_sensitive;
    }
}

impl From<String> for ValueString {
    fn from(value: String) -> Self {
        Self::plain(value)
    }
}

impl From<&str> for ValueString {
    fn from(value: &str) -> Self {
        Self::plain(value)
    }
}

impl fmt::Debug for ValueString {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.diagnostic_literal() {
            Some(value) => fmt::Debug::fmt(value, f),
            None => f.write_str("<sensitive string>"),
        }
    }
}

impl PartialEq for ValueString {
    fn eq(&self, other: &Self) -> bool {
        self.plaintext == other.plaintext
    }
}

impl Eq for ValueString {}

impl Hash for ValueString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.plaintext.hash(state);
    }
}

impl PartialEq<str> for ValueString {
    fn eq(&self, other: &str) -> bool {
        self.plaintext == other
    }
}

impl PartialEq<&str> for ValueString {
    fn eq(&self, other: &&str) -> bool {
        self.plaintext == *other
    }
}

impl PartialEq<String> for ValueString {
    fn eq(&self, other: &String) -> bool {
        self.plaintext == *other
    }
}

impl PartialEq<ValueString> for str {
    fn eq(&self, other: &ValueString) -> bool {
        self == other.plaintext
    }
}

impl PartialEq<ValueString> for String {
    fn eq(&self, other: &ValueString) -> bool {
        *self == other.plaintext
    }
}

impl Serialize for ValueString {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose_plaintext())
    }
}

impl<'de> Deserialize<'de> for ValueString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::plain)
    }
}

/// Concrete-axis variants: values that carry their own runtime type
/// and are safe to type-check at validate time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConcreteValue {
    String(ValueString),
    /// An enum identifier — a value written in the DSL as a bare
    /// identifier (`tcp`) or namespaced identifier
    /// (`awscc.ec2.SecurityGroup.IpProtocol.tcp`), not as a quoted
    /// string literal. The parser routes `namespaced_id` and bare
    /// `variable_ref`s in `Enum` attribute positions into this
    /// variant so the validator can distinguish "identifier value"
    /// from "string value" without re-deriving the question from
    /// string content. See carina#2986.
    EnumIdentifier(RawEnumIdentifier),
    CanonicalEnum(CanonicalEnumValue),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// Time duration carried as `std::time::Duration`.
    ///
    /// Constructed from a `<integer><unit>` literal in DSL source
    /// `75min`, `1h`, `30s`. Serialises to JSON as integer seconds at
    /// every value-tree boundary (state file, plan file, WIT plugin
    /// contract).
    #[serde(with = "duration_secs")]
    Duration(std::time::Duration),
    List(Vec<Value>),
    /// Canonical form for fields whose schema type is
    /// `Union(vec![String, list(String)])` — the IAM-style
    /// `string_or_list_of_strings` shape. See #2481, #2510.
    StringList(Vec<ValueString>),
    Map(IndexMap<String, Value>),
}

/// Shared typed operand used while function and module arguments cross
/// evaluation boundaries. A structural handle never becomes a public
/// [`Value`]; only its canonical access base is exposed to reference walkers.
///
/// [`crate::eval_value::StructuralHandleMetadata`] is intentionally separate:
/// it is a pre-classification tree for directory fixed-point parsing, whereas
/// this enum represents one already-classified argument operand.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StructuralOperand {
    Value {
        value: Value,
        access_base: Option<Value>,
    },
    Handle {
        handle: crate::eval_value::StructuralHandle,
        access_base: Value,
    },
    /// A bare module parameter forwarded across a nested argument boundary
    /// before the outermost caller has determined whether it is a value or a
    /// handle. Keeping this state beside `Value` and `Handle` prevents a
    /// placeholder `Value` from prematurely deciding the classification.
    Parameter { name: String, access_base: Value },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum StructuralOperandRef<'a> {
    Value(&'a Value),
    Handle {
        handle: &'a crate::eval_value::StructuralHandle,
        access_base: &'a Value,
    },
    Parameter {
        name: &'a str,
        access_base: &'a Value,
    },
}

impl StructuralOperand {
    pub(crate) fn from_value(value: Value) -> Self {
        if let Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument {
            name, ..
        })) = &value
        {
            return Self::Parameter {
                name: name.to_string(),
                access_base: value,
            };
        }
        Self::Value {
            value,
            access_base: None,
        }
    }

    pub(crate) fn from_value_with_access_base(value: Value, access_base: Option<Value>) -> Self {
        if access_base.is_none()
            && let Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument {
                name,
                ..
            })) = &value
        {
            return Self::Parameter {
                name: name.to_string(),
                access_base: value,
            };
        }
        Self::Value { value, access_base }
    }

    pub(crate) fn from_handle(handle: crate::eval_value::StructuralHandle) -> Self {
        let access_base = handle.access_base_value();
        Self::Handle {
            handle,
            access_base,
        }
    }

    pub(crate) fn from_handle_with_access_base(
        handle: crate::eval_value::StructuralHandle,
        access_base: Value,
    ) -> Self {
        Self::Handle {
            handle,
            access_base,
        }
    }

    pub(crate) fn value(&self) -> Option<&Value> {
        match self {
            Self::Value { value, .. } => Some(value),
            Self::Handle { .. } | Self::Parameter { .. } => None,
        }
    }

    pub(crate) fn handle(&self) -> Option<&crate::eval_value::StructuralHandle> {
        match self {
            Self::Value { .. } | Self::Parameter { .. } => None,
            Self::Handle { handle, .. } => Some(handle),
        }
    }

    pub(crate) fn parameter(&self) -> Option<&str> {
        match self {
            Self::Parameter { name, .. } => Some(name),
            Self::Value { .. } | Self::Handle { .. } => None,
        }
    }

    pub(crate) fn value_ref(&self) -> StructuralOperandRef<'_> {
        match self {
            Self::Value { value, .. } => StructuralOperandRef::Value(value),
            Self::Handle {
                handle,
                access_base,
            } => StructuralOperandRef::Handle {
                handle,
                access_base,
            },
            Self::Parameter { name, access_base } => {
                StructuralOperandRef::Parameter { name, access_base }
            }
        }
    }

    pub(crate) fn access_ref(&self) -> StructuralOperandRef<'_> {
        match self {
            Self::Value { value, access_base } => {
                StructuralOperandRef::Value(access_base.as_ref().unwrap_or(value))
            }
            Self::Handle {
                handle,
                access_base,
            } => StructuralOperandRef::Handle {
                handle,
                access_base,
            },
            Self::Parameter { name, access_base } => {
                StructuralOperandRef::Parameter { name, access_base }
            }
        }
    }

    pub(crate) fn explicit_access_base(&self) -> Option<&Value> {
        match self {
            Self::Value { access_base, .. } => access_base.as_ref(),
            Self::Handle { access_base, .. } => Some(access_base),
            Self::Parameter { access_base, .. } => Some(access_base),
        }
    }

    pub(crate) fn traversal_value(&self) -> &Value {
        match self {
            Self::Value { value, .. } => value,
            Self::Handle { access_base, .. } | Self::Parameter { access_base, .. } => access_base,
        }
    }

    pub(crate) fn traversal_value_mut(&mut self) -> &mut Value {
        match self {
            Self::Value { value, .. } => value,
            Self::Handle { access_base, .. } | Self::Parameter { access_base, .. } => access_base,
        }
    }

    pub(crate) fn try_map<E>(
        &self,
        mut map: impl FnMut(&Value) -> Result<Value, E>,
    ) -> Result<Self, E> {
        Ok(match self {
            Self::Value { value, access_base } => Self::Value {
                value: map(value)?,
                access_base: access_base.as_ref().map(&mut map).transpose()?,
            },
            Self::Handle {
                handle,
                access_base,
            } => Self::Handle {
                handle: handle.clone(),
                access_base: map(access_base)?,
            },
            Self::Parameter { name, access_base } => Self::Parameter {
                name: name.clone(),
                access_base: map(access_base)?,
            },
        })
    }
}

impl<'a> StructuralOperandRef<'a> {
    pub(crate) fn value(self) -> &'a Value {
        match self {
            Self::Value(value) => value,
            Self::Handle { access_base, .. } | Self::Parameter { access_base, .. } => access_base,
        }
    }
}

/// One argument retained by a deferred function call.
///
/// Most arguments are ordinary values. The shared handle arm exists only so
/// parsing can carry a structural argument across a sibling/forward function
/// lookup without lowering it into the public value model. It is consumed by
/// function resolution before plans or state are serialized.
#[derive(Clone, PartialEq)]
pub struct FunctionArgument {
    operand: StructuralOperand,
}

impl FunctionArgument {
    pub fn from_value(value: Value) -> Self {
        Self {
            operand: StructuralOperand::from_value(value),
        }
    }

    pub(crate) fn from_handle(handle: crate::eval_value::StructuralHandle) -> Self {
        Self {
            operand: StructuralOperand::from_handle(handle),
        }
    }

    pub fn value(&self) -> Option<&Value> {
        self.operand.value()
    }

    pub(crate) fn handle(&self) -> Option<&crate::eval_value::StructuralHandle> {
        self.operand.handle()
    }

    pub(crate) fn pending_parameter(&self) -> Option<&str> {
        self.operand.parameter()
    }

    /// Value-shaped projection reserved for crate-internal dependency,
    /// canonicalization, and diagnostic walkers. Semantic consumers must use
    /// [`Self::value`] and [`Self::handle`] so a handle cannot accidentally be
    /// treated as a value.
    pub(crate) fn walker_value(&self) -> &Value {
        self.operand.traversal_value()
    }

    pub(crate) fn walker_value_mut(&mut self) -> &mut Value {
        self.operand.traversal_value_mut()
    }

    pub(crate) fn try_map_value<E>(
        &self,
        map: impl FnOnce(&Value) -> Result<Value, E>,
    ) -> Result<Self, E> {
        let operand = match &self.operand {
            StructuralOperand::Value { value, .. } => StructuralOperand::from_value(map(value)?),
            StructuralOperand::Handle {
                handle,
                access_base,
            } => StructuralOperand::from_handle_with_access_base(handle.clone(), map(access_base)?),
            StructuralOperand::Parameter { name, access_base } => StructuralOperand::Parameter {
                name: name.clone(),
                access_base: map(access_base)?,
            },
        };
        Ok(Self { operand })
    }
}

impl From<Value> for FunctionArgument {
    fn from(value: Value) -> Self {
        Self::from_value(value)
    }
}

impl fmt::Debug for FunctionArgument {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.operand.value_ref() {
            StructuralOperandRef::Value(value) => value.fmt(f),
            StructuralOperandRef::Handle { handle, .. } => f
                .debug_tuple("StructuralHandleArgument")
                .field(handle)
                .finish(),
            StructuralOperandRef::Parameter { name, .. } => f
                .debug_tuple("ForwardedParameterArgument")
                .field(&name)
                .finish(),
        }
    }
}

impl Serialize for FunctionArgument {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.operand.value_ref() {
            StructuralOperandRef::Value(value) => value.serialize(serializer),
            StructuralOperandRef::Handle { handle, .. } => Err(serde::ser::Error::custom(format!(
                "unresolved structural handle argument '{}' cannot be serialized",
                handle.canonical_binding_name()
            ))),
            StructuralOperandRef::Parameter { name, .. } => Err(serde::ser::Error::custom(
                format!("unresolved forwarded parameter argument '{name}' cannot be serialized"),
            )),
        }
    }
}

impl<'de> Deserialize<'de> for FunctionArgument {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::from_value)
    }
}

/// Parser diagnostic metadata attached to a value-shaped node without
/// becoming part of that node's semantic identity or serialized form.
/// `None` represents a synthetic/deserialized node, so line zero is not a
/// representable source location.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiagnosticSourceLine(Option<NonZeroUsize>);

impl DiagnosticSourceLine {
    pub const fn authored(line: usize) -> Self {
        Self(NonZeroUsize::new(line))
    }

    pub const fn synthetic() -> Self {
        Self(None)
    }

    pub const fn get(self) -> Option<usize> {
        match self.0 {
            Some(line) => Some(line.get()),
            None => None,
        }
    }

    pub const fn nonzero(self) -> Option<NonZeroUsize> {
        self.0
    }

    pub const fn unwrap_or(self, fallback: usize) -> usize {
        match self.get() {
            Some(line) => line,
            None => fallback,
        }
    }
}

impl From<usize> for DiagnosticSourceLine {
    fn from(line: usize) -> Self {
        Self::authored(line)
    }
}

// Source positions are diagnostic metadata, not value identity. Keeping this
// rule on the metadata type lets every containing value derive/use equality
// without ad-hoc `source_line: _` patterns.
impl PartialEq for DiagnosticSourceLine {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for DiagnosticSourceLine {}

/// Diagnostic-safe explanation retained after a built-in was attempted and
/// failed during permissive resolution.
///
/// The inner string has no public constructor. Built-in handlers are the
/// diagnostic-safety boundary: values derived from sensitive inputs must be
/// masked before their errors reach the resolver (as `env()` and `decrypt()`
/// do). Keeping the constructor crate-private prevents downstream callers from
/// attaching arbitrary, potentially sensitive payloads to an unavailable
/// value.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct UnavailableReason(String);

impl UnavailableReason {
    pub(crate) fn from_builtin_error(message: String) -> Self {
        Self(message)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for UnavailableReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("UnavailableReason").field(&self.0).finish()
    }
}

impl fmt::Display for UnavailableReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Deferred-axis variants: placeholders that resolve later
/// (apply time, upstream load, function evaluation).
#[derive(Clone)]
pub enum DeferredValue {
    /// Reference to another resource's attribute via an access path.
    ResourceRef { path: AccessPath },
    /// Reference to a binding without an attribute selector.
    BindingRef { binding: String },
    /// Null/missing coalescing deferred until the left operand's resolution
    /// boundary can distinguish a definitive absence from an unknown value.
    Coalesce {
        value: Box<Value>,
        default: Box<Value>,
    },
    /// String interpolation: `"prefix-${expr}-suffix"`
    Interpolation(Vec<InterpolationPart>),
    /// Built-in function call: `join("-", ["a", "b"])` or via pipe
    /// `["a", "b"] |> join("-")`. Evaluated during reference resolution.
    FunctionCall {
        name: crate::builtins::BuiltinFunction,
        args: Vec<FunctionArgument>,
        /// One-based authored line, or `None` for a synthetic/deserialized
        /// call. Diagnostic metadata is excluded from equality and serde by
        /// [`DiagnosticSourceLine`].
        source_line: DiagnosticSourceLine,
    },
    /// A built-in whose arguments were ready and whose evaluation was
    /// attempted, but failed while the resolver was operating in a
    /// permissive mode. Unlike [`Self::FunctionCall`], this is not pending
    /// work. Persistence deliberately encodes it as the equivalent
    /// [`Self::FunctionCall`], so the runtime-only failure is re-derived when
    /// the saved expression is evaluated again.
    Unavailable {
        name: crate::builtins::BuiltinFunction,
        args: Vec<FunctionArgument>,
        reason: UnavailableReason,
        /// One-based authored line, or `None` for a synthetic node.
        source_line: DiagnosticSourceLine,
    },
    /// A deferred user-function call whose lexical definition has already
    /// been resolved. Unlike `FunctionCall`, this node never consults the
    /// function table of whichever later consumer happens to evaluate it.
    ResolvedFunctionCall {
        function: std::sync::Arc<crate::parser::UserFunction>,
        /// Lexical function scope captured with `function`. Function bodies
        /// may call sibling helpers after this outer call is deferred.
        functions: std::sync::Arc<std::collections::HashMap<String, crate::parser::UserFunction>>,
        args: Vec<FunctionArgument>,
        source_line: DiagnosticSourceLine,
    },
    /// Field/index access whose base is not concrete yet. Once `base`
    /// resolves, the resolver applies `step` through
    /// [`apply_value_path_segment`].
    Access(AccessExpr),
    /// A secret value. The inner value is sent to the provider but
    /// stored as an Argon2id hash in state.
    Secret(Box<Value>),
    /// An opaque secret hash loaded from persisted state or a serialized
    /// saved plan.
    ///
    /// It is deliberately deferred (rather than an ordinary string), so
    /// sensitivity is carried by the type instead of inferred from content.
    /// Desired and ordinary provider boundaries reject it; persisted-state
    /// boundaries preserve it through structural tags and typed wire variants.
    /// The DSL parser has no syntax that can construct it.
    RedactedSecret { hash: RedactedSecretHash },
    /// A value not known at plan time. RFC #2371.
    Unknown(UnknownReason),
}

impl DeferredValue {
    /// Project a pending or unsuccessfully attempted built-in call onto the
    /// representation shared by persistence, identity, and display.
    ///
    /// `Unavailable`'s diagnostic reason and source line are runtime-only;
    /// the function and resolved arguments are its stable value material.
    pub fn as_function_call_parts(
        &self,
    ) -> Option<(crate::builtins::BuiltinFunction, &[FunctionArgument])> {
        match self {
            Self::FunctionCall { name, args, .. } => Some((*name, args)),
            Self::Unavailable { name, args, .. } => Some((*name, args)),
            _ => None,
        }
    }
}

/// A sealed postfix-access node.  Its fields are read-only outside this
/// module, so every rebuild must pass through [`apply_value_path_segment`]
/// and cannot leave an Access wrapped around a concrete or reference root.
#[derive(Clone)]
pub struct AccessExpr {
    base: Box<Value>,
    step: PathSegment,
    source_line: usize,
    /// Parser-only authored root for deferred-for diagnostics (`item` in
    /// `item.missing`). It is display metadata, like `source_line`, and is
    /// intentionally omitted from the serialized Value representation.
    display_root: Option<String>,
}

impl AccessExpr {
    pub fn base(&self) -> &Value {
        &self.base
    }

    pub fn step(&self) -> &PathSegment {
        &self.step
    }

    pub fn source_line(&self) -> usize {
        self.source_line
    }

    pub fn display_root(&self) -> Option<&str> {
        self.display_root.as_deref()
    }

    /// Rebuild this access after a value-tree transform. The operands still
    /// pass through the single access primitive, while source/display metadata
    /// is retained when the result remains deferred.
    pub(crate) fn rebuild_result(
        &self,
        base: Value,
        step: PathSegment,
        miss_policy: AccessMissPolicy,
    ) -> Result<ValuePathSegmentResult, ValueAccessError> {
        apply_value_path_segment(base, step, self.source_line, miss_policy).map(|result| {
            let restore_display_root = |value: Value| match &self.display_root {
                Some(root) => value.with_access_display_root(root.clone()),
                None => value,
            };
            match result {
                ValuePathSegmentResult::Resolved(value) => {
                    ValuePathSegmentResult::Resolved(restore_display_root(value))
                }
                ValuePathSegmentResult::Deferred(value) => {
                    ValuePathSegmentResult::Deferred(restore_display_root(value))
                }
                ValuePathSegmentResult::DeferredMiss(error) => {
                    ValuePathSegmentResult::DeferredMiss(error)
                }
            }
        })
    }

    pub(crate) fn rebuild(
        &self,
        base: Value,
        step: PathSegment,
        miss_policy: AccessMissPolicy,
    ) -> Result<Value, ValueAccessError> {
        self.rebuild_result(base, step, miss_policy)
            .and_then(ValuePathSegmentResult::into_value)
    }

    pub(crate) fn rebuild_with_base(
        &self,
        base: Value,
        miss_policy: AccessMissPolicy,
    ) -> Result<Value, ValueAccessError> {
        self.rebuild(base, self.step.clone(), miss_policy)
    }

    /// Rebuild without selecting the access yet.
    ///
    /// This is reserved for a lazy coalesce fallback while deferred-for
    /// placeholders are being substituted. The fallback may never be chosen,
    /// so a concrete missing-field error must remain dormant until `??`
    /// selects it at a later resolution boundary.
    pub(crate) fn rebuild_lazy_with_base(&self, base: Value) -> Value {
        Value::Deferred(DeferredValue::Access(Self {
            base: Box::new(base),
            step: self.step.clone(),
            source_line: self.source_line,
            display_root: self.display_root.clone(),
        }))
    }
}

impl Value {
    /// Whether this value is the one syntactic shape for which `??` may
    /// interpret a missing selection as absence: a direct reference, or an
    /// access chain whose base is itself a direct reference.
    ///
    /// References nested in collections, interpolations, function arguments,
    /// or secret wrappers are inputs to a larger computation. Their absence is
    /// therefore not the absence of that computation.
    pub fn is_coalesce_optional_reference(&self) -> bool {
        match self {
            Value::Deferred(
                DeferredValue::ResourceRef { .. } | DeferredValue::BindingRef { .. },
            ) => true,
            Value::Deferred(DeferredValue::Access(access)) => {
                access.base().is_coalesce_optional_reference()
            }
            _ => false,
        }
    }

    /// Whether this expression can propagate definitive absence when it is
    /// itself the left operand of another coalesce. Besides a direct optional
    /// reference, a nested coalesce can do so: in `(a ?? b) ?? c`, both `a`
    /// and `b` are optional alternatives before `c` is selected.
    pub fn is_coalesce_optional_operand(&self) -> bool {
        match self {
            Value::Deferred(DeferredValue::Coalesce { .. }) => true,
            _ => self.is_coalesce_optional_reference(),
        }
    }

    /// Collect the root binding names referenced anywhere in this value.
    /// Resource references and bare binding references share this one helper
    /// so dependency and diagnostic callers cannot drift.
    pub fn root_bindings(&self) -> std::collections::BTreeSet<String> {
        let mut roots = std::collections::BTreeSet::new();
        self.visit_resource_refs(&mut |path| {
            roots.insert(path.binding().to_string());
        });
        self.visit_binding_refs(&mut |binding| {
            roots.insert(binding.to_string());
        });
        roots
    }

    /// Whether this value contains no runtime-deferred computation.
    pub fn is_fully_resolved(&self) -> bool {
        match self {
            Value::Concrete(ConcreteValue::List(values)) => {
                values.iter().all(Value::is_fully_resolved)
            }
            Value::Concrete(ConcreteValue::Map(values)) => {
                values.values().all(Value::is_fully_resolved)
            }
            Value::Deferred(DeferredValue::Secret(inner)) => inner.is_fully_resolved(),
            Value::Concrete(
                ConcreteValue::String(_)
                | ConcreteValue::EnumIdentifier(_)
                | ConcreteValue::CanonicalEnum(_)
                | ConcreteValue::Int(_)
                | ConcreteValue::Float(_)
                | ConcreteValue::Bool(_)
                | ConcreteValue::Duration(_)
                | ConcreteValue::StringList(_),
            )
            | Value::Deferred(DeferredValue::RedactedSecret { .. }) => true,
            Value::Deferred(
                DeferredValue::ResourceRef { .. }
                | DeferredValue::BindingRef { .. }
                | DeferredValue::Coalesce { .. }
                | DeferredValue::Interpolation(_)
                | DeferredValue::FunctionCall { .. }
                | DeferredValue::Unavailable { .. }
                | DeferredValue::ResolvedFunctionCall { .. }
                | DeferredValue::Access(_)
                | DeferredValue::Unknown(_),
            ) => false,
        }
    }

    /// Attach authored display context to a valid deferred access. This does
    /// not expose or mutate the access operands; construction and rebuilding
    /// still go through [`apply_value_path_segment`].
    pub(crate) fn with_access_display_root(mut self, root: impl Into<String>) -> Self {
        if let Value::Deferred(DeferredValue::Access(access)) = &mut self {
            access.display_root = Some(root.into());
        }
        self
    }
}

impl Serialize for DeferredValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        enum SerializableDeferredValue<'a> {
            ResourceRef {
                path: &'a AccessPath,
            },
            BindingRef {
                binding: &'a str,
            },
            Coalesce {
                value: &'a Value,
                default: &'a Value,
            },
            Interpolation(&'a [InterpolationPart]),
            FunctionCall {
                name: &'a str,
                args: &'a [FunctionArgument],
            },
            Access {
                base: &'a Value,
                step: &'a PathSegment,
            },
            RedactedSecret {
                hash: &'a RedactedSecretHash,
            },
            Unknown(&'a UnknownReason),
        }

        match self {
            Self::ResourceRef { path } => {
                SerializableDeferredValue::ResourceRef { path }.serialize(serializer)
            }
            Self::BindingRef { binding } => SerializableDeferredValue::BindingRef {
                binding: binding.as_str(),
            }
            .serialize(serializer),
            Self::Coalesce { value, default } => {
                SerializableDeferredValue::Coalesce { value, default }.serialize(serializer)
            }
            Self::Interpolation(parts) => {
                SerializableDeferredValue::Interpolation(parts).serialize(serializer)
            }
            Self::FunctionCall { name, args, .. } | Self::Unavailable { name, args, .. } => {
                SerializableDeferredValue::FunctionCall {
                    name: name.as_str(),
                    args,
                }
                .serialize(serializer)
            }
            Self::ResolvedFunctionCall { function, .. } => Err(serde::ser::Error::custom(format!(
                "deferred user-function call '{}' cannot be serialized before evaluation",
                function.name
            ))),
            Self::Access(access) => SerializableDeferredValue::Access {
                base: access.base(),
                step: access.step(),
            }
            .serialize(serializer),
            Self::Secret(_) => Err(serde::ser::Error::custom(
                "raw secret values cannot be serialized; redact or materialize them at the typed boundary",
            )),
            Self::RedactedSecret { hash } => {
                SerializableDeferredValue::RedactedSecret { hash }.serialize(serializer)
            }
            Self::Unknown(reason) => {
                SerializableDeferredValue::Unknown(reason).serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for DeferredValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        enum DeserializableDeferredValue {
            ResourceRef {
                path: AccessPath,
            },
            BindingRef {
                binding: String,
            },
            Coalesce {
                value: Box<Value>,
                default: Box<Value>,
            },
            Interpolation(Vec<InterpolationPart>),
            FunctionCall {
                name: String,
                args: Vec<FunctionArgument>,
            },
            Access {
                base: Box<Value>,
                step: PathSegment,
            },
            Secret(Box<Value>),
            RedactedSecret {
                hash: RedactedSecretHash,
            },
            Unknown(UnknownReason),
        }

        match DeserializableDeferredValue::deserialize(deserializer)? {
            DeserializableDeferredValue::ResourceRef { path } => Ok(Self::ResourceRef { path }),
            DeserializableDeferredValue::BindingRef { binding } => Ok(Self::BindingRef { binding }),
            DeserializableDeferredValue::Coalesce { value, default } => {
                Ok(Self::Coalesce { value, default })
            }
            DeserializableDeferredValue::Interpolation(parts) => Ok(Self::Interpolation(parts)),
            DeserializableDeferredValue::FunctionCall { name, args, .. } => {
                let name = crate::builtins::BuiltinFunction::from_name(&name).ok_or_else(|| {
                    serde::de::Error::custom(format!("unknown built-in function '{name}'"))
                })?;
                Ok(Self::FunctionCall {
                    name,
                    args,
                    source_line: DiagnosticSourceLine::synthetic(),
                })
            }
            DeserializableDeferredValue::Access { base, step } => {
                let canonical = apply_postfix_access(*base, step, 0, AccessMissPolicy::Error)
                    .map_err(serde::de::Error::custom)?;
                match canonical {
                    Value::Deferred(value) => Ok(value),
                    Value::Concrete(_) => Err(serde::de::Error::custom(
                        "serialized deferred access resolved to a concrete value",
                    )),
                }
            }
            DeserializableDeferredValue::Secret(inner) => Ok(Self::Secret(inner)),
            DeserializableDeferredValue::RedactedSecret { hash } => {
                Ok(Self::RedactedSecret { hash })
            }
            DeserializableDeferredValue::Unknown(reason) => Ok(Self::Unknown(reason)),
        }
    }
}

impl fmt::Debug for DeferredValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResourceRef { path } => {
                f.debug_struct("ResourceRef").field("path", path).finish()
            }
            Self::BindingRef { binding } => f
                .debug_struct("BindingRef")
                .field("binding", binding)
                .finish(),
            Self::Coalesce { value, default } => f
                .debug_struct("Coalesce")
                .field("value", value)
                .field("default", default)
                .finish(),
            Self::Interpolation(parts) => f.debug_tuple("Interpolation").field(parts).finish(),
            Self::FunctionCall {
                name,
                args,
                source_line,
            } => f
                .debug_struct("FunctionCall")
                .field("name", name)
                .field("args", args)
                .field("source_line", source_line)
                .finish(),
            Self::Unavailable {
                name,
                args,
                reason,
                source_line,
            } => f
                .debug_struct("Unavailable")
                .field("name", name)
                .field("args", args)
                .field("reason", reason)
                .field("source_line", source_line)
                .finish(),
            Self::ResolvedFunctionCall {
                function,
                args,
                source_line,
                ..
            } => f
                .debug_struct("ResolvedFunctionCall")
                .field("name", &function.name)
                .field("args", args)
                .field("source_line", source_line)
                .finish(),
            Self::Access(access) => f
                .debug_struct("Access")
                .field("base", access.base())
                .field("step", access.step())
                .field("source_line", &access.source_line())
                .field("display_root", &access.display_root())
                .finish(),
            Self::Secret(_) => f.write_str("Secret((secret))"),
            Self::RedactedSecret { .. } => f.write_str("RedactedSecret((secret hash))"),
            Self::Unknown(reason) => f.debug_tuple("Unknown").field(reason).finish(),
        }
    }
}

/// Typed digest carried by [`DeferredValue::RedactedSecret`].
///
/// New digests retain a validated Argon2id PHC string verbatim. The PHC
/// string embeds the salt, algorithm version, and m/t/p parameters needed for
/// context-free verification. Legacy string encodings are upgraded at
/// state-file load to carry the row-derived historical salt bytes alongside the hash.
/// The fixed historical fallback salt lives in code, so no representable
/// legacy digest depends on row context.
#[derive(Clone)]
pub struct RedactedSecretHash(RedactedSecretHashKind);

#[derive(Clone)]
enum RedactedSecretHashKind {
    Argon2idPhc {
        encoded: String,
    },
    ExplicitSaltLegacy {
        salt: Vec<u8>,
        hash: [u8; RedactedSecretHash::HASH_LEN],
    },
}

impl RedactedSecretHash {
    const SALT_LEN: usize = 16;
    const MAX_EXPLICIT_LEGACY_SALT_LEN: usize = 4096;
    pub const HASH_LEN: usize = 32;
    /// Historical prefix retained only as stable identity and list-pairing hash
    /// material. Current persisted state uses the structural `$secret` tag.
    const V2_MARKER_PREFIX: &'static str = "_secret:$argon2id$";
    const LEGACY_MARKER_PREFIX: &'static str = "_secret:argon2:";
    const STATE_MARKER_PREFIX: &'static str = "_secret:";
    const EXPLICIT_LEGACY_DIGEST_PREFIX: &'static str = "argon2-legacy:";
    const LEGACY_FALLBACK_SALT: &'static [u8] = b"carina-secret-v1";

    pub(crate) fn generate(plaintext: &[u8]) -> Self {
        let salt = SaltString::generate(&mut OsRng);
        let encoded = Argon2::default()
            .hash_password(plaintext, &salt)
            .expect("Argon2id hashing with a generated salt should not fail")
            .to_string();
        Self(RedactedSecretHashKind::Argon2idPhc { encoded })
    }

    /// Reuse a matching self-contained digest or generate a fresh PHC digest.
    /// Legacy hashes are verification-only and are always upgraded on write.
    pub(crate) fn reuse_or_generate(plaintext: &[u8], previous: Option<&Self>) -> Self {
        if let Some(previous @ Self(RedactedSecretHashKind::Argon2idPhc { .. })) = previous
            && previous.verify(plaintext)
        {
            return previous.clone();
        }
        Self::generate(plaintext)
    }

    /// Verify a candidate against this typed digest.
    ///
    /// New PHC digests use only their embedded algorithm parameters. An
    /// upgraded legacy digest carries its row-derived salt and also tries the
    /// fixed pre-v12 fallback salt, so verification is context-free for both
    /// representations.
    pub fn verify(&self, plaintext: impl AsRef<[u8]>) -> bool {
        match &self.0 {
            RedactedSecretHashKind::Argon2idPhc { encoded } => PasswordHash::new(encoded)
                .is_ok_and(|parsed| {
                    Argon2::default()
                        .verify_password(plaintext.as_ref(), &parsed)
                        .is_ok()
                }),
            RedactedSecretHashKind::ExplicitSaltLegacy { salt, hash } => {
                let verify_with_salt = |salt: &[u8]| {
                    let mut candidate = [0_u8; Self::HASH_LEN];
                    legacy_argon2()
                        .hash_password_into(plaintext.as_ref(), salt, &mut candidate)
                        .is_ok()
                        && bool::from(candidate.ct_eq(hash))
                };
                verify_with_salt(salt) | verify_with_salt(Self::LEGACY_FALLBACK_SALT)
            }
        }
    }

    /// Parse a self-contained state marker. Raw pre-v12 legacy markers
    /// deliberately fail; only the state-file row decoder may upgrade those.
    fn parse_legacy_persisted_marker(marker: &str) -> Option<Self> {
        marker
            .strip_prefix(Self::STATE_MARKER_PREFIX)
            .and_then(Self::from_encoded_digest)
    }

    fn upgrade_raw_legacy_marker_for_state_row(
        marker: &str,
        display_type: &str,
        identity: &str,
        top_level_attribute_key: &str,
    ) -> Result<Option<Self>, legacy_secret_migration::LegacySecretMigrationError> {
        let Some(encoded) = marker.strip_prefix(Self::LEGACY_MARKER_PREFIX) else {
            return Ok(None);
        };
        let mut hash = [0_u8; Self::HASH_LEN];
        if hex::decode_to_slice(encoded, &mut hash).is_err() {
            return Ok(None);
        }
        let row_salt = format!(
            "carina:{}:{}:{}",
            display_type, identity, top_level_attribute_key
        )
        .into_bytes();
        if row_salt.len() > Self::MAX_EXPLICIT_LEGACY_SALT_LEN {
            return Err(
                legacy_secret_migration::LegacySecretMigrationError::LegacySaltTooLong {
                    actual: row_salt.len(),
                    maximum: Self::MAX_EXPLICIT_LEGACY_SALT_LEN,
                },
            );
        }
        Ok(Some(Self(RedactedSecretHashKind::ExplicitSaltLegacy {
            // Pre-v12 write-only attributes could use a context-free fallback
            // salt. Verification tries that fixed constant from code after
            // the row-derived salt, so the marker need not store it.
            salt: row_salt,
            hash,
        })))
    }

    pub fn from_encoded_digest(encoded: &str) -> Option<Self> {
        if encoded.starts_with("$argon2id$") {
            let parsed = PasswordHash::new(encoded).ok()?;
            if parsed.algorithm != argon2::ARGON2ID_IDENT
                || parsed.version != Some(19)
                || parsed.params.get("m").is_none()
                || parsed.params.get("t").is_none()
                || parsed.params.get("p").is_none()
                || parsed.hash.as_ref()?.len() != Self::HASH_LEN
            {
                return None;
            }
            let mut salt = [0_u8; Self::SALT_LEN];
            if parsed.salt?.decode_b64(&mut salt).ok()?.len() != Self::SALT_LEN {
                return None;
            }
            let params = Params::try_from(&parsed).ok()?;
            if params.m_cost() != Params::DEFAULT_M_COST
                || params.t_cost() != Params::DEFAULT_T_COST
                || params.p_cost() != Params::DEFAULT_P_COST
                || params.output_len() != Some(Self::HASH_LEN)
                || !params.keyid().is_empty()
                || !params.data().is_empty()
            {
                return None;
            }
            return Some(Self(RedactedSecretHashKind::Argon2idPhc {
                encoded: encoded.to_string(),
            }));
        }
        let mut fields = encoded
            .strip_prefix(Self::EXPLICIT_LEGACY_DIGEST_PREFIX)?
            .split(':');
        let row_salt_hex = fields.next()?;
        let hash_hex = fields.next()?;
        if fields.next().is_some()
            || !valid_legacy_salt_hex(row_salt_hex)
            || hash_hex.len() != Self::HASH_LEN * 2
            || !hash_hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        let salt = hex::decode(row_salt_hex).ok()?;
        let mut hash = [0_u8; Self::HASH_LEN];
        hex::decode_to_slice(hash_hex, &mut hash).ok()?;
        Some(Self(RedactedSecretHashKind::ExplicitSaltLegacy {
            salt,
            hash,
        }))
    }

    pub fn encoded_digest(&self) -> String {
        match &self.0 {
            RedactedSecretHashKind::Argon2idPhc { encoded } => encoded.clone(),
            RedactedSecretHashKind::ExplicitSaltLegacy { salt, hash } => {
                format!(
                    "{}{}:{}",
                    Self::EXPLICIT_LEGACY_DIGEST_PREFIX,
                    hex::encode(salt),
                    hex::encode(hash)
                )
            }
        }
    }
}

pub(crate) fn legacy_secret_identity_material(hash: &RedactedSecretHash) -> String {
    format!(
        "{}{}",
        RedactedSecretHash::STATE_MARKER_PREFIX,
        hash.encoded_digest()
    )
}

/// Parsers and rewrites for state formats that encoded secret identity in
/// `_secret:...` strings. Current persisted/provider/WASM decoders do not use
/// this module; only version-checked whole-document migrations are public.
/// Bare JSON values cannot be submitted for marker classification:
///
/// ```compile_fail
/// use carina_core::resource::legacy_secret_migration;
///
/// let mut value = serde_json::json!("_secret:argon2:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
/// let _ = legacy_secret_migration::migrate_state_row_attribute(
///     &mut value,
///     "mock.test.resource",
///     "example",
///     "password",
/// );
/// ```
///
/// ```compile_fail
/// use carina_core::resource::legacy_secret_migration;
///
/// let mut value = serde_json::json!("_secret:argon2-legacy:73616c74:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
/// legacy_secret_migration::migrate_self_contained_markers(&mut value);
/// ```
pub mod legacy_secret_migration {
    use super::RedactedSecretHash;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum LegacySecretMigrationError {
        InvalidDocument {
            artifact: &'static str,
            detail: &'static str,
        },
        UnsupportedVersion {
            artifact: &'static str,
            found: u64,
            expected: &'static str,
        },
        LegacySaltTooLong {
            actual: usize,
            maximum: usize,
        },
        StateAttribute {
            key: String,
            source: Box<LegacySecretMigrationError>,
        },
    }

    impl std::fmt::Display for LegacySecretMigrationError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::InvalidDocument { artifact, detail } => {
                    write!(f, "invalid {artifact} migration document: {detail}")
                }
                Self::UnsupportedVersion {
                    artifact,
                    found,
                    expected,
                } => write!(
                    f,
                    "cannot apply {artifact} migration to version {found}; expected {expected}"
                ),
                Self::LegacySaltTooLong { actual, maximum } => write!(
                    f,
                    "pre-v12 legacy salt has {actual} bytes; the maximum is {maximum} bytes"
                ),
                Self::StateAttribute { key, source } => {
                    write!(f, "failed to migrate state attribute {key:?}: {source}")
                }
            }
        }
    }

    impl std::error::Error for LegacySecretMigrationError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Self::StateAttribute { source, .. } => Some(source),
                Self::InvalidDocument { .. }
                | Self::UnsupportedVersion { .. }
                | Self::LegacySaltTooLong { .. } => None,
            }
        }
    }

    fn structural_secret(hash: &RedactedSecretHash) -> serde_json::Value {
        serde_json::json!({
            crate::value::PERSISTED_SECRET_KEY: hash.encoded_digest(),
        })
    }

    /// Migrate the self-contained marker strings understood by mock-state v1.
    /// Raw row-bound markers remain strings because that format never carried
    /// the resource-row inputs required to assign them secret semantics.
    fn migrate_self_contained_markers(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(marker) => {
                if let Some(hash) = RedactedSecretHash::parse_legacy_persisted_marker(marker) {
                    *value = structural_secret(&hash);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    migrate_self_contained_markers(value);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    migrate_self_contained_markers(value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    /// Migrate one top-level resource attribute from state v13 or earlier.
    /// The row metadata binds exact pre-v12 raw digests to their historical
    /// salt; self-contained markers need no row context.
    fn migrate_state_row_attribute(
        value: &mut serde_json::Value,
        display_type: &str,
        identity: &str,
        top_level_attribute_key: &str,
    ) -> Result<(), LegacySecretMigrationError> {
        match value {
            serde_json::Value::String(marker) => {
                if let Some(hash) = RedactedSecretHash::parse_legacy_persisted_marker(marker) {
                    *value = structural_secret(&hash);
                } else if let Some(hash) =
                    RedactedSecretHash::upgrade_raw_legacy_marker_for_state_row(
                        marker,
                        display_type,
                        identity,
                        top_level_attribute_key,
                    )?
                {
                    *value = structural_secret(&hash);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    migrate_state_row_attribute(
                        value,
                        display_type,
                        identity,
                        top_level_attribute_key,
                    )?;
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    migrate_state_row_attribute(
                        value,
                        display_type,
                        identity,
                        top_level_attribute_key,
                    )?;
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
        Ok(())
    }

    fn document_version(
        document: &serde_json::Value,
        artifact: &'static str,
    ) -> Result<u64, LegacySecretMigrationError> {
        let object = document
            .as_object()
            .ok_or(LegacySecretMigrationError::InvalidDocument {
                artifact,
                detail: "root must be an object",
            })?;
        object
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or(LegacySecretMigrationError::InvalidDocument {
                artifact,
                detail: "version must be an unsigned integer",
            })
    }

    /// Upgrade a complete Carina state document through the v13 secret-marker
    /// migration. Only current and deposed resource attribute trees are
    /// classified; exports and all host metadata remain untouched.
    pub fn migrate_state_document_v13(
        document: &mut serde_json::Value,
    ) -> Result<(), LegacySecretMigrationError> {
        const ARTIFACT: &str = "Carina state v13";
        let version = document_version(document, ARTIFACT)?;
        if version > 13 {
            return Err(LegacySecretMigrationError::UnsupportedVersion {
                artifact: ARTIFACT,
                found: version,
                expected: "version 13 or earlier",
            });
        }

        let mut migrated = document.clone();
        let resources = migrated
            .get_mut("resources")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or(LegacySecretMigrationError::InvalidDocument {
                artifact: ARTIFACT,
                detail: "resources must be an array",
            })?;
        for resource in resources {
            let provider = resource
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let resource_type = resource
                .get("resource_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let identity = resource
                .get("identity")
                .or_else(|| resource.get("name"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let display_type = if provider.is_empty() {
                resource_type
            } else {
                format!("{provider}.{resource_type}")
            };

            let migrate_attributes =
                |attributes: &mut serde_json::Map<String, serde_json::Value>| {
                    for (key, value) in attributes {
                        migrate_state_row_attribute(value, &display_type, &identity, key).map_err(
                            |source| LegacySecretMigrationError::StateAttribute {
                                key: key.clone(),
                                source: Box::new(source),
                            },
                        )?;
                    }
                    Ok::<(), LegacySecretMigrationError>(())
                };

            if let Some(attributes) = resource
                .get_mut("attributes")
                .and_then(serde_json::Value::as_object_mut)
            {
                migrate_attributes(attributes)?;
            }
            if let Some(deposed) = resource
                .get_mut("deposed")
                .and_then(serde_json::Value::as_array_mut)
            {
                for generation in deposed {
                    if let Some(attributes) = generation
                        .get_mut("attributes")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        migrate_attributes(attributes)?;
                    }
                }
            }
        }
        migrated["version"] = serde_json::json!(14);
        *document = migrated;
        Ok(())
    }

    /// Upgrade a complete mock-provider v1 state document. Only values below
    /// `states.<resource>.<attribute>` are eligible for legacy marker
    /// classification.
    pub fn migrate_mock_state_document_v1(
        document: &mut serde_json::Value,
    ) -> Result<(), LegacySecretMigrationError> {
        const ARTIFACT: &str = "mock-provider state v1";
        let version = document_version(document, ARTIFACT)?;
        if version != 1 {
            return Err(LegacySecretMigrationError::UnsupportedVersion {
                artifact: ARTIFACT,
                found: version,
                expected: "version 1",
            });
        }

        let mut migrated = document.clone();
        let states = migrated
            .get_mut("states")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or(LegacySecretMigrationError::InvalidDocument {
                artifact: ARTIFACT,
                detail: "states must be an object",
            })?;
        for attributes in states.values_mut() {
            let attributes =
                attributes
                    .as_object_mut()
                    .ok_or(LegacySecretMigrationError::InvalidDocument {
                        artifact: ARTIFACT,
                        detail: "each resource state must be an attribute object",
                    })?;
            for value in attributes.values_mut() {
                migrate_self_contained_markers(value);
            }
        }
        migrated["version"] = serde_json::json!(2);
        *document = migrated;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn migrate_legacy_state_attribute_for_test(
        value: serde_json::Value,
        display_type: &str,
        identity: &str,
        key: &str,
    ) -> Result<serde_json::Value, LegacySecretMigrationError> {
        let mut attributes = serde_json::Map::new();
        attributes.insert(key.to_string(), value);
        let mut document = serde_json::json!({
            "version": 13,
            "resources": [{
                "provider": "",
                "resource_type": display_type,
                "identity": identity,
                "attributes": attributes,
                "deposed": [],
            }],
        });
        migrate_state_document_v13(&mut document)?;
        Ok(document["resources"][0]["attributes"]
            .as_object_mut()
            .and_then(|attributes| attributes.remove(key))
            .expect("migrated test document must retain its attribute"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn state_v13_migration_checks_version_and_only_walks_resource_attributes() {
            let raw_marker = format!("_secret:argon2:{}", "ab".repeat(32));
            let mut document = serde_json::json!({
                "version": 13,
                "exports": {"literal": raw_marker},
                "resources": [{
                    "provider": "mock",
                    "resource_type": "test.resource",
                    "identity": "example",
                    "attributes": {"token": raw_marker},
                    "deposed": [{"attributes": {"nested": [raw_marker]}}]
                }]
            });

            migrate_state_document_v13(&mut document).expect("migrate v13 state document");

            assert_eq!(document["version"], 14);
            assert!(
                document["resources"][0]["attributes"]["token"][crate::value::PERSISTED_SECRET_KEY]
                    .as_str()
                    .is_some_and(|digest| digest.starts_with("argon2-legacy:"))
            );
            assert!(
                document["resources"][0]["deposed"][0]["attributes"]["nested"][0]
                    [crate::value::PERSISTED_SECRET_KEY]
                    .as_str()
                    .is_some_and(|digest| digest.starts_with("argon2-legacy:"))
            );
            assert_eq!(
                document["exports"]["literal"],
                serde_json::Value::String(raw_marker)
            );

            let mut current = serde_json::json!({
                "version": 14,
                "resources": [{
                    "provider": "mock",
                    "resource_type": "test.resource",
                    "identity": "example",
                    "attributes": {"token": format!("_secret:argon2:{}", "cd".repeat(32))}
                }]
            });
            let unchanged = current.clone();
            assert!(migrate_state_document_v13(&mut current).is_err());
            assert_eq!(current, unchanged);
        }

        #[test]
        fn mock_v1_migration_checks_version_and_walks_only_states() {
            let marker = format!("_secret:argon2-legacy:73616c74:{}", "ab".repeat(32));
            let mut document = serde_json::json!({
                "version": 1,
                "metadata": marker,
                "states": {
                    "test.resource.example": {
                        "token": marker,
                        "nested": [marker]
                    }
                }
            });

            migrate_mock_state_document_v1(&mut document).expect("migrate mock v1 document");

            assert_eq!(document["version"], 2);
            assert!(
                document["states"]["test.resource.example"]["token"]
                    [crate::value::PERSISTED_SECRET_KEY]
                    .is_string()
            );
            assert!(
                document["states"]["test.resource.example"]["nested"][0]
                    [crate::value::PERSISTED_SECRET_KEY]
                    .is_string()
            );
            assert_eq!(document["metadata"], marker);

            let mut current = serde_json::json!({
                "version": 2,
                "states": {
                    "test.resource.example": {"token": marker}
                }
            });
            let unchanged = current.clone();
            assert!(migrate_mock_state_document_v1(&mut current).is_err());
            assert_eq!(current, unchanged);
        }
    }
}

fn legacy_argon2() -> Argon2<'static> {
    let params = Params::new(19 * 1024, 2, 1, Some(RedactedSecretHash::HASH_LEN))
        .expect("pre-v12 legacy Argon2 parameters are valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

fn valid_legacy_salt_hex(value: &str) -> bool {
    !value.is_empty()
        && value.len().is_multiple_of(2)
        && value.len() <= RedactedSecretHash::MAX_EXPLICIT_LEGACY_SALT_LEN * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl PartialEq for RedactedSecretHash {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (
                RedactedSecretHashKind::Argon2idPhc { encoded: left },
                RedactedSecretHashKind::Argon2idPhc { encoded: right },
            ) => left == right,
            (
                RedactedSecretHashKind::ExplicitSaltLegacy {
                    salt: left_salt,
                    hash: left_hash,
                },
                RedactedSecretHashKind::ExplicitSaltLegacy {
                    salt: right_salt,
                    hash: right_hash,
                },
            ) => left_salt == right_salt && left_hash == right_hash,
            _ => false,
        }
    }
}

impl Eq for RedactedSecretHash {}

impl Hash for RedactedSecretHash {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match &self.0 {
            RedactedSecretHashKind::Argon2idPhc { encoded } => {
                0_u8.hash(state);
                encoded.hash(state);
            }
            RedactedSecretHashKind::ExplicitSaltLegacy { salt, hash } => {
                1_u8.hash(state);
                salt.hash(state);
                hash.hash(state);
            }
        }
    }
}

impl Serialize for RedactedSecretHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encoded_digest())
    }
}

impl<'de> Deserialize<'de> for RedactedSecretHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        Self::from_encoded_digest(&encoded)
            .ok_or_else(|| serde::de::Error::custom("invalid self-contained secret digest"))
    }
}

impl fmt::Debug for RedactedSecretHash {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("(secret hash)")
    }
}

/// Borrowing projection of [`Value`] restricted to the **concrete** axis
/// — variants that carry their own runtime type and are safe to type-check
/// at validate time.
///
/// Phase 1 of [RFC #2972](https://github.com/carina-rs/carina/issues/2972).
/// Sub-systems that only operate on resolved values (`validate`, the
/// differ, the serializer) take `&ConcreteValueRef<'_>` so the deferred
/// case is structurally unreachable rather than a runtime `matches!`
/// guard. Today `Value` is still flat; once every concrete-only path
/// has migrated to this view, the underlying `Value` enum will be
/// physically split into `Value { Concrete(ConcreteValue), Deferred(DeferredValue) }`
/// and this borrow type will become a thin wrapper over the inner
/// `ConcreteValue`.
///
/// Recursive container variants (`List`, `Map`, `StringList`) borrow
/// from the parent `Value`; the inner element / key types stay
/// `Value` because lists and maps can mix concrete and deferred
/// elements (e.g. `["literal", vpc.id]`).
#[derive(Debug, Clone, Copy)]
pub enum ConcreteValueRef<'a> {
    String(&'a ValueString),
    /// See `ConcreteValue::EnumIdentifier`.
    EnumIdentifier(&'a RawEnumIdentifier),
    CanonicalEnum(&'a CanonicalEnumValue),
    Int(i64),
    Float(f64),
    Bool(bool),
    Duration(std::time::Duration),
    List(&'a [Value]),
    StringList(&'a [ValueString]),
    Map(&'a IndexMap<String, Value>),
}

impl<'a> ConcreteValueRef<'a> {
    /// Return the inner `&str` for either textual variant at call sites where
    /// source shape is deliberately irrelevant (for example, generic custom
    /// scalar validation). Enum-aware formatting, comparison, and resolution
    /// must match the variants directly instead.
    pub fn as_string_like(&self) -> Option<&'a str> {
        match self {
            ConcreteValueRef::String(s) => Some(s.expose_plaintext()),
            ConcreteValueRef::EnumIdentifier(s) => Some(s.as_str()),
            ConcreteValueRef::CanonicalEnum(_) => None,
            _ => None,
        }
    }
}

impl ConcreteValue {
    /// Build a parser-surface enum identifier from source/display text.
    ///
    /// This is a raw-value convenience API for parsers and tests. Durable
    /// identity comparisons must use schema-resolved `CanonicalEnum` values.
    pub fn enum_identifier(text: impl Into<String>) -> Self {
        Self::EnumIdentifier(RawEnumIdentifier::parse(text))
    }
}

/// Borrowing projection of [`Value`] restricted to the **deferred** axis
/// — placeholders that resolve later (apply time, upstream load,
/// function evaluation).
///
/// Sub-systems that need to walk only deferred values (e.g.
/// `BindingIndex::ref_type`, `validate_resource_ref_types`,
/// dependency analysis) take `&DeferredValueRef<'_>` so they cannot
/// accidentally consume a concrete value as a placeholder.
///
/// Phase 1 of [RFC #2972](https://github.com/carina-rs/carina/issues/2972).
#[derive(Debug, Clone, Copy)]
pub enum DeferredValueRef<'a> {
    ResourceRef {
        path: &'a AccessPath,
    },
    BindingRef {
        binding: &'a str,
    },
    Coalesce {
        value: &'a Value,
        default: &'a Value,
    },
    Interpolation(&'a [InterpolationPart]),
    FunctionCall {
        name: &'a str,
        args: &'a [FunctionArgument],
        source_line: Option<NonZeroUsize>,
    },
    Unavailable {
        name: crate::builtins::BuiltinFunction,
        args: &'a [FunctionArgument],
        reason: &'a UnavailableReason,
        source_line: Option<NonZeroUsize>,
    },
    Access {
        base: &'a Value,
        step: &'a PathSegment,
        source_line: usize,
    },
    Secret(&'a Value),
    RedactedSecret(&'a RedactedSecretHash),
    Unknown(&'a UnknownReason),
}

impl Value {
    /// Whether this value tree carries any `env()`-derived string whose
    /// plaintext must be excluded from diagnostics.
    pub(crate) fn contains_diagnostic_sensitive_string(&self) -> bool {
        match self {
            Value::Concrete(ConcreteValue::String(value)) => value.is_diagnostic_sensitive(),
            Value::Concrete(ConcreteValue::StringList(values)) => {
                values.iter().any(ValueString::is_diagnostic_sensitive)
            }
            Value::Concrete(ConcreteValue::List(values)) => values
                .iter()
                .any(Value::contains_diagnostic_sensitive_string),
            Value::Concrete(ConcreteValue::Map(values)) => values
                .values()
                .any(Value::contains_diagnostic_sensitive_string),
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                parts.iter().any(|part| match part {
                    InterpolationPart::Literal(value) => value.is_diagnostic_sensitive(),
                    InterpolationPart::Expr(value) => value.contains_diagnostic_sensitive_string(),
                })
            }
            Value::Deferred(
                DeferredValue::FunctionCall { args, .. }
                | DeferredValue::Unavailable { args, .. }
                | DeferredValue::ResolvedFunctionCall { args, .. },
            ) => args.iter().any(|argument| {
                argument
                    .walker_value()
                    .contains_diagnostic_sensitive_string()
            }),
            Value::Deferred(DeferredValue::Access(access)) => {
                access.base().contains_diagnostic_sensitive_string()
                    || matches!(
                        access.step(),
                        PathSegment::Subscript {
                            index: Subscript::Str { key },
                        } if key.is_diagnostic_sensitive()
                    )
            }
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                value.contains_diagnostic_sensitive_string()
                    || default.contains_diagnostic_sensitive_string()
            }
            Value::Deferred(DeferredValue::Secret(inner)) => {
                inner.contains_diagnostic_sensitive_string()
            }
            Value::Concrete(
                ConcreteValue::EnumIdentifier(_)
                | ConcreteValue::CanonicalEnum(_)
                | ConcreteValue::Int(_)
                | ConcreteValue::Float(_)
                | ConcreteValue::Bool(_)
                | ConcreteValue::Duration(_),
            )
            | Value::Deferred(
                DeferredValue::ResourceRef { .. }
                | DeferredValue::BindingRef { .. }
                | DeferredValue::RedactedSecret { .. }
                | DeferredValue::Unknown(_),
            ) => false,
        }
    }

    /// If this value is on the concrete axis, return a borrowing
    /// projection. Returns `None` for deferred-resolution variants
    /// ([`Value::ResourceRef`], [`Value::BindingRef`],
    /// [`Value::Interpolation`], [`Value::FunctionCall`],
    /// [`Value::Access`], [`Value::Secret`], saved-plan secret
    /// placeholders, and [`Value::Unknown`]).
    ///
    /// Concrete-only sub-systems should consume `Value` exclusively
    /// through this accessor — the returned [`ConcreteValueRef`]
    /// cannot represent a deferred value, so the "deferred leaked
    /// into concrete-only path" bug class becomes structurally
    /// unrepresentable. See RFC #2972.
    pub fn as_concrete(&self) -> Option<ConcreteValueRef<'_>> {
        match self {
            Value::Concrete(c) => Some(match c {
                ConcreteValue::String(s) => ConcreteValueRef::String(s),
                ConcreteValue::EnumIdentifier(s) => ConcreteValueRef::EnumIdentifier(s),
                ConcreteValue::CanonicalEnum(c) => ConcreteValueRef::CanonicalEnum(c),
                ConcreteValue::Int(n) => ConcreteValueRef::Int(*n),
                ConcreteValue::Float(f) => ConcreteValueRef::Float(*f),
                ConcreteValue::Bool(b) => ConcreteValueRef::Bool(*b),
                ConcreteValue::Duration(d) => ConcreteValueRef::Duration(*d),
                ConcreteValue::List(items) => ConcreteValueRef::List(items),
                ConcreteValue::StringList(items) => ConcreteValueRef::StringList(items),
                ConcreteValue::Map(map) => ConcreteValueRef::Map(map),
            }),
            Value::Deferred(_) => None,
        }
    }

    /// If this value is on the deferred axis, return a borrowing
    /// projection. Returns `None` for concrete variants. Mirror of
    /// [`Self::as_concrete`].
    pub fn as_deferred(&self) -> Option<DeferredValueRef<'_>> {
        match self {
            Value::Deferred(d) => Some(match d {
                DeferredValue::ResourceRef { path } => DeferredValueRef::ResourceRef { path },
                DeferredValue::BindingRef { binding } => DeferredValueRef::BindingRef { binding },
                DeferredValue::Coalesce { value, default } => {
                    DeferredValueRef::Coalesce { value, default }
                }
                DeferredValue::Interpolation(parts) => DeferredValueRef::Interpolation(parts),
                DeferredValue::FunctionCall {
                    name,
                    args,
                    source_line,
                } => DeferredValueRef::FunctionCall {
                    name,
                    args,
                    source_line: source_line.nonzero(),
                },
                DeferredValue::Unavailable {
                    name,
                    args,
                    reason,
                    source_line,
                } => DeferredValueRef::Unavailable {
                    name: *name,
                    args,
                    reason,
                    source_line: source_line.nonzero(),
                },
                DeferredValue::ResolvedFunctionCall {
                    function,
                    args,
                    source_line,
                    ..
                } => DeferredValueRef::FunctionCall {
                    name: &function.name,
                    args,
                    source_line: source_line.nonzero(),
                },
                DeferredValue::Access(access) => DeferredValueRef::Access {
                    base: access.base(),
                    step: access.step(),
                    source_line: access.source_line(),
                },
                DeferredValue::Secret(inner) => DeferredValueRef::Secret(inner),
                DeferredValue::RedactedSecret { hash } => DeferredValueRef::RedactedSecret(hash),
                DeferredValue::Unknown(reason) => DeferredValueRef::Unknown(reason),
            }),
            Value::Concrete(_) => None,
        }
    }
}

/// Why a `Value::Unknown` is unknown. Each variant carries only the
/// information its own consumer needs — no shared "context" field invites
/// drift across reasons. See RFC #2371.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UnknownReason {
    /// One export reached through an attribute path on an `upstream_state`
    /// binding that did not resolve (state file missing, or the export was absent).
    /// Renders as `(known after upstream apply: <path.to_dot_string()>)`.
    /// Retains the original `AccessPath` so display preserves subscripts and
    /// chained field access (`network.accounts[0]`, `vpc.tags["Name"]`).
    // Keep the wire tag independent of the Rust variant name.
    #[serde(rename = "UpstreamRef")]
    UpstreamExport { path: AccessPath },
    /// The complete set of exports from an `upstream_state` binding referenced
    /// without an attribute selector — `let v = bootstrap` (since the type-split
    /// in #2856 lowers a bare identifier to `Value::BindingRef`). Renders as
    /// `(known after upstream apply: <binding>)`. Parallel to
    /// `UpstreamExport`; kept as a distinct variant so the type system
    /// carries the "no attribute" condition rather than a runtime
    /// `path.attribute().is_empty()` check inside `UpstreamExport`. See
    /// #2876.
    // Keep the wire tag independent of the Rust variant name.
    #[serde(rename = "UpstreamBareRef")]
    UpstreamAllExports { binding: String },
    /// Map-binding key in a deferred for-expression
    /// (`for (k, _) in iterable`). Substituted with the actual key when
    /// the iterable is later resolved.
    ForKey,
    /// Indexed-binding index in a deferred for-expression
    /// (`for (i, _) in iterable`). Substituted with the actual index.
    ForIndex,
    /// Loop-variable value in a deferred for-expression
    /// (`for v in iterable`). Substituted with the actual element.
    ForValue,
    /// Function parameter placeholder while parsing a user-defined
    /// function body. Substituted with the call argument when the
    /// function is evaluated.
    FnParam { name: String },
    /// Module argument placeholder while parsing a module definition.
    /// This is a value slot, not a structural resource address; module
    /// expansion substitutes it before provider-facing validation.
    ModuleArgument {
        name: String,
        /// One-based line of this reference, absent on a neutral declaration
        /// or seed placeholder. This parser metadata is never value data.
        #[serde(skip, default)]
        source_line: DiagnosticSourceLine,
        /// Authored inner parameter name retained while a nested module
        /// forwards this placeholder to an outer parameter.
        #[serde(skip, default)]
        diagnostic_name: Option<String>,
    },
    /// Function-local let placeholder while parsing a user-defined
    /// function body. Substituted after local lets are evaluated for a
    /// call.
    FnLocal { name: String },
    /// Mid-edit empty `${}` interpolation. Carries no payload — the
    /// presence of the marker is enough for the LSP to surface a
    /// diagnostic at the `${}` span and for downstream resolvers to
    /// stay tolerant. See #2480.
    EmptyInterpolation,
    /// A create completed, but the provider could not read all attributes.
    PostCreateReadIncomplete { detail: String },
}

/// A part of a string interpolation expression
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InterpolationPart {
    /// Literal text segment
    Literal(ValueString),
    /// An expression to be evaluated and converted to string
    Expr(Value),
}

/// `PartialEq` for `Value` is hand-rolled (not derived) to enforce one
/// invariant the differ depends on: runtime **`Value::Unknown` is never equal
/// to anything**, including another `Value::Unknown` with the same
/// reason. An unresolved value is, by definition, unknown — two
/// independently-unresolved attributes are not the "same value", so a
/// derived `PartialEq` would silently suppress real diffs in
/// `merge_lists_hashed` and `semantically_equal`. The parser-only module
/// argument token is the sole exception: fixed-point directory parsing must
/// be able to recognize the same lexical placeholder across passes, and it is
/// always substituted before values reach the differ. See RFC #2371.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        if let (Value::Deferred(left), Value::Deferred(right)) = (self, other)
            && let (Some((left_name, left_args)), Some((right_name, right_args))) = (
                left.as_function_call_parts(),
                right.as_function_call_parts(),
            )
        {
            return left_name == right_name && left_args == right_args;
        }

        match (self, other) {
            (
                Value::Deferred(DeferredValue::Unknown(a @ UnknownReason::ModuleArgument { .. })),
                Value::Deferred(DeferredValue::Unknown(b @ UnknownReason::ModuleArgument { .. })),
            ) => a == b,
            // Constraint: Unknown is never equal to anything.
            (Value::Deferred(DeferredValue::Unknown(_)), _)
            | (_, Value::Deferred(DeferredValue::Unknown(_))) => false,
            (
                Value::Concrete(ConcreteValue::String(a)),
                Value::Concrete(ConcreteValue::String(b)),
            ) => a == b,
            // Generic value-tree equality keeps same-text compatibility
            // between `EnumIdentifier` and `String`. Enum-sensitive consumers
            // (schema-aware differ paths, rendering, alias resolution) match
            // the variants directly and must not infer enum semantics from a
            // String's text. This equality never strips a namespace or applies
            // an alias. See carina#2986 Phase 5.
            (
                Value::Concrete(ConcreteValue::EnumIdentifier(a)),
                Value::Concrete(ConcreteValue::EnumIdentifier(b)),
            ) => a == b,
            (
                Value::Concrete(ConcreteValue::EnumIdentifier(a)),
                Value::Concrete(ConcreteValue::String(b)),
            ) => a.as_str() == b,
            (
                Value::Concrete(ConcreteValue::String(a)),
                Value::Concrete(ConcreteValue::EnumIdentifier(b)),
            ) => a == b.as_str(),
            (
                Value::Concrete(ConcreteValue::CanonicalEnum(a)),
                Value::Concrete(ConcreteValue::CanonicalEnum(b)),
            ) => a == b,
            (Value::Concrete(ConcreteValue::Int(a)), Value::Concrete(ConcreteValue::Int(b))) => {
                a == b
            }
            (
                Value::Concrete(ConcreteValue::Float(a)),
                Value::Concrete(ConcreteValue::Float(b)),
            ) => a == b,
            (Value::Concrete(ConcreteValue::Bool(a)), Value::Concrete(ConcreteValue::Bool(b))) => {
                a == b
            }
            (
                Value::Concrete(ConcreteValue::Duration(a)),
                Value::Concrete(ConcreteValue::Duration(b)),
            ) => a == b,
            (Value::Concrete(ConcreteValue::List(a)), Value::Concrete(ConcreteValue::List(b))) => {
                a == b
            }
            (
                Value::Concrete(ConcreteValue::StringList(a)),
                Value::Concrete(ConcreteValue::StringList(b)),
            ) => a == b,
            (Value::Concrete(ConcreteValue::Map(a)), Value::Concrete(ConcreteValue::Map(b))) => {
                a == b
            }
            (
                Value::Deferred(DeferredValue::ResourceRef { path: a }),
                Value::Deferred(DeferredValue::ResourceRef { path: b }),
            ) => a == b,
            (
                Value::Deferred(DeferredValue::BindingRef { binding: a }),
                Value::Deferred(DeferredValue::BindingRef { binding: b }),
            ) => a == b,
            (
                Value::Deferred(DeferredValue::Coalesce {
                    value: av,
                    default: ad,
                }),
                Value::Deferred(DeferredValue::Coalesce {
                    value: bv,
                    default: bd,
                }),
            ) => av == bv && ad == bd,
            (
                Value::Deferred(DeferredValue::Interpolation(a)),
                Value::Deferred(DeferredValue::Interpolation(b)),
            ) => a == b,
            (
                Value::Deferred(DeferredValue::ResolvedFunctionCall {
                    function: af,
                    args: aa,
                    source_line: al,
                    ..
                }),
                Value::Deferred(DeferredValue::ResolvedFunctionCall {
                    function: bf,
                    args: ba,
                    source_line: bl,
                    ..
                }),
            ) => af.name == bf.name && aa == ba && al == bl,
            (
                Value::Deferred(DeferredValue::Access(a)),
                Value::Deferred(DeferredValue::Access(b)),
            ) => a.base() == b.base() && a.step() == b.step(),
            (
                Value::Deferred(DeferredValue::Secret(a)),
                Value::Deferred(DeferredValue::Secret(b)),
            ) => a == b,
            (
                Value::Deferred(DeferredValue::RedactedSecret { hash: a }),
                Value::Deferred(DeferredValue::RedactedSecret { hash: b }),
            ) => a == b,
            _ => false,
        }
    }
}

/// Body of `Value::canonicalize_in_place` for the `Interpolation` arm:
/// fold simple `Expr(scalar)` into a `Literal` (consuming the scalar's
/// string), merge adjacent `Literal`s, and collapse the result to
/// `Value::String` when no `Expr` parts remain.
fn canonicalize_interpolation(parts: Vec<InterpolationPart>) -> Value {
    let mut merged: Vec<InterpolationPart> = Vec::with_capacity(parts.len());
    let mut contains_folded_secret = false;
    for part in parts {
        let next = match part {
            InterpolationPart::Literal(s) => InterpolationPart::Literal(s),
            InterpolationPart::Expr(mut v) => {
                v.canonicalize_in_place();
                match value_into_literal(v) {
                    Ok((s, is_secret)) => {
                        contains_folded_secret |= is_secret;
                        InterpolationPart::Literal(s)
                    }
                    Err(other) => InterpolationPart::Expr(other),
                }
            }
        };
        match (merged.last_mut(), next) {
            (Some(InterpolationPart::Literal(prev)), InterpolationPart::Literal(s)) => {
                prev.push_derived(&s);
            }
            (_, p) => merged.push(p),
        }
    }
    let value = if merged.is_empty() {
        Value::Concrete(ConcreteValue::String("".into()))
    } else if merged.len() == 1 {
        // Pop the sole element. If it is a Literal, we collapse to
        // `Value::String`; otherwise it is a non-foldable Expr and we
        // rebuild the single-element Interpolation.
        match merged.pop().expect("len == 1") {
            InterpolationPart::Literal(s) => Value::Concrete(ConcreteValue::String(s)),
            expr @ InterpolationPart::Expr(_) => {
                merged.push(expr);
                Value::Deferred(DeferredValue::Interpolation(merged))
            }
        }
    } else {
        Value::Deferred(DeferredValue::Interpolation(merged))
    };
    if contains_folded_secret {
        Value::Deferred(DeferredValue::Secret(Box::new(value)))
    } else {
        value
    }
}

/// If `v` is a string-shaped scalar, return its string form (consuming
/// `v`); otherwise return `v` unchanged so the caller can keep wrapping
/// it as an `Expr`.
///
/// A scalar inside `Secret(_)` is folded through the same conversion and
/// reports that provenance to the caller, which wraps the completed
/// interpolation in `Secret` rather than dropping the sensitivity tag.
fn value_into_literal(v: Value) -> Result<(ValueString, bool), Value> {
    match v {
        Value::Concrete(ConcreteValue::String(s)) => Ok((s, false)),
        Value::Concrete(ConcreteValue::Int(n)) => Ok((n.to_string().into(), false)),
        Value::Concrete(ConcreteValue::Float(f)) => Ok((f.to_string().into(), false)),
        Value::Concrete(ConcreteValue::Bool(b)) => Ok((b.to_string().into(), false)),
        Value::Deferred(DeferredValue::Secret(inner)) => match value_into_literal(*inner) {
            Ok((literal, _)) => Ok((literal, true)),
            Err(inner) => Err(Value::Deferred(DeferredValue::Secret(Box::new(inner)))),
        },
        other => Err(other),
    }
}

/// Whether a resource reference must exist or is the value tested by `??`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRefUsage {
    Required,
    CoalesceValue,
}

impl Value {
    /// Recursively normalize this `Value` so that downstream code does not
    /// have to handle redundant `Interpolation` shapes.
    ///
    /// Applied bottom-up:
    ///
    /// - Adjacent `InterpolationPart::Literal`s are merged.
    /// - `InterpolationPart::Expr(v)` whose `v` is a bare `String`/`Int`/
    ///   `Float`/`Bool` is folded into a `Literal`, then merged with
    ///   neighbors per the previous rule. A scalar `Secret(_)` is folded
    ///   through its inner value and the completed interpolation is wrapped
    ///   in `Secret`, preserving redaction while allowing static evaluation.
    /// - An `Interpolation` whose parts collapse to a single `Literal` is
    ///   replaced with `Value::String(s)`.
    ///
    /// `List`, `Map`, `Secret`, `FunctionCall`, and `Access` recurse into their
    /// children at the `Value` level. Other variants are returned
    /// unchanged. The transformation is idempotent.
    ///
    /// See #2227.
    pub fn canonicalize(mut self) -> Value {
        self.canonicalize_in_place();
        self
    }

    /// In-place variant of [`Self::canonicalize`]. Useful when the caller
    /// only has a `&mut Value` (e.g. inside `IndexMap::values_mut()`).
    pub fn canonicalize_in_place(&mut self) {
        match self {
            Value::Concrete(ConcreteValue::List(items)) => {
                for item in items {
                    item.canonicalize_in_place();
                }
            }
            Value::Concrete(ConcreteValue::Map(map)) => {
                for v in map.values_mut() {
                    v.canonicalize_in_place();
                }
            }
            Value::Deferred(DeferredValue::Secret(inner)) => inner.canonicalize_in_place(),
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                value.canonicalize_in_place();
                default.canonicalize_in_place();
            }
            Value::Deferred(
                DeferredValue::FunctionCall { args, .. }
                | DeferredValue::Unavailable { args, .. }
                | DeferredValue::ResolvedFunctionCall { args, .. },
            ) => {
                for arg in args {
                    arg.walker_value_mut().canonicalize_in_place();
                }
            }
            Value::Deferred(DeferredValue::Access(access)) => {
                let access = access.clone();
                let mut base = access.base().clone();
                base.canonicalize_in_place();
                if let Ok(rebuilt) = access.rebuild_with_base(base, AccessMissPolicy::Error) {
                    *self = rebuilt;
                }
            }
            Value::Deferred(DeferredValue::Interpolation(_)) => {
                // Move the parts out so we can consume and rebuild them.
                let parts = match std::mem::replace(
                    self,
                    Value::Deferred(DeferredValue::Interpolation(Vec::new())),
                ) {
                    Value::Deferred(DeferredValue::Interpolation(parts)) => parts,
                    _ => unreachable!("matched Value::Interpolation"),
                };
                *self = canonicalize_interpolation(parts);
            }
            _ => {}
        }
    }

    /// Create a `ResourceRef` from binding name, attribute name, and optional field path.
    ///
    /// `attribute_name` must be non-empty. A bare-binding reference (no
    /// `.attr`) is a [`Value::BindingRef`], not a `ResourceRef` with an
    /// empty attribute — see #2847 for the regression that motivated
    /// the type-level split.
    pub fn resource_ref(
        binding_name: impl Into<ValueString>,
        attribute_name: impl Into<String>,
        field_path: Vec<String>,
    ) -> Self {
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::with_fields(binding_name, attribute_name, field_path),
        })
    }

    /// If this is a `ResourceRef`, returns the binding name.
    pub fn ref_binding(&self) -> Option<&str> {
        match self {
            Value::Deferred(DeferredValue::ResourceRef { path }) => Some(path.binding()),
            _ => None,
        }
    }

    /// If this is a `ResourceRef`, returns the attribute name.
    pub fn ref_attribute(&self) -> Option<&str> {
        match self {
            Value::Deferred(DeferredValue::ResourceRef { path }) => Some(path.attribute()),
            _ => None,
        }
    }

    /// If this is a `ResourceRef`, returns the ordered path segments
    /// past `binding.attribute`. carina#3025.
    pub fn ref_segments(&self) -> Option<&[PathSegment]> {
        match self {
            Value::Deferred(DeferredValue::ResourceRef { path }) => Some(path.segments()),
            _ => None,
        }
    }

    /// Recursively walk this value, invoking `f` on each `ResourceRef` access path.
    ///
    /// Companion: `visit_binding_refs` for bare `BindingRef` names. A consumer that
    /// needs both must call both — drift is visible at the call site.
    pub fn visit_resource_refs(&self, f: &mut impl FnMut(&AccessPath)) {
        self.visit_resource_refs_with_usage(&mut |path, _usage| f(path));
    }

    /// Recursively walk resource references while identifying references whose
    /// existence is guarded by the left operand of `??`.
    ///
    /// The right operand normally starts a required context. When the whole
    /// coalesce is itself a left operand, a direct-reference default remains
    /// optional so `(a ?? b) ?? c` has the same existence semantics as
    /// `a ?? (b ?? c)`. References nested in computations remain required.
    pub fn visit_resource_refs_with_usage(
        &self,
        f: &mut impl FnMut(&AccessPath, ResourceRefUsage),
    ) {
        self.visit_resource_refs_with_usage_inner(ResourceRefUsage::Required, f);
    }

    fn visit_resource_refs_with_usage_inner(
        &self,
        usage: ResourceRefUsage,
        f: &mut impl FnMut(&AccessPath, ResourceRefUsage),
    ) {
        match self {
            Value::Deferred(DeferredValue::ResourceRef { path }) => f(path, usage),
            Value::Concrete(ConcreteValue::List(items)) => {
                for v in items {
                    v.visit_resource_refs_with_usage_inner(usage, f);
                }
            }
            Value::Concrete(ConcreteValue::Map(map)) => {
                for v in map.values() {
                    v.visit_resource_refs_with_usage_inner(usage, f);
                }
            }
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                for part in parts {
                    match part {
                        InterpolationPart::Literal(_) => {}
                        InterpolationPart::Expr(v) => {
                            v.visit_resource_refs_with_usage_inner(usage, f)
                        }
                    }
                }
            }
            Value::Deferred(
                DeferredValue::FunctionCall { args, .. }
                | DeferredValue::Unavailable { args, .. }
                | DeferredValue::ResolvedFunctionCall { args, .. },
            ) => {
                for arg in args {
                    arg.walker_value()
                        .visit_resource_refs_with_usage_inner(usage, f);
                }
            }
            Value::Deferred(DeferredValue::Access(access)) => {
                access.base().visit_resource_refs_with_usage_inner(usage, f);
            }
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                let value_usage = if value.is_coalesce_optional_operand() {
                    ResourceRefUsage::CoalesceValue
                } else {
                    ResourceRefUsage::Required
                };
                let default_usage = if usage == ResourceRefUsage::CoalesceValue
                    && default.is_coalesce_optional_operand()
                {
                    ResourceRefUsage::CoalesceValue
                } else {
                    ResourceRefUsage::Required
                };
                value.visit_resource_refs_with_usage_inner(value_usage, f);
                default.visit_resource_refs_with_usage_inner(default_usage, f);
            }
            Value::Deferred(DeferredValue::Secret(inner)) => {
                inner.visit_resource_refs_with_usage_inner(usage, f)
            }
            Value::Concrete(ConcreteValue::String(_))
            | Value::Concrete(ConcreteValue::EnumIdentifier(_))
            | Value::Concrete(ConcreteValue::CanonicalEnum(_))
            | Value::Concrete(ConcreteValue::Int(_))
            | Value::Concrete(ConcreteValue::Float(_))
            | Value::Concrete(ConcreteValue::Bool(_))
            | Value::Concrete(ConcreteValue::Duration(_))
            | Value::Concrete(ConcreteValue::StringList(_))
            | Value::Deferred(DeferredValue::BindingRef { .. })
            | Value::Deferred(DeferredValue::RedactedSecret { .. }) => {}
            // `Value::Unknown` is what a previously-unresolved
            // `ResourceRef` was *replaced with* by `stamp_unresolved_upstream`.
            // It carries an `AccessPath` for display, but it is no longer
            // a live reference to walk — dependency analysis happens
            // upstream of the stamping pass.
            Value::Deferred(DeferredValue::Unknown(_)) => {}
        }
    }

    /// Recursively walk this value, invoking `f` on each bare `BindingRef` name.
    ///
    /// Companion: `visit_resource_refs` for `ResourceRef` access paths. A consumer
    /// that needs both must call both — drift is visible at the call site.
    pub fn visit_binding_refs(&self, f: &mut impl FnMut(&str)) {
        match self {
            Value::Deferred(DeferredValue::BindingRef { binding }) => f(binding),
            Value::Concrete(ConcreteValue::List(items)) => {
                for v in items {
                    v.visit_binding_refs(f);
                }
            }
            Value::Concrete(ConcreteValue::Map(map)) => {
                for v in map.values() {
                    v.visit_binding_refs(f);
                }
            }
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                for part in parts {
                    match part {
                        InterpolationPart::Literal(_) => {}
                        InterpolationPart::Expr(v) => v.visit_binding_refs(f),
                    }
                }
            }
            Value::Deferred(
                DeferredValue::FunctionCall { args, .. }
                | DeferredValue::Unavailable { args, .. }
                | DeferredValue::ResolvedFunctionCall { args, .. },
            ) => {
                for arg in args {
                    arg.walker_value().visit_binding_refs(f);
                }
            }
            Value::Deferred(DeferredValue::Access(access)) => {
                access.base().visit_binding_refs(f);
            }
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                value.visit_binding_refs(f);
                default.visit_binding_refs(f);
            }
            Value::Deferred(DeferredValue::Secret(inner)) => inner.visit_binding_refs(f),
            Value::Concrete(ConcreteValue::String(_))
            | Value::Concrete(ConcreteValue::EnumIdentifier(_))
            | Value::Concrete(ConcreteValue::CanonicalEnum(_))
            | Value::Concrete(ConcreteValue::Int(_))
            | Value::Concrete(ConcreteValue::Float(_))
            | Value::Concrete(ConcreteValue::Bool(_))
            | Value::Concrete(ConcreteValue::Duration(_))
            | Value::Concrete(ConcreteValue::StringList(_))
            | Value::Deferred(DeferredValue::ResourceRef { .. })
            | Value::Deferred(DeferredValue::RedactedSecret { .. }) => {}
            // `Value::Unknown` is what a previously-unresolved reference was
            // *replaced with* by stamping passes. It is no longer a live
            // reference to walk.
            Value::Deferred(DeferredValue::Unknown(_)) => {}
        }
    }
}

/// Project an `IndexMap<String, Value>` (the shape `Resource.attributes`
/// uses since #2222) into a plain `HashMap<String, Value>` for callers
/// that only need key-based lookup (state merging, ResourceRef
/// resolution, provider trait inputs). Source-order is dropped on
/// purpose at this boundary — keep it on `Resource.attributes` itself
/// when iteration order matters.
///
/// Despite the historical name (the helper used to operate on the now-removed
/// `Expr` newtype), this function does **not** resolve `Value::ResourceRef` /
/// `Value::Interpolation` / `Value::FunctionCall` / `Value::Access` /
/// `Value::Secret` — it just
/// projects ordered attribute storage to a hashmap. Callers that need concrete
/// values must run `carina_core::resolver::resolve_refs_with_state_and_remote`
/// (or an equivalent) first. See #1683 for a regression caused by assuming
/// this method performed resolution.
pub fn attrs_to_hashmap(attrs: &IndexMap<String, Value>) -> HashMap<String, Value> {
    attrs.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Check if a Value contains any ResourceRef (possibly nested)
pub fn contains_resource_ref(value: &Value) -> bool {
    let mut found = false;
    value.visit_resource_refs(&mut |_| found = true);
    found
}

/// Return whether `value` recursively contains
/// `Value::Deferred(DeferredValue::Unknown)`.
pub fn value_contains_unknown(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::Unknown(_)) => true,
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(value_contains_unknown),
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(value_contains_unknown),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(
                part,
                InterpolationPart::Expr(value) if value_contains_unknown(value)
            )
        }),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args
            .iter()
            .any(|argument| value_contains_unknown(argument.walker_value())),
        Value::Deferred(DeferredValue::Access(access)) => value_contains_unknown(access.base()),
        Value::Deferred(DeferredValue::Secret(inner)) => value_contains_unknown(inner),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            value_contains_unknown(value) || value_contains_unknown(default)
        }
        _ => false,
    }
}

/// Whether unavailable-value searches inspect a coalesce default before the
/// left operand has selected it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnavailableCoalescePolicy {
    ExcludeDefault,
    IncludeDefault,
}

/// How unavailable-value searches treat typed secret wrappers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnavailableSecretPolicy {
    Exclude,
    Include,
    OnlyInside,
}

/// Find the first built-in evaluation that was attempted and definitively
/// failed, using the caller's explicit coalesce and secret traversal policy.
pub(crate) fn find_unavailable_evaluation(
    value: &Value,
    coalesce_policy: UnavailableCoalescePolicy,
    secret_policy: UnavailableSecretPolicy,
) -> Option<(
    crate::builtins::BuiltinFunction,
    &UnavailableReason,
    DiagnosticSourceLine,
)> {
    fn find(
        value: &Value,
        coalesce_policy: UnavailableCoalescePolicy,
        secret_policy: UnavailableSecretPolicy,
        inside_secret: bool,
    ) -> Option<(
        crate::builtins::BuiltinFunction,
        &UnavailableReason,
        DiagnosticSourceLine,
    )> {
        let recurse = |value| find(value, coalesce_policy, secret_policy, inside_secret);
        match value {
            Value::Deferred(DeferredValue::Unavailable {
                name,
                reason,
                source_line,
                ..
            }) if secret_policy != UnavailableSecretPolicy::OnlyInside || inside_secret => {
                Some((*name, reason, *source_line))
            }
            Value::Deferred(DeferredValue::Unavailable { args, .. }) => args
                .iter()
                .find_map(|argument| recurse(argument.walker_value())),
            Value::Concrete(ConcreteValue::List(items)) => items.iter().find_map(recurse),
            Value::Concrete(ConcreteValue::Map(map)) => map.values().find_map(recurse),
            Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().find_map(|part| {
                if let InterpolationPart::Expr(value) = part {
                    recurse(value)
                } else {
                    None
                }
            }),
            Value::Deferred(
                DeferredValue::FunctionCall { args, .. }
                | DeferredValue::ResolvedFunctionCall { args, .. },
            ) => args
                .iter()
                .find_map(|argument| recurse(argument.walker_value())),
            Value::Deferred(DeferredValue::Access(access)) => recurse(access.base()),
            Value::Deferred(DeferredValue::Secret(inner)) => match secret_policy {
                UnavailableSecretPolicy::Exclude => None,
                UnavailableSecretPolicy::Include | UnavailableSecretPolicy::OnlyInside => {
                    find(inner, coalesce_policy, secret_policy, true)
                }
            },
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                recurse(value).or_else(|| match coalesce_policy {
                    UnavailableCoalescePolicy::ExcludeDefault => None,
                    UnavailableCoalescePolicy::IncludeDefault => recurse(default),
                })
            }
            Value::Concrete(
                ConcreteValue::String(_)
                | ConcreteValue::EnumIdentifier(_)
                | ConcreteValue::CanonicalEnum(_)
                | ConcreteValue::Int(_)
                | ConcreteValue::Float(_)
                | ConcreteValue::Bool(_)
                | ConcreteValue::Duration(_)
                | ConcreteValue::StringList(_),
            )
            | Value::Deferred(
                DeferredValue::ResourceRef { .. }
                | DeferredValue::BindingRef { .. }
                | DeferredValue::RedactedSecret { .. }
                | DeferredValue::Unknown(_),
            ) => None,
        }
    }

    find(value, coalesce_policy, secret_policy, false)
}

impl Value {
    /// Semantic equality: for Lists, compares as multisets (order-insensitive);
    /// for Maps, compares values recursively with semantic equality;
    /// for single-element List([Map]) vs Map, treats them as equivalent (bare struct);
    /// for all other variants, falls back to PartialEq.
    pub fn semantically_equal(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Concrete(ConcreteValue::List(a)), Value::Concrete(ConcreteValue::List(b))) => {
                lists_equal(a, b)
            }
            (Value::Concrete(ConcreteValue::Map(a)), Value::Concrete(ConcreteValue::Map(b))) => {
                maps_semantically_equal(a, b)
            }
            _ => self == other,
        }
    }

    /// Produce a deterministic hash for use in hash-based multiset comparison.
    /// For Maps, keys are sorted to ensure deterministic output.
    fn canonical_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash_into(&mut hasher);
        hasher.finish()
    }

    /// Feed a deterministic representation of this Value into a Hasher.
    fn hash_into(&self, hasher: &mut impl Hasher) {
        std::mem::discriminant(self).hash(hasher);
        match self {
            Value::Concrete(ConcreteValue::String(s)) => s.hash(hasher),
            Value::Concrete(ConcreteValue::EnumIdentifier(s)) => s.hash(hasher),
            Value::Concrete(ConcreteValue::CanonicalEnum(c)) => c.hash(hasher),
            Value::Concrete(ConcreteValue::Int(i)) => i.hash(hasher),
            Value::Concrete(ConcreteValue::Float(f)) => {
                // Float equality treats +0.0 and -0.0 as equal, so their
                // canonical hashes must also agree. Other values use their
                // bits for deterministic hashing (NaN never compares equal).
                if *f == 0.0 {
                    0.0_f64.to_bits().hash(hasher);
                } else {
                    f.to_bits().hash(hasher);
                }
            }
            Value::Concrete(ConcreteValue::Bool(b)) => b.hash(hasher),
            Value::Concrete(ConcreteValue::Duration(d)) => d.as_secs().hash(hasher),
            Value::Concrete(ConcreteValue::List(items)) => {
                // For list hashing, use an order-independent combination (wrapping sum)
                // so that lists with same elements in different order hash the same.
                // Wrapping sum is preferred over XOR because XOR causes all lists
                // with duplicate elements to collide (e.g., [1,1] XOR = 0, [2,2] XOR = 0).
                items.len().hash(hasher);
                let mut sum_hash: u64 = 0;
                for item in items {
                    sum_hash = sum_hash.wrapping_add(item.canonical_hash());
                }
                sum_hash.hash(hasher);
            }
            Value::Concrete(ConcreteValue::StringList(items)) => {
                // Hash with the same order-independent shape as
                // `Value::List` so that a `List([String("x")])` and a
                // `StringList(vec!["x"])` cannot collide on hash equality
                // (the outer discriminant separates them) but each
                // individually preserves the merge-list invariant.
                items.len().hash(hasher);
                let mut sum_hash: u64 = 0;
                for s in items {
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    s.hash(&mut h);
                    sum_hash = sum_hash.wrapping_add(h.finish());
                }
                sum_hash.hash(hasher);
            }
            Value::Concrete(ConcreteValue::Map(map)) => {
                map.len().hash(hasher);
                // Sort keys for deterministic hashing
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for key in keys {
                    key.hash(hasher);
                    map[key].hash_into(hasher);
                }
            }
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                path.hash(hasher);
            }
            Value::Deferred(DeferredValue::BindingRef { binding }) => {
                binding.hash(hasher);
            }
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                value.hash_into(hasher);
                default.hash_into(hasher);
            }
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                parts.len().hash(hasher);
                for part in parts {
                    match part {
                        InterpolationPart::Literal(s) => {
                            0u8.hash(hasher);
                            s.hash(hasher);
                        }
                        InterpolationPart::Expr(v) => {
                            1u8.hash(hasher);
                            v.hash_into(hasher);
                        }
                    }
                }
            }
            Value::Deferred(
                DeferredValue::FunctionCall { name, args, .. }
                | DeferredValue::Unavailable { name, args, .. },
            ) => {
                name.hash(hasher);
                args.len().hash(hasher);
                for arg in args {
                    arg.walker_value().hash_into(hasher);
                }
            }
            Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. }) => {
                function.name.hash(hasher);
                args.len().hash(hasher);
                for arg in args {
                    arg.walker_value().hash_into(hasher);
                }
            }
            Value::Deferred(DeferredValue::Access(access)) => {
                access.base().hash_into(hasher);
                access.step().hash(hasher);
            }
            Value::Deferred(DeferredValue::Secret(inner)) => {
                inner.hash_into(hasher);
            }
            Value::Deferred(DeferredValue::RedactedSecret { hash }) => {
                hash.hash(hasher);
            }
            Value::Deferred(DeferredValue::Unknown(reason)) => {
                // `Value::Unknown` reaches `merge_lists_hashed` → this
                // function whenever a list element is unresolved. Hash
                // deterministically: the outer `discriminant(self)`
                // already separates `Unknown` from concrete variants;
                // here we add the reason discriminant + payload so two
                // structurally-identical `Unknown`s produce the same
                // hash. Note: `PartialEq for Value` still returns
                // `false` between any two `Unknown`s (RFC #2371) — the
                // `a == b ⇒ hash(a) == hash(b)` precondition only
                // matters between concrete values.
                std::mem::discriminant(reason).hash(hasher);
                match reason {
                    // Reuse `AccessPath`'s native `Hash` impl (matches
                    // the `ResourceRef` arm above) — no per-call String
                    // allocation.
                    UnknownReason::UpstreamExport { path } => path.hash(hasher),
                    UnknownReason::UpstreamAllExports { binding } => binding.hash(hasher),
                    UnknownReason::FnParam { name }
                    | UnknownReason::FnLocal { name }
                    | UnknownReason::ModuleArgument { name, .. } => {
                        name.hash(hasher);
                    }
                    UnknownReason::PostCreateReadIncomplete { detail } => detail.hash(hasher),
                    // `For{Key,Index,Value}` and `EmptyInterpolation`
                    // carry no payload; the discriminant alone already
                    // distinguishes them.
                    UnknownReason::ForKey
                    | UnknownReason::ForIndex
                    | UnknownReason::ForValue
                    | UnknownReason::EmptyInterpolation => {}
                }
            }
        }
    }
}

/// Compare two maps using semantic equality for their values.
/// This ensures nested lists within maps are compared order-insensitively.
fn maps_semantically_equal(a: &IndexMap<String, Value>, b: &IndexMap<String, Value>) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .all(|(k, v)| b.get(k).map(|bv| v.semantically_equal(bv)).unwrap_or(false))
}

/// Threshold below which we use the simple O(n^2) algorithm.
/// For small lists, the overhead of hashing is not worth it.
const HASH_THRESHOLD: usize = 20;

/// Multiset (bag) comparison for two lists of Values.
/// Returns true if both lists contain the same elements with the same multiplicities,
/// regardless of order.
///
/// For small lists (< 20 elements), uses O(n^2) matching.
/// For large lists, uses hash-based bucketing to achieve O(n) average case.
fn lists_equal(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if a.len() < HASH_THRESHOLD {
        return lists_equal_quadratic(a, b);
    }
    lists_equal_hashed(a, b)
}

/// O(n^2) multiset comparison for small lists.
fn lists_equal_quadratic(a: &[Value], b: &[Value]) -> bool {
    let mut matched = vec![false; b.len()];
    for item_a in a {
        let mut found = false;
        for (j, item_b) in b.iter().enumerate() {
            if !matched[j] && item_a.semantically_equal(item_b) {
                matched[j] = true;
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// Hash-based multiset comparison for large lists.
/// Groups elements by hash, then does quadratic matching only within same-hash buckets.
/// Average case O(n), worst case O(n^2) on hash collisions.
fn lists_equal_hashed(a: &[Value], b: &[Value]) -> bool {
    // Build hash buckets for b
    let mut b_buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (j, item) in b.iter().enumerate() {
        b_buckets.entry(item.canonical_hash()).or_default().push(j);
    }

    let mut matched = vec![false; b.len()];
    for item_a in a {
        let hash = item_a.canonical_hash();
        let mut found = false;
        if let Some(bucket) = b_buckets.get(&hash) {
            for &j in bucket {
                if !matched[j] && item_a.semantically_equal(&b[j]) {
                    matched[j] = true;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// Merge desired value with saved state to fill in unmanaged nested fields.
///
/// `prior_explicit` is the authoring tree for this value. At each map node:
/// - [`ExplicitFields::Unrecorded`], [`ExplicitFields::Leaf`], or a shape mismatch keeps the
///   legacy full merge;
/// - [`ExplicitFields::WholeValue`] only has special meaning for lists; at a
///   malformed map position it keeps the conservative legacy full merge;
/// - a saved key absent from prior authored children is unmanaged and is retained;
/// - a saved key present in prior authored children but absent from desired was removed by the
///   user and is omitted;
/// - matching desired/saved children recurse with that child's prior authoring node.
///
/// Removal detection at list positions requires a stored-index-aligned
/// [`ExplicitFields::ListElements`] node. Legacy list authoring shapes keep the conservative full
/// merge for every element.
#[derive(Clone, Copy)]
pub(crate) struct SavedValueViews<'a> {
    projected: &'a Value,
    raw: &'a Value,
}

impl<'a> SavedValueViews<'a> {
    pub(crate) fn new(projected: &'a Value, raw: &'a Value) -> Self {
        Self { projected, raw }
    }

    pub(crate) fn same(value: &'a Value) -> Self {
        Self::new(value, value)
    }
}

pub(crate) fn merge_with_saved(
    desired: &Value,
    saved: SavedValueViews<'_>,
    prior_explicit: &ExplicitFields,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> Value {
    match (desired, saved.projected) {
        (
            Value::Concrete(ConcreteValue::Map(desired_map)),
            Value::Concrete(ConcreteValue::Map(projected_saved_map)),
        ) => {
            let raw_saved_map = match saved.raw {
                Value::Concrete(ConcreteValue::Map(raw_saved_map)) => raw_saved_map,
                _ => projected_saved_map,
            };
            merge_maps(
                desired_map,
                projected_saved_map,
                raw_saved_map,
                prior_explicit,
                attr_type,
                defs,
            )
        }
        (
            Value::Concrete(ConcreteValue::List(desired_list)),
            Value::Concrete(ConcreteValue::List(projected_saved_list)),
        ) => {
            let raw_saved_list = match saved.raw {
                Value::Concrete(ConcreteValue::List(raw_saved_list)) => raw_saved_list,
                _ => projected_saved_list,
            };
            Value::Concrete(ConcreteValue::List(merge_lists(
                desired_list,
                projected_saved_list,
                raw_saved_list,
                prior_explicit,
                attr_type,
                defs,
            )))
        }
        _ => desired.clone(),
    }
}

fn merge_maps(
    desired: &IndexMap<String, Value>,
    projected_saved: &IndexMap<String, Value>,
    raw_saved: &IndexMap<String, Value>,
    prior_explicit: &ExplicitFields,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> Value {
    let prior_children = match prior_explicit {
        ExplicitFields::Struct { children } => Some(children),
        ExplicitFields::Unrecorded
        | ExplicitFields::Leaf
        | ExplicitFields::List { .. }
        | ExplicitFields::ListElements { .. }
        | ExplicitFields::WholeValue { .. } => None,
    };

    let mut merged = projected_saved.clone();
    if let Some(children) = prior_children {
        merged.retain(|key, _| {
            let was_authored = children
                .get(key)
                .is_some_and(|child| !matches!(child, ExplicitFields::Unrecorded));
            desired.contains_key(key) || !was_authored
        });
    }
    for (key, desired_value) in desired {
        let merged_value = match projected_saved.get(key) {
            Some(projected_saved_value) => {
                let child_views = raw_saved
                    .get(key)
                    .map(|raw_saved_value| {
                        SavedValueViews::new(projected_saved_value, raw_saved_value)
                    })
                    .unwrap_or_else(|| SavedValueViews::same(projected_saved_value));
                merge_with_saved(
                    desired_value,
                    child_views,
                    prior_children
                        .and_then(|children| children.get(key))
                        .unwrap_or(&ExplicitFields::Unrecorded),
                    map_entry_subtype(attr_type, key, defs),
                    defs,
                )
            }
            None => desired_value.clone(),
        };
        merged.insert(key.clone(), merged_value);
    }
    Value::Concrete(ConcreteValue::Map(merged))
}

/// Pair each desired-list index to at most one saved-list index.
///
/// Every positive-score candidate is computed before any element is consumed.
/// Candidates are then committed from highest score to lowest score. Equal
/// scores preserve stable list-order preferences when equivalent alternatives
/// exist. This keeps legacy assignments for indistinguishable peers without
/// allowing a lower score to beat a higher one. Accepting every positive score
/// preserves the existing score-1 fallback behavior.
///
/// Below [`HASH_THRESHOLD`] every pair is scored directly. Larger lists use
/// canonical-hash buckets to generate only pairs that can share an equal field
/// (or equal scalar), then run the exact same global best-first commit phase.
/// Hash collisions only add a candidate: [`similarity_score`] remains the
/// authority before a candidate can be committed.
pub(crate) fn pair_list_elements<D, S>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> Vec<Option<usize>>
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    let candidates = list_pair_candidates(desired, saved, attr_type, defs);
    commit_list_element_pairs(desired.len(), saved.len(), &candidates)
}

/// A stable list-element pairing together with the evidence for that pairing.
///
/// The ordinary differ separately uses a stable match for ambiguous peers as a
/// display/merge heuristic. State authoring transfer is not allowed to do so:
/// only [`Self::Unique`] establishes element identity. A zero-similarity
/// element is [`Self::Unmatched`] even when list position could supply a
/// tempting guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListElementPairing {
    Unique(usize),
    Ambiguous,
    Unmatched,
}

/// Pair list elements while distinguishing evidence-backed identity from the
/// stable tie-breaking used by the normal differ.
///
/// A match is unique only when the selected saved element is the sole
/// positive, highest-scoring candidate for that desired element. Global
/// contention that forces a lower-scoring choice is therefore ambiguous too.
pub(crate) fn pair_list_elements_with_uniqueness<D, S>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> Vec<ListElementPairing>
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    let candidates = list_pair_candidates(desired, saved, attr_type, defs);
    let pairs = commit_list_element_pairs(desired.len(), saved.len(), &candidates);
    classify_list_element_pairs(&pairs, saved.len(), &candidates)
}

fn list_pair_candidates<D, S>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> PairCandidates
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    if desired.len().max(saved.len()) < HASH_THRESHOLD {
        pair_candidates_quadratic(desired, saved, attr_type, defs)
    } else {
        pair_candidates_hashed(desired, saved, attr_type, defs)
    }
}

fn commit_list_element_pairs(
    desired_len: usize,
    saved_len: usize,
    candidates: &PairCandidates,
) -> Vec<Option<usize>> {
    // Assign each desired element one stable, schema-aware preference before
    // committing anything. Preferences are not pair commits: every explicit
    // candidate and every implicit complete-row score has already been
    // computed, and score remains the first ordering key. Choosing an
    // unreserved equal-best saved index when possible is the O(n + m)
    // equivalent of the former desired×saved reservation matrix.
    let mut preferred_saved = vec![None; desired_len];
    let mut preferred_score = vec![0; desired_len];
    let mut preference_reserved = vec![false; saved_len];
    for (desired_index, preferred_slot) in preferred_saved.iter_mut().enumerate() {
        let entries = &candidates.entries[candidates.desired_offsets[desired_index]
            ..candidates.desired_offsets[desired_index + 1]];
        let best_score = entries
            .iter()
            .map(|(score, _, _)| *score)
            .max()
            .unwrap_or(0)
            .max(candidates.implicit_scores[desired_index]);
        if best_score == 0 {
            continue;
        }
        preferred_score[desired_index] = best_score;
        let preferred = if candidates.implicit_scores[desired_index] == best_score {
            // Every saved index has this score. Prefer the first index not
            // already reserved by an earlier desired element, matching the
            // explicit-candidate tie rule without materializing n×m edges.
            (0..saved_len)
                .find(|saved_index| !preference_reserved[*saved_index])
                .or((saved_len != 0).then_some(0))
        } else {
            let fallback = entries
                .iter()
                .filter(|(score, _, _)| *score == best_score)
                .map(|(_, _, saved_index)| *saved_index)
                .min();
            entries
                .iter()
                .filter(|(score, _, saved_index)| {
                    *score == best_score && !preference_reserved[*saved_index]
                })
                .map(|(_, _, saved_index)| *saved_index)
                .min()
                .or(fallback)
        };
        if let Some(saved_index) = preferred {
            *preferred_slot = Some(saved_index);
            preference_reserved[saved_index] = true;
        }
    }

    // Commit by the same total order as the former explicit edge sort:
    // descending score, preferred candidates first, then desired and saved
    // list order. Score levels let a complete row of equal-score candidates
    // stay compressed instead of allocating or visiting every edge.
    let mut score_levels = BTreeSet::new();
    score_levels.extend(candidates.entries.iter().map(|(score, _, _)| *score));
    score_levels.extend(
        candidates
            .implicit_scores
            .iter()
            .copied()
            .filter(|score| *score > 0),
    );
    let mut pairs = vec![None; desired_len];
    let mut available_saved: BTreeSet<usize> = (0..saved_len).collect();
    for score in score_levels.into_iter().rev() {
        // Preferred candidates sort before every non-preferred candidate at
        // the same score.
        for desired_index in 0..desired_len {
            if pairs[desired_index].is_some() || preferred_score[desired_index] != score {
                continue;
            }
            let Some(saved_index) = preferred_saved[desired_index] else {
                continue;
            };
            if available_saved.remove(&saved_index) {
                pairs[desired_index] = Some(saved_index);
            }
        }

        // Non-preferred candidates sort by desired index and then saved index.
        for (desired_index, pair) in pairs.iter_mut().enumerate() {
            if pair.is_some() {
                continue;
            }
            let saved_index = if candidates.implicit_scores[desired_index] == score {
                // A complete-row score means every still-available saved edge
                // has at least this score. If one had a higher score, the
                // explicit candidate would already have consumed one of its
                // endpoints in an earlier score phase. Therefore the first
                // available saved index is exactly the next edge in the old
                // global sort order.
                available_saved.first().copied()
            } else {
                candidates.entries[candidates.desired_offsets[desired_index]
                    ..candidates.desired_offsets[desired_index + 1]]
                    .iter()
                    .find_map(|(candidate_score, _, saved_index)| {
                        (*candidate_score == score && available_saved.contains(saved_index))
                            .then_some(*saved_index)
                    })
            };
            if let Some(saved_index) = saved_index {
                available_saved.remove(&saved_index);
                *pair = Some(saved_index);
            }
        }
    }
    pairs
}

fn classify_list_element_pairs(
    pairs: &[Option<usize>],
    saved_len: usize,
    candidates: &PairCandidates,
) -> Vec<ListElementPairing> {
    pairs
        .iter()
        .copied()
        .enumerate()
        .map(|(desired_index, selected)| {
            let entries = &candidates.entries[candidates.desired_offsets[desired_index]
                ..candidates.desired_offsets[desired_index + 1]];
            let explicit_best = entries
                .iter()
                .map(|(score, _, _)| *score)
                .max()
                .unwrap_or(0);
            let implicit_best = candidates.implicit_scores[desired_index];
            let best_score = explicit_best.max(implicit_best);

            if best_score == 0 {
                return ListElementPairing::Unmatched;
            }

            let sole_best = if implicit_best == best_score {
                (saved_len == 1).then_some(0)
            } else {
                let mut best_indices = entries
                    .iter()
                    .filter(|(score, _, _)| *score == best_score)
                    .map(|(_, _, saved_index)| *saved_index);
                let first = best_indices.next();
                match (first, best_indices.next()) {
                    (Some(index), None) => Some(index),
                    (None, None) | (Some(_), Some(_)) | (None, Some(_)) => None,
                }
            };

            match (selected, sole_best) {
                (Some(selected), Some(sole)) if selected == sole => {
                    ListElementPairing::Unique(selected)
                }
                (Some(_), Some(_) | None) | (None, Some(_) | None) => ListElementPairing::Ambiguous,
            }
        })
        .collect()
}

type PairCandidate = (usize, usize, usize);

struct PairCandidates {
    entries: Vec<PairCandidate>,
    desired_offsets: Vec<usize>,
    /// Per desired index, a positive score shared by every saved index.
    /// Explicit entries override it where selective fields add more matches.
    implicit_scores: Vec<usize>,
}

fn pair_candidates_quadratic<D, S>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> PairCandidates
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    let mut entries = Vec::new();
    let mut desired_offsets = Vec::with_capacity(desired.len() + 1);
    desired_offsets.push(0);
    for (desired_index, desired_value) in desired.iter().enumerate() {
        for (saved_index, saved_value) in saved.iter().enumerate() {
            let score = similarity_score(
                desired_value.borrow(),
                saved_value.borrow(),
                attr_type,
                defs,
            );
            if score > 0 {
                entries.push((score, desired_index, saved_index));
            }
        }
        desired_offsets.push(entries.len());
    }
    PairCandidates {
        entries,
        desired_offsets,
        implicit_scores: vec![0; desired.len()],
    }
}

/// Generate a complete representation of the positive-score candidate set.
///
/// Map elements are indexed by `(field key, schema-aware field hash)`, so a
/// desired/saved pair is scored only if at least one field could compare equal.
/// Non-map elements are indexed by their schema-aware whole-value hashes.
/// A field proven equal across every saved element is stored once as an
/// implicit complete-row contribution rather than expanding its bucket for
/// every desired element. Selective fields still produce explicit, exact
/// scores. Thus all candidates are scored before commitment while ubiquitous
/// values such as IAM `effect: "Allow"` do not create n×m entries.
///
/// Coarse fallback hashes for structural schema shapes may admit collisions,
/// but never exclude a pair that [`schema_aware_equal`] could accept. A dense
/// but non-universal bucket can still make the worst case quadratic; the
/// universal-field compression covers the common policy-statement shape while
/// preserving exactly the same pairing decisions.
fn pair_candidates_hashed<D, S>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> PairCandidates
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    let mut map_field_buckets: HashMap<&str, HashMap<u64, Vec<usize>>> = HashMap::new();
    let mut value_buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (saved_index, saved_value) in saved.iter().enumerate() {
        match saved_value.borrow() {
            Value::Concrete(ConcreteValue::Map(map)) => {
                for (key, value) in map {
                    let field_type = map_entry_subtype(attr_type, key, defs);
                    for value_hash in pairing_hashes(value, field_type, defs) {
                        map_field_buckets
                            .entry(key)
                            .or_default()
                            .entry(value_hash)
                            .or_default()
                            .push(saved_index);
                    }
                }
            }
            value => {
                for value_hash in pairing_hashes(value, attr_type, defs) {
                    value_buckets
                        .entry(value_hash)
                        .or_default()
                        .push(saved_index);
                }
            }
        }
    }

    // Identify hash buckets that really are one schema-aware equivalence class
    // spanning every saved element. The equality verification makes the
    // compression exact even when canonical hashes collide or enum spellings
    // reach the same schema-normalized hash.
    let mut universal_map_fields: HashMap<&str, HashMap<u64, &Value>> = HashMap::new();
    for (key, value_buckets) in &map_field_buckets {
        let field_type = map_entry_subtype(attr_type, key, defs);
        let Some(representative) = saved.first().and_then(|saved_value| {
            let Value::Concrete(ConcreteValue::Map(map)) = saved_value.borrow() else {
                return None;
            };
            map.get(*key)
        }) else {
            continue;
        };
        for (value_hash, bucket) in value_buckets {
            if bucket.len() != saved.len()
                || !bucket.iter().copied().eq(0..saved.len())
                || !saved.iter().all(|saved_value| {
                    let Value::Concrete(ConcreteValue::Map(map)) = saved_value.borrow() else {
                        return false;
                    };
                    map.get(*key).is_some_and(|value| {
                        schema_aware_equal(representative, value, field_type, defs)
                    })
                })
            {
                continue;
            }
            universal_map_fields
                .entry(key)
                .or_default()
                .insert(*value_hash, representative);
        }
    }

    let mut universal_values = HashMap::new();
    if let Some(representative) = saved.first().map(Borrow::borrow) {
        for (value_hash, bucket) in &value_buckets {
            if bucket.len() == saved.len()
                && bucket.iter().copied().eq(0..saved.len())
                && saved.iter().all(|saved_value| {
                    schema_aware_equal(representative, saved_value.borrow(), attr_type, defs)
                })
            {
                universal_values.insert(*value_hash, representative);
            }
        }
    }

    let mut entries = Vec::new();
    let mut desired_offsets = Vec::with_capacity(desired.len() + 1);
    let mut implicit_scores = Vec::with_capacity(desired.len());
    desired_offsets.push(0);
    let mut possible_saved = Vec::new();
    for (desired_index, desired_value) in desired.iter().enumerate() {
        possible_saved.clear();
        let mut implicit_score = 0;
        match desired_value.borrow() {
            Value::Concrete(ConcreteValue::Map(map)) => {
                for (key, value) in map {
                    let field_type = map_entry_subtype(attr_type, key, defs);
                    let value_hashes = pairing_hashes(value, field_type, defs);
                    let field_is_universal =
                        universal_map_fields
                            .get(key.as_str())
                            .is_some_and(|universal_buckets| {
                                value_hashes.iter().any(|value_hash| {
                                    universal_buckets.get(value_hash).is_some_and(
                                        |representative| {
                                            schema_aware_equal(
                                                value,
                                                representative,
                                                field_type,
                                                defs,
                                            )
                                        },
                                    )
                                })
                            });
                    if field_is_universal {
                        implicit_score += 1;
                        continue;
                    }
                    if let Some(field_buckets) = map_field_buckets.get(key.as_str()) {
                        for value_hash in value_hashes {
                            if let Some(bucket) = field_buckets.get(&value_hash) {
                                possible_saved.extend(bucket.iter().copied());
                            }
                        }
                    }
                }
            }
            value => {
                let value_hashes = pairing_hashes(value, attr_type, defs);
                let value_is_universal = value_hashes.iter().any(|value_hash| {
                    universal_values
                        .get(value_hash)
                        .is_some_and(|representative| {
                            schema_aware_equal(value, representative, attr_type, defs)
                        })
                });
                if value_is_universal {
                    implicit_score = 1;
                } else {
                    for value_hash in value_hashes {
                        let Some(bucket) = value_buckets.get(&value_hash) else {
                            continue;
                        };
                        possible_saved.extend(bucket.iter().copied());
                    }
                }
            }
        }
        possible_saved.sort_unstable();
        possible_saved.dedup();
        for saved_index in possible_saved.iter().copied() {
            let score = similarity_score(
                desired_value.borrow(),
                saved[saved_index].borrow(),
                attr_type,
                defs,
            );
            if score > 0 {
                entries.push((score, desired_index, saved_index));
            }
        }
        desired_offsets.push(entries.len());
        implicit_scores.push(implicit_score);
    }
    PairCandidates {
        entries,
        desired_offsets,
        implicit_scores,
    }
}

fn pairing_hashes(
    value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> Vec<u64> {
    let mut hashes = vec![value.canonical_hash()];
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => {
            // `type_aware_equal` checks Secret before schema dispatch. A
            // secret can equal either its plaintext inner String or its typed
            // persisted-state hash, so candidate
            // generation must expose both relations as well. Recursive inner
            // hashes cover plaintext (including schema normalizations); the
            // fixed tag deliberately admits every state-secret spelling for
            // exact verification by `similarity_score`.
            hashes.extend(pairing_hashes(inner, attr_type, defs));
            hashes.push(secret_state_pairing_hash());
        }
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => {
            hashes.push(secret_state_pairing_hash());
        }
        _ => {}
    }
    if let Some(attr_type) = attr_type {
        append_schema_pairing_hashes(value, attr_type, defs, &mut hashes, 0);
    }
    hashes.sort_unstable();
    hashes.dedup();
    hashes
}

fn secret_state_pairing_hash() -> u64 {
    pairing_component_hash(0x08, &RedactedSecretHash::V2_MARKER_PREFIX)
}

fn append_schema_pairing_hashes(
    value: &Value,
    attr_type: &AttributeType,
    defs: &BTreeMap<String, AttributeType>,
    hashes: &mut Vec<u64>,
    depth: usize,
) {
    if depth == 16 {
        hashes.push(pairing_component_hash(0xff, &0_u8));
        return;
    }
    match attr_type.shape_with_defs(defs) {
        Shape::String {
            to_dsl: Some(transform),
            ..
        } => {
            if let Value::Concrete(ConcreteValue::String(text)) = value {
                hashes.push(pairing_component_hash(
                    0x01,
                    &transform.apply(text.expose_plaintext()),
                ));
            }
        }
        Shape::String { to_dsl: None, .. } | Shape::Bool | Shape::Duration => {}
        Shape::Int { .. } | Shape::Float { .. } => {
            if let Some(hash) = numeric_pairing_hash(value) {
                hashes.push(hash);
            }
        }
        Shape::Enum {
            values,
            dsl_aliases,
            to_dsl,
            ..
        } => {
            let canonical = match value {
                Value::Concrete(ConcreteValue::String(text)) => {
                    Some(text.expose_plaintext().to_string())
                }
                Value::Concrete(ConcreteValue::EnumIdentifier(text)) => {
                    let valid_values: Vec<&str> =
                        values.into_iter().flatten().map(String::as_str).collect();
                    let dsl_map = DslMap::new(dsl_aliases, to_dsl);
                    Some(text.canonical_api_spelling(&valid_values, &dsl_map))
                }
                Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
                    Some(value.api_value().to_string())
                }
                _ => None,
            };
            if let Some(canonical) = canonical {
                hashes.push(pairing_component_hash(
                    0x02,
                    &enum_value_comparison_key(&canonical),
                ));
            }
        }
        Shape::List { ordered, .. } => {
            if let Value::Concrete(ConcreteValue::List(items)) = value {
                hashes.push(pairing_component_hash(0x03, &(ordered, items.len())));
            }
        }
        Shape::Map { .. } => {
            if let Value::Concrete(ConcreteValue::Map(map)) = value {
                let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
                keys.sort_unstable();
                hashes.push(pairing_component_hash(0x04, &(map.len(), keys)));
            }
        }
        Shape::Struct { .. } => {
            if matches!(value, Value::Concrete(ConcreteValue::Map(_))) {
                // Struct equality tolerates omitted default-valued fields, so
                // raw map hashes are not a complete equivalence key.
                hashes.push(pairing_component_hash(0x05, &0_u8));
            }
        }
        Shape::Union => {
            let members = crate::schema::union_members_with_defs(attr_type, defs)
                .expect("Shape::Union must expose union members internally");
            for member in members.iter().flatten() {
                append_schema_pairing_hashes(value, member.as_attr(), defs, hashes, depth + 1);
            }
        }
    }
}

fn numeric_pairing_hash(value: &Value) -> Option<u64> {
    match value {
        Value::Concrete(ConcreteValue::Int(value)) => Some(pairing_component_hash(0x06, value)),
        Value::Concrete(ConcreteValue::Float(value)) => {
            let integer = *value as i64;
            if integer as f64 == *value {
                Some(pairing_component_hash(0x06, &integer))
            } else {
                Some(pairing_component_hash(0x07, &value.to_bits()))
            }
        }
        _ => None,
    }
}

fn pairing_component_hash<T: Hash>(tag: u8, value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tag.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

/// Merge two lists using projected saved values for value pairing and raw saved values to
/// corroborate stored-index authoring attribution.
fn merge_lists(
    desired: &[Value],
    projected_saved: &[Value],
    raw_saved: &[Value],
    prior_explicit: &ExplicitFields,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> Vec<Value> {
    // `WholeValue` is the explicit whole-value fallback used when a carried row
    // cannot transfer a pending nested removal to a uniquely identified list
    // element. Merging individual saved elements here would fill the removed
    // child back in and hide the very diff the fallback represents.
    let aligned_elements = match prior_explicit {
        ExplicitFields::WholeValue { .. } => return desired.to_vec(),
        ExplicitFields::ListElements { elements }
            if elements.len() == projected_saved.len()
                && raw_saved.len() == projected_saved.len() =>
        {
            Some(elements)
        }
        ExplicitFields::Leaf
        | ExplicitFields::Struct { .. }
        | ExplicitFields::List { .. }
        | ExplicitFields::ListElements { .. }
        | ExplicitFields::Unrecorded => None,
    };

    let item_type = list_element_type(attr_type, defs);
    let projected_pairs = pair_list_elements(desired, projected_saved, item_type, defs);
    let raw_pairs = pair_list_elements(desired, raw_saved, item_type, defs);

    projected_pairs
        .into_iter()
        .zip(raw_pairs)
        .enumerate()
        .map(
            |(desired_index, (projected_index, raw_index))| match projected_index {
                Some(projected_index) => {
                    // Writeback assigned `elements` using raw-list pairing, while the legacy value
                    // merge pairs against the projected list. Only matching indices corroborate the
                    // authored claim; disagreement is uncertainty and must stay conservative.
                    let authoring = aligned_elements
                        .filter(|_| raw_index == Some(projected_index))
                        .map(|elements| &elements[projected_index])
                        .unwrap_or(&ExplicitFields::Unrecorded);
                    let projected_value = &projected_saved[projected_index];
                    let saved_views = raw_saved
                        .get(projected_index)
                        .map(|raw_value| SavedValueViews::new(projected_value, raw_value))
                        .unwrap_or_else(|| SavedValueViews::same(projected_value));
                    merge_with_saved(
                        &desired[desired_index],
                        saved_views,
                        authoring,
                        item_type,
                        defs,
                    )
                }
                None => desired[desired_index].clone(),
            },
        )
        .collect()
}

/// Compute a similarity score between two Values.
/// For Maps: count matching key-value pairs.
/// For equal scalars: return 1.
/// Otherwise: return 0.
fn similarity_score(
    a: &Value,
    b: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> usize {
    match (a, b) {
        (Value::Concrete(ConcreteValue::Map(am)), Value::Concrete(ConcreteValue::Map(bm))) => am
            .iter()
            .filter(|(k, v)| {
                bm.get(*k)
                    .map(|bv| {
                        schema_aware_equal(v, bv, map_entry_subtype(attr_type, k, defs), defs)
                    })
                    .unwrap_or(false)
            })
            .count(),
        _ => usize::from(schema_aware_equal(a, b, attr_type, defs)),
    }
}

/// Carina-side directives for a resource.
///
/// These are instructions to Carina about how to handle the resource,
/// not metadata about the resource itself. Mirrors the WIT
/// `directives` record.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Directives {
    /// If true, force-delete the resource (e.g., non-empty S3 buckets)
    #[serde(default)]
    pub force_delete: bool,
    /// If true, create the new resource before destroying the old one during replacement
    #[serde(default)]
    pub create_before_destroy: bool,
    /// If true, prevent the resource from being destroyed
    #[serde(default)]
    pub prevent_destroy: bool,
    /// Explicit ordering edges declared by the user. Each element is the
    /// binding name of a sibling `let` (resource / wait / module).
    /// Set semantics (deduplicated, order-insensitive); represented as
    /// Vec to preserve source order for `carina fmt` round-tripping.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Binding name of the provider instance to use for this resource.
    /// `None` resolves to the kind's default instance. Phase 2 only
    /// holds the slot; `directives { provider = ... }` reading and
    /// routing land in Phase 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_instance: Option<String>,
}

/// The full expansion scope of a module-owned resource.
///
/// `key` is an internal, collision-resistant path used only when grouping
/// anonymous resources for identity validation. `display` retains the
/// authored call form so diagnostics never expose synthetic instance hashes.
#[derive(Debug, Clone)]
pub struct ModuleInstanceScope {
    key: String,
    display: ModuleInstanceScopeDisplay,
}

#[derive(Debug, Clone)]
enum ModuleInstanceScopeDisplay {
    Bound(String),
    Anonymous(String),
}

impl ModuleInstanceScope {
    pub(crate) fn for_call(
        module_name: &str,
        binding: Option<&str>,
        instance_prefix: &str,
    ) -> Self {
        let display = match binding {
            Some(binding) => ModuleInstanceScopeDisplay::Bound(binding.to_string()),
            None => ModuleInstanceScopeDisplay::Anonymous(module_name.to_string()),
        };
        Self {
            key: instance_prefix.to_string(),
            display,
        }
    }

    fn bound(instance: &str) -> Self {
        Self {
            key: instance.to_string(),
            display: ModuleInstanceScopeDisplay::Bound(instance.to_string()),
        }
    }

    pub(crate) fn with_child(&self, child: &Self) -> Self {
        let display = match (&self.display, &child.display) {
            (
                ModuleInstanceScopeDisplay::Bound(parent),
                ModuleInstanceScopeDisplay::Bound(child),
            ) => ModuleInstanceScopeDisplay::Bound(format!("{parent}.{child}")),
            (_, child) => child.clone(),
        };
        Self {
            key: format!("{}.{}", self.key, child.key),
            display,
        }
    }

    pub(crate) fn remap_prefix(&self, from: &str, to: &str) -> Self {
        let key = if self.key == from {
            to.to_string()
        } else if let Some(rest) = self.key.strip_prefix(&format!("{from}.")) {
            format!("{to}.{rest}")
        } else {
            self.key.clone()
        };
        Self {
            key,
            display: self.display.clone(),
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn description(&self) -> String {
        match &self.display {
            ModuleInstanceScopeDisplay::Bound(path) => format!("module instance '{path}'"),
            ModuleInstanceScopeDisplay::Anonymous(module_name) => {
                format!("an anonymous call of module '{module_name}'")
            }
        }
    }
}

/// Source of a resource (root, a live module call, or saved display metadata).
///
/// Live module sources always retain their parser-minted call path. Saved
/// resources intentionally decode into [`ModuleSource::SavedModule`] because
/// the portable resource representation stores only display metadata; it must
/// never masquerade as a live source with a missing identity.
#[derive(Debug, Clone)]
pub enum ModuleSource {
    /// Resource defined at the root level
    Root,
    /// Resource from a module instantiation
    Module {
        /// Module name (e.g., "web_tier")
        name: String,
        /// Instance binding name (e.g., "web")
        instance: String,
        /// Full nested instance path plus its authored diagnostic form.
        scope: Option<ModuleInstanceScope>,
        /// Parser-minted call path that owns this expanded node.
        call_path: Box<composition::CompositionCallPath>,
    },
    /// Display metadata decoded from a persisted resource.
    SavedModule {
        /// Module name (e.g., "web_tier")
        name: String,
        /// Instance binding name (e.g., "web")
        instance: String,
    },
}

/// Stable persisted representation of [`ModuleSource`]. Live-only identity is
/// deliberately projected away and decodes into the explicit saved variant.
#[derive(Serialize, Deserialize)]
enum SavedModuleSource {
    Root,
    Module { name: String, instance: String },
}

impl Serialize for ModuleSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let saved = match self {
            Self::Root => SavedModuleSource::Root,
            Self::Module { name, instance, .. } | Self::SavedModule { name, instance } => {
                SavedModuleSource::Module {
                    name: name.clone(),
                    instance: instance.clone(),
                }
            }
        };
        saved.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ModuleSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match SavedModuleSource::deserialize(deserializer)? {
            SavedModuleSource::Root => Ok(Self::Root),
            SavedModuleSource::Module { name, instance } => {
                Ok(Self::SavedModule { name, instance })
            }
        }
    }
}

impl PartialEq for ModuleSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Root, Self::Root) => true,
            (
                Self::Module {
                    call_path: left_call_path,
                    ..
                },
                Self::Module {
                    call_path: right_call_path,
                    ..
                },
            ) => left_call_path == right_call_path,
            (
                Self::SavedModule {
                    name: left_name,
                    instance: left_instance,
                },
                Self::SavedModule {
                    name: right_name,
                    instance: right_instance,
                },
            ) => left_name == right_name && left_instance == right_instance,
            _ => false,
        }
    }
}

impl Eq for ModuleSource {}

impl Hash for ModuleSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Keep hashing aligned with `PartialEq`: live identity is the complete
        // call path; saved display metadata has its own, separate variant.
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Root => {}
            Self::Module { call_path, .. } => call_path.hash(state),
            Self::SavedModule { name, instance } => {
                name.hash(state);
                instance.hash(state);
            }
        }
    }
}

impl ModuleSource {
    /// Create persisted/display-only module metadata with no live call ID.
    pub fn saved_module(name: impl Into<String>, instance: impl Into<String>) -> Self {
        Self::SavedModule {
            name: name.into(),
            instance: instance.into(),
        }
    }

    pub(crate) fn expanded_module(
        name: impl Into<String>,
        instance: impl Into<String>,
        scope: ModuleInstanceScope,
        call_path: composition::CompositionCallPath,
    ) -> Self {
        Self::Module {
            name: name.into(),
            instance: instance.into(),
            scope: Some(scope),
            call_path: Box::new(call_path),
        }
    }

    pub fn module_call_path(&self) -> Option<&composition::CompositionCallPath> {
        match self {
            Self::Module { call_path, .. } => Some(call_path),
            Self::Root | Self::SavedModule { .. } => None,
        }
    }

    pub(crate) fn identity_scope(&self) -> Option<ModuleInstanceScope> {
        match self {
            Self::Root => None,
            Self::Module {
                instance, scope, ..
            } => Some(
                scope
                    .clone()
                    .unwrap_or_else(|| ModuleInstanceScope::bound(instance)),
            ),
            Self::SavedModule { instance, .. } => Some(ModuleInstanceScope::bound(instance)),
        }
    }

    /// User-facing module name and instance for either live or saved sources.
    pub fn module_display(&self) -> Option<(&str, &str)> {
        match self {
            Self::Root => None,
            Self::Module { name, instance, .. } | Self::SavedModule { name, instance } => {
                Some((name, instance))
            }
        }
    }

    /// Check if this is the root source
    pub fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }

    /// System identity of the outer module instance that owns this leaf.
    ///
    /// Expanded nested leaves retain their outermost instance in `instance`;
    /// the deeper ancestry remains in the private validation scope.
    pub fn outer_instance_identity(&self) -> Option<ResourceIdentity> {
        match self {
            Self::Root => None,
            Self::Module { instance, .. } | Self::SavedModule { instance, .. } => {
                ResourceIdentity::try_from(instance.clone()).ok()
            }
        }
    }
}

/// The sorted tag keys injected by a provider's `default_tags` configuration.
///
/// The inner set is intentionally private, so other crates cannot add individual
/// keys. Non-empty provenance is produced by `carina-core`'s default-tag merge
/// or decoded from a saved plan. This is display-only provenance: the merge
/// never deletes or overwrites tags based on it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DefaultTagKeys(BTreeSet<String>);

impl DefaultTagKeys {
    /// Returns whether `key` was injected from provider `default_tags`.
    pub fn contains(&self, key: &str) -> bool {
        self.0.contains(key)
    }

    /// Returns whether no tag keys were injected from provider `default_tags`.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn insert(&mut self, key: String) {
        self.0.insert(key);
    }
}

/// A managed infrastructure resource declared in DSL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    /// Source-order preserving map of attribute name → expression.
    ///
    /// `IndexMap` (not `HashMap`) so iteration order matches the order
    /// the user wrote attributes in the `.crn` file. Anything that
    /// re-renders attributes — diagnostic messages, formatter output,
    /// plan display, snapshot tests — depends on this stability (#2222).
    pub attributes: IndexMap<String, Value>,
    /// `directives` meta-argument block: Carina-side instructions for
    /// how to handle this resource (force-delete, create-before-destroy,
    /// prevent-destroy).
    #[serde(default)]
    pub directives: Directives,
    /// Attribute prefixes: maps attribute name -> prefix string
    /// e.g., {"bucket_name": "my-app-"} from `bucket_name_prefix = "my-app-"`
    #[serde(default)]
    pub prefixes: HashMap<String, String>,
    /// Binding name from `let` bindings in DSL (e.g., `let vpc = ...`)
    #[serde(default)]
    pub binding: Option<String>,
    /// Binding names of resources this resource depends on (via ResourceRef).
    ///
    /// Set semantics (BTreeSet): the same binding referenced multiple times
    /// in the resource's attributes contributes a single entry, and
    /// iteration is alphabetically sorted so consumers (plan display,
    /// state files) see a deterministic order. See #2228.
    #[serde(default)]
    pub dependency_bindings: BTreeSet<String>,
    /// Module source info for resources that belong to a module
    #[serde(default)]
    pub module_source: Option<ModuleSource>,
    /// Tag keys injected by the routed provider's `default_tags` during
    /// desired-side normalization.
    ///
    /// This is host-owned provenance, not a provider attribute. It is
    /// serialized with saved plans so plan display can retain annotations
    /// after a plan round-trip, but it never enters persisted provider state.
    #[serde(default, skip_serializing_if = "DefaultTagKeys::is_empty")]
    pub default_tag_keys: DefaultTagKeys,
    /// Top-level attribute names whose value was written as a quoted
    /// string literal (`attr = "..."`) in the source `.crn`.
    ///
    /// **Why on `Resource`, not on `Value`:** the alternative is a
    /// `Value::QuotedString` variant, but that ripples through every
    /// `match` arm in the codebase. Co-locating the bit with the
    /// owning resource is enough for the only consumer that needs it
    /// (enum-attribute diagnostics — see #2094) without that blast
    /// radius. Sharing a struct with the attributes also makes the
    /// lookup rename-proof: there is no separate identifier keying
    /// the metadata, so `compute_anonymous_identifiers` can rewrite
    /// `Resource.id.identity` freely (#2229).
    ///
    /// Parse-time only; `#[serde(skip)]` keeps it out of state.
    #[serde(default, skip)]
    pub quoted_string_attrs: HashSet<String>,
}

impl Resource {
    pub fn from_id(id: ResourceId) -> Self {
        Self {
            id,
            attributes: IndexMap::new(),
            directives: Directives::default(),
            prefixes: HashMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            default_tag_keys: DefaultTagKeys::default(),
            quoted_string_attrs: HashSet::new(),
        }
    }

    pub fn pending(resource_type: impl Into<String>) -> Self {
        Self::from_id(ResourceId::pending(resource_type))
    }

    pub fn new(resource_type: impl Into<String>, identity: impl Into<ResourceIdentity>) -> Self {
        Self::from_id(ResourceId::with_identity(resource_type, identity))
    }

    /// The resource's id wrapped as a [`PersistentId`] — i.e., an id
    /// that may legally enter state-load APIs.
    ///
    /// `Resource` is a leaf node and persists in state, so its id is
    /// `PersistentId`-typed. The wrapper is created by cloning the
    /// inner `ResourceId`; for owned consumption use `PersistentId::new(r.id.clone())`.
    pub fn persistent_id(&self) -> persistent_id::PersistentId {
        persistent_id::PersistentId::new(self.id.clone())
    }

    pub fn with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
        provider_instance: Option<String>,
    ) -> Self {
        Self::from_id(ResourceId::with_provider_identity(
            provider,
            resource_type,
            identity,
            provider_instance,
        ))
    }

    pub fn pending_with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        provider_instance: Option<String>,
    ) -> Self {
        Self::from_id(ResourceId::pending_with_provider(
            provider,
            resource_type,
            provider_instance,
        ))
    }

    pub(crate) fn instantiate(&self) -> Self {
        let mut resource = self.clone();
        resource.id = self.id.instantiate_pending();
        resource
    }

    pub(crate) fn instantiate_with_identity(&self, identity: ResourceIdentity) -> Self {
        let mut resource = self.clone();
        resource.id = self.id.instantiate_with_identity(identity);
        resource
    }

    /// Returns the attributes projected to a `HashMap<String, Value>`.
    ///
    /// Lookup-only callers (validation, differ, plan display) still
    /// receive a `HashMap` — iteration over user-authored order is done
    /// directly via `self.attributes` (`IndexMap`), so flipping this
    /// helper would just force every downstream caller to widen its
    /// signature for no order benefit.
    pub fn resolved_attributes(&self) -> HashMap<String, Value> {
        attrs_to_hashmap(&self.attributes)
    }

    /// Get an attribute value by key, returning `Option<&Value>`.
    pub fn get_attr(&self, key: &str) -> Option<&Value> {
        self.attributes.get(key)
    }

    /// Get an enum attribute using its provider API spelling.
    ///
    /// `Ok(None)` is returned only when the resource attribute is absent.
    /// Schema and present-value errors are returned as [`EnumAttrError`].
    pub fn get_enum_attr<'a>(
        &'a self,
        schema: &'a ResourceSchema,
        name: &str,
    ) -> Result<Option<EnumAttr<'a>>, EnumAttrError> {
        enum_attr_at_schema_path(schema, name, &[], self.get_attr(name))
    }

    /// Get an enum field from a struct-valued resource attribute.
    ///
    /// The root schema attribute must resolve directly to a struct; a list of
    /// structs is not accepted by this accessor.
    ///
    /// `Ok(None)` is returned only when the struct attribute or requested field
    /// value is absent. Schema and present-value errors are returned as
    /// [`EnumAttrError`].
    pub fn get_enum_struct_field<'a>(
        &'a self,
        schema: &'a ResourceSchema,
        struct_attr: &str,
        field_name: &str,
    ) -> Result<Option<EnumAttr<'a>>, EnumAttrError> {
        let attr_type = &schema
            .attributes
            .get(struct_attr)
            .ok_or_else(|| EnumAttrError::SchemaAttributeNotFound {
                path: struct_attr.to_string(),
            })?
            .attr_type;
        if !matches!(schema.shape_of(attr_type), Shape::Struct { .. }) {
            return Err(EnumAttrError::NotStruct {
                path: struct_attr.to_string(),
            });
        }

        let (field_type, path) = enum_attr_type_at_schema_path(schema, struct_attr, &[field_name])?;

        let value = match self.get_attr(struct_attr) {
            None => None,
            Some(Value::Concrete(ConcreteValue::Map(map))) => map.get(field_name),
            Some(_) => {
                return Err(EnumAttrError::StructValueNotMap {
                    path: struct_attr.to_string(),
                });
            }
        };
        enum_attr_for_value(schema, field_type, value, &path)
    }

    /// Get a mutable attribute value by key, returning `Option<&mut Value>`.
    pub fn get_attr_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.attributes.get_mut(key)
    }

    /// Set an attribute value.
    pub fn set_attr(&mut self, key: impl Into<String>, value: Value) {
        self.attributes.insert(key.into(), value);
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    /// Set attributes from a `HashMap<String, Value>`.
    pub fn with_value_attributes(mut self, attrs: HashMap<String, Value>) -> Self {
        self.attributes = attrs.into_iter().collect();
        self
    }

    pub fn with_binding(mut self, binding: impl Into<String>) -> Self {
        self.binding = Some(binding.into());
        self
    }

    pub fn with_dependency_bindings(mut self, deps: BTreeSet<String>) -> Self {
        self.dependency_bindings = deps;
        self
    }

    pub fn with_module_source(mut self, source: ModuleSource) -> Self {
        self.module_source = Some(source);
        self
    }
}

#[cfg(test)]
mod enum_attr_tests {
    use super::*;
    use crate::schema::{AttributeSchema, AttributeType, StructField, enum_identity};

    fn status_enum_type() -> AttributeType {
        AttributeType::enum_(
            enum_identity("Status", Some("aws.s3.BucketVersioning")),
            Some(vec!["Enabled".to_string(), "Suspended".to_string()]),
            vec![],
            None,
            None,
        )
    }

    fn status_identity() -> TypeIdentity {
        enum_identity("Status", Some("aws.s3.BucketVersioning"))
    }

    fn open_status_enum_type() -> AttributeType {
        AttributeType::enum_(status_identity(), None, vec![], None, None)
    }

    fn ip_protocol_enum_type() -> AttributeType {
        AttributeType::enum_(
            enum_identity("IpProtocol", Some("aws.ec2.SecurityGroupRule")),
            Some(
                ["tcp", "udp", "icmp", "icmpv6", "-1"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            ),
            vec![("-1".to_string(), "all".to_string())],
            None,
            None,
        )
    }

    fn mfa_delete_status_identity() -> TypeIdentity {
        enum_identity("Status", Some("aws.s3.MfaDelete"))
    }

    fn schema_with_status() -> ResourceSchema {
        ResourceSchema::new("aws.s3.BucketVersioning")
            .attribute(AttributeSchema::optional("status", status_enum_type()).build())
    }

    fn options_struct_type(fields: Vec<StructField>) -> AttributeType {
        AttributeType::struct_("Options", fields)
    }

    fn schema_with_options(fields: Vec<StructField>) -> ResourceSchema {
        ResourceSchema::new("aws.s3.BucketVersioning")
            .attribute(AttributeSchema::optional("options", options_struct_type(fields)).build())
    }

    fn schema_with_rule_statements(
        statements_type: AttributeType,
        protocol_type: AttributeType,
    ) -> ResourceSchema {
        ResourceSchema::new("aws.ec2.SecurityGroup")
            .with_def(
                "Rule",
                AttributeType::struct_(
                    "Rule",
                    vec![StructField::optional("statements", statements_type).build()],
                ),
            )
            .with_def(
                "Statement",
                AttributeType::struct_(
                    "Statement",
                    vec![StructField::optional("protocol", protocol_type).build()],
                ),
            )
            .attribute(
                AttributeSchema::optional(
                    "rules",
                    AttributeType::list(AttributeType::ref_("Rule")),
                )
                .build(),
            )
    }

    fn schema_with_nested_rule_protocol(protocol_type: AttributeType) -> ResourceSchema {
        schema_with_rule_statements(
            AttributeType::unordered_list(AttributeType::ref_("Statement")),
            protocol_type,
        )
    }

    fn resource_with_attr(name: &str, value: ConcreteValue) -> Resource {
        Resource::with_provider("aws", "s3.BucketVersioning", "example", None)
            .with_attribute(name, Value::Concrete(value))
    }

    fn resource_with_options(fields: Vec<(&str, ConcreteValue)>) -> Resource {
        let map = fields
            .into_iter()
            .map(|(name, value)| (name.to_string(), Value::Concrete(value)))
            .collect();
        resource_with_attr("options", ConcreteValue::Map(map))
    }

    fn canonical_status(api_value: &str) -> ConcreteValue {
        ConcreteValue::CanonicalEnum(CanonicalEnumValue::new_for_test(
            status_identity(),
            api_value,
        ))
    }

    fn assert_struct_field_enabled(resource: &Resource, schema: &ResourceSchema) {
        let attr = resource
            .get_enum_struct_field(schema, "options", "dns_support")
            .expect("valid enum field")
            .expect("present enum field");

        assert_eq!(attr.api_value(), "Enabled");
        assert_eq!(attr.identity(), &status_identity());
    }

    #[test]
    fn attribute_absent_is_ok_none() {
        let schema = schema_with_status();
        let resource = Resource::with_provider("aws", "s3.BucketVersioning", "example", None);

        assert!(matches!(
            resource.get_enum_attr(&schema, "status"),
            Ok(None)
        ));
    }

    #[test]
    fn missing_schema_attribute_is_an_error() {
        let schema = schema_with_status();
        let resource = Resource::with_provider("aws", "s3.BucketVersioning", "example", None);

        assert!(matches!(
            resource.get_enum_attr(&schema, "missing"),
            Err(EnumAttrError::SchemaAttributeNotFound { path }) if path == "missing"
        ));
    }

    #[test]
    fn non_enum_attribute_is_an_error() {
        let schema = ResourceSchema::new("aws.s3.BucketVersioning")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build());
        let resource = resource_with_attr("name", ConcreteValue::String("x".into()));

        assert!(matches!(
            resource.get_enum_attr(&schema, "name"),
            Err(EnumAttrError::NotEnum { path }) if path == "name"
        ));
    }

    #[test]
    fn matching_canonical_enum_is_returned() {
        let schema = schema_with_status();
        let resource = resource_with_attr("status", canonical_status("Enabled"));

        let attr = resource
            .get_enum_attr(&schema, "status")
            .expect("valid enum")
            .expect("present enum");

        assert_eq!(attr.api_value(), "Enabled");
        assert_eq!(attr.identity(), &status_identity());
    }

    #[test]
    fn matching_canonical_enum_with_non_member_api_value_is_an_error() {
        let schema = schema_with_status();
        let resource = resource_with_attr("status", canonical_status("Retired"));

        assert!(matches!(
            resource.get_enum_attr(&schema, "status"),
            Err(EnumAttrError::NotAMember { path, .. }) if path == "status"
        ));
    }

    #[test]
    fn enum_identifier_is_an_error() {
        let schema = schema_with_status();
        let resource = resource_with_attr(
            "status",
            ConcreteValue::enum_identifier("aws.s3.BucketVersioning.Status.enabled"),
        );

        assert!(matches!(
            resource.get_enum_attr(&schema, "status"),
            Err(EnumAttrError::UnexpectedShape { path, .. }) if path == "status"
        ));
    }

    #[test]
    fn exact_closed_enum_string_is_returned_as_is() {
        let schema = schema_with_status();
        let resource = resource_with_attr("status", ConcreteValue::String("Enabled".into()));

        let attr = resource
            .get_enum_attr(&schema, "status")
            .expect("valid enum")
            .expect("present enum");

        assert_eq!(attr.api_value(), "Enabled");
        assert_eq!(attr.identity(), &status_identity());
    }

    #[test]
    fn folded_closed_enum_string_is_an_error() {
        let schema = schema_with_status();
        let resource = resource_with_attr("status", ConcreteValue::String("enabled".into()));

        assert!(matches!(
            resource.get_enum_attr(&schema, "status"),
            Err(EnumAttrError::NotAMember { path, .. }) if path == "status"
        ));
    }

    #[test]
    fn dsl_alias_is_not_accepted_as_a_string_api_value() {
        let schema = ResourceSchema::new("aws.ec2.SecurityGroupRule")
            .attribute(AttributeSchema::optional("protocol", ip_protocol_enum_type()).build());
        let alias = Value::Concrete(ConcreteValue::String("all".into()));
        let api_value = Value::Concrete(ConcreteValue::String("-1".into()));

        assert!(matches!(
            enum_attr_at_schema_path(&schema, "protocol", &[], Some(&alias)),
            Err(EnumAttrError::NotAMember { path, .. }) if path == "protocol"
        ));

        let attr = enum_attr_at_schema_path(&schema, "protocol", &[], Some(&api_value))
            .expect("API spelling is valid")
            .expect("protocol is present");
        assert_eq!(attr.api_value(), "-1");
    }

    #[test]
    fn nested_list_struct_schema_path_returns_valid_member() {
        let schema = schema_with_nested_rule_protocol(ip_protocol_enum_type());
        let value = Value::Concrete(ConcreteValue::String("tcp".into()));

        let attr =
            enum_attr_at_schema_path(&schema, "rules", &["statements", "protocol"], Some(&value))
                .expect("valid nested enum")
                .expect("nested enum is present");

        assert_eq!(attr.api_value(), "tcp");
    }

    #[test]
    fn nested_list_struct_schema_path_returns_matching_canonical_enum() {
        let schema = schema_with_nested_rule_protocol(ip_protocol_enum_type());
        let value = Value::Concrete(ConcreteValue::CanonicalEnum(
            CanonicalEnumValue::new_for_test(
                enum_identity("IpProtocol", Some("aws.ec2.SecurityGroupRule")),
                "tcp",
            ),
        ));

        let attr =
            enum_attr_at_schema_path(&schema, "rules", &["statements", "protocol"], Some(&value))
                .expect("valid nested enum")
                .expect("nested enum is present");

        assert_eq!(attr.api_value(), "tcp");
        assert_eq!(
            attr.identity(),
            &enum_identity("IpProtocol", Some("aws.ec2.SecurityGroupRule"))
        );
    }

    #[test]
    fn nested_list_struct_schema_path_rejects_non_member() {
        let schema = schema_with_nested_rule_protocol(ip_protocol_enum_type());
        let value = Value::Concrete(ConcreteValue::String("gre".into()));

        assert!(matches!(
            enum_attr_at_schema_path(
                &schema,
                "rules",
                &["statements", "protocol"],
                Some(&value),
            ),
            Err(EnumAttrError::NotAMember { path, .. })
                if path == "rules.statements.protocol"
        ));
    }

    #[test]
    fn nested_list_struct_schema_path_reports_missing_segment() {
        let schema = schema_with_nested_rule_protocol(ip_protocol_enum_type());
        let value = Value::Concrete(ConcreteValue::String("tcp".into()));

        assert!(matches!(
            enum_attr_at_schema_path(
                &schema,
                "rules",
                &["statements", "missing"],
                Some(&value),
            ),
            Err(EnumAttrError::SchemaFieldNotFound { path })
                if path == "rules.statements.missing"
        ));
    }

    #[test]
    fn nested_list_struct_schema_path_reports_non_struct_segment() {
        let schema = schema_with_nested_rule_protocol(ip_protocol_enum_type());

        assert!(matches!(
            enum_attr_at_schema_path(
                &schema,
                "rules",
                &["statements", "protocol", "member"],
                None,
            ),
            Err(EnumAttrError::NotStruct { path })
                if path == "rules.statements.protocol"
        ));
    }

    #[test]
    fn nested_map_struct_schema_path_is_not_peeled() {
        let schema = schema_with_rule_statements(
            AttributeType::map(AttributeType::ref_("Statement")),
            ip_protocol_enum_type(),
        );

        assert!(matches!(
            enum_attr_at_schema_path(
                &schema,
                "rules",
                &["statements", "protocol"],
                None,
            ),
            Err(EnumAttrError::NotStruct { path }) if path == "rules.statements"
        ));
    }

    #[test]
    fn nested_union_struct_schema_path_is_not_peeled() {
        let schema = schema_with_rule_statements(
            AttributeType::union(vec![
                AttributeType::ref_("Statement"),
                AttributeType::string(),
            ]),
            ip_protocol_enum_type(),
        );

        assert!(matches!(
            enum_attr_at_schema_path(
                &schema,
                "rules",
                &["statements", "protocol"],
                None,
            ),
            Err(EnumAttrError::NotStruct { path }) if path == "rules.statements"
        ));
    }

    #[test]
    fn self_referential_list_schema_path_exhausts_the_walk_budget() {
        let schema = ResourceSchema::new("aws.ec2.SecurityGroup")
            .with_def(
                "Recursive",
                AttributeType::list(AttributeType::ref_("Recursive")),
            )
            .attribute(
                AttributeSchema::optional("rules", AttributeType::ref_("Recursive")).build(),
            );

        assert!(matches!(
            enum_attr_at_schema_path(&schema, "rules", &["protocol"], None),
            Err(EnumAttrError::SchemaTooDeep { path }) if path == "rules"
        ));
    }

    #[test]
    fn nested_list_struct_schema_path_rejects_non_enum_leaf() {
        let schema = schema_with_nested_rule_protocol(AttributeType::string());
        let value = Value::Concrete(ConcreteValue::String("tcp".into()));

        assert!(matches!(
            enum_attr_at_schema_path(
                &schema,
                "rules",
                &["statements", "protocol"],
                Some(&value),
            ),
            Err(EnumAttrError::NotEnum { path }) if path == "rules.statements.protocol"
        ));
    }

    #[test]
    fn nested_list_enum_leaf_is_not_peeled() {
        let schema = schema_with_nested_rule_protocol(AttributeType::list(ip_protocol_enum_type()));

        assert!(matches!(
            enum_attr_at_schema_path(&schema, "rules", &["statements", "protocol"], None,),
            Err(EnumAttrError::NotEnum { path }) if path == "rules.statements.protocol"
        ));
    }

    #[test]
    fn nested_list_struct_schema_path_absent_value_is_ok_none() {
        let schema = schema_with_nested_rule_protocol(ip_protocol_enum_type());

        assert!(matches!(
            enum_attr_at_schema_path(&schema, "rules", &["statements", "protocol"], None,),
            Ok(None)
        ));
    }

    #[test]
    fn open_enum_string_is_returned_verbatim() {
        let schema = ResourceSchema::new("aws.s3.BucketVersioning")
            .attribute(AttributeSchema::optional("status", open_status_enum_type()).build());
        let resource = resource_with_attr(
            "status",
            ConcreteValue::String("vendor_VALUE-with-MixedCase".into()),
        );

        let attr = resource
            .get_enum_attr(&schema, "status")
            .expect("valid enum")
            .expect("present enum");

        assert_eq!(attr.api_value(), "vendor_VALUE-with-MixedCase");
        assert_eq!(attr.identity(), &status_identity());
    }

    #[test]
    fn mismatched_canonical_enum_identity_is_an_error() {
        let schema = schema_with_status();
        let resource = resource_with_attr(
            "status",
            ConcreteValue::CanonicalEnum(CanonicalEnumValue::new_for_test(
                mfa_delete_status_identity(),
                "Enabled",
            )),
        );

        assert!(matches!(
            resource.get_enum_attr(&schema, "status"),
            Err(EnumAttrError::EnumIdentityMismatch { path, .. }) if path == "status"
        ));
    }

    #[test]
    fn non_enum_value_shape_is_an_error() {
        let schema = schema_with_status();
        let resource = resource_with_attr("status", ConcreteValue::Int(1));

        assert!(matches!(
            resource.get_enum_attr(&schema, "status"),
            Err(EnumAttrError::UnexpectedShape { path, .. }) if path == "status"
        ));
    }

    #[test]
    fn invalid_sensitive_string_is_redacted_in_error_display() {
        let schema = schema_with_status();
        let secret = "enum-secret-that-must-not-leak";
        let resource = resource_with_attr(
            "status",
            ConcreteValue::String(ValueString::diagnostic_sensitive(secret)),
        );

        let error = match resource.get_enum_attr(&schema, "status") {
            Err(error) => error,
            Ok(_) => panic!("invalid closed enum member must fail"),
        };

        assert!(matches!(&error, EnumAttrError::NotAMember { .. }));
        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn invalid_public_string_is_in_error_display() {
        let schema = schema_with_status();
        let invalid = "Retired";
        let resource = resource_with_attr("status", ConcreteValue::String(invalid.into()));

        let error = match resource.get_enum_attr(&schema, "status") {
            Err(error) => error,
            Ok(_) => panic!("invalid closed enum member must fail"),
        };

        assert!(matches!(&error, EnumAttrError::NotAMember { .. }));
        assert!(error.to_string().contains(invalid));
    }

    #[test]
    fn ref_typed_attribute_is_resolved_through_defs() {
        let schema = ResourceSchema::new("aws.s3.BucketVersioning")
            .with_def("VersioningStatus", status_enum_type())
            .attribute(
                AttributeSchema::optional("status", AttributeType::ref_("VersioningStatus"))
                    .build(),
            );
        let cases = [
            canonical_status("Enabled"),
            ConcreteValue::String("Enabled".into()),
        ];

        for value in cases {
            let resource = resource_with_attr("status", value);

            let attr = resource
                .get_enum_attr(&schema, "status")
                .expect("valid enum")
                .expect("present enum");

            assert_eq!(attr.api_value(), "Enabled");
            assert_eq!(attr.identity(), &status_identity());
        }
    }

    #[test]
    fn struct_attribute_absent_is_ok_none() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", status_enum_type()).build(),
        ]);
        let resource = Resource::with_provider("aws", "s3.BucketVersioning", "example", None);

        assert!(matches!(
            resource.get_enum_struct_field(&schema, "options", "dns_support"),
            Ok(None)
        ));
    }

    #[test]
    fn struct_value_that_is_not_a_map_is_an_error() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", status_enum_type()).build(),
        ]);
        let resource = resource_with_attr("options", ConcreteValue::String("Enabled".into()));

        assert!(matches!(
            resource
                .get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::StructValueNotMap { path }) if path == "options"
        ));
    }

    #[test]
    fn missing_struct_schema_field_is_an_error() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_hostnames", status_enum_type()).build(),
        ]);
        let resource = resource_with_options(vec![("dns_support", canonical_status("Enabled"))]);

        assert!(matches!(
            resource
                .get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::SchemaFieldNotFound { path }) if path == "options.dns_support"
        ));
    }

    #[test]
    fn non_struct_schema_attribute_is_an_error() {
        let schema = ResourceSchema::new("aws.s3.BucketVersioning")
            .attribute(AttributeSchema::optional("options", AttributeType::string()).build());
        let resource = resource_with_attr("options", ConcreteValue::String("Enabled".into()));

        assert!(matches!(
            resource.get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::NotStruct { path }) if path == "options"
        ));
    }

    #[test]
    fn list_struct_schema_is_not_a_struct_field_accessor_when_value_is_absent() {
        let schema = ResourceSchema::new("aws.s3.BucketVersioning").attribute(
            AttributeSchema::optional(
                "options",
                AttributeType::list(options_struct_type(vec![
                    StructField::optional("dns_support", status_enum_type()).build(),
                ])),
            )
            .build(),
        );
        let resource = Resource::with_provider("aws", "s3.BucketVersioning", "example", None);

        assert!(matches!(
            resource.get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::NotStruct { path }) if path == "options"
        ));
    }

    #[test]
    fn list_struct_schema_is_not_a_struct_field_accessor_when_value_is_present() {
        let schema = ResourceSchema::new("aws.s3.BucketVersioning").attribute(
            AttributeSchema::optional(
                "options",
                AttributeType::list(options_struct_type(vec![
                    StructField::optional("dns_support", status_enum_type()).build(),
                ])),
            )
            .build(),
        );
        let resource = resource_with_attr("options", ConcreteValue::List(Vec::new()));

        assert!(matches!(
            resource.get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::NotStruct { path }) if path == "options"
        ));
    }

    #[test]
    fn non_enum_struct_field_is_an_error() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", AttributeType::string()).build(),
        ]);
        let resource = resource_with_options(vec![(
            "dns_support",
            ConcreteValue::String("Enabled".into()),
        )]);

        assert!(matches!(
            resource
                .get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::NotEnum { path }) if path == "options.dns_support"
        ));
    }

    #[test]
    fn absent_struct_attribute_does_not_hide_non_enum_field() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", AttributeType::string()).build(),
        ]);
        let resource = Resource::with_provider("aws", "s3.BucketVersioning", "example", None);

        assert!(matches!(
            resource.get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::NotEnum { path }) if path == "options.dns_support"
        ));
    }

    #[test]
    fn struct_field_absent_from_map_is_ok_none() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", status_enum_type()).build(),
        ]);
        let resource = resource_with_options(vec![("dns_hostnames", canonical_status("Enabled"))]);

        assert!(matches!(
            resource.get_enum_struct_field(&schema, "options", "dns_support"),
            Ok(None)
        ));
    }

    #[test]
    fn struct_field_accepts_matching_canonical_enum() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", status_enum_type()).build(),
        ]);
        let resource = resource_with_options(vec![("dns_support", canonical_status("Enabled"))]);

        assert_struct_field_enabled(&resource, &schema);
    }

    #[test]
    fn struct_field_rejects_enum_identifier() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", status_enum_type()).build(),
        ]);
        let resource = resource_with_options(vec![(
            "dns_support",
            ConcreteValue::enum_identifier("aws.s3.BucketVersioning.Status.enabled"),
        )]);

        assert!(matches!(
            resource.get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::UnexpectedShape { path, .. }) if path == "options.dns_support"
        ));
    }

    #[test]
    fn struct_field_accepts_exact_closed_enum_string() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", status_enum_type()).build(),
        ]);
        let resource = resource_with_options(vec![(
            "dns_support",
            ConcreteValue::String("Enabled".into()),
        )]);

        assert_struct_field_enabled(&resource, &schema);
    }

    #[test]
    fn struct_field_rejects_folded_closed_enum_string() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", status_enum_type()).build(),
        ]);
        let resource = resource_with_options(vec![(
            "dns_support",
            ConcreteValue::String("enabled".into()),
        )]);

        assert!(matches!(
            resource.get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::NotAMember { path, .. }) if path == "options.dns_support"
        ));
    }

    #[test]
    fn struct_field_rejects_mismatched_canonical_enum_identity() {
        let schema = schema_with_options(vec![
            StructField::optional("dns_support", status_enum_type()).build(),
        ]);
        let resource = resource_with_options(vec![(
            "dns_support",
            ConcreteValue::CanonicalEnum(CanonicalEnumValue::new_for_test(
                mfa_delete_status_identity(),
                "Enabled",
            )),
        )]);

        assert!(matches!(
            resource.get_enum_struct_field(&schema, "options", "dns_support"),
            Err(EnumAttrError::EnumIdentityMismatch { path, .. })
                if path == "options.dns_support"
        ));
    }

    #[test]
    fn struct_field_resolves_ref_typed_struct_through_defs() {
        let schema = ResourceSchema::new("aws.s3.BucketVersioning")
            .with_def(
                "Options",
                options_struct_type(vec![
                    StructField::optional("dns_support", status_enum_type()).build(),
                ]),
            )
            .attribute(
                AttributeSchema::optional("options", AttributeType::ref_("Options")).build(),
            );
        let cases = [
            canonical_status("Enabled"),
            ConcreteValue::String("Enabled".into()),
        ];

        for value in cases {
            let resource = resource_with_options(vec![("dns_support", value)]);

            assert_struct_field_enabled(&resource, &schema);
        }
    }

    #[test]
    fn struct_field_resolves_ref_typed_field_through_defs() {
        let schema = ResourceSchema::new("aws.s3.BucketVersioning")
            .with_def("VersioningStatus", status_enum_type())
            .attribute(
                AttributeSchema::optional(
                    "options",
                    options_struct_type(vec![
                        StructField::optional(
                            "dns_support",
                            AttributeType::ref_("VersioningStatus"),
                        )
                        .build(),
                    ]),
                )
                .build(),
            );
        let cases = [
            canonical_status("Enabled"),
            ConcreteValue::String("Enabled".into()),
        ];

        for value in cases {
            let resource = resource_with_options(vec![("dns_support", value)]);

            assert_struct_field_enabled(&resource, &schema);
        }
    }
}

/// Check that a resource's attribute tree is free of any
/// [`Value::Deferred`] placeholder before provider dispatch.
pub(crate) fn assert_resource_fully_resolved(
    resource: &Resource,
) -> Result<(), crate::value::SerializationError> {
    for value in resource.attributes.values() {
        assert_value_fully_resolved(value)?;
    }
    Ok(())
}

pub(crate) fn assert_value_fully_resolved(
    value: &Value,
) -> Result<(), crate::value::SerializationError> {
    let context = crate::value::SerializationContext::WasmBoundary;
    match value {
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                assert_value_fully_resolved(item)?;
            }
            Ok(())
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for v in map.values() {
                assert_value_fully_resolved(v)?;
            }
            Ok(())
        }
        Value::Deferred(DeferredValue::Secret(inner)) => assert_value_fully_resolved(inner),
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => {
            Err(crate::value::SerializationError::RedactedSecretNotAllowed { context })
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            Err(crate::value::SerializationError::UnresolvedResourceRef {
                path: path.to_dot_string(),
                context,
            })
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            Err(crate::value::SerializationError::UnresolvedResourceRef {
                path: binding.clone(),
                context,
            })
        }
        Value::Deferred(DeferredValue::Coalesce { .. }) => {
            Err(crate::value::SerializationError::UnresolvedCoalesce { context })
        }
        Value::Deferred(DeferredValue::Interpolation(_)) => {
            Err(crate::value::SerializationError::UnresolvedInterpolation { context })
        }
        Value::Deferred(
            DeferredValue::FunctionCall { name, .. } | DeferredValue::Unavailable { name, .. },
        ) => Err(crate::value::SerializationError::UnresolvedFunctionCall {
            name: name.to_string(),
            context,
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) => {
            Err(crate::value::SerializationError::UnresolvedFunctionCall {
                name: function.name.clone(),
                context,
            })
        }
        Value::Deferred(DeferredValue::Access(_)) => {
            Err(crate::value::SerializationError::UnresolvedAccess {
                expression: crate::value::format_value(value),
                context,
            })
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            Err(crate::value::SerializationError::UnknownNotAllowed {
                reason: reason.clone(),
                context,
            })
        }
        Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Concrete(ConcreteValue::Int(_))
        | Value::Concrete(ConcreteValue::Float(_))
        | Value::Concrete(ConcreteValue::Bool(_))
        | Value::Concrete(ConcreteValue::Duration(_))
        | Value::Concrete(ConcreteValue::StringList(_)) => Ok(()),
    }
}

/// Marker for a state produced by a partial-success create.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartialReadMarker {
    pub detail: String,
    pub missing_attributes: BTreeSet<String>,
}

/// A loaded managed resource that is known to exist and has a validated
/// provider identifier.
///
/// The fields stay private so production callers cannot pair one state's
/// attributes with another resource's identifier. [`Current::from_state`] is
/// the only production path that can mint this type.
#[derive(Debug, Clone, PartialEq)]
pub struct PresentResourceState {
    state: State,
    identifier: AppliedIdentifier,
}

impl PresentResourceState {
    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn identifier(&self) -> &AppliedIdentifier {
        &self.identifier
    }

    pub fn into_parts(self) -> (State, AppliedIdentifier) {
        (self.state, self.identifier)
    }
}

/// A loaded managed resource that is known not to exist.
///
/// The field is private so callers outside `carina-core` must classify raw
/// provider state through [`Current::from_state`].
#[derive(Debug, Clone, PartialEq)]
pub struct AbsentState(State);

impl AbsentState {
    pub fn state(&self) -> &State {
        &self.0
    }

    pub fn into_state(self) -> State {
        self.0
    }
}

/// A loaded managed resource that exists but has no valid provider identifier.
///
/// The field is private so callers outside `carina-core` must classify raw
/// provider state through [`Current::from_state`].
#[derive(Debug, Clone, PartialEq)]
pub struct UnidentifiedState(State);

impl UnidentifiedState {
    pub fn state(&self) -> &State {
        &self.0
    }

    pub fn into_state(self) -> State {
        self.0
    }
}

/// Typed current-instance state for a managed resource.
///
/// Callers must match every variant instead of flattening this type. That
/// keeps a future lifecycle variant compile-visible at every classification
/// boundary.
///
/// ```compile_fail
/// use carina_core::resource::{Current, State};
///
/// fn flatten(current: Current) -> State {
///     current.into_state()
/// }
/// ```
///
/// ```compile_fail
/// use carina_core::resource::{Current, State};
///
/// fn flatten(current: &Current) -> &State {
///     current.state()
/// }
/// ```
///
/// Lifecycle variants cannot be constructed from raw state outside
/// `carina-core`:
///
/// ```compile_fail
/// use carina_core::resource::{Current, ResourceId, State};
///
/// let id = ResourceId::with_identity("test.Resource", "example");
/// let current = Current::Absent(State::not_found(id));
/// ```
///
/// ```compile_fail
/// use carina_core::resource::{Current, ResourceId, State};
///
/// let id = ResourceId::with_identity("test.Resource", "example");
/// let state = State::existing(id, Default::default());
/// let current = Current::MissingIdentifier(state);
/// ```
///
/// Resource routes cannot be reassigned directly outside the checked move
/// seam:
///
/// ```compile_fail
/// use carina_core::effect::MoveAuthority;
/// use carina_core::resource::Current;
/// fn bypass_resume_route(mut current: Current, authority: &MoveAuthority<'_>) {
///     current.rebind_for_move(authority);
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Current {
    Absent(AbsentState),
    Present(PresentResourceState),
    CreateInFlight(InFlightCreate),
    MissingIdentifier(UnidentifiedState),
}

/// Mutable projection of a [`Current`] variant that carries provider state.
pub struct StateBearingCurrentMut<'a> {
    state: &'a mut State,
}

impl StateBearingCurrentMut<'_> {
    pub fn attributes_mut(&mut self) -> &mut HashMap<String, Value> {
        &mut self.state.attributes
    }

    pub fn apply_transformed_state(self, transformed: State) {
        let State {
            id: _id,
            identifier: _identifier,
            attributes,
            exists: _exists,
            dependency_bindings,
            partial_read,
        } = transformed;
        self.state.attributes = attributes;
        self.state.dependency_bindings = dependency_bindings;
        self.state.partial_read = partial_read;
    }
}

impl Current {
    pub fn absent(id: ResourceId) -> Self {
        Self::Absent(AbsentState(State::not_found(id)))
    }

    pub fn from_state(state: State) -> Self {
        if !state.exists {
            return Self::Absent(AbsentState(state));
        }
        let Some(identifier) = state
            .identifier
            .as_deref()
            .and_then(|identifier| AppliedIdentifier::new(identifier).ok())
        else {
            return Self::MissingIdentifier(UnidentifiedState(state));
        };
        Self::Present(PresentResourceState { state, identifier })
    }

    pub fn create_in_flight(pending: InFlightCreate) -> Self {
        Self::CreateInFlight(pending)
    }

    pub fn id(&self) -> &ResourceId {
        match self {
            Self::Absent(state) => &state.0.id,
            Self::Present(present) => &present.state.id,
            Self::CreateInFlight(pending) => pending.id(),
            Self::MissingIdentifier(state) => &state.0.id,
        }
    }

    /// Narrow to variants that carry provider-observed state.
    ///
    /// ```compile_fail
    /// use carina_core::resource::{Current, State};
    ///
    /// fn overwrite(current: &mut Current, transformed: State) {
    ///     current.apply_transformed_state(transformed);
    /// }
    /// ```
    pub fn state_bearing_mut(&mut self) -> Option<StateBearingCurrentMut<'_>> {
        match self {
            Self::Absent(state) => Some(StateBearingCurrentMut {
                state: &mut state.0,
            }),
            Self::Present(present) => Some(StateBearingCurrentMut {
                state: &mut present.state,
            }),
            Self::CreateInFlight(_pending) => None,
            Self::MissingIdentifier(state) => Some(StateBearingCurrentMut {
                state: &mut state.0,
            }),
        }
    }

    pub fn attributes_mut(&mut self) -> Option<&mut HashMap<String, Value>> {
        self.state_bearing_mut()
            .map(|projection| &mut projection.state.attributes)
    }

    pub(crate) fn rebind_for_move(&mut self, authority: &MoveAuthority<'_>) {
        let to = authority.to();
        match self {
            Self::Absent(state) => state.0.id = to.clone(),
            Self::Present(present) => present.state.id = to.clone(),
            Self::CreateInFlight(pending) => {
                *pending = pending
                    .clone()
                    .rebind_for_move(authority.destination_address());
            }
            Self::MissingIdentifier(state) => state.0.id = to.clone(),
        }
    }

    pub fn set_dependency_bindings(&mut self, dependency_bindings: BTreeSet<String>) {
        match self {
            Self::Absent(state) => state.0.dependency_bindings = dependency_bindings,
            Self::Present(present) => {
                present.state.dependency_bindings = dependency_bindings;
            }
            Self::CreateInFlight(_pending) => {}
            Self::MissingIdentifier(state) => {
                state.0.dependency_bindings = dependency_bindings;
            }
        }
    }

    fn into_plan_input(mut self) -> PlanInputCurrent {
        match &mut self {
            Self::Absent(state) => state.0.materialize_partial_read_unknowns(),
            Self::Present(present) => present.state.materialize_partial_read_unknowns(),
            Self::CreateInFlight(_pending) => {}
            Self::MissingIdentifier(state) => state.0.materialize_partial_read_unknowns(),
        }
        PlanInputCurrent(self)
    }
}

/// Construct an adversarial `Present` fixture whose raw `State::identifier`
/// may differ from the identifier already validated by `Current`.
///
/// Production code must always use [`Current::from_state`]. This helper exists
/// only so cross-crate regression tests can prove that consumers treat the
/// typed identifier as authoritative instead of re-validating the raw field.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn fixture_present_current(state: State, identifier: AppliedIdentifier) -> Current {
    Current::Present(PresentResourceState { state, identifier })
}

/// Managed current state after plan-only canonicalization and unknown
/// reconstruction.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanInputCurrent(Current);

impl PlanInputCurrent {
    pub(crate) fn absent(id: ResourceId) -> Self {
        Current::absent(id).into_plan_input()
    }

    pub fn current(&self) -> &Current {
        &self.0
    }

    pub fn into_current(self) -> Current {
        self.0
    }
}

pub fn into_plan_input_current_map(
    mut current: HashMap<ResourceId, Current>,
    schemas: &crate::schema::SchemaRegistry,
    resources: &[Resource],
) -> HashMap<ResourceId, PlanInputCurrent> {
    crate::value::canonicalize_states_with_schemas(&mut current, schemas);
    crate::utils::lift_current_state_enum_leaves(&mut current, resources, schemas);
    current
        .into_iter()
        .map(|(id, current)| (id, current.into_plan_input()))
        .collect()
}

/// Borrowing counterpart used when a caller layers configured and orphan
/// state. Each current value is cloned once into the canonicalized plan-input
/// map; the source maps and resource graph are never deep-cloned first.
pub fn plan_input_current_map_from_refs<'a>(
    current: impl IntoIterator<Item = (&'a ResourceId, &'a Current)>,
    schemas: &crate::schema::SchemaRegistry,
    resources: impl IntoIterator<Item = &'a Resource>,
) -> HashMap<ResourceId, PlanInputCurrent> {
    let mut current = current
        .into_iter()
        .map(|(id, current)| (id.clone(), current.clone()))
        .collect::<HashMap<_, _>>();
    crate::value::canonicalize_states_with_schemas(&mut current, schemas);
    for resource in resources {
        if let Some(schema) = schemas.get_for(resource)
            && let Some(state) = current.get_mut(&resource.id)
            && let Some(attributes) = state.attributes_mut()
        {
            crate::utils::lift_state_enum_leaves(attributes, schema.as_schema());
        }
    }
    current
        .into_iter()
        .map(|(id, current)| (id, current.into_plan_input()))
        .collect()
}

#[cfg(test)]
pub(crate) fn current_map_from_states(
    states: HashMap<ResourceId, State>,
) -> HashMap<ResourceId, Current> {
    states
        .into_iter()
        .map(|(id, state)| (id, Current::from_state(state)))
        .collect()
}

#[cfg(test)]
pub(crate) fn into_plan_input_current_map_from_states(
    states: HashMap<ResourceId, State>,
    registry: &crate::schema::SchemaRegistry,
    resources: &[Resource],
) -> HashMap<ResourceId, PlanInputCurrent> {
    into_plan_input_current_map(current_map_from_states(states), registry, resources)
}

/// Current state fetched from actual infrastructure
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub id: ResourceId,
    /// AWS internal identifier (e.g., vpc-xxx, subnet-xxx)
    pub identifier: Option<String>,
    pub attributes: HashMap<String, Value>,
    /// Whether this state exists
    pub exists: bool,
    /// Binding names this resource depended on when it was last applied.
    /// Used by the executor to determine delete ordering during replace operations.
    ///
    /// Set semantics (BTreeSet) — see Resource::dependency_bindings (#2228).
    pub dependency_bindings: BTreeSet<String>,
    /// Present when this state came from a partial-success create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_read: Option<PartialReadMarker>,
}

impl State {
    pub fn not_found(id: ResourceId) -> Self {
        Self {
            id,
            identifier: None,
            attributes: HashMap::new(),
            exists: false,
            dependency_bindings: BTreeSet::new(),
            partial_read: None,
        }
    }

    pub fn existing(id: ResourceId, attributes: HashMap<String, Value>) -> Self {
        Self {
            id,
            identifier: None,
            attributes,
            exists: true,
            dependency_bindings: BTreeSet::new(),
            partial_read: None,
        }
    }

    pub fn with_identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    pub fn with_dependency_bindings(mut self, deps: BTreeSet<String>) -> Self {
        self.dependency_bindings = deps;
        self
    }

    /// Reconstruct plan-time unknowns from the partial-read marker.
    pub(crate) fn materialize_partial_read_unknowns(&mut self) {
        let Some(marker) = &self.partial_read else {
            return;
        };
        for attr in &marker.missing_attributes {
            self.attributes.entry(attr.clone()).or_insert_with(|| {
                Value::Deferred(DeferredValue::Unknown(
                    UnknownReason::PostCreateReadIncomplete {
                        detail: marker.detail.clone(),
                    },
                ))
            });
        }
    }
}

#[cfg(test)]
mod tests;

pub mod composition;
pub mod data_source;
pub mod expansion_trace;
pub mod graph_node;
pub mod leaf_node;
pub mod persistent_id;

pub(crate) use composition::CompositionRuntimeRef;
pub use composition::{
    CompositionCall, CompositionCallPath, CompositionSourceIdentityError,
    CompositionSourceIdentityResolver, LiveComposition, LiveCompositionArgument,
    LiveCompositionAttribute, LiveCompositionSignature, ModuleConstraintId,
    PendingModuleConstraint, SavedComposition, SavedCompositionArgument, SavedCompositionAttribute,
    SavedCompositionOriginId, SavedCompositionSignature, SavedModuleCallId,
};
pub use data_source::{DataSource, ResolvedDataSource};
pub use expansion_trace::{CallSite, ExpansionTrace};
pub use graph_node::GraphNode;
pub use leaf_node::{CompositionNotALeaf, LeafNode, expand_to_leaves};
pub use persistent_id::{EphemeralId, NodeId, PersistentId};
