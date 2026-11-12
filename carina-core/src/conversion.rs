//! Static conversion syntax and diagnostics. Runtime values carry no conversions.

use crate::parser::{DeclarationLocation, ProviderContext, ResolvedSchemaType, TypeExpr};
use crate::resource::InsideSecret;
use crate::schema::{AttrTypeKind, CompiledAttributeType, RawShape, RefinedScalarKind};

/// A path authored in a conversion's type position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversionTypePath(String);

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
    Parse(ParseCause),
    Destination(Box<crate::schema::TypeError>),
}

#[derive(Clone)]
struct ParseCause(std::sync::Arc<crate::parser::ParseError>);
impl PartialEq for ParseCause {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl ConversionCause {
    fn error(&self) -> &(dyn std::error::Error + 'static) {
        match self {
            Self::Validation(error) => error,
            Self::TypeResolution(error) => error,
            Self::Parse(error) => error.0.as_ref(),
            Self::Destination(error) => error.as_ref(),
        }
    }
}

/// A located, structured error; it holds no runtime value or admission receipt.
#[derive(Clone, PartialEq)]
pub struct ConversionError(std::sync::Arc<ConversionErrorData>);

#[doc(hidden)]
#[derive(Clone, PartialEq)]
pub struct ConversionErrorData {
    kind: ConversionErrorKind,
    owner: Option<crate::parser::retained::NodeId>,
    location: DeclarationLocation,
    message: String,
    cause: Option<std::sync::Arc<ConversionCause>>,
}

impl std::ops::Deref for ConversionError {
    type Target = ConversionErrorData;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ConversionError {
    fn deref_mut(&mut self) -> &mut Self::Target {
        std::sync::Arc::make_mut(&mut self.0)
    }
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
    pub(crate) fn owned_by(mut self, node: crate::parser::retained::NodeId) -> Self {
        self.owner = Some(node);
        self
    }
    pub fn owner(&self) -> Option<crate::parser::retained::NodeId> {
        self.owner
    }
    pub fn from_error<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a Self> {
        let mut current = Some(error);
        while let Some(error) = current {
            if let Some(conversion) = error.downcast_ref::<Self>() {
                return Some(conversion);
            }
            current = error.source();
        }
        None
    }
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
        Self(std::sync::Arc::new(ConversionErrorData {
            kind,
            owner: None,
            location: location.clone(),
            message: message.into(),
            cause: None,
        }))
    }
    pub(crate) fn parse_failure(
        kind: ConversionErrorKind,
        location: &DeclarationLocation,
        node: crate::parser::retained::NodeId,
        cause: crate::parser::ParseError,
    ) -> Self {
        let mut error = Self::new(
            kind,
            location,
            match kind {
                ConversionErrorKind::UnsupportedPosition => {
                    "conversion is not supported inside this expression"
                }
                ConversionErrorKind::Source => "conversion operand could not be evaluated",
                ConversionErrorKind::Destination
                | ConversionErrorKind::AmbiguousDestination
                | ConversionErrorKind::Unused
                | ConversionErrorKind::Cycle
                | ConversionErrorKind::Target
                | ConversionErrorKind::ScalarKind
                | ConversionErrorKind::Disjoint => "checked conversion failed",
            },
        )
        .owned_by(node);
        error.cause = Some(std::sync::Arc::new(ConversionCause::Parse(ParseCause(
            std::sync::Arc::new(cause),
        ))));
        error
    }
    pub(crate) fn destination(
        location: &DeclarationLocation,
        node: crate::parser::retained::NodeId,
        cause: crate::schema::TypeError,
    ) -> Self {
        let mut error = Self::new(
            ConversionErrorKind::Destination,
            location,
            "conversion destination rejected the value",
        )
        .owned_by(node);
        error.cause = Some(std::sync::Arc::new(ConversionCause::Destination(Box::new(
            cause,
        ))));
        error
    }
    pub fn diagnostic_message(&self) -> String {
        let mut message = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(error) = source {
            message.push_str(": ");
            message.push_str(&error.to_string());
            source = error.source();
        }
        message
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
                ConversionCause::Parse(_) => ConversionCause::Parse(ParseCause(
                    std::sync::Arc::new(crate::parser::ParseError::InsideSecret {
                        line: self.location.span().start_line(),
                    }),
                )),
                ConversionCause::Destination(error) => {
                    ConversionCause::Destination(error.inside_secret())
                }
            })
        });
        self
    }
}

/// Structured aggregate diagnostics; conversions keep their authored owner and
/// range through CLI validation and the error source chain.
#[derive(Debug, Default)]
pub struct ConversionFindings {
    findings: Vec<ConversionError>,
    ordinary: Vec<String>,
    owners: std::collections::HashSet<crate::parser::retained::NodeId>,
}
impl ConversionFindings {
    /// The sole diagnostic deduplication owner, shared by CLI and LSP.
    pub fn extend(&mut self, channel: ConversionChannel) {
        for error in channel.findings {
            if error.owner().is_none_or(|owner| self.owners.insert(owner)) {
                self.findings.push(error);
            }
        }
    }
    pub(crate) fn with_ordinary(mut self, ordinary: Vec<String>) -> Self {
        self.ordinary = ordinary;
        self
    }
    pub fn findings(&self) -> &[ConversionError] {
        &self.findings
    }
    pub fn into_parts(self) -> (Vec<ConversionError>, Vec<String>) {
        (self.findings, self.ordinary)
    }
}

/// Uncollected conversion output from one walk or completeness report. It
/// cannot carry an ordinary error and does not deduplicate per-use evidence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConversionChannel {
    findings: Vec<ConversionError>,
}
impl ConversionChannel {
    pub fn findings(&self) -> &[ConversionError] {
        &self.findings
    }
    pub(crate) fn append(&mut self, other: Self) {
        self.findings.extend(other.findings);
    }
    /// Called only while constructing output reports, never by accessors or
    /// diagnostic consumers. Internal TypeError carriers stop at this boundary.
    pub(crate) fn ordinary(
        &mut self,
        error: crate::schema::TypeError,
    ) -> Option<crate::schema::TypeError> {
        if let Some(conversion) = ConversionError::from_error(&error) {
            self.findings.push(conversion.clone());
            None
        } else {
            Some(error)
        }
    }
}
impl FromIterator<ConversionError> for ConversionChannel {
    fn from_iter<T: IntoIterator<Item = ConversionError>>(iter: T) -> Self {
        Self {
            findings: iter.into_iter().collect(),
        }
    }
}
impl InsideSecret for ConversionChannel {
    fn inside_secret(self) -> Self {
        Self {
            findings: self.findings.inside_secret(),
        }
    }
}
impl std::fmt::Display for ConversionChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.findings.iter().enumerate() {
            if index != 0 {
                writeln!(f)?;
            }
            write!(
                f,
                "{}",
                crate::parser::Located::new(error.location().clone(), error)
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for ConversionChannel {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.findings.first().map(|error| error as _)
    }
}
impl std::fmt::Display for ConversionFindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.findings.iter().enumerate() {
            if index != 0 {
                writeln!(f)?;
            }
            write!(
                f,
                "{}",
                crate::parser::Located::new(error.location().clone(), error)
            )?;
        }
        for error in &self.ordinary {
            write!(f, "\n{error}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ConversionFindings {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.findings.first().map(|error| error as _)
    }
}

pub(crate) fn resolve_target(
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
    let mut captured: Option<ResolvedSchemaType> = None;
    for (owner, declarations) in declarations {
        if declarations.is_empty() {
            return Err(fail());
        }
        for declaration in declarations {
            if scalar_kind(&declaration).is_none() {
                return Err(fail());
            }
            let evidence =
                ResolvedSchemaType::from_annotation_declaration(owner.clone(), &declaration)
                    .ok_or_else(fail)?;
            if evidence.identity() != owned.identity()
                || evidence.scalar_kind() != owned.scalar_kind()
            {
                return Err(fail());
            }
            captured = Some(match captured {
                Some(previous) => previous.conjoin(&evidence),
                None => evidence,
            });
        }
    }
    let captured = captured.ok_or_else(fail)?;
    if captured
        .provider_names()
        .collect::<std::collections::BTreeSet<_>>()
        != owned
            .provider_names()
            .collect::<std::collections::BTreeSet<_>>()
    {
        return Err(fail());
    }
    let owned = captured;
    let declaration = source_declaration(&owned, location, config).map_err(|mut error| {
        error.kind = ConversionErrorKind::Target;
        error
    })?;
    if owned.has_empty_intersection() || scalar_kind(&declaration).is_none() {
        return Err(fail());
    }
    Ok(owned)
}

pub(crate) fn scalar_kind(declaration: &CompiledAttributeType) -> Option<RefinedScalarKind> {
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

pub(crate) fn source_declaration(
    owned: &ResolvedSchemaType,
    location: &DeclarationLocation,
    config: &ProviderContext,
) -> Result<CompiledAttributeType, ConversionError> {
    if config.custom_types.may_defer(None) {
        let mut error = ConversionError::validation(
            location,
            crate::parser::CustomTypeValidationError::ProvidersNotLoaded,
        );
        error.kind = ConversionErrorKind::Source;
        return Err(error);
    }
    // Captured evidence already conjoins annotation owners and preserves read
    // constraints. A registry-only source must not be rebound through catalog.
    Ok(owned.materialize())
}

/// Intervals describe original possibilities, never validation success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeclarationBounds {
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
mod cursor_policy_tests;
#[cfg(test)]
mod unit_tests;
