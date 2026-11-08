//! Provider context for provider-injected validators and decryptor
//!
//! `ProviderContext` allows CLI/providers to inject custom type validators
//! and a decryptor function into the parser without using global mutable state.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::resource::Value;
use crate::schema::{
    CompiledAttributeType as AttributeType, RefinedScalarKind, SchemaRegistry, TypeIdentity,
};

/// Schema provenance for an identified refined type.
///
/// Provider ownership is deliberately independent from
/// [`TypeIdentity::provider`]: AWSCC schemas can use identities in the shared
/// `aws` namespace while the AWSCC factory still owns validation dispatch.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum SchemaTypeOwner {
    Provider(String),
}

/// A set of schema owners that is non-empty by construction and decoding.
///
/// Authored type names describe a semantic [`TypeIdentity`], not a factory.
/// When several loaded factories declare that identity, an annotation must
/// retain every owner so validation can apply their constraints conjunctively.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct NonEmptySchemaTypeOwners(BTreeSet<SchemaTypeOwner>);

impl NonEmptySchemaTypeOwners {
    pub fn singleton(owner: SchemaTypeOwner) -> Self {
        Self(BTreeSet::from([owner]))
    }

    pub fn try_from_iter(owners: impl IntoIterator<Item = SchemaTypeOwner>) -> Option<Self> {
        let owners = owners.into_iter().collect::<BTreeSet<_>>();
        (!owners.is_empty()).then_some(Self(owners))
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &SchemaTypeOwner> {
        self.0.iter()
    }

    pub fn is_singleton(&self) -> bool {
        self.0.len() == 1
    }

    pub fn contains(&self, owner: &SchemaTypeOwner) -> bool {
        self.0.contains(owner)
    }

    pub fn union(&self, other: &Self) -> Self {
        Self(self.0.union(&other.0).cloned().collect())
    }
}

impl<'de> serde::Deserialize<'de> for NonEmptySchemaTypeOwners {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let owners = BTreeSet::<SchemaTypeOwner>::deserialize(deserializer)?;
        Self::try_from_iter(owners)
            .ok_or_else(|| serde::de::Error::custom("schema type owner set cannot be empty"))
    }
}

/// A catalog routing key. This alone cannot be lifted as an annotation type.
///
/// Production construction derives the scalar kind from an admitted schema
/// declaration. Merging catalog owners retains that same declaration evidence.
/// Resolved annotation evidence is carried separately by [`ResolvedSchemaType`].
///
/// ```compile_fail
/// use carina_core::parser::OwnedTypeIdentity;
/// use carina_core::schema::{RefinedScalarKind, TypeIdentity};
/// let _ = OwnedTypeIdentity::provider("test", TypeIdentity::bare("Count"), RefinedScalarKind::String);
/// ```
///
/// ```compile_fail
/// use carina_core::parser::{NonEmptySchemaTypeOwners, OwnedTypeIdentity, SchemaTypeOwner};
/// use carina_core::schema::{RefinedScalarKind, TypeIdentity};
/// let owners = NonEmptySchemaTypeOwners::singleton(SchemaTypeOwner::Provider("test".into()));
/// let _ = OwnedTypeIdentity::with_owners(TypeIdentity::bare("Count"), owners, RefinedScalarKind::String);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnedTypeIdentity {
    owners: NonEmptySchemaTypeOwners,
    declaration: IdentifiedScalarDeclaration,
}

/// A scalar shape and its identity are one declaration, including on the wire.
/// The redundant wire metadata is checked against this declaration on decode.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
enum IdentifiedScalarDeclaration {
    String(TypeIdentity),
    Int(TypeIdentity),
    Float(TypeIdentity),
}

impl IdentifiedScalarDeclaration {
    fn identity(&self) -> &TypeIdentity {
        match self {
            Self::String(identity) | Self::Int(identity) | Self::Float(identity) => identity,
        }
    }

    fn scalar_kind(&self) -> RefinedScalarKind {
        match self {
            Self::String(_) => RefinedScalarKind::String,
            Self::Int(_) => RefinedScalarKind::Int,
            Self::Float(_) => RefinedScalarKind::Float,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct OwnedTypeIdentityWire {
    owners: NonEmptySchemaTypeOwners,
    identity: TypeIdentity,
    scalar_kind: RefinedScalarKind,
    declaration: IdentifiedScalarDeclaration,
}

impl serde::Serialize for OwnedTypeIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        OwnedTypeIdentityWire {
            owners: self.owners.clone(),
            identity: self.identity().clone(),
            scalar_kind: self.scalar_kind(),
            declaration: self.declaration.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for OwnedTypeIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = OwnedTypeIdentityWire::deserialize(deserializer)?;
        if wire.identity != *wire.declaration.identity()
            || wire.scalar_kind != wire.declaration.scalar_kind()
        {
            return Err(serde::de::Error::custom(
                "schema type identity and scalar kind must match their declaration",
            ));
        }
        Ok(Self {
            owners: wire.owners,
            declaration: wire.declaration,
        })
    }
}

impl OwnedTypeIdentity {
    /// Reference annotations retain string shape and owner identity, while
    /// their declaration remains offline-only in the factory's schema index.
    pub(crate) fn from_schema_declaration(
        owner: impl Into<String>,
        declaration: &AttributeType,
    ) -> Option<Self> {
        if let crate::schema::RawShape::Reference { identity, .. } = declaration.raw_shape() {
            return Some(Self {
                owners: NonEmptySchemaTypeOwners::singleton(SchemaTypeOwner::Provider(
                    owner.into(),
                )),
                declaration: IdentifiedScalarDeclaration::String(identity.clone()),
            });
        }
        IdentifiedCustomTypeDeclaration::new(declaration)
            .map(|declaration| Self::from_declaration(owner, declaration))
    }
    pub(crate) fn from_declaration(
        owner: impl Into<String>,
        declaration: IdentifiedCustomTypeDeclaration<'_>,
    ) -> Self {
        Self::from_declaration_with_owners(
            NonEmptySchemaTypeOwners::singleton(SchemaTypeOwner::Provider(owner.into())),
            declaration,
        )
    }

    fn from_declaration_with_owners(
        owners: NonEmptySchemaTypeOwners,
        declaration: IdentifiedCustomTypeDeclaration<'_>,
    ) -> Self {
        let identity = declaration.identity().clone();
        let kind = declaration.scalar_kind();
        let declaration = match kind {
            RefinedScalarKind::String => IdentifiedScalarDeclaration::String(identity),
            RefinedScalarKind::Int => IdentifiedScalarDeclaration::Int(identity),
            RefinedScalarKind::Float => IdentifiedScalarDeclaration::Float(identity),
        };
        Self {
            owners,
            declaration,
        }
    }

    pub(crate) fn with_merged_owners(&self, owners: NonEmptySchemaTypeOwners) -> Self {
        Self {
            owners,
            declaration: self.declaration.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn provider(
        owner: impl Into<String>,
        identity: TypeIdentity,
        scalar_kind: RefinedScalarKind,
    ) -> Self {
        let declared_type = scalar_kind.identified_type(identity);
        Self::from_declaration(
            owner,
            IdentifiedCustomTypeDeclaration::new(&declared_type).unwrap(),
        )
    }

    #[cfg(test)]
    pub(crate) fn with_owners(
        identity: TypeIdentity,
        owners: NonEmptySchemaTypeOwners,
        scalar_kind: RefinedScalarKind,
    ) -> Self {
        let declared_type = scalar_kind.identified_type(identity);
        Self::from_declaration_with_owners(
            owners,
            IdentifiedCustomTypeDeclaration::new(&declared_type).unwrap(),
        )
    }

    #[cfg(test)]
    pub(crate) fn unbounded_witness_for_test(self) -> ResolvedSchemaType {
        let declared = self.scalar_kind().identified_type(self.identity().clone());
        ResolvedSchemaType(std::sync::Arc::new(ResolvedScalarDeclaration {
            constraints: super::scalar_constraints::ScalarConstraints::from_declaration(&declared)
                .unwrap(),
            owned: self,
            projection: None,
            projection_fingerprint: None,
        }))
    }

    pub fn owners(&self) -> &NonEmptySchemaTypeOwners {
        &self.owners
    }

    pub fn provider_names(&self) -> impl Iterator<Item = &str> {
        self.owners.iter().map(|owner| match owner {
            SchemaTypeOwner::Provider(owner) => owner.as_str(),
        })
    }

    pub fn identity(&self) -> &TypeIdentity {
        self.declaration.identity()
    }

    pub fn scalar_kind(&self) -> RefinedScalarKind {
        self.declaration.scalar_kind()
    }

    /// Render the type's authored semantic name.
    ///
    /// Schema ownership remains hidden routing data. In particular, an
    /// AWSCC-owned `aws.*` identity is still authored as `aws.*`; rendering
    /// the owner here would silently change the public type name.
    pub fn authored_name(&self) -> String {
        self.identity().to_string()
    }

    /// Return whether this key's semantic identity is named by an authored
    /// dotted type path. The schema owner is intentionally not consulted.
    pub fn matches_authored_path(
        &self,
        semantic_provider: &str,
        segments: &[String],
        kind: &str,
    ) -> bool {
        self.identity().provider.as_deref() == Some(semantic_provider)
            && self.identity().segments == segments
            && self.identity().kind == kind
    }
}

/// Opaque resolved scalar-declaration evidence for an annotation or an exact
/// schema projection. Catalog annotations conjoin every declaration of every
/// owner; exact projections retain only their own declaration. Shared storage
/// keeps compiled patterns out of catalog keys and makes nested lifting cheap.
///
/// ```compile_fail
/// use carina_core::parser::{OwnedTypeIdentity, ResolvedSchemaType};
/// fn forge(key: OwnedTypeIdentity) -> ResolvedSchemaType { key.into() }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSchemaType(std::sync::Arc<ResolvedScalarDeclaration>);

#[derive(Debug, Clone)]
struct ResolvedScalarDeclaration {
    owned: OwnedTypeIdentity,
    constraints: super::scalar_constraints::ScalarConstraints,
    projection: Option<AttributeType>,
    // Portable declaration provenance. Decoding retains this key until the
    // catalog can rebind the exact host declaration, including its callback.
    projection_fingerprint: Option<String>,
}

// Type equality compares portable scalar evidence. The exact host declaration
// retains callback dispatch provenance without making closures part of equality.
impl PartialEq for ResolvedScalarDeclaration {
    fn eq(&self, other: &Self) -> bool {
        self.owned == other.owned && self.constraints == other.constraints
    }
}
impl Eq for ResolvedScalarDeclaration {}

#[derive(serde::Serialize, serde::Deserialize)]
enum ResolvedDeclarationOrigin {
    Annotation,
    Projection,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ResolvedSchemaTypeWire {
    #[serde(flatten)]
    owned: OwnedTypeIdentity,
    constraints: super::scalar_constraints::ScalarConstraints,
    origin: ResolvedDeclarationOrigin,
    projection_fingerprint: Option<String>,
}

impl serde::Serialize for ResolvedSchemaType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ResolvedSchemaTypeWire {
            owned: self.0.owned.clone(),
            constraints: self.0.constraints.clone(),
            origin: if self.0.projection_fingerprint.is_some() {
                ResolvedDeclarationOrigin::Projection
            } else {
                ResolvedDeclarationOrigin::Annotation
            },
            projection_fingerprint: self.0.projection_fingerprint.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ResolvedSchemaType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut wire = ResolvedSchemaTypeWire::deserialize(deserializer)?;
        if wire.owned.scalar_kind() != wire.constraints.kind() {
            return Err(serde::de::Error::custom(
                "schema type scalar kind must match its constraints",
            ));
        }
        wire.constraints
            .validate_record()
            .map_err(serde::de::Error::custom)?;
        match wire.origin {
            ResolvedDeclarationOrigin::Annotation => {
                if wire.projection_fingerprint.is_some() {
                    return Err(serde::de::Error::custom(
                        "an annotation cannot carry exact projection provenance",
                    ));
                }
            }
            ResolvedDeclarationOrigin::Projection => {
                if !wire.owned.owners().is_singleton() {
                    return Err(serde::de::Error::custom(
                        "an exact schema projection must have one owner",
                    ));
                }
                if !wire
                    .projection_fingerprint
                    .as_ref()
                    .is_some_and(|fingerprint| {
                        fingerprint.len() == 64
                            && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                {
                    return Err(serde::de::Error::custom(
                        "an exact schema projection requires its declaration fingerprint",
                    ));
                }
            }
        }
        Ok(Self(std::sync::Arc::new(ResolvedScalarDeclaration {
            owned: wire.owned,
            constraints: wire.constraints,
            projection: None,
            projection_fingerprint: wire.projection_fingerprint,
        })))
    }
}

impl std::ops::Deref for ResolvedSchemaType {
    type Target = OwnedTypeIdentity;
    fn deref(&self) -> &Self::Target {
        &self.0.owned
    }
}

impl ResolvedSchemaType {
    pub(crate) fn from_schema_declaration(
        owner: impl Into<String>,
        declaration: &AttributeType,
    ) -> Option<Self> {
        Some(Self(std::sync::Arc::new(ResolvedScalarDeclaration {
            owned: OwnedTypeIdentity::from_schema_declaration(owner, declaration)?,
            constraints: super::scalar_constraints::ScalarConstraints::from_declaration(
                declaration,
            )?,
            projection: Some(declaration.clone()),
            projection_fingerprint: Some(declaration.declaration_fingerprint()),
        })))
    }

    pub(crate) fn from_declaration(
        owner: impl Into<String>,
        declaration: IdentifiedCustomTypeDeclaration<'_>,
    ) -> Self {
        Self::from_schema_declaration(owner, declaration.declared_type())
            .expect("admitted scalar declaration")
    }

    pub(crate) fn conjoin(&self, other: &Self) -> Self {
        assert_eq!(self.identity(), other.identity());
        assert_eq!(self.scalar_kind(), other.scalar_kind());
        let mut constraints = self.0.constraints.clone();
        constraints.intersect(&other.0.constraints);
        Self(std::sync::Arc::new(ResolvedScalarDeclaration {
            owned: self
                .0
                .owned
                .with_merged_owners(self.owners().union(other.owners())),
            constraints,
            projection: None,
            projection_fingerprint: None,
        }))
    }

    pub(crate) fn into_annotation(mut self) -> Self {
        std::sync::Arc::make_mut(&mut self.0).projection = None;
        std::sync::Arc::make_mut(&mut self.0).projection_fingerprint = None;
        self
    }

    pub(crate) fn resolve_projection(
        &self,
        config: &ProviderContext,
    ) -> Result<Option<AttributeType>, CustomTypeValidationError> {
        match (&self.0.projection, &self.0.projection_fingerprint) {
            (Some(declaration), _) => Ok(Some(declaration.clone())),
            (None, Some(_)) => config.custom_types.projection_declaration(self).map(Some),
            (None, None) => Ok(None),
        }
    }

    pub(crate) fn matches_projection_declaration(&self, declaration: &AttributeType) -> bool {
        self.0
            .projection_fingerprint
            .as_ref()
            .is_some_and(|fingerprint| {
                *fingerprint == declaration.declaration_fingerprint()
                    && super::scalar_constraints::ScalarConstraints::from_declaration(declaration)
                        .as_ref()
                        == Some(&self.0.constraints)
            })
    }

    pub(crate) fn materialize(&self) -> AttributeType {
        self.0.constraints.materialize(self)
    }
    pub(crate) fn has_empty_intersection(&self) -> bool {
        self.0.constraints.is_empty()
    }
    pub(crate) fn patterns(&self) -> impl Iterator<Item = &crate::schema::CompiledPattern> {
        self.0.constraints.patterns()
    }
}

/// An identified schema declaration checked at construction time.
///
/// Keeping the identity beside the borrowed declaration makes it impossible
/// for custom-type routing APIs to receive an anonymous `AttributeType` and
/// then guess an identity from its display name.
#[derive(Clone, Copy)]
pub struct IdentifiedCustomTypeDeclaration<'a> {
    declared_type: &'a AttributeType,
    identity: &'a TypeIdentity,
    scalar_kind: RefinedScalarKind,
}

impl<'a> IdentifiedCustomTypeDeclaration<'a> {
    pub fn new(declared_type: &'a AttributeType) -> Option<Self> {
        let scalar_kind =
            crate::provider::identified_custom_type_scalar_kind(declared_type).ok()??;
        let identity = crate::provider::identified_custom_type_identity(declared_type)?;
        Some(Self {
            declared_type,
            identity,
            scalar_kind,
        })
    }

    pub fn declared_type(self) -> &'a AttributeType {
        self.declared_type
    }

    pub fn identity(self) -> &'a TypeIdentity {
        self.identity
    }

    pub fn scalar_kind(self) -> RefinedScalarKind {
        self.scalar_kind
    }
}

/// The declaration authority for one custom-type validation.
///
/// Resource/config schema walks carry the exact declaration. Authored module
/// annotations instead carry every owner of their semantic identity and are
/// checked against the pre-indexed declarations of each owner.
///
/// A raw (and potentially anonymous) attribute type cannot enter the
/// declaration route:
///
/// ```compile_fail
/// use carina_core::parser::CustomTypeValidationTarget;
/// use carina_core::schema::AttributeType;
///
/// let draft = AttributeType::string();
/// let _ = CustomTypeValidationTarget::Declaration {
///     owner: "test",
///     declaration: &draft,
/// };
/// ```
#[derive(Clone, Copy)]
pub enum CustomTypeValidationTarget<'a> {
    Annotation(&'a OwnedTypeIdentity),
    Declaration {
        owner: &'a str,
        declaration: IdentifiedCustomTypeDeclaration<'a>,
    },
}

impl<'a> CustomTypeValidationTarget<'a> {
    pub fn identity(self) -> &'a TypeIdentity {
        match self {
            Self::Annotation(owned) => owned.identity(),
            Self::Declaration { declaration, .. } => declaration.identity(),
        }
    }
}

/// Typed failures produced while resolving owner-qualified custom types.
#[derive(Debug, Clone)]
pub enum CustomTypeValidationError {
    ProvidersNotLoaded,
    Rejected {
        owner: String,
        identity: TypeIdentity,
        message: crate::provider_text::DiagnosticText,
    },
    ProviderRejected {
        owner: String,
        identity: TypeIdentity,
        error: std::sync::Arc<crate::provider::HostError>,
        masked: bool,
    },
    InvalidDeclarationValue {
        owner: String,
        identity: TypeIdentity,
        error: Box<crate::schema::TypeError>,
    },
    MissingDeclaration {
        owner: String,
        identity: TypeIdentity,
    },
    ConflictingEnumDefinitions {
        identity: TypeIdentity,
        first_owner: String,
        second_owner: String,
    },
    ConflictingEnumSpelling {
        identity: Box<TypeIdentity>,
        first_owner: String,
        first_spelling: String,
        second_owner: String,
        second_spelling: String,
    },
}

impl std::fmt::Display for CustomTypeValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::provider_text::DiagnosticText;
        match self {
            Self::ProvidersNotLoaded => formatter.write_str("provider custom types are not loaded"),
            Self::Rejected {
                owner,
                identity,
                message,
            } => write!(
                formatter,
                "{owner} rejected `{identity}`: {message}",
                owner = DiagnosticText::new(owner),
                identity = DiagnosticText::new(identity.to_string())
            ),
            Self::ProviderRejected {
                owner,
                identity,
                error,
                masked,
            } => {
                if *masked {
                    write!(
                        formatter,
                        "{owner} rejected `{identity}`: {} failed validation",
                        crate::schema::SECRET_VALUE_MASK,
                        owner = DiagnosticText::new(owner),
                        identity = DiagnosticText::new(identity.to_string())
                    )
                } else {
                    write!(
                        formatter,
                        "{owner} rejected `{identity}`: {error}",
                        owner = DiagnosticText::new(owner),
                        identity = DiagnosticText::new(identity.to_string())
                    )
                }
            }
            Self::InvalidDeclarationValue {
                owner,
                identity,
                error,
            } => write!(
                formatter,
                "{owner} rejected `{identity}`: {error}",
                owner = DiagnosticText::new(owner),
                identity = DiagnosticText::new(identity.to_string())
            ),
            Self::MissingDeclaration { owner, identity } => {
                write!(
                    formatter,
                    "{owner} has no declaration for `{identity}`",
                    owner = DiagnosticText::new(owner),
                    identity = DiagnosticText::new(identity.to_string())
                )
            }
            Self::ConflictingEnumDefinitions {
                identity,
                first_owner,
                second_owner,
            } => write!(
                formatter,
                "enum `{identity}` has conflicting variants or aliases in owners `{first_owner}` and `{second_owner}`",
                identity = DiagnosticText::new(identity.to_string()),
                first_owner = DiagnosticText::new(first_owner),
                second_owner = DiagnosticText::new(second_owner)
            ),
            Self::ConflictingEnumSpelling {
                identity,
                first_owner,
                first_spelling,
                second_owner,
                second_spelling,
            } => write!(
                formatter,
                "enum `{identity}` resolves to `{first_spelling}` for owner `{first_owner}` but `{second_spelling}` for owner `{second_owner}`",
                identity = DiagnosticText::new(identity.to_string()),
                first_spelling = DiagnosticText::new(first_spelling),
                first_owner = DiagnosticText::new(first_owner),
                second_spelling = DiagnosticText::new(second_spelling),
                second_owner = DiagnosticText::new(second_owner)
            ),
        }
    }
}

impl std::error::Error for CustomTypeValidationError {}

impl PartialEq for CustomTypeValidationError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ProvidersNotLoaded, Self::ProvidersNotLoaded) => true,
            (
                Self::Rejected {
                    owner: left_owner,
                    identity: left_identity,
                    message: left_message,
                },
                Self::Rejected {
                    owner: right_owner,
                    identity: right_identity,
                    message: right_message,
                },
            ) => {
                left_owner == right_owner
                    && left_identity == right_identity
                    && left_message == right_message
            }
            (
                Self::ProviderRejected {
                    owner: left_owner,
                    identity: left_identity,
                    error: left_error,
                    masked: left_masked,
                },
                Self::ProviderRejected {
                    owner: right_owner,
                    identity: right_identity,
                    error: right_error,
                    masked: right_masked,
                },
            ) => {
                let left_detail = left_error.detail();
                let right_detail = right_error.detail();
                left_owner == right_owner
                    && left_identity == right_identity
                    && left_masked == right_masked
                    && left_error.kind() == right_error.kind()
                    && left_error.to_string() == right_error.to_string()
                    && left_detail.host_cause() == right_detail.host_cause()
                    && left_detail.provider_name == right_detail.provider_name
            }
            (
                Self::InvalidDeclarationValue {
                    owner: left_owner,
                    identity: left_identity,
                    error: left_error,
                },
                Self::InvalidDeclarationValue {
                    owner: right_owner,
                    identity: right_identity,
                    error: right_error,
                },
            ) => {
                left_owner == right_owner
                    && left_identity == right_identity
                    && left_error == right_error
            }
            (
                Self::MissingDeclaration {
                    owner: left_owner,
                    identity: left_identity,
                },
                Self::MissingDeclaration {
                    owner: right_owner,
                    identity: right_identity,
                },
            ) => left_owner == right_owner && left_identity == right_identity,
            (
                Self::ConflictingEnumDefinitions {
                    identity: left_identity,
                    first_owner: left_first,
                    second_owner: left_second,
                },
                Self::ConflictingEnumDefinitions {
                    identity: right_identity,
                    first_owner: right_first,
                    second_owner: right_second,
                },
            ) => {
                left_identity == right_identity
                    && left_first == right_first
                    && left_second == right_second
            }
            (
                Self::ConflictingEnumSpelling {
                    identity: left_identity,
                    first_owner: left_first_owner,
                    first_spelling: left_first_spelling,
                    second_owner: left_second_owner,
                    second_spelling: left_second_spelling,
                },
                Self::ConflictingEnumSpelling {
                    identity: right_identity,
                    first_owner: right_first_owner,
                    first_spelling: right_first_spelling,
                    second_owner: right_second_owner,
                    second_spelling: right_second_spelling,
                },
            ) => {
                left_identity == right_identity
                    && left_first_owner == right_first_owner
                    && left_first_spelling == right_first_spelling
                    && left_second_owner == right_second_owner
                    && left_second_spelling == right_second_spelling
            }
            _ => false,
        }
    }
}

impl CustomTypeValidationError {
    pub(crate) fn diagnostic_shape(&self) -> crate::schema::ValidationDiagnostic<'_> {
        use crate::provider_text::DiagnosticText;
        use crate::schema::ValidationDiagnostic;
        match self {
            Self::ProviderRejected {
                owner,
                identity,
                error,
                masked,
            } => {
                let prefix = format!(
                    "{} rejected `{}`: ",
                    DiagnosticText::new(owner),
                    DiagnosticText::new(identity.to_string())
                );
                if *masked {
                    ValidationDiagnostic::HiddenChild {
                        message: format!(
                            "{prefix}{} failed validation",
                            crate::schema::SECRET_VALUE_MASK
                        ),
                        child: error.as_ref(),
                    }
                } else {
                    ValidationDiagnostic::Child {
                        prefix,
                        child: error.as_ref(),
                    }
                }
            }
            Self::InvalidDeclarationValue {
                owner,
                identity,
                error,
            } => ValidationDiagnostic::Child {
                prefix: format!(
                    "{} rejected `{}`: ",
                    DiagnosticText::new(owner),
                    DiagnosticText::new(identity.to_string())
                ),
                child: error.as_ref(),
            },
            Self::ProvidersNotLoaded
            | Self::Rejected { .. }
            | Self::MissingDeclaration { .. }
            | Self::ConflictingEnumDefinitions { .. }
            | Self::ConflictingEnumSpelling { .. } => ValidationDiagnostic::Plain(self),
        }
    }
}

impl CustomTypeValidationError {
    pub fn provider_error(&self) -> Option<&crate::provider::HostError> {
        match self {
            Self::ProviderRejected { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn fixture_custom_type_rejection(
    target: CustomTypeValidationTarget<'_>,
    message: impl Into<String>,
) -> CustomTypeValidationError {
    let owner = match target {
        CustomTypeValidationTarget::Annotation(owned) => owned
            .provider_names()
            .next()
            .expect("fixture annotation must carry a provider owner"),
        CustomTypeValidationTarget::Declaration { owner, .. } => owner,
    };
    CustomTypeValidationError::Rejected {
        owner: owner.to_string(),
        identity: target.identity().clone(),
        message: crate::provider_text::DiagnosticText::new(message),
    }
}

#[cfg(test)]
pub(crate) fn fixture_invalid_custom_type_value(
    target: CustomTypeValidationTarget<'_>,
    error: crate::schema::TypeError,
) -> CustomTypeValidationError {
    let owner = match target {
        CustomTypeValidationTarget::Annotation(owned) => owned
            .provider_names()
            .next()
            .expect("fixture annotation must carry a provider owner"),
        CustomTypeValidationTarget::Declaration { owner, .. } => owner,
    };
    CustomTypeValidationError::InvalidDeclarationValue {
        owner: owner.to_string(),
        identity: target.identity().clone(),
        error: Box::new(error),
    }
}

/// Signature for a factory-based custom type validator.
///
/// Takes either an authored multi-owner annotation or an exact schema-path
/// declaration plus the original typed scalar value. The factory/schema index
/// is built once when the provider context is enriched.
pub type CustomTypeValidatorFn = Box<
    dyn for<'a> Fn(
            CustomTypeValidationTarget<'a>,
            &'a Value,
        ) -> Result<(), CustomTypeValidationError>
        + Send
        + Sync,
>;

type CustomTypeEnumLookupFn = Box<
    dyn Fn(&OwnedTypeIdentity) -> Result<Option<AttributeType>, CustomTypeValidationError>
        + Send
        + Sync,
>;

type CustomTypeDeclarationLookupFn = Box<
    dyn Fn(&OwnedTypeIdentity) -> Result<ResolvedSchemaType, CustomTypeValidationError>
        + Send
        + Sync,
>;

type CustomTypeProjectionLookupFn = Box<
    dyn Fn(&ResolvedSchemaType) -> Result<AttributeType, CustomTypeValidationError> + Send + Sync,
>;

/// Owner-aware custom-type validation capability.
///
/// Both the factory-backed resolver and [`ProviderCustomTypes`] implement this
/// trait, so every schema walk is adapted through one owner-bound lookup
/// constructor.
pub trait CustomTypeResolver: Send + Sync {
    /// Dispatch an already checked declaration using its sealed canonical proof.
    fn validate_validated(
        &self,
        owner: &str,
        value: crate::schema::LocallyValidatedCustomType<'_>,
    ) -> Result<(), CustomTypeValidationError>;

    /// Resolve every declaration of every requested owner from the same
    /// authoritative catalog used for runtime validation. Implementations
    /// must fail when any requested owner/declaration is unavailable.
    fn annotation_declaration(
        &self,
        owned: &OwnedTypeIdentity,
    ) -> Result<ResolvedSchemaType, CustomTypeValidationError> {
        Err(CustomTypeValidationError::MissingDeclaration {
            owner: owned
                .provider_names()
                .next()
                .expect("nonempty owners")
                .into(),
            identity: owned.identity().clone(),
        })
    }

    /// Return the exact declarations checked by an authored annotation,
    /// including native callbacks. This supplies data to the local validator;
    /// only `validate_validated` can dispatch a value to a provider.
    fn annotation_declarations(
        &self,
        owned: &OwnedTypeIdentity,
    ) -> Result<BTreeMap<String, Vec<AttributeType>>, CustomTypeValidationError> {
        Err(CustomTypeValidationError::MissingDeclaration {
            owner: owned
                .provider_names()
                .next()
                .expect("nonempty owners")
                .into(),
            identity: owned.identity().clone(),
        })
    }

    /// Rebind an exact wire projection to its original declaration. Missing or
    /// ambiguous provenance must fail rather than fall back to scalar metadata.
    fn projection_declaration(
        &self,
        projection: &ResolvedSchemaType,
    ) -> Result<AttributeType, CustomTypeValidationError> {
        Err(CustomTypeValidationError::MissingDeclaration {
            owner: projection
                .provider_names()
                .next()
                .expect("nonempty owners")
                .into(),
            identity: projection.identity().clone(),
        })
    }

    /// Return the exact enum declaration owned by `owned`, when the custom
    /// type is an enum. Implementations must resolve by structured identity
    /// and owner; callers must not reconstruct declarations from authored
    /// names.
    fn enum_declaration(
        &self,
        _owned: &OwnedTypeIdentity,
    ) -> Result<Option<AttributeType>, CustomTypeValidationError> {
        Ok(None)
    }
}

/// Validate raw custom-type values locally before dispatching their sealed
/// proofs. Resolver implementations cannot override this entry point.
pub fn validate_custom_type_value(
    resolver: &dyn CustomTypeResolver,
    target: CustomTypeValidationTarget<'_>,
    value: &crate::resource::knowledge::KnownValue<'_>,
) -> Result<(), CustomTypeValidationError> {
    match target {
        CustomTypeValidationTarget::Declaration { owner, declaration } => {
            crate::schema::validate_custom_type_leaf(declaration, value, |proof| {
                proof.map_or(Ok(()), |proof| resolver.validate_validated(owner, proof))
            })
            .map_err(|error| CustomTypeValidationError::InvalidDeclarationValue {
                owner: owner.to_string(),
                identity: declaration.identity().clone(),
                error: Box::new(error),
            })?
        }
        CustomTypeValidationTarget::Annotation(owned) => {
            crate::provider::validate_custom_type_annotation(resolver, owned, value)
        }
    }
}

/// A provider-derived resolver whose recognition keys come from the same
/// indexed schema snapshot as its validation implementation.
pub(crate) trait ProviderCustomTypeSource: CustomTypeResolver + 'static {
    fn known_custom_types(&self) -> BTreeSet<OwnedTypeIdentity>;
    fn assignability_closures(
        &self,
    ) -> BTreeMap<SchemaTypeOwner, crate::schema::AssignabilityClosure>;
}

// Closure resolvers support fixture catalogs only. Production dispatch keeps
// the canonical proof through FactoryCustomTypeResolver.
#[cfg(any(test, feature = "test-utils"))]
impl<F> CustomTypeResolver for F
where
    F: for<'a> Fn(
            CustomTypeValidationTarget<'a>,
            &'a Value,
        ) -> Result<(), CustomTypeValidationError>
        + Send
        + Sync,
{
    fn annotation_declarations(
        &self,
        owned: &OwnedTypeIdentity,
    ) -> Result<BTreeMap<String, Vec<AttributeType>>, CustomTypeValidationError> {
        let declaration = owned
            .scalar_kind()
            .identified_type(owned.identity().clone());
        Ok(owned
            .provider_names()
            .map(|owner| (owner.to_string(), vec![declaration.clone()]))
            .collect())
    }

    fn validate_validated(
        &self,
        owner: &str,
        value: crate::schema::LocallyValidatedCustomType<'_>,
    ) -> Result<(), CustomTypeValidationError> {
        self(
            CustomTypeValidationTarget::Declaration {
                owner,
                declaration: value.declaration(),
            },
            value.value(),
        )
    }
}

/// Provider custom-type registration state.
///
/// The opaque state always carries recognition keys and the resolver that owns
/// their declarations together. Provider-backed construction is exposed via
/// `carina_core::provider::provider_custom_types_from_factories`; callers
/// cannot manufacture a loaded state with independently supplied pieces.
///
/// ```compile_fail
/// use carina_core::parser::ProviderCustomTypes;
///
/// let _ = ProviderCustomTypes { state: unreachable!() };
/// ```
#[derive(Clone)]
pub struct ProviderCustomTypes {
    state: std::sync::Arc<ProviderCustomTypeState>,
}

enum ProviderCustomTypeState {
    Unloaded,
    Partial {
        loaded: ProviderCustomTypes,
        failed_providers: BTreeSet<String>,
    },
    Loaded {
        known: BTreeSet<OwnedTypeIdentity>,
        resolver: std::sync::Arc<dyn CustomTypeResolver>,
        enum_lookup: CustomTypeEnumLookupFn,
        annotation_lookup: CustomTypeDeclarationLookupFn,
        projection_lookup: CustomTypeProjectionLookupFn,
        assignability_closures: BTreeMap<SchemaTypeOwner, crate::schema::AssignabilityClosure>,
    },
}

impl Default for ProviderCustomTypes {
    fn default() -> Self {
        Self {
            state: std::sync::Arc::new(ProviderCustomTypeState::Unloaded),
        }
    }
}

impl ProviderCustomTypes {
    /// Supply exact enum declarations to legacy callback-based fixtures so
    /// local validation and canonicalization use the same declaration.
    #[cfg(test)]
    pub(crate) fn with_declarations_for_test(
        mut self,
        declarations: BTreeMap<String, Vec<AttributeType>>,
    ) -> Self {
        struct FixtureDeclarations {
            resolver: std::sync::Arc<dyn CustomTypeResolver>,
            declarations: BTreeMap<String, Vec<AttributeType>>,
        }

        impl CustomTypeResolver for FixtureDeclarations {
            fn validate_validated(
                &self,
                owner: &str,
                value: crate::schema::LocallyValidatedCustomType<'_>,
            ) -> Result<(), CustomTypeValidationError> {
                self.resolver.validate_validated(owner, value)
            }

            fn annotation_declarations(
                &self,
                owned: &OwnedTypeIdentity,
            ) -> Result<BTreeMap<String, Vec<AttributeType>>, CustomTypeValidationError>
            {
                owned
                    .provider_names()
                    .map(|owner| {
                        let declarations = self
                            .declarations
                            .get(owner)
                            .into_iter()
                            .flatten()
                            .filter(|declaration| {
                                crate::provider::identified_custom_type_identity(declaration)
                                    == Some(owned.identity())
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        if declarations.is_empty() {
                            return Err(CustomTypeValidationError::MissingDeclaration {
                                owner: owner.to_string(),
                                identity: owned.identity().clone(),
                            });
                        }
                        Ok((owner.to_string(), declarations))
                    })
                    .collect()
            }
        }

        let ProviderCustomTypeState::Loaded { resolver, .. } =
            std::sync::Arc::get_mut(&mut self.state)
                .expect("fixture catalog must be uniquely owned")
        else {
            panic!("fixture catalog must be loaded");
        };
        *resolver = std::sync::Arc::new(FixtureDeclarations {
            resolver: std::sync::Arc::clone(resolver),
            declarations,
        });
        self
    }

    // Unit tests can inject deliberately malformed routing fixtures. Like every
    // loaded catalog, this declares no edges for owners absent from the map.
    // Production construction is single-source through `from_source` below.
    #[cfg(test)]
    pub(crate) fn loaded(
        known: HashSet<OwnedTypeIdentity>,
        resolver: CustomTypeValidatorFn,
    ) -> Self {
        let declarations = known.clone();
        // These fixtures model annotation callbacks. The public resolver still
        // receives only a locally checked proof before invoking the callback.
        let resolver = move |target: CustomTypeValidationTarget<'_>, value: &Value| match target {
            CustomTypeValidationTarget::Declaration { owner, declaration } => {
                let owned = OwnedTypeIdentity::from_declaration(owner, declaration);
                resolver(CustomTypeValidationTarget::Annotation(&owned), value)
            }
            target => resolver(target, value),
        };
        Self {
            state: std::sync::Arc::new(ProviderCustomTypeState::Loaded {
                known: known.into_iter().collect(),
                resolver: std::sync::Arc::new(resolver),
                enum_lookup: Box::new(|_| Ok(None)),
                projection_lookup: Box::new(|projection| {
                    Err(CustomTypeValidationError::MissingDeclaration {
                        owner: projection
                            .provider_names()
                            .next()
                            .expect("nonempty owners")
                            .into(),
                        identity: projection.identity().clone(),
                    })
                }),
                annotation_lookup: Box::new(move |owned| {
                    for owner in owned.owners().iter() {
                        if !declarations.iter().any(|candidate| {
                            candidate.identity() == owned.identity()
                                && candidate.scalar_kind() == owned.scalar_kind()
                                && candidate.owners().contains(owner)
                        }) {
                            let SchemaTypeOwner::Provider(owner) = owner;
                            return Err(CustomTypeValidationError::MissingDeclaration {
                                owner: owner.clone(),
                                identity: owned.identity().clone(),
                            });
                        }
                    }
                    Ok(owned.clone().unbounded_witness_for_test())
                }),
                assignability_closures: BTreeMap::new(),
            }),
        }
    }

    pub(crate) fn from_source(source: impl ProviderCustomTypeSource) -> Self {
        let known = source.known_custom_types();
        let assignability_closures = source.assignability_closures();
        let source = std::sync::Arc::new(source);
        let validation_source = std::sync::Arc::clone(&source);
        let declaration_source = std::sync::Arc::clone(&source);
        let projection_source = std::sync::Arc::clone(&source);
        Self {
            state: std::sync::Arc::new(ProviderCustomTypeState::Loaded {
                known,
                resolver: validation_source,
                enum_lookup: Box::new(move |owned| source.enum_declaration(owned)),
                annotation_lookup: Box::new(move |owned| {
                    declaration_source.annotation_declaration(owned)
                }),
                projection_lookup: Box::new(move |projection| {
                    projection_source.projection_declaration(projection)
                }),
                assignability_closures,
            }),
        }
    }

    /// Mark an otherwise loaded snapshot as non-authoritative for these owners.
    pub fn with_failed_providers(&self, failed_providers: BTreeSet<String>) -> Self {
        if failed_providers.is_empty() {
            return self.clone();
        }
        Self {
            state: std::sync::Arc::new(ProviderCustomTypeState::Partial {
                loaded: self.clone(),
                failed_providers,
            }),
        }
    }

    pub fn is_partial(&self) -> bool {
        matches!(self.state.as_ref(), ProviderCustomTypeState::Partial { .. })
    }

    /// Whether a missing identity may belong to an unavailable provider.
    pub fn may_defer(&self, provider: Option<&str>) -> bool {
        match self.state.as_ref() {
            ProviderCustomTypeState::Unloaded => true,
            ProviderCustomTypeState::Loaded {
                known: _,
                resolver: _,
                enum_lookup: _,
                annotation_lookup: _,
                projection_lookup: _,
                assignability_closures: _,
            } => false,
            ProviderCustomTypeState::Partial {
                failed_providers, ..
            } => provider.map_or(!failed_providers.is_empty(), |owner| {
                failed_providers.contains(owner)
            }),
        }
    }

    pub(crate) fn same_snapshot(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.state, &other.state)
    }

    pub fn is_loaded(&self) -> bool {
        matches!(
            self.state.as_ref(),
            ProviderCustomTypeState::Loaded {
                known: _known,
                resolver: _resolver,
                enum_lookup: _enum_lookup,
                annotation_lookup: _annotation_lookup,
                projection_lookup: _,
                assignability_closures: _assignability_closures,
            }
        )
    }

    pub fn known(&self) -> &BTreeSet<OwnedTypeIdentity> {
        match self.state.as_ref() {
            ProviderCustomTypeState::Partial { loaded, .. } => loaded.known(),
            ProviderCustomTypeState::Unloaded => {
                static EMPTY: std::sync::LazyLock<BTreeSet<OwnedTypeIdentity>> =
                    std::sync::LazyLock::new(BTreeSet::new);
                &EMPTY
            }
            ProviderCustomTypeState::Loaded {
                known,
                resolver: _resolver,
                enum_lookup: _enum_lookup,
                annotation_lookup: _annotation_lookup,
                projection_lookup: _,
                assignability_closures: _assignability_closures,
            } => known,
        }
    }

    /// Whether this snapshot is authoritative for the owner's edges. Loaded
    /// catalogs treat an absent closure as an empty declaration; only unloaded
    /// catalogs and failed owners in partial catalogs lack evidence.
    pub(crate) fn assignability_edges_available(&self, owner: &SchemaTypeOwner) -> bool {
        match self.state.as_ref() {
            ProviderCustomTypeState::Unloaded => false,
            ProviderCustomTypeState::Partial {
                loaded,
                failed_providers,
            } => {
                let SchemaTypeOwner::Provider(provider) = owner;
                !failed_providers.contains(provider) && loaded.assignability_edges_available(owner)
            }
            ProviderCustomTypeState::Loaded {
                known: _,
                resolver: _,
                enum_lookup: _,
                annotation_lookup: _,
                projection_lookup: _,
                assignability_closures: _,
            } => true,
        }
    }

    /// Whether this owner declared a nonempty path from `source` to `sink`.
    /// Paths never compose declarations belonging to different owners.
    pub fn reachable(
        &self,
        owner: &SchemaTypeOwner,
        source: &TypeIdentity,
        sink: &TypeIdentity,
    ) -> bool {
        match self.state.as_ref() {
            ProviderCustomTypeState::Unloaded => false,
            ProviderCustomTypeState::Partial {
                loaded,
                failed_providers,
            } => {
                let SchemaTypeOwner::Provider(provider) = owner;
                !failed_providers.contains(provider) && loaded.reachable(owner, source, sink)
            }
            ProviderCustomTypeState::Loaded {
                known: _known,
                resolver: _resolver,
                enum_lookup: _enum_lookup,
                annotation_lookup: _annotation_lookup,
                projection_lookup: _,
                assignability_closures,
            } => assignability_closures
                .get(owner)
                .is_some_and(|closure| closure.reachable(source, sink)),
        }
    }

    /// Whether every owner declared a nonempty path from `source` to `sink`.
    pub fn reachable_for_all(
        &self,
        owners: &NonEmptySchemaTypeOwners,
        source: &TypeIdentity,
        sink: &TypeIdentity,
    ) -> bool {
        owners
            .iter()
            .all(|owner| self.reachable(owner, source, sink))
    }
}

impl CustomTypeResolver for ProviderCustomTypes {
    fn projection_declaration(
        &self,
        projection: &ResolvedSchemaType,
    ) -> Result<AttributeType, CustomTypeValidationError> {
        match self.state.as_ref() {
            ProviderCustomTypeState::Partial {
                loaded,
                failed_providers,
            } => {
                if projection
                    .provider_names()
                    .any(|owner| failed_providers.contains(owner))
                {
                    Err(CustomTypeValidationError::ProvidersNotLoaded)
                } else {
                    loaded.projection_declaration(projection)
                }
            }
            ProviderCustomTypeState::Loaded {
                projection_lookup, ..
            } => projection_lookup(projection),
            ProviderCustomTypeState::Unloaded => Err(CustomTypeValidationError::ProvidersNotLoaded),
        }
    }

    fn annotation_declaration(
        &self,
        owned: &OwnedTypeIdentity,
    ) -> Result<ResolvedSchemaType, CustomTypeValidationError> {
        match self.state.as_ref() {
            ProviderCustomTypeState::Partial {
                loaded,
                failed_providers,
            } => {
                if owned
                    .provider_names()
                    .any(|owner| failed_providers.contains(owner))
                {
                    Err(CustomTypeValidationError::ProvidersNotLoaded)
                } else {
                    loaded.annotation_declaration(owned)
                }
            }
            ProviderCustomTypeState::Loaded {
                annotation_lookup, ..
            } => annotation_lookup(owned),
            ProviderCustomTypeState::Unloaded => Err(CustomTypeValidationError::ProvidersNotLoaded),
        }
    }

    fn annotation_declarations(
        &self,
        owned: &OwnedTypeIdentity,
    ) -> Result<BTreeMap<String, Vec<AttributeType>>, CustomTypeValidationError> {
        match self.state.as_ref() {
            ProviderCustomTypeState::Partial {
                loaded,
                failed_providers,
            } => {
                if owned
                    .provider_names()
                    .any(|owner| failed_providers.contains(owner))
                {
                    Err(CustomTypeValidationError::ProvidersNotLoaded)
                } else {
                    loaded.annotation_declarations(owned)
                }
            }
            ProviderCustomTypeState::Loaded { resolver, .. } => {
                resolver.annotation_declarations(owned)
            }
            ProviderCustomTypeState::Unloaded => Err(CustomTypeValidationError::ProvidersNotLoaded),
        }
    }

    fn validate_validated(
        &self,
        owner: &str,
        value: crate::schema::LocallyValidatedCustomType<'_>,
    ) -> Result<(), CustomTypeValidationError> {
        match self.state.as_ref() {
            ProviderCustomTypeState::Unloaded => Err(CustomTypeValidationError::ProvidersNotLoaded),
            ProviderCustomTypeState::Partial {
                loaded,
                failed_providers,
            } => {
                if failed_providers.contains(owner) {
                    Err(CustomTypeValidationError::ProvidersNotLoaded)
                } else {
                    loaded.validate_validated(owner, value)
                }
            }
            ProviderCustomTypeState::Loaded { resolver, .. } => {
                resolver.validate_validated(owner, value)
            }
        }
    }

    fn enum_declaration(
        &self,
        owned: &OwnedTypeIdentity,
    ) -> Result<Option<AttributeType>, CustomTypeValidationError> {
        match self.state.as_ref() {
            ProviderCustomTypeState::Partial {
                loaded,
                failed_providers,
            } => {
                if owned
                    .provider_names()
                    .any(|owner| failed_providers.contains(owner))
                {
                    Err(CustomTypeValidationError::ProvidersNotLoaded)
                } else {
                    loaded.enum_declaration(owned)
                }
            }
            ProviderCustomTypeState::Loaded {
                known: _known,
                resolver: _resolver,
                enum_lookup,
                annotation_lookup: _,
                projection_lookup: _,
                assignability_closures: _assignability_closures,
            } => enum_lookup(owned),
            ProviderCustomTypeState::Unloaded => Err(CustomTypeValidationError::ProvidersNotLoaded),
        }
    }
}

/// Signature for a decryptor function.
///
/// Takes `(ciphertext, optional_key)` and returns the decrypted plaintext or an error.
pub type DecryptorFn =
    std::sync::Arc<dyn Fn(&str, Option<&str>) -> Result<String, String> + Send + Sync>;

/// Configuration for the parser, allowing providers to inject behavior.
///
/// This replaces the global `Mutex`-based decryptor registration and enables
/// provider-specific validators (e.g., ARN, availability_zone) to be injected
/// from provider crates while being callable during parsing.
#[derive(Clone)]
pub struct ProviderContext {
    /// Owned environment for the `env()` built-in function.
    pub environment: crate::EnvironmentSnapshot,
    /// Optional decryptor for the `decrypt()` built-in function.
    pub decryptor: Option<DecryptorFn>,
    /// The catalog is unloaded, fully loaded, or partial with explicit failed owners.
    /// Recognition and validation share one snapshot. Missing names defer only
    /// when they may belong to an unavailable owner.
    pub custom_types: ProviderCustomTypes,
    /// Resource kinds loaded from provider schemas, keyed by `(provider,
    /// resource_type)` such as `("aws", "iam.Role")`.
    pub resource_types: HashSet<(String, String)>,
}

impl ProviderContext {
    pub fn new(environment: crate::EnvironmentSnapshot) -> Self {
        Self {
            environment,
            decryptor: None,
            custom_types: ProviderCustomTypes::default(),
            resource_types: HashSet::new(),
        }
    }

    pub fn custom_types_loaded(&self) -> bool {
        self.custom_types.is_loaded()
    }

    pub fn known_custom_types(&self) -> &BTreeSet<OwnedTypeIdentity> {
        self.custom_types.known()
    }

    pub fn custom_type_resolver(&self) -> &dyn CustomTypeResolver {
        &self.custom_types
    }

    pub fn enum_declaration(
        &self,
        owned: &OwnedTypeIdentity,
    ) -> Result<Option<AttributeType>, CustomTypeValidationError> {
        self.custom_types.enum_declaration(owned)
    }

    /// Return `true` iff `(provider, resource_type)` is a loaded managed
    /// resource or data source kind.
    pub fn has_resource_type(&self, provider: &str, resource_type: &str) -> bool {
        self.resource_types
            .contains(&(provider.to_string(), resource_type.to_string()))
    }

    /// Build the resource-kind registry used to resolve dotted type refs.
    pub fn resource_types_from_schema_registry(
        schemas: &SchemaRegistry,
    ) -> HashSet<(String, String)> {
        schemas
            .iter()
            .map(|(provider, resource_type, _, _)| {
                (provider.to_string(), resource_type.to_string())
            })
            .collect()
    }
}

/// Proof that a [`ProviderContext`] carries a loaded provider custom-type
/// catalog.
///
/// Production code can obtain this witness only through
/// [`crate::provider::loaded_provider_snapshot_from_factories`], which retains
/// it together with schema evidence from one provider-factory snapshot.
/// Value-checking APIs that can reach provider custom types accept this type
/// so a bootstrap [`ProviderContext`] cannot be passed accidentally.
///
/// ```compile_fail
/// use carina_core::parser::{LoadedProviderContext, ProviderContext};
///
/// let _ = LoadedProviderContext(std::sync::Arc::new(ProviderContext::new(carina_core::EnvironmentSnapshot::empty())));
/// ```
#[derive(Clone)]
pub struct LoadedProviderContext(std::sync::Arc<ProviderContext>);

impl LoadedProviderContext {
    pub(crate) fn new(context: ProviderContext) -> Self {
        assert!(
            context.custom_types_loaded(),
            "LoadedProviderContext requires loaded provider custom types"
        );
        Self(std::sync::Arc::new(context))
    }

    pub fn as_provider_context(&self) -> &ProviderContext {
        &self.0
    }

    /// Retain catalog authority with the caller's immutable runtime hooks.
    pub fn with_runtime(&self, runtime: &ProviderContext) -> Self {
        let mut catalog = self.as_provider_context().clone();
        catalog.environment = runtime.environment.clone();
        catalog.decryptor = runtime.decryptor.clone();
        Self::new(catalog)
    }

    /// Test-only constructor for fixtures that deliberately inject a loaded
    /// custom-type resolver without a provider factory.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(context: ProviderContext) -> Self {
        assert!(
            context.custom_types_loaded(),
            "LoadedProviderContext test fixtures must use loaded custom types"
        );
        Self::new(context)
    }

    /// Loaded context with no registered custom types, for tests whose value
    /// checks use only built-in types.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn empty_for_test() -> Self {
        Self::new(ProviderContext {
            custom_types: ProviderCustomTypes {
                state: std::sync::Arc::new(ProviderCustomTypeState::Loaded {
                    known: BTreeSet::new(),
                    resolver: std::sync::Arc::new(
                        |_target: CustomTypeValidationTarget<'_>, _value: &Value| Ok(()),
                    ),
                    enum_lookup: Box::new(|_owned| Ok(None)),
                    projection_lookup: Box::new(|projection| {
                        Err(CustomTypeValidationError::MissingDeclaration {
                            owner: projection
                                .provider_names()
                                .next()
                                .expect("nonempty owners")
                                .into(),
                            identity: projection.identity().clone(),
                        })
                    }),
                    annotation_lookup: Box::new(|owned| {
                        Err(CustomTypeValidationError::MissingDeclaration {
                            owner: owned
                                .provider_names()
                                .next()
                                .expect("nonempty owners")
                                .into(),
                            identity: owned.identity().clone(),
                        })
                    }),
                    assignability_closures: BTreeMap::new(),
                }),
            },
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn set_decryptor_for_test(&mut self, decryptor: DecryptorFn) {
        std::sync::Arc::get_mut(&mut self.0)
            .expect("test fixture must not mutate a cloned LoadedProviderContext")
            .decryptor = Some(decryptor);
    }
}

impl std::ops::Deref for LoadedProviderContext {
    type Target = ProviderContext;

    fn deref(&self) -> &Self::Target {
        self.as_provider_context()
    }
}

impl std::fmt::Debug for LoadedProviderContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("LoadedProviderContext")
            .field(self.as_provider_context())
            .finish()
    }
}

impl std::fmt::Debug for ProviderContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderContext")
            .field("environment", &self.environment)
            .field("decryptor", &self.decryptor.as_ref().map(|_| "..."))
            .field("custom_types", &self.custom_types)
            .field("resource_types", &self.resource_types)
            .finish()
    }
}

impl std::fmt::Debug for ProviderCustomTypes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.state.as_ref() {
            ProviderCustomTypeState::Unloaded => formatter.write_str("Unloaded"),
            ProviderCustomTypeState::Partial {
                loaded,
                failed_providers,
            } => formatter
                .debug_struct("Partial")
                .field("loaded", loaded)
                .field("failed_providers", failed_providers)
                .finish(),
            ProviderCustomTypeState::Loaded {
                known,
                resolver: _resolver,
                enum_lookup: _enum_lookup,
                annotation_lookup: _annotation_lookup,
                projection_lookup: _,
                assignability_closures: _assignability_closures,
            } => formatter
                .debug_struct("Loaded")
                .field("known", known)
                .field("resolver", &"...")
                .finish(),
        }
    }
}

/// Provider kinds whose declarations request unavailable factory authority.
/// A workspace error outside these declarations cannot make this directory's
/// otherwise complete catalog partial. Named instances do not request plugins.
pub fn unavailable_provider_kinds<
    P: crate::resource::PlaceholderKind,
    B: crate::resource::BindingAddress,
>(
    providers: &[super::ProviderConfig<P, B>],
    loaded: &std::collections::HashSet<String>,
    load_errors: &std::collections::HashMap<String, String>,
) -> std::collections::BTreeSet<String> {
    providers
        .iter()
        .filter(|provider| {
            provider.is_default()
                && (!loaded.contains(&provider.name) || load_errors.contains_key(&provider.name))
        })
        .map(|provider| provider.name.clone())
        .collect()
}

impl crate::resource::InsideSecret for CustomTypeValidationError {
    fn inside_secret(self) -> Self {
        match self {
            Self::Rejected {
                owner,
                identity,
                message: _message,
            } => Self::Rejected {
                owner,
                identity,
                message: format!("{} failed validation", crate::schema::SECRET_VALUE_MASK).into(),
            },
            Self::ProviderRejected {
                owner,
                identity,
                error,
                masked: _masked,
            } => Self::ProviderRejected {
                owner,
                identity,
                error,
                masked: true,
            },
            Self::InvalidDeclarationValue {
                owner,
                identity,
                error,
            } => Self::InvalidDeclarationValue {
                owner,
                identity,
                error: Box::new((*error).inside_secret()),
            },
            error @ (Self::ProvidersNotLoaded
            | Self::MissingDeclaration { .. }
            | Self::ConflictingEnumDefinitions { .. }
            | Self::ConflictingEnumSpelling { .. }) => error,
        }
    }
}
