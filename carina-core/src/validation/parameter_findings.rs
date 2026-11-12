//! Typed failures behind the attribute/export string compatibility APIs.
use super::{HandleSinkError, typecheck};
use crate::parser::{ResourceTypePath, retained::MissingEvidence};
use crate::schema::MismatchTypeNames;
use crate::static_type::StaticTypeResolutionError;

#[derive(Debug, thiserror::Error)]
pub enum ParameterValueError {
    #[error("{0}")]
    Type(typecheck::TypeExprValueError),
    #[error(transparent)]
    Resolution(#[from] Box<crate::resolver::ResolutionError>),
    #[error(transparent)]
    Evidence(#[from] MissingEvidence),
    #[error(transparent)]
    TypeResolution(#[from] StaticTypeResolutionError),
    #[error(transparent)]
    Handle(#[from] HandleSinkError),
    #[error("expected resource handle {expected}; got {actual}")]
    ExpectedHandle {
        expected: ResourceTypePath,
        actual: &'static str,
    },
    #[error("expected {}; got resource handle {}", .0.expected(), .0.actual())]
    HandleForValue(MismatchTypeNames),
}

impl From<typecheck::TypeExprValueError> for ParameterValueError {
    fn from(error: typecheck::TypeExprValueError) -> Self {
        Self::Type(error)
    }
}

#[derive(Debug)]
pub enum ParameterValueFinding {
    Invalid {
        label: String,
        error: ParameterValueError,
        line: Option<usize>,
    },
    SecretExport {
        name: String,
    },
}

impl ParameterValueFinding {
    pub(super) fn invalid(
        kind: &str,
        name: &str,
        error: impl Into<ParameterValueError>,
        line: Option<usize>,
    ) -> Self {
        Self::Invalid {
            label: format!("{kind} '{name}'"),
            error: error.into(),
            line,
        }
    }
}

impl std::fmt::Display for ParameterValueFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SecretExport { name } => {
                write!(formatter, "export '{name}' cannot hold a secret value")
            }
            Self::Invalid { label, error, line } => {
                write!(formatter, "{label}: {error}")?;
                if let Some(line) = line {
                    write!(formatter, " at line {line}")?;
                }
                Ok(())
            }
        }
    }
}
impl std::error::Error for ParameterValueFinding {}
