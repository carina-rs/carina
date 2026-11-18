#![allow(
    clippy::wildcard_enum_match_arm,
    reason = "error rendering and classification, not conversion: a catch-all here cannot drop a protocol field"
)]

use crate::provider_text::DiagnosticText;
use crate::schema::TypeIdentity;
use std::error::Error;
use std::fmt;

/// A key or index locating a nested value in a provider input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WirePathSegment {
    Key(WirePathKey),
    Index(usize),
    /// The failing value is inside an opaque secret; its inner path is hidden.
    Secret,
}

/// A value's map key retained for secret-path traversal, with a separate safe
/// diagnostic spelling. Sanitizing diagnostic text must not change data lookup.
#[derive(Clone, PartialEq, Eq)]
pub struct WirePathKey {
    key: String,
    diagnostic: DiagnosticText,
}

impl fmt::Debug for WirePathKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.diagnostic.as_str(), f)
    }
}

impl WirePathKey {
    pub fn as_str(&self) -> &str {
        self.diagnostic.as_str()
    }

    pub(crate) fn lookup_key(&self) -> &str {
        &self.key
    }
}

impl From<String> for WirePathKey {
    fn from(key: String) -> Self {
        Self {
            diagnostic: DiagnosticText::new(&key),
            key,
        }
    }
}

impl From<&str> for WirePathKey {
    fn from(key: &str) -> Self {
        key.to_string().into()
    }
}

impl fmt::Display for WirePathKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.diagnostic.fmt(f)
    }
}

/// A core value or type cannot be represented by the provider protocol.
#[derive(Debug)]
pub enum ProtocolConvertError {
    DurationOutOfRange,
    InvalidRedactedSecretDigest,
    UnavailableSecret(crate::resource::UnavailableSecretError),
    NonWireValue {
        variant: &'static str,
    },
    NonWireAttributeType {
        kind: &'static str,
    },
    ValidatorWithoutIdentity {
        kind: &'static str,
    },
    OpenEnumWithoutNamespace {
        identity: TypeIdentity,
    },
    OpenEnumWithAliases {
        identity: TypeIdentity,
    },
    EmptyCustomEnumNamespace {
        name: String,
    },
    ClosedEnumWithTransform {
        identity: TypeIdentity,
    },
    ClosedEnumUnsupportedBase {
        identity: TypeIdentity,
        base_kind: String,
    },
    LegacyCustomUnsupportedBase {
        identity: String,
        base_kind: &'static str,
    },
    LegacyCustomUnsupportedMetadata {
        identity: String,
        field: &'static str,
        base_kind: &'static str,
    },
    Context {
        context: DiagnosticText,
        source: Box<ProtocolConvertError>,
    },
    ValuePath {
        segment: WirePathSegment,
        error: Box<ProtocolConvertError>,
    },
}

impl fmt::Display for ProtocolConvertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DurationOutOfRange => formatter
                .write_str("duration in seconds does not fit the provider protocol integer domain"),
            Self::UnavailableSecret(error) => fmt::Display::fmt(error, formatter),
            Self::InvalidRedactedSecretDigest => {
                formatter.write_str("persisted state contains an invalid redacted-secret digest")
            }
            Self::NonWireValue { variant } => write!(
                formatter,
                "core value variant `{variant}` cannot cross the ordinary provider protocol boundary"
            ),
            Self::NonWireAttributeType { kind } => write!(
                formatter,
                "core attribute type `{kind}` has no provider protocol representation"
            ),
            Self::ValidatorWithoutIdentity { kind } => write!(
                formatter,
                "validator callback on {kind} is missing its required TypeIdentity"
            ),
            Self::OpenEnumWithoutNamespace { identity } => write!(
                formatter,
                "open enum `{identity}` has no namespace for the protocol CustomEnum record",
                identity = DiagnosticText::new(identity.to_string())
            ),
            Self::OpenEnumWithAliases { identity } => write!(
                formatter,
                "open enum `{identity}` carries DSL aliases that CustomEnum cannot represent",
                identity = DiagnosticText::new(identity.to_string())
            ),
            Self::EmptyCustomEnumNamespace { name } => write!(
                formatter,
                "protocol CustomEnum `{}` must have a non-empty namespace",
                DiagnosticText::new(name)
            ),
            Self::ClosedEnumWithTransform { identity } => write!(
                formatter,
                "closed enum `{identity}` carries a DSL transform that StringEnum cannot represent",
                identity = DiagnosticText::new(identity.to_string())
            ),
            Self::ClosedEnumUnsupportedBase {
                identity,
                base_kind,
            } => write!(
                formatter,
                "closed enum `{identity}` has {base_kind} base metadata that StringEnum cannot represent",
                identity = DiagnosticText::new(identity.to_string()),
                base_kind = DiagnosticText::new(base_kind)
            ),
            Self::LegacyCustomUnsupportedBase {
                identity,
                base_kind,
            } => write!(
                formatter,
                "legacy Custom `{identity}` has {base_kind} base metadata that core cannot preserve with the enclosing identity",
                identity = DiagnosticText::new(identity.to_string())
            ),
            Self::LegacyCustomUnsupportedMetadata {
                identity,
                field,
                base_kind,
            } => write!(
                formatter,
                "legacy Custom `{identity}` carries `{field}` metadata that a {base_kind} base cannot represent",
                identity = DiagnosticText::new(identity.to_string())
            ),
            Self::Context {
                context,
                source: _source,
            } => formatter.write_str(context),
            Self::ValuePath { segment, .. } => match segment {
                WirePathSegment::Key(key) => write!(formatter, "map key `{key}`"),
                WirePathSegment::Index(index) => write!(formatter, "list index {index}"),
                WirePathSegment::Secret => formatter.write_str("secret value"),
            },
        }
    }
}

impl Error for ProtocolConvertError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context {
                context: _context,
                source,
            } => Some(source.as_ref()),
            Self::ValuePath { error, .. } => Some(error.as_ref()),
            Self::DurationOutOfRange => None,
            Self::InvalidRedactedSecretDigest => None,
            Self::UnavailableSecret(error) => error.source(),
            Self::NonWireValue { variant: _variant } => None,
            Self::NonWireAttributeType { kind: _kind } => None,
            Self::ValidatorWithoutIdentity { kind: _kind } => None,
            Self::OpenEnumWithoutNamespace {
                identity: _identity,
            } => None,
            Self::OpenEnumWithAliases {
                identity: _identity,
            } => None,
            Self::EmptyCustomEnumNamespace { name: _name } => None,
            Self::ClosedEnumWithTransform {
                identity: _identity,
            } => None,
            Self::ClosedEnumUnsupportedBase {
                identity: _identity,
                base_kind: _base_kind,
            } => None,
            Self::LegacyCustomUnsupportedBase {
                identity: _identity,
                base_kind: _field,
            } => None,
            Self::LegacyCustomUnsupportedMetadata {
                identity: _identity,
                field: _field,
                base_kind: _base_kind,
            } => None,
        }
    }
}

impl crate::resource::InsideSecret for ProtocolConvertError {
    fn inside_secret(self) -> Self {
        let leaf = match self.into_leaf_error() {
            error @ (Self::DurationOutOfRange
            | Self::InvalidRedactedSecretDigest
            | Self::NonWireValue { .. }
            | Self::NonWireAttributeType { .. }
            | Self::ValidatorWithoutIdentity { .. }) => error,
            _ => Self::NonWireValue { variant: "Secret" },
        };
        leaf.at_value_path(WirePathSegment::Secret)
    }
}

impl ProtocolConvertError {
    /// Structured container path, in outermost-to-innermost order.
    pub fn value_path(&self) -> Vec<WirePathSegment> {
        match self {
            Self::ValuePath { segment, error } => {
                let mut path = vec![segment.clone()];
                path.extend(error.value_path());
                path
            }
            Self::Context { source, .. } => source.value_path(),
            _ => Vec::new(),
        }
    }

    /// The typed conversion reason beneath contextual wrappers.
    #[cfg(test)]
    pub fn leaf_error(&self) -> &Self {
        match self {
            Self::ValuePath { error, .. } => error.leaf_error(),
            Self::Context { source, .. } => source.leaf_error(),
            error => error,
        }
    }

    pub(crate) fn into_leaf_error(self) -> Self {
        match self {
            Self::ValuePath { error, .. } => error.into_leaf_error(),
            Self::Context { source, .. } => source.into_leaf_error(),
            error => error,
        }
    }

    pub(crate) fn at_value_path(self, segment: WirePathSegment) -> Self {
        Self::ValuePath {
            segment,
            error: Box::new(self),
        }
    }

    pub(super) fn context(self, context: impl Into<String>) -> Self {
        Self::Context {
            context: DiagnosticText::new(context),
            source: Box::new(self),
        }
    }
}

pub(crate) fn protocol_knowledge_error(
    error: crate::resource::knowledge::KnowledgeError,
) -> ProtocolConvertError {
    use crate::resource::knowledge::KnowledgeError;
    match error {
        KnowledgeError::Unavailable { .. } => ProtocolConvertError::NonWireValue {
            variant: "Unavailable",
        },
        KnowledgeError::UnavailableHistoricalSecret(error) => {
            ProtocolConvertError::UnavailableSecret(error)
        }
        KnowledgeError::InvalidStage { .. }
        | KnowledgeError::Cycle { .. }
        | KnowledgeError::UnresolvedAtBoundary { .. } => ProtocolConvertError::NonWireValue {
            variant: "Deferred",
        },
    }
}
