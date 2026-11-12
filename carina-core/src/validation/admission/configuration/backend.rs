//! Built-in backend declarations and their sole configuration admission owner.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, OnceLock};
use thiserror::Error;

use super::*;
use crate::parser::{self, AttributeKey};
use crate::resource::ConcreteValue;
use crate::schema::{AttributeType, DslTransform, TypeIdentity};

const AWS_PROVIDER_NAME: &str = "aws";
const AWS_REGION_TYPE_NAME: &str = "aws.Region";

/// A built-in destination, independent of installed providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    S3,
    Local,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Local => "local",
        }
    }

    fn declaration(self) -> &'static Arc<ConfigurationDeclaration> {
        static S3: OnceLock<Arc<ConfigurationDeclaration>> = OnceLock::new();
        static LOCAL: OnceLock<Arc<ConfigurationDeclaration>> = OnceLock::new();
        let (slot, schema): (_, fn() -> ValidatedConfigSchema) = match self {
            Self::S3 => (&S3, s3_schema),
            Self::Local => (&LOCAL, local_schema),
        };
        slot.get_or_init(|| {
            Arc::new(ConfigurationDeclaration {
                identity: DeclarationId::default(),
                owner: DeclarationOwner::Backend(self),
                schema: Arc::new(schema()),
            })
        })
    }
}

/// Failure to resolve a raw backend block through its declared schema.
#[derive(Debug, Error)]
pub enum BackendConfigError {
    #[error("Unsupported backend type: {0}")]
    UnsupportedBackend(String),
    #[error(transparent)]
    Invalid(#[from] BackendConfigErrors),
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

fn s3_schema() -> ValidatedConfigSchema {
    ValidatedConfigSchema::validate(BTreeMap::from([
        ("bucket".to_string(), AttributeType::string()),
        ("key".to_string(), AttributeType::string()),
        (
            "region".to_string(),
            AttributeType::enum_(
                TypeIdentity::new(Some(AWS_PROVIDER_NAME), Vec::<String>::new(), "Region"),
                None,
                Vec::new(),
                None,
                Some(DslTransform::HyphenToUnderscore),
            ),
        ),
        ("encrypt".to_string(), AttributeType::bool()),
        ("auto_create".to_string(), AttributeType::bool()),
    ]))
    .expect("the built-in S3 backend config schema must be valid")
}

fn local_schema() -> ValidatedConfigSchema {
    ValidatedConfigSchema::validate(BTreeMap::from([(
        "path".to_string(),
        AttributeType::string(),
    )]))
    .expect("the built-in local backend config schema must be valid")
}

struct BuiltinLookup;
impl ValidatedCustomTypeLookup for BuiltinLookup {
    fn validate(&self, _: crate::schema::LocallyValidatedCustomType<'_>) -> Result<(), TypeError> {
        // Built-in declarations contain only primitives and an open enum.
        Ok(())
    }
}

fn invalid_shape<P: PlaceholderKind, B: BindingAddress>(
    kind: BackendKind,
    key: &AttributeKey,
    value: &Value<P, B>,
) -> BackendAttributeError {
    let expected = match (kind, key.transport_identity()) {
        (BackendKind::S3, "region") => AWS_REGION_TYPE_NAME,
        (BackendKind::S3, "encrypt" | "auto_create") => "Bool",
        _ => "String",
    };
    BackendAttributeError::InvalidAttribute {
        backend_type: kind.name().into(),
        attribute: key.clone(),
        error: Box::new(TypeError::value_type_mismatch(
            expected.into(),
            value.type_name(),
        )),
    }
}

fn concrete_setting<P: PlaceholderKind, B: BindingAddress>(
    kind: BackendKind,
    key: &AttributeKey,
    value: &Value<P, B>,
) -> bool {
    match (kind, key.transport_identity()) {
        (BackendKind::S3, "region") => {
            matches!(value, Value::Concrete(ConcreteValue::CanonicalEnum(_)))
        }
        (BackendKind::S3, "encrypt" | "auto_create") => {
            matches!(value, Value::Concrete(ConcreteValue::Bool(_)))
        }
        _ => matches!(value, Value::Concrete(ConcreteValue::String(_))),
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
/// ```compile_fail,E0596
/// use carina_core::validation::admission::{configuration::AdmittedConfiguration, kind::BackendAttribute};
/// fn replace(config: &mut AdmittedConfiguration<BackendAttribute>) { config.attributes().clear(); }
/// ```
pub fn admit<P: PlaceholderKind, B: BindingAddress>(
    config: &parser::BackendConfig<P, B>,
) -> Result<AdmittedConfiguration<kind::BackendAttribute>, BackendConfigError> {
    let kind = match config.backend_type.as_str() {
        "s3" => BackendKind::S3,
        "local" => BackendKind::Local,
        other => return Err(BackendConfigError::UnsupportedBackend(other.into())),
    };
    let declaration = kind.declaration();
    let mut errors = Vec::new();
    if kind == BackendKind::S3 {
        for attribute in ["bucket", "key"] {
            if !config.attributes.contains_key(attribute) {
                errors.push(BackendAttributeError::MissingRequiredAttribute {
                    backend_type: kind.name().into(),
                    attribute: attribute.into(),
                });
            }
        }
    }
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::new([]));
    let lookup = BuiltinLookup;
    let checked = crate::validation::deferred_types::TypeCheckSession::run(&catalog, |session| {
        let mut payload = ConfigurationAttributes::new();
        let mut fields = Vec::new();
        for (key, value) in &config.attributes {
            let Some(destination) = declaration.destination_for::<kind::BackendAttribute>(
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
            // The enum's actual namespace is intentional: backend regions do
            // not use provider-block aliases or depend on provider loading.
            let canonical = match key
                .schema_type(&declaration.schema)
                .expect("declared backend field")
                .canonicalize_provider_config(AWS_PROVIDER_NAME, value.clone())
            {
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
            let admission = match field(&destination, &canonical, session, &lookup, false) {
                Ok(admission) => admission,
                Err(error) => {
                    let error = match error.into_resolution_error() {
                        ResolutionError::AdmissionConstraints(findings) => {
                            findings.findings()[0].error.clone()
                        }
                        _ => {
                            errors.push(invalid_shape(kind, key, &canonical));
                            continue;
                        }
                    };
                    errors.push(BackendAttributeError::InvalidAttribute {
                        backend_type: kind.name().into(),
                        attribute: key.clone(),
                        error: Box::new(error),
                    });
                    continue;
                }
            };
            // Preserve the backend boundary's existing concrete extraction
            // policy, including rejection of pending and Secret wrappers.
            if !concrete_setting(kind, key, &canonical) {
                errors.push(invalid_shape(kind, key, &canonical));
                continue;
            }
            let Admission::Ready(placed) = admission else {
                errors.push(invalid_shape(kind, key, &canonical));
                continue;
            };
            // This changes only phase/address parameters on the exact checked
            // concrete value; no replacement payload can accompany a receipt.
            let value = placed
                .value()
                .to_expanded()
                .and_then(Value::try_into_address_free_absolute)
                .expect("concrete backend setting");
            let placed = PlacedValue::admitted(&destination, value, placed.inner.findings);
            let mut slot = TypedSlot::new(destination);
            slot.place(placed.clone())
                .expect("backend declaration slot");
            payload.insert(key.clone(), placed.value().clone());
            fields.push(placed);
        }
        (payload, fields)
    });
    let mut deferred = Vec::new();
    let (payload, fields) = checked.publish(&mut deferred);
    assert!(
        deferred.is_empty(),
        "built-in backend declaration has no remote types"
    );
    if !errors.is_empty() {
        return Err(BackendConfigErrors::new(errors).into());
    }
    Ok(AdmittedConfiguration {
        payload,
        fields,
        declaration: Arc::clone(declaration),
        instance: kind,
    })
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
