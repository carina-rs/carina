//! Static-type states carried outside [`crate::parser::TypeExpr`].

use crate::parser::TypeExpr;
use crate::resource::PathSegment;

#[derive(Debug, Clone, PartialEq)]
pub enum StaticType<R> {
    Known(TypeExpr),
    Unchecked(R),
}

impl<R> StaticType<R> {
    pub fn known(&self) -> Option<&TypeExpr> {
        match self {
            Self::Known(type_expr) => Some(type_expr),
            Self::Unchecked(_reason) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StaticTypeResolutionError {
    #[error("unknown custom type '{name}'")]
    UnknownCustomType { name: String },
    #[error("unknown custom type '{name}'; suggestion: use '{suggestion}'")]
    UnknownCustomTypeWithSuggestion { name: String, suggestion: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnnameableInConsumer {
    pub original: TypeExpr,
    pub resolution_error: StaticTypeResolutionError,
}

/// Parser/local declarations carry [`TypeExpr`] directly; only consumer
/// resolution outputs can carry [`ConsumerStaticType`] and its unchecked state.
pub type ConsumerStaticType = StaticType<UnnameableInConsumer>;

/// User-facing label for a declared type. An unchecked consumer boundary is
/// still displayed with its authored declaration; unchecked controls type
/// enforcement, not whether the declaration remains visible.
pub fn consumer_static_type_label(static_type: &ConsumerStaticType) -> &TypeExpr {
    match static_type {
        StaticType::Known(type_expr) => type_expr,
        StaticType::Unchecked(reason) => &reason.original,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotInferableReason {
    #[error("missing value")]
    MissingValue,
    #[error("binding `{binding}` is not inferable in this context")]
    NonInferableBinding { binding: String },
    #[error("schema is unavailable for binding `{binding}`")]
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
    #[error(transparent)]
    Failed(InferenceError),
    #[error(transparent)]
    NotInferable(NotInferableReason),
}

impl From<InferenceError> for InferenceIssue {
    fn from(error: InferenceError) -> Self {
        Self::Failed(error)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InferredUncheckedReason {
    InferenceFailed(InferenceError),
    NotInferable(NotInferableReason),
}

impl From<InferenceIssue> for InferredUncheckedReason {
    fn from(issue: InferenceIssue) -> Self {
        match issue {
            InferenceIssue::Failed(error) => Self::InferenceFailed(error),
            InferenceIssue::NotInferable(reason) => Self::NotInferable(reason),
        }
    }
}

pub type InferredStaticType = StaticType<InferredUncheckedReason>;

#[derive(Debug, Clone, PartialEq)]
pub enum ConsumerInferredUncheckedReason {
    InferenceFailed(InferenceError),
    NotInferable(NotInferableReason),
    UnnameableInConsumer(UnnameableInConsumer),
}

impl From<InferredUncheckedReason> for ConsumerInferredUncheckedReason {
    fn from(reason: InferredUncheckedReason) -> Self {
        match reason {
            InferredUncheckedReason::InferenceFailed(error) => Self::InferenceFailed(error),
            InferredUncheckedReason::NotInferable(reason) => Self::NotInferable(reason),
        }
    }
}

pub type ConsumerInferredStaticType = StaticType<ConsumerInferredUncheckedReason>;

#[derive(Debug, Clone, PartialEq)]
pub enum ExportStaticType {
    Declared(TypeExpr),
    Inferred(InferredStaticType),
}

impl ExportStaticType {
    pub fn known(&self) -> Option<&TypeExpr> {
        match self {
            Self::Declared(type_expr) => Some(type_expr),
            Self::Inferred(static_type) => static_type.known(),
        }
    }

    /// Return an explicitly declared, consumer-nameable type. Inferred
    /// types are deliberately not authoritative across an upstream-state
    /// boundary.
    pub fn declared_known(&self) -> Option<&TypeExpr> {
        match self {
            Self::Declared(type_expr) => Some(type_expr),
            Self::Inferred(StaticType::Known(_type_expr)) => None,
            Self::Inferred(StaticType::Unchecked(InferredUncheckedReason::InferenceFailed(
                _error,
            ))) => None,
            Self::Inferred(StaticType::Unchecked(InferredUncheckedReason::NotInferable(
                _reason,
            ))) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConsumerExportStaticType {
    Declared(ConsumerStaticType),
    Inferred(ConsumerInferredStaticType),
}

impl ConsumerExportStaticType {
    pub fn known(&self) -> Option<&TypeExpr> {
        match self {
            Self::Declared(static_type) => static_type.known(),
            Self::Inferred(static_type) => static_type.known(),
        }
    }

    pub fn declared_known(&self) -> Option<&TypeExpr> {
        match self {
            Self::Declared(StaticType::Known(type_expr)) => Some(type_expr),
            Self::Declared(StaticType::Unchecked(_reason)) => None,
            Self::Inferred(StaticType::Known(_type_expr)) => None,
            Self::Inferred(StaticType::Unchecked(
                ConsumerInferredUncheckedReason::InferenceFailed(_error),
            )) => None,
            Self::Inferred(StaticType::Unchecked(
                ConsumerInferredUncheckedReason::NotInferable(_reason),
            )) => None,
            Self::Inferred(StaticType::Unchecked(
                ConsumerInferredUncheckedReason::UnnameableInConsumer(_reason),
            )) => None,
        }
    }
}
