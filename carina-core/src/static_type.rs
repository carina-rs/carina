//! Static-type states carried outside [`crate::parser::TypeExpr`].

use crate::parser::TypeExpr;
use crate::resource::PathSegment;

#[derive(Debug, Clone, PartialEq)]
pub enum StaticType<R, K = TypeExpr> {
    Known(K),
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

/// A type expression that successfully passed consumer-side resolution.
///
/// The private field prevents parser-phase [`TypeExpr`] values from being
/// relabeled as consumer-resolved by downstream crates. Consumer resolution
/// returns this proof as the `Known` payload of [`ConsumerStaticType`].
///
/// ```compile_fail
/// use carina_core::parser::TypeExpr;
/// use carina_core::static_type::{ConsumerResolvedType, ConsumerStaticType, StaticType};
///
/// let _forged: ConsumerStaticType =
///     StaticType::Known(ConsumerResolvedType(TypeExpr::String));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ConsumerResolvedType(TypeExpr);

impl ConsumerResolvedType {
    pub(crate) fn new(type_expr: TypeExpr) -> Self {
        Self(type_expr)
    }

    pub fn type_expr(&self) -> &TypeExpr {
        &self.0
    }
}

impl<R> StaticType<R, ConsumerResolvedType> {
    pub fn known(&self) -> Option<&TypeExpr> {
        match self {
            Self::Known(resolved) => Some(resolved.type_expr()),
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
    original: TypeExpr,
    resolution_error: StaticTypeResolutionError,
}

impl UnnameableInConsumer {
    pub(crate) fn new(original: TypeExpr, resolution_error: StaticTypeResolutionError) -> Self {
        Self {
            original,
            resolution_error,
        }
    }

    pub fn original(&self) -> &TypeExpr {
        &self.original
    }

    pub fn resolution_error(&self) -> &StaticTypeResolutionError {
        &self.resolution_error
    }
}

/// Parser/local declarations carry [`TypeExpr`] directly; only consumer
/// resolution outputs can carry [`ConsumerStaticType`] and its unchecked state.
///
/// ```compile_fail
/// use carina_core::parser::TypeExpr;
/// use carina_core::static_type::{
///     ConsumerStaticType, StaticType, StaticTypeResolutionError, UnnameableInConsumer,
/// };
///
/// let _forged: ConsumerStaticType = StaticType::Unchecked(UnnameableInConsumer {
///     original: TypeExpr::String,
///     resolution_error: StaticTypeResolutionError::UnknownCustomType {
///         name: "forged".to_string(),
///     },
/// });
/// ```
pub type ConsumerStaticType = StaticType<UnnameableInConsumer, ConsumerResolvedType>;

/// User-facing label for a declared type. An unchecked consumer boundary is
/// still displayed with its authored declaration; unchecked controls type
/// enforcement, not whether the declaration remains visible.
pub fn consumer_static_type_label(static_type: &ConsumerStaticType) -> &TypeExpr {
    match static_type {
        StaticType::Known(resolved) => resolved.type_expr(),
        StaticType::Unchecked(reason) => reason.original(),
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
    #[error("operator '{op}': {side} operand must be Int, got {actual}")]
    InvalidIntegerArithmeticOperand {
        op: crate::integer_arithmetic::IntegerOp,
        side: crate::integer_arithmetic::IntegerOperandSide,
        actual: String,
        operator_span: crate::integer_arithmetic::IntegerOperatorSpan,
    },
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

pub type ConsumerInferredStaticType =
    StaticType<ConsumerInferredUncheckedReason, ConsumerResolvedType>;

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
            Self::Declared(StaticType::Known(resolved)) => Some(resolved.type_expr()),
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
