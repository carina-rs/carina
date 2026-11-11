//! Static conversion syntax and diagnostics. Runtime values carry no conversions.

use crate::parser::{DeclarationLocation, ProviderContext, ResolvedSchemaType, TypeExpr};
use crate::resource::InsideSecret;
use crate::schema::{AttrTypeKind, CompiledAttributeType, RawShape, RefinedScalarKind};

/// A path authored in a conversion's type position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionTypePath(String);

impl ConversionTypePath {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Static failures never authorize a destination value check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionErrorKind {
    Destination,
    AmbiguousDestination,
    /// Unsupported authored placement, including boundary placeholders until B2.
    UnsupportedPosition,
    Unused,
    Cycle,
    Target,
    Source,
    ScalarKind,
    Disjoint,
}

#[derive(Clone, PartialEq)]
enum ConversionCause {
    Validation(crate::parser::CustomTypeValidationError),
    TypeResolution(crate::static_type::StaticTypeResolutionError),
}

impl ConversionCause {
    fn error(&self) -> &(dyn std::error::Error + 'static) {
        match self {
            Self::Validation(error) => error,
            Self::TypeResolution(error) => error,
        }
    }
}

/// A located, structured error; it holds no runtime value or admission receipt.
#[derive(Clone, PartialEq)]
pub struct ConversionError {
    kind: ConversionErrorKind,
    location: DeclarationLocation,
    message: String,
    cause: Option<std::sync::Arc<ConversionCause>>,
}

impl Eq for ConversionError {}
impl std::fmt::Debug for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversionError")
            .field("kind", &self.kind)
            .field("location", &self.location)
            .field("message", &self.message)
            .finish()
    }
}
impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ConversionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.as_deref().map(ConversionCause::error)
    }
}
impl ConversionError {
    pub fn kind(&self) -> ConversionErrorKind {
        self.kind
    }
    pub fn location(&self) -> &DeclarationLocation {
        &self.location
    }
    pub(crate) fn new(
        kind: ConversionErrorKind,
        location: &DeclarationLocation,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            location: location.clone(),
            message: message.into(),
            cause: None,
        }
    }
    fn validation(
        location: &DeclarationLocation,
        error: crate::parser::CustomTypeValidationError,
    ) -> Self {
        let mut failure = Self::new(
            ConversionErrorKind::Target,
            location,
            "cannot resolve conversion declaration",
        );
        failure.cause = Some(std::sync::Arc::new(ConversionCause::Validation(error)));
        failure
    }
}
impl InsideSecret for ConversionError {
    fn inside_secret(mut self) -> Self {
        self.message = "checked conversion failed: (secret) failed validation".into();
        self.cause = self.cause.take().map(|cause| {
            std::sync::Arc::new(match (*cause).clone() {
                ConversionCause::Validation(error) => {
                    ConversionCause::Validation(error.inside_secret())
                }
                error @ ConversionCause::TypeResolution(_) => error,
            })
        });
        self
    }
}

pub fn resolve_target(
    path: &ConversionTypePath,
    location: &DeclarationLocation,
    config: &ProviderContext,
) -> Result<ResolvedSchemaType, ConversionError> {
    let fail = || {
        ConversionError::new(
            ConversionErrorKind::Target,
            location,
            "conversion target must be a loaded named scalar type",
        )
    };
    let candidates = config
        .known_custom_types()
        .iter()
        .filter(|owned| owned.authored_name() == path.as_str())
        .collect::<Vec<_>>();
    if config.custom_types.may_defer(None)
        || candidates.iter().any(|owned| {
            owned
                .provider_names()
                .any(|owner| config.custom_types.may_defer(Some(owner)))
        })
    {
        return Err(fail());
    }
    let first = candidates.first().ok_or_else(fail)?;
    let resolved =
        crate::parser::resolve_custom_type_identity(first.identity(), config).map_err(|error| {
            let mut failure = fail();
            failure.cause = Some(std::sync::Arc::new(ConversionCause::TypeResolution(error)));
            failure
        })?;
    let Some(TypeExpr::SchemaType { owned }) = resolved else {
        return Err(fail());
    };
    config
        .custom_type_resolver()
        .enum_declaration(&owned)
        .map_err(|error| ConversionError::validation(location, error))?;
    let declarations = config
        .custom_type_resolver()
        .annotation_declarations(&owned)
        .map_err(|error| ConversionError::validation(location, error))?;
    if owned.has_empty_intersection()
        || declarations
            .values()
            .flatten()
            .any(|decl| scalar_kind(decl).is_none())
    {
        return Err(fail());
    }
    Ok(owned)
}

pub fn scalar_kind(declaration: &CompiledAttributeType) -> Option<RefinedScalarKind> {
    match declaration.kind() {
        AttrTypeKind::String { .. } => Some(RefinedScalarKind::String),
        AttrTypeKind::Int { .. } => Some(RefinedScalarKind::Int),
        AttrTypeKind::Float { .. } => Some(RefinedScalarKind::Float),
        AttrTypeKind::Enum { base, .. } => scalar_kind(base),
        AttrTypeKind::UnloadedSchemaType(_)
        | AttrTypeKind::Reference { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::List { .. }
        | AttrTypeKind::Map { .. }
        | AttrTypeKind::Struct { .. }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => None,
    }
}

pub fn source_declarations(
    owned: &ResolvedSchemaType,
    location: &DeclarationLocation,
    config: &ProviderContext,
) -> Result<Vec<CompiledAttributeType>, ConversionError> {
    let failure = |error| {
        let mut error = ConversionError::validation(location, error);
        error.kind = ConversionErrorKind::Source;
        error
    };
    match owned.resolve_projection(config).map_err(failure)? {
        Some(declaration) => Ok(vec![declaration]),
        None => config
            .custom_type_resolver()
            .annotation_declarations(owned)
            .map(|declarations| declarations.into_values().flatten().collect())
            .map_err(failure),
    }
}

/// Intervals describe original possibilities, never validation success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclarationBounds {
    String {
        min: u64,
        max: Option<u64>,
    },
    Int {
        min: Option<i64>,
        max: Option<i64>,
    },
    Float {
        min: Option<crate::FiniteF64>,
        max: Option<crate::FiniteF64>,
    },
}
impl DeclarationBounds {
    pub fn from_declaration(declaration: &CompiledAttributeType) -> Option<Self> {
        match declaration.raw_shape() {
            RawShape::String {
                identity_layer,
                member_layer,
            } => {
                let (a, b) = identity_layer.length.unwrap_or((None, None));
                let (c, d) = member_layer.length.unwrap_or((None, None));
                Some(Self::String {
                    min: a.unwrap_or(0).max(c.unwrap_or(0)),
                    max: minimum(b, d),
                })
            }
            RawShape::Int {
                identity_layer,
                member_layer,
            } => {
                let (a, b) = identity_layer.range.unwrap_or((None, None));
                let (c, d) = member_layer.range.unwrap_or((None, None));
                Some(Self::Int {
                    min: maximum(a, c),
                    max: minimum(b, d),
                })
            }
            RawShape::Float { range, .. } => {
                let (min, max) = range.unwrap_or((None, None));
                Some(Self::Float { min, max })
            }
            RawShape::Enum { base, .. } => Self::from_declaration(base),
            RawShape::UnloadedSchemaType(_)
            | RawShape::Reference { .. }
            | RawShape::Bool
            | RawShape::Duration
            | RawShape::List { .. }
            | RawShape::Map { .. }
            | RawShape::Struct { .. }
            | RawShape::Union(_)
            | RawShape::Ref(_) => None,
        }
    }
    pub fn disjoint(&self, other: &Self) -> bool {
        fn disjoint<T: Ord + Copy>(a: Option<T>, b: Option<T>, c: Option<T>, d: Option<T>) -> bool {
            matches!((a,d),(Some(a),Some(d)) if a>d) || matches!((b,c),(Some(b),Some(c)) if b<c)
        }
        match (self, other) {
            (Self::String { min: a, max: b }, Self::String { min: c, max: d }) => {
                disjoint(Some(*a), *b, Some(*c), *d)
            }
            (Self::Int { min: a, max: b }, Self::Int { min: c, max: d }) => {
                disjoint(*a, *b, *c, *d)
            }
            (Self::Float { min: a, max: b }, Self::Float { min: c, max: d }) => {
                disjoint(*a, *b, *c, *d)
            }
            (Self::String { .. }, Self::Int { .. } | Self::Float { .. })
            | (Self::Int { .. }, Self::String { .. } | Self::Float { .. })
            | (Self::Float { .. }, Self::String { .. } | Self::Int { .. }) => false,
        }
    }
}
fn minimum<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
fn maximum<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod unit_tests;
