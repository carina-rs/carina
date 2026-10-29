//! Static-type states carried outside [`crate::parser::TypeExpr`].

use crate::parser::TypeExpr;
use crate::resource::PathSegment;

#[derive(Debug, Clone, PartialEq)]
pub enum StaticType<R> {
    Known(TypeExpr),
    Unchecked(R),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TypeResolutionError {
    #[error("unknown custom type '{name}'")]
    UnknownCustomType { name: String },
    #[error("unknown custom type '{name}'; suggestion: use '{suggestion}'")]
    UnknownCustomTypeWithSuggestion { name: String, suggestion: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnnameableInConsumer {
    pub original: TypeExpr,
    pub resolution_error: TypeResolutionError,
}

pub type ConsumerStaticType = StaticType<UnnameableInConsumer>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotInferableReason {
    MissingValue,
    NonInferableBinding { binding: String },
    SchemaUnavailable { binding: String },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InferenceError {
    #[error("{reason}")]
    UnknownType { reason: String },
    #[error("{reason}")]
    HeterogeneousCollection { reason: String },
    #[error("coalesce default type `{default}` does not match left type `{left}`")]
    IncompatibleCoalesceTypes { left: String, default: String },
    #[error("unknown binding `{binding}`")]
    UnknownBinding { binding: String },
    #[error("unknown attribute `{attribute}` on `{binding}`")]
    UnknownAttribute { binding: String, attribute: String },
    #[error("cannot apply access {step} to {base_type}")]
    InvalidAccess {
        base_type: String,
        step: PathSegment,
    },
    #[error("cyclic composition binding chain: {}", cycle.join(" -> "))]
    CyclicVirtualBinding { cycle: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InferenceIssue {
    #[error("{reason}")]
    UnknownType { reason: String },
    #[error("{reason}")]
    HeterogeneousCollection { reason: String },
    #[error("coalesce default type `{default}` does not match left type `{left}`")]
    IncompatibleCoalesceTypes { left: String, default: String },
    #[error("unknown binding `{binding}`")]
    UnknownBinding { binding: String },
    #[error("binding `{binding}` is not inferable in this context")]
    NonInferableBinding { binding: String },
    #[error("unknown attribute `{attribute}` on `{binding}`")]
    UnknownAttribute { binding: String, attribute: String },
    #[error("schema is unavailable for binding `{binding}`")]
    SchemaUnavailable { binding: String },
    #[error("cannot apply access {step} to {base_type}")]
    InvalidAccess {
        base_type: String,
        step: PathSegment,
    },
    #[error("cyclic composition binding chain: {}", cycle.join(" -> "))]
    CyclicVirtualBinding { cycle: Vec<String> },
}

impl InferenceIssue {
    pub fn into_unchecked_reason(self) -> InferredUncheckedReason {
        match self {
            Self::UnknownType { reason } => {
                InferredUncheckedReason::InferenceFailed(InferenceError::UnknownType { reason })
            }
            Self::HeterogeneousCollection { reason } => {
                InferredUncheckedReason::InferenceFailed(InferenceError::HeterogeneousCollection {
                    reason,
                })
            }
            Self::IncompatibleCoalesceTypes { left, default } => {
                InferredUncheckedReason::InferenceFailed(
                    InferenceError::IncompatibleCoalesceTypes { left, default },
                )
            }
            Self::UnknownBinding { binding } => {
                InferredUncheckedReason::InferenceFailed(InferenceError::UnknownBinding { binding })
            }
            Self::NonInferableBinding { binding } => {
                InferredUncheckedReason::NotInferable(NotInferableReason::NonInferableBinding {
                    binding,
                })
            }
            Self::UnknownAttribute { binding, attribute } => {
                InferredUncheckedReason::InferenceFailed(InferenceError::UnknownAttribute {
                    binding,
                    attribute,
                })
            }
            Self::SchemaUnavailable { binding } => {
                InferredUncheckedReason::NotInferable(NotInferableReason::SchemaUnavailable {
                    binding,
                })
            }
            Self::InvalidAccess { base_type, step } => {
                InferredUncheckedReason::InferenceFailed(InferenceError::InvalidAccess {
                    base_type,
                    step,
                })
            }
            Self::CyclicVirtualBinding { cycle } => {
                InferredUncheckedReason::InferenceFailed(InferenceError::CyclicVirtualBinding {
                    cycle,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InferredUncheckedReason {
    InferenceFailed(InferenceError),
    NotInferable(NotInferableReason),
    UnnameableInConsumer(UnnameableInConsumer),
}

pub type InferredStaticType = StaticType<InferredUncheckedReason>;

#[derive(Debug, Clone, PartialEq)]
pub enum ExportStaticType {
    Declared(ConsumerStaticType),
    Inferred(InferredStaticType),
}
