//! Provider context for provider-injected validators and decryptor
//!
//! `ProviderContext` allows CLI/providers to inject custom type validators
//! and a decryptor function into the parser without using global mutable state.

use std::collections::{BTreeSet, HashSet};

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

/// The routing key for an identified refined type.
///
/// A bare [`TypeIdentity`] is descriptive metadata, not routing authority.
/// Pairing it with its schema owner prevents two factories that expose the
/// same identity from competing in an identity-only map. The declared scalar
/// kind travels with the identity so consumers do not need its schema to lift it.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct OwnedTypeIdentity {
    owners: NonEmptySchemaTypeOwners,
    identity: TypeIdentity,
    scalar_kind: RefinedScalarKind,
}

impl OwnedTypeIdentity {
    pub fn provider(
        owner: impl Into<String>,
        identity: TypeIdentity,
        scalar_kind: RefinedScalarKind,
    ) -> Self {
        Self {
            owners: NonEmptySchemaTypeOwners::singleton(SchemaTypeOwner::Provider(owner.into())),
            identity,
            scalar_kind,
        }
    }

    pub fn with_owners(
        identity: TypeIdentity,
        owners: NonEmptySchemaTypeOwners,
        scalar_kind: RefinedScalarKind,
    ) -> Self {
        Self {
            owners,
            identity,
            scalar_kind,
        }
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
        &self.identity
    }

    pub fn scalar_kind(&self) -> RefinedScalarKind {
        self.scalar_kind
    }

    /// Render the type's authored semantic name.
    ///
    /// Schema ownership remains hidden routing data. In particular, an
    /// AWSCC-owned `aws.*` identity is still authored as `aws.*`; rendering
    /// the owner here would silently change the public type name.
    pub fn authored_name(&self) -> String {
        self.identity.to_string()
    }

    /// Return whether this key's semantic identity is named by an authored
    /// dotted type path. The schema owner is intentionally not consulted.
    pub fn matches_authored_path(
        &self,
        semantic_provider: &str,
        segments: &[String],
        kind: &str,
    ) -> bool {
        self.identity.provider.as_deref() == Some(semantic_provider)
            && self.identity.segments == segments
            && self.identity.kind == kind
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
}

impl<'a> IdentifiedCustomTypeDeclaration<'a> {
    pub fn new(declared_type: &'a AttributeType) -> Option<Self> {
        crate::provider::identified_custom_type_scalar_kind(declared_type).ok()?;
        let identity = crate::provider::identified_custom_type_identity(declared_type)?;
        Some(Self {
            declared_type,
            identity,
        })
    }

    pub fn declared_type(self) -> &'a AttributeType {
        self.declared_type
    }

    pub fn identity(self) -> &'a TypeIdentity {
        self.identity
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
        message: String,
    },
    ProviderRejected {
        owner: String,
        identity: TypeIdentity,
        error: std::sync::Arc<crate::provider::ProviderError>,
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
        match self {
            Self::ProvidersNotLoaded => formatter.write_str("provider custom types are not loaded"),
            Self::Rejected {
                owner,
                identity,
                message,
            } => write!(formatter, "{owner} rejected `{identity}`: {message}"),
            Self::ProviderRejected {
                owner,
                identity,
                error,
                masked,
            } => {
                if *masked {
                    write!(
                        formatter,
                        "{owner} rejected `{identity}`: (secret) failed validation"
                    )
                } else {
                    write!(formatter, "{owner} rejected `{identity}`: {error}")
                }
            }
            Self::InvalidDeclarationValue {
                owner,
                identity,
                error,
            } => write!(formatter, "{owner} rejected `{identity}`: {error}"),
            Self::MissingDeclaration { owner, identity } => {
                write!(formatter, "{owner} has no declaration for `{identity}`")
            }
            Self::ConflictingEnumDefinitions {
                identity,
                first_owner,
                second_owner,
            } => write!(
                formatter,
                "enum `{identity}` has conflicting variants or aliases in owners `{first_owner}` and `{second_owner}`"
            ),
            Self::ConflictingEnumSpelling {
                identity,
                first_owner,
                first_spelling,
                second_owner,
                second_spelling,
            } => write!(
                formatter,
                "enum `{identity}` resolves to `{first_spelling}` for owner `{first_owner}` but `{second_spelling}` for owner `{second_owner}`"
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
    pub fn provider_error(&self) -> Option<&crate::provider::ProviderError> {
        match self {
            Self::ProviderRejected { error, .. } => Some(error),
            _ => None,
        }
    }

    pub(crate) fn masked_for_secret(self) -> Self {
        const MASKED_MESSAGE: &str = "(secret) failed validation";

        match self {
            Self::Rejected {
                owner,
                identity,
                message: _message,
            } => Self::Rejected {
                owner,
                identity,
                message: MASKED_MESSAGE.to_string(),
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
                error: Box::new(error.masked_for_secret()),
            },
            error @ (Self::ProvidersNotLoaded
            | Self::MissingDeclaration { .. }
            | Self::ConflictingEnumDefinitions { .. }
            | Self::ConflictingEnumSpelling { .. }) => error,
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
        message: message.into(),
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

/// Owner-aware custom-type validation capability.
///
/// Both the factory-backed resolver and [`ProviderCustomTypes`] implement this
/// trait, so every schema walk is adapted through one owner-bound lookup
/// constructor.
pub trait CustomTypeResolver: Send + Sync {
    fn validate<'a>(
        &self,
        target: CustomTypeValidationTarget<'a>,
        value: &'a Value,
    ) -> Result<(), CustomTypeValidationError>;

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

/// A provider-derived resolver whose recognition keys come from the same
/// indexed schema snapshot as its validation implementation.
pub(crate) trait ProviderCustomTypeSource: CustomTypeResolver + 'static {
    fn known_custom_types(&self) -> BTreeSet<OwnedTypeIdentity>;
}

impl<F> CustomTypeResolver for F
where
    F: for<'a> Fn(
            CustomTypeValidationTarget<'a>,
            &'a Value,
        ) -> Result<(), CustomTypeValidationError>
        + Send
        + Sync,
{
    fn validate<'a>(
        &self,
        target: CustomTypeValidationTarget<'a>,
        value: &'a Value,
    ) -> Result<(), CustomTypeValidationError> {
        self(target, value)
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
    Loaded {
        known: BTreeSet<OwnedTypeIdentity>,
        resolver: CustomTypeValidatorFn,
        enum_lookup: CustomTypeEnumLookupFn,
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
    // Unit tests can inject deliberately malformed routing fixtures. Production
    // construction is single-source through `from_source` below.
    #[cfg(test)]
    pub(crate) fn loaded(
        known: HashSet<OwnedTypeIdentity>,
        resolver: CustomTypeValidatorFn,
    ) -> Self {
        Self {
            state: std::sync::Arc::new(ProviderCustomTypeState::Loaded {
                known: known.into_iter().collect(),
                resolver,
                enum_lookup: Box::new(|_| Ok(None)),
            }),
        }
    }

    pub(crate) fn from_source(source: impl ProviderCustomTypeSource) -> Self {
        let known = source.known_custom_types();
        let source = std::sync::Arc::new(source);
        let validation_source = std::sync::Arc::clone(&source);
        Self {
            state: std::sync::Arc::new(ProviderCustomTypeState::Loaded {
                known,
                resolver: Box::new(move |target, value| validation_source.validate(target, value)),
                enum_lookup: Box::new(move |owned| source.enum_declaration(owned)),
            }),
        }
    }

    pub fn is_loaded(&self) -> bool {
        matches!(
            self.state.as_ref(),
            ProviderCustomTypeState::Loaded {
                known: _known,
                resolver: _resolver,
                enum_lookup: _enum_lookup,
            }
        )
    }

    pub fn known(&self) -> &BTreeSet<OwnedTypeIdentity> {
        match self.state.as_ref() {
            ProviderCustomTypeState::Unloaded => {
                static EMPTY: std::sync::LazyLock<BTreeSet<OwnedTypeIdentity>> =
                    std::sync::LazyLock::new(BTreeSet::new);
                &EMPTY
            }
            ProviderCustomTypeState::Loaded {
                known,
                resolver: _resolver,
                enum_lookup: _enum_lookup,
            } => known,
        }
    }
}

impl CustomTypeResolver for ProviderCustomTypes {
    fn validate<'a>(
        &self,
        target: CustomTypeValidationTarget<'a>,
        value: &'a Value,
    ) -> Result<(), CustomTypeValidationError> {
        match self.state.as_ref() {
            ProviderCustomTypeState::Loaded {
                known: _known,
                resolver,
                enum_lookup: _enum_lookup,
            } => resolver(target, value),
            ProviderCustomTypeState::Unloaded => Err(CustomTypeValidationError::ProvidersNotLoaded),
        }
    }

    fn enum_declaration(
        &self,
        owned: &OwnedTypeIdentity,
    ) -> Result<Option<AttributeType>, CustomTypeValidationError> {
        match self.state.as_ref() {
            ProviderCustomTypeState::Loaded {
                known: _known,
                resolver: _resolver,
                enum_lookup,
            } => enum_lookup(owned),
            ProviderCustomTypeState::Unloaded => Err(CustomTypeValidationError::ProvidersNotLoaded),
        }
    }
}

/// Signature for a decryptor function.
///
/// Takes `(ciphertext, optional_key)` and returns the decrypted plaintext or an error.
pub type DecryptorFn = Box<dyn Fn(&str, Option<&str>) -> Result<String, String> + Send + Sync>;

/// Configuration for the parser, allowing providers to inject behavior.
///
/// This replaces the global `Mutex`-based decryptor registration and enables
/// provider-specific validators (e.g., ARN, availability_zone) to be injected
/// from provider crates while being callable during parsing.
#[derive(Default)]
pub struct ProviderContext {
    /// Optional decryptor for the `decrypt()` built-in function.
    pub decryptor: Option<DecryptorFn>,
    /// Provider custom types are either unavailable or loaded together with
    /// both their owner-qualified recognition keys and resolver. When loaded,
    /// unknown PascalCase type names are rejected; the default unloaded state
    /// preserves bootstrap parsing before provider registration.
    pub custom_types: ProviderCustomTypes,
    /// Resource kinds loaded from provider schemas, keyed by `(provider,
    /// resource_type)` such as `("aws", "iam.Role")`.
    pub resource_types: HashSet<(String, String)>,
}

impl ProviderContext {
    /// Explicit bootstrap authority for callers that have no provider catalog.
    pub fn unloaded() -> &'static Self {
        static CONTEXT: std::sync::LazyLock<ProviderContext> =
            std::sync::LazyLock::new(ProviderContext::default);
        &CONTEXT
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
/// let _ = LoadedProviderContext(std::sync::Arc::new(ProviderContext::default()));
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
                    resolver: Box::new(|_target, _value| Ok(())),
                    enum_lookup: Box::new(|_owned| Ok(None)),
                }),
            },
            ..ProviderContext::default()
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
            ProviderCustomTypeState::Loaded {
                known,
                resolver: _resolver,
                enum_lookup: _enum_lookup,
            } => formatter
                .debug_struct("Loaded")
                .field("known", known)
                .field("resolver", &"...")
                .finish(),
        }
    }
}
