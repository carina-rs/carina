//! Built-in backend declarations and their sole configuration admission owner.

use std::fmt;
use std::sync::{Arc, OnceLock};
use thiserror::Error;

use super::*;
use crate::parser::{self, AttributeKey};
use crate::resource::{ConcreteValue, ValueKind};
use crate::schema::{AttributeType, DslTransform, TypeIdentity};

const AWS_PROVIDER_NAME: &str = "aws";

#[path = "github.rs"]
mod github;
pub use github::{
    GitBranch, GitHubBackendSettings, GitHubDestinationError, LeasePath, RepositorySlug, StatePath,
};

/// Typed settings retained by the sole backend admission owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSettings {
    S3,
    Local,
    GitHub(GitHubBackendSettings),
}

/// Canonical attributes and their typed backend-specific interpretation.
#[derive(Debug, Clone)]
pub struct BackendPayload {
    pub(super) attributes: crate::parser::ConfigurationAttributes<Value>,
    pub(super) settings: BackendSettings,
}

/// A built-in destination, independent of installed providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    S3,
    Local,
    GitHub,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Local => "local",
            Self::GitHub => "github",
        }
    }

    pub const ALL: [Self; 3] = [Self::S3, Self::Local, Self::GitHub];

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Declaration-backed editor metadata; no provider loading is needed.
    pub fn fields(
        self,
    ) -> impl Iterator<
        Item = (
            &'static str,
            &'static crate::schema::CompiledAttributeType,
            bool,
        ),
    > {
        let declaration = self.declaration();
        declaration
            .configuration
            .schema
            .parts()
            .iter()
            .map(move |(name, ty)| {
                (
                    name.as_str(),
                    ty,
                    declaration.required.contains(&name.as_str()),
                )
            })
    }

    pub fn documentation(self) -> String {
        let mut text = format!(
            "## backend {}\n\nState backend configuration.\n",
            self.name()
        );
        for (name, ty, required) in self.fields() {
            text.push_str(&format!(
                "\n- `{name}`: {} ({})",
                ty.type_name(),
                if required { "required" } else { "optional" }
            ));
        }
        text
    }

    fn declaration(self) -> &'static BackendDeclaration {
        static S3: OnceLock<BackendDeclaration> = OnceLock::new();
        static LOCAL: OnceLock<BackendDeclaration> = OnceLock::new();
        static GITHUB: OnceLock<BackendDeclaration> = OnceLock::new();
        let slot = match self {
            Self::S3 => &S3,
            Self::Local => &LOCAL,
            Self::GitHub => &GITHUB,
        };
        slot.get_or_init(|| {
            let fields = match self {
                Self::S3 => vec![
                    ("bucket", AttributeType::string(), true),
                    ("key", AttributeType::string(), true),
                    (
                        "region",
                        AttributeType::enum_(
                            TypeIdentity::new(
                                Some(AWS_PROVIDER_NAME),
                                Vec::<String>::new(),
                                "Region",
                            ),
                            None,
                            vec![],
                            None,
                            Some(DslTransform::HyphenToUnderscore),
                        ),
                        false,
                    ),
                    ("encrypt", AttributeType::bool(), false),
                    ("auto_create", AttributeType::bool(), false),
                ],
                Self::Local => vec![("path", AttributeType::string(), false)],
                Self::GitHub => vec![
                    ("repository", AttributeType::string(), true),
                    ("branch", AttributeType::string(), true),
                    ("path", AttributeType::string(), true),
                ],
            };
            let required = fields
                .iter()
                .filter(|(_, _, required)| *required)
                .map(|(name, _, _)| *name)
                .collect();
            let schema = ValidatedConfigSchema::validate(
                fields
                    .into_iter()
                    .map(|(name, ty, _)| (name.to_owned(), ty))
                    .collect(),
            )
            .expect("built-in backend declaration");
            BackendDeclaration {
                configuration: Arc::new(ConfigurationDeclaration {
                    identity: DeclarationId::default(),
                    owner: DeclarationOwner::Backend(self),
                    schema: Arc::new(schema),
                }),
                required,
            }
        })
    }
}
struct BackendDeclaration {
    configuration: Arc<ConfigurationDeclaration>,
    required: Vec<&'static str>,
}

/// Failure to resolve a raw backend block through its declared schema.
#[derive(Debug, Error)]
pub enum BackendConfigError {
    #[error(transparent)]
    UnsupportedBackend(#[from] UnsupportedBackendError),
    #[error(transparent)]
    Invalid(#[from] BackendConfigErrors),
}

/// A backend type with no built-in declaration.
#[derive(Debug, Error)]
#[error("Unsupported backend type: {backend_type}")]
pub struct UnsupportedBackendError {
    backend_type: String,
}

/// Backend failures built only by [`parser::AnalysisTree::admit_backend`].
#[derive(Debug)]
pub enum LocatedBackendError {
    Attribute(parser::Located<BackendAttributeError>),
    Unsupported(parser::Located<UnsupportedBackendError>),
}

/// One typed backend failure when no authored analysis tree is available.
#[derive(Debug, Error)]
pub enum UnlocatedBackendError {
    #[error(transparent)]
    Attribute(#[from] BackendAttributeError),
    #[error(transparent)]
    Unsupported(#[from] UnsupportedBackendError),
}

impl BackendConfigError {
    /// Preserve individual failures when analysis cannot provide locations.
    pub fn into_unlocated(self) -> Vec<UnlocatedBackendError> {
        match self {
            Self::Invalid(errors) => errors
                .errors
                .into_iter()
                .map(UnlocatedBackendError::Attribute)
                .collect(),
            Self::UnsupportedBackend(error) => vec![UnlocatedBackendError::Unsupported(error)],
        }
    }
}

impl<P: PlaceholderKind, T: ValueKind, D, B: BindingAddress, E>
    parser::AnalysisTree<'_, P, T, D, B, E>
where
    D: parser::DeferredForPhaseTraversal<P, T, B>,
{
    /// Admit and locate this tree's backend, if present in a root configuration.
    pub fn admit_backend(
        &self,
    ) -> Option<Result<AdmittedConfiguration<kind::BackendAttribute>, Vec<LocatedBackendError>>>
    {
        if self.is_module_definition() {
            return None;
        }
        let backend = self.file().backend.as_ref()?;
        Some(admit(backend).map_err(|error| {
            error
                .into_unlocated()
                .into_iter()
                .map(|error| match error {
                    UnlocatedBackendError::Attribute(error) => {
                        LocatedBackendError::Attribute(parser::Located::new(
                            self.backend_attribute_location(error.attribute()).clone(),
                            error,
                        ))
                    }
                    UnlocatedBackendError::Unsupported(error) => {
                        LocatedBackendError::Unsupported(parser::Located::new(
                            self.backend_location()
                                .expect("backend declaration provenance")
                                .clone(),
                            error,
                        ))
                    }
                })
                .collect()
        }))
    }
}

/// All attribute-level failures from resolving one backend block.
#[derive(Debug)]
pub struct BackendConfigErrors {
    errors: Vec<BackendAttributeError>,
}

impl BackendConfigErrors {
    fn new(mut errors: Vec<BackendAttributeError>) -> Self {
        debug_assert!(!errors.is_empty());
        errors.sort_by(|left, right| {
            left.attribute()
                .diagnostic_cmp(right.attribute())
                .then_with(|| left.sort_rank().cmp(&right.sort_rank()))
        });
        Self { errors }
    }

    /// Attribute failures in deterministic attribute-name order.
    /// Test-only introspection for structural admission assertions.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn errors(&self) -> &[BackendAttributeError] {
        &self.errors
    }
}

impl fmt::Display for BackendConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index != 0 {
                f.write_str("\n")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for BackendConfigErrors {}

/// One schema failure associated with a backend attribute.
#[derive(Debug, Error)]
pub enum BackendAttributeError {
    #[error("Unknown attribute '{attribute}' for backend type '{backend_type}'")]
    UnknownAttribute {
        backend_type: String,
        attribute: crate::parser::AttributeKey,
    },
    #[error("Missing required attribute: {attribute}")]
    MissingRequiredAttribute {
        backend_type: String,
        attribute: crate::parser::AttributeKey,
    },
    #[error("Invalid attribute '{attribute}' for backend type '{backend_type}': {error}")]
    InvalidAttribute {
        backend_type: String,
        attribute: crate::parser::AttributeKey,
        error: Box<TypeError>,
    },
}

impl BackendAttributeError {
    pub fn attribute(&self) -> &crate::parser::AttributeKey {
        match self {
            Self::UnknownAttribute { attribute, .. }
            | Self::MissingRequiredAttribute { attribute, .. }
            | Self::InvalidAttribute { attribute, .. } => attribute,
        }
    }

    fn sort_rank(&self) -> u8 {
        match self {
            Self::UnknownAttribute { .. } => 0,
            Self::MissingRequiredAttribute { .. } => 1,
            Self::InvalidAttribute { .. } => 2,
        }
    }
}

struct BuiltinLookup;
impl ValidatedCustomTypeLookup for BuiltinLookup {
    fn validate(&self, _: crate::schema::LocallyValidatedCustomType<'_>) -> Result<(), TypeError> {
        // Built-in declarations contain only primitives and an open enum.
        Ok(())
    }
}

fn attribute_error(
    kind: BackendKind,
    key: &AttributeKey,
    error: ConfigurationErrors,
) -> BackendAttributeError {
    BackendAttributeError::InvalidAttribute {
        backend_type: kind.name().into(),
        attribute: key.clone(),
        error: Box::new(
            error
                .errors
                .into_iter()
                .next()
                .expect("a failed backend field has a diagnostic"),
        ),
    }
}

fn invalid_shape<P: PlaceholderKind, B: BindingAddress>(
    kind: BackendKind,
    key: &AttributeKey,
    ty: &crate::schema::CompiledAttributeType,
    value: &Value<P, B>,
) -> BackendAttributeError {
    BackendAttributeError::InvalidAttribute {
        backend_type: kind.name().into(),
        attribute: key.clone(),
        error: Box::new(TypeError::value_type_mismatch(
            ty.type_name(),
            value.type_name(),
        )),
    }
}
fn concrete_setting<P: PlaceholderKind, B: BindingAddress>(
    ty: &crate::schema::CompiledAttributeType,
    value: &Value<P, B>,
) -> bool {
    matches!(
        (ty.kind(), value),
        (
            crate::schema::AttrTypeKind::String { .. },
            Value::Concrete(ConcreteValue::String(_))
        ) | (
            crate::schema::AttrTypeKind::Bool,
            Value::Concrete(ConcreteValue::Bool(_))
        ) | (
            crate::schema::AttrTypeKind::Enum { .. },
            Value::Concrete(ConcreteValue::CanonicalEnum(_)),
        )
    )
}
impl AdmittedConfiguration<kind::BackendAttribute> {
    pub fn string(&self, name: &str) -> Option<&crate::resource::ValueString> {
        match self.field(name)?.value() {
            Value::Concrete(ConcreteValue::String(value)) => Some(value),
            _ => None,
        }
    }
    pub fn boolean(&self, name: &str) -> Option<bool> {
        match self.field(name)?.value() {
            Value::Concrete(ConcreteValue::Bool(value)) => Some(*value),
            _ => None,
        }
    }
    pub fn enum_api_value(&self, name: &str) -> Option<&str> {
        match self.field(name)?.value() {
            Value::Concrete(ConcreteValue::CanonicalEnum(value)) => Some(value.api_value()),
            _ => None,
        }
    }
}

/// Admit every declared field before concrete backend settings can be extracted.
/// The built-in declaration, required fields and concrete-input policy are not
/// caller supplied. Locks, saved plans and URL/default adapters are candidates.
///
/// ```
/// use carina_core::{parser::BackendConfig, validation::admission::configuration::backend};
/// let raw = BackendConfig::<carina_core::resource::NoPlaceholder> { backend_type: "local".into(), attributes: Default::default(), attribute_spans: Default::default() };
/// let admitted = backend::admit(&raw).unwrap();
/// assert!(admitted.attributes().is_empty());
/// ```
/// ```compile_fail
/// use carina_core::validation::admission::{configuration::AdmittedConfiguration, kind::BackendAttribute};
/// fn replace(config: &mut AdmittedConfiguration<BackendAttribute>) {
///     config.attributes().clear(); // forbidden: the admitted payload is immutable
/// }
/// ```
/// ```
/// # use carina_core::validation::admission::{configuration::AdmittedConfiguration, kind::BackendAttribute};
/// # fn replace(config: &mut AdmittedConfiguration<BackendAttribute>) {
/// # }
/// ```
pub fn admit<P: PlaceholderKind, B: BindingAddress>(
    config: &parser::BackendConfig<P, B>,
) -> Result<AdmittedConfiguration<kind::BackendAttribute>, BackendConfigError> {
    let kind = match BackendKind::from_name(&config.backend_type) {
        Some(kind) => kind,
        None => {
            return Err(UnsupportedBackendError {
                backend_type: config.backend_type.clone(),
            }
            .into());
        }
    };
    let backend = kind.declaration();
    let declaration = &backend.configuration;
    let mut errors = Vec::new();
    for attribute in &backend.required {
        if !config.attributes.contains_key(*attribute) {
            errors.push(BackendAttributeError::MissingRequiredAttribute {
                backend_type: kind.name().into(),
                attribute: (*attribute).into(),
            });
        }
    }
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::new([]));
    let lookup = BuiltinLookup;
    let mut github_settings = github::GitHubSettingsBuilder::default();
    let checked = crate::validation::deferred_types::TypeCheckSession::run(&catalog, |_| {
        let mut aggregate = ConfigurationBuilder::new(Arc::clone(declaration), kind);
        for (key, value) in &config.attributes {
            let Some(destination) = declaration.destination::<kind::BackendAttribute>(
                key,
                InstanceIdentity::Backend(kind),
                CatalogRevision(CatalogSnapshot::Backend(declaration.identity)),
            ) else {
                errors.push(BackendAttributeError::UnknownAttribute {
                    backend_type: kind.name().into(),
                    attribute: key.clone(),
                });
                continue;
            };
            let ty = destination.contract.attr();
            // The enum's actual namespace is intentional: backend regions do
            // not use provider-block aliases or depend on provider loading.
            let canonical = match crate::value::canonicalize_provider_config_with_type_and_ancestry(
                AWS_PROVIDER_NAME,
                value.clone(),
                ty,
                crate::schema::empty_defs_for_schema_walks(),
                FieldAncestry::WRITABLE,
            ) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(BackendAttributeError::InvalidAttribute {
                        backend_type: kind.name().into(),
                        attribute: key.clone(),
                        error: Box::new(error),
                    });
                    continue;
                }
            };
            let admission = match field(&destination, &canonical, &lookup) {
                Ok(admission) => admission,
                Err(error) => {
                    errors.push(attribute_error(kind, key, error));
                    continue;
                }
            };
            // Preserve the backend boundary's existing concrete extraction
            // policy, including rejection of pending and Secret wrappers.
            if !concrete_setting(ty, &canonical) {
                errors.push(invalid_shape(kind, key, ty, &canonical));
                continue;
            }
            if kind == BackendKind::GitHub {
                let Value::Concrete(ConcreteValue::String(value)) = &canonical else {
                    unreachable!()
                };
                let value = value.expose_plaintext();
                if let Err(error) = github_settings.place(&destination.contract.attribute, value) {
                    errors.push(BackendAttributeError::InvalidAttribute {
                        backend_type: kind.name().into(),
                        attribute: key.clone(),
                        error: Box::new(TypeError::ValidationFailed {
                            message: error.to_string().into(),
                        }),
                    });
                    continue;
                }
            }
            let placed = match admission {
                ConfigurationAdmission::Ready(placed) => placed,
                ConfigurationAdmission::Unresolved(_) => {
                    errors.push(invalid_shape(kind, key, ty, &canonical));
                    continue;
                }
            };
            // This changes only phase/address parameters on the exact checked
            // concrete value; no replacement payload can accompany a receipt.
            let value = placed
                .value()
                .to_expanded()
                .and_then(Value::try_into_address_free_absolute)
                .expect("concrete backend setting");
            let placed = PlacedValue::admitted(&destination, value, placed.inner.findings);
            aggregate.place(key.clone(), placed);
        }
        aggregate
    });
    let mut deferred = Vec::new();
    let aggregate = checked.publish(&mut deferred);
    assert!(
        deferred.is_empty(),
        "built-in backend declaration has no remote types"
    );
    if !errors.is_empty() {
        return Err(BackendConfigErrors::new(errors).into());
    }
    let settings = match kind {
        BackendKind::GitHub => BackendSettings::GitHub(github_settings.finish()?),
        BackendKind::S3 => BackendSettings::S3,
        BackendKind::Local => BackendSettings::Local,
    };
    Ok(aggregate.admit_payload(settings))
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
