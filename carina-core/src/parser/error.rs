//! Parse-time error and warning types.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::{Rule, SourceSpan};
use crate::eval_value::EvalValueError;
use std::path::{Path, PathBuf};

pub const SINGLE_QUOTED_INTERPOLATION_WARNING_MESSAGE: &str = "single-quoted string contains '${...}' which is not interpolated; use double quotes (\"...\") for interpolation, or keep single quotes if the literal text is intended";
pub const NON_LITERAL_PREVENT_DESTROY_WARNING_MESSAGE: &str = "prevent_destroy uses a non-literal expression; only a literal `false` opts out of schema-default protection";

/// A body kind that cannot be expanded after a deferred `for` iterable
/// becomes known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredForBodyKind {
    ModuleCall { name: String },
    Read { resource_type: String },
}

impl std::fmt::Display for DeferredForBodyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModuleCall { name } => write!(f, "module call `{name}`"),
            Self::Read { resource_type } => write!(f, "read `{resource_type}`"),
        }
    }
}

/// State of the iterable at the final post-module-expansion conversion seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredForIterableResolution {
    StillDeferred,
    DependsOnEnclosingModuleArgument,
    ConcreteAfterModuleArgumentSubstitution,
}

/// Details for a deferred `for` body that cannot cross the final module
/// expansion seam.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", format_deferred_for_body_unsupported(*line, header, body, *iterable_resolution))]
pub struct DeferredForBodyUnsupportedError {
    pub line: usize,
    pub header: String,
    pub body: DeferredForBodyKind,
    pub span: SourceSpan,
    pub iterable_resolution: DeferredForIterableResolution,
}

/// Typed identity and authored token span for an unresolved declaration type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnresolvedTypeToken {
    name: String,
    span: SourceSpan,
}

impl UnresolvedTypeToken {
    pub(crate) fn new(name: impl Into<String>, span: SourceSpan) -> Self {
        Self {
            name: name.into(),
            span,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn span(&self) -> SourceSpan {
        self.span
    }
}

/// Parse error
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// A semantic parse/evaluation error whose expression is owned by another
    /// file in the same directory. The inner error retains the precise line;
    /// this wrapper retains which document that line belongs to.
    #[error("{path}: {error}")]
    Located {
        path: PathBuf,
        error: Box<ParseError>,
    },

    // A pest diagnostic is a terminal, already formatted leaf. Keep it in
    // Display and do not expose it again through Error::source.
    #[error("Syntax error: {0}")]
    Syntax(pest::error::Error<Rule>),

    #[error("Invalid expression at line {line}: {message}")]
    InvalidExpression { line: usize, message: String },

    #[error(
        "Invalid {kind} expression at line {line}: an unbound {kind} cannot contain `read`; bind the {kind} expression with `let` so the data source has an address"
    )]
    UnboundLoopRead {
        kind: crate::resource::LoopKind,
        line: usize,
        span: Option<super::SourceSpan>,
    },

    #[error("{0}")]
    DeferredForBodyUnsupported(Box<DeferredForBodyUnsupportedError>),

    #[error(
        "Deferred `for` expression `{header}` at line {line} cannot build its body template because its loop value is not known at plan time: {error}"
    )]
    DeferredForBodyTemplate {
        line: usize,
        header: String,
        span: SourceSpan,
        error: Box<ParseError>,
    },

    /// Provider-aware parsing could not resolve a type annotation. The
    /// rendered message is deliberately separate from the typed token used by
    /// diagnostic coverage and deduplication.
    #[error("Invalid expression at line {line}: {message}")]
    UnresolvedDeclarationType {
        token: UnresolvedTypeToken,
        line: usize,
        message: String,
    },

    /// A call whose name is neither a declared user function nor a builtin.
    /// Keeping this structured lets the declaration prepass defer forward and
    /// sibling calls without treating a genuinely unknown name as a builtin.
    #[error("Invalid expression at line {line}: Unknown function '{name}'")]
    UnknownFunction {
        name: String,
        line: usize,
        /// One-based source column of the function name.
        column: usize,
    },

    #[error(
        "Reserved attribute name '{attribute}' at line {line}, column {column}: names starting with '_' are reserved for Carina"
    )]
    ReservedResourceAttribute {
        attribute: String,
        line: usize,
        column: usize,
        end_line: usize,
        end_column: usize,
    },

    #[error(
        "Invalid expression at line {line}: backend {backend_type} attribute '{attribute}' must be a plain string or other non-secret value; secret()- and decrypt()-derived values are not allowed"
    )]
    SecretDerivedBackendAttribute {
        backend_type: String,
        attribute: String,
        line: usize,
        start_column: usize,
        end_column: usize,
    },

    #[error(
        "Invalid expression at line {line}: interpolation is not allowed in this string literal"
    )]
    InterpolationNotAllowedInLiteralString { line: usize },

    #[error("Invalid expression at line {line}: {error}")]
    InvalidResourceIdentity {
        line: usize,
        error: crate::resource::ResourceIdentityError,
    },

    #[error("Undefined variable: {0}")]
    UndefinedVariable(String),

    #[error("Invalid resource type: {0}")]
    InvalidResourceType(String),

    #[error("Duplicate module definition: {0}")]
    DuplicateModule(String),

    #[error("Duplicate binding at line {line}: {name}")]
    DuplicateBinding { name: String, line: usize },

    #[error("{}", format_undefined_identifier(name, suggestion.as_deref(), in_scope))]
    UndefinedIdentifier {
        name: String,
        line: usize,
        /// Exact authored expression span when the reporting seam retains it.
        /// Older identifier checks that only know a line leave this as `None`.
        span: Option<Box<SourceSpan>>,
        /// Edit-distance close match among in-scope bindings, if any.
        /// Filled by the check site when a `suggest_similar_name` result is
        /// available; None for hand-constructed errors.
        suggestion: Option<String>,
        /// Concrete binding names in scope at the check site, sorted for
        /// deterministic rendering. Empty when no bindings have been
        /// declared at all.
        in_scope: Vec<String>,
    },

    /// A bare deferred-`for` iterable that remained unbound after the
    /// directory binding namespace was known. This is distinct from an
    /// ordinary spanned identifier error so module-call ownership is decided
    /// by semantic origin rather than by the incidental presence of a span.
    #[error("{}", format_undefined_identifier(name, suggestion.as_deref(), in_scope))]
    UndefinedDeferredIterable {
        name: String,
        line: usize,
        span: Box<SourceSpan>,
        suggestion: Option<String>,
        in_scope: Vec<String>,
    },

    #[error("Module not found: {0}")]
    ModuleNotFound(String),

    #[error("Internal parser error: expected {expected} in {context}")]
    InternalError { expected: String, context: String },

    #[error("Recursive function call detected: {0}")]
    RecursiveFunction(String),

    #[error("{}", format_cyclic_let_binding(chain))]
    CyclicLetBinding { chain: Vec<String> },

    #[error("User-defined function error: {0}")]
    UserFunctionError(String),
}

impl From<pest::error::Error<Rule>> for ParseError {
    fn from(error: pest::error::Error<Rule>) -> Self {
        Self::Syntax(error)
    }
}

fn format_cyclic_let_binding(chain: &[String]) -> String {
    let rendered = chain.join(" -> ");
    if chain.len() == 2 && chain[0] == chain[1] {
        format!(
            "Cyclic let binding detected: {rendered}; a `let` binding cannot reference itself; \
             remove this line or replace the RHS with a real value"
        )
    } else {
        format!("Cyclic let binding detected: {rendered}")
    }
}

fn format_deferred_for_body_unsupported(
    line: usize,
    header: &str,
    body: &DeferredForBodyKind,
    iterable_resolution: DeferredForIterableResolution,
) -> String {
    match iterable_resolution {
        DeferredForIterableResolution::StillDeferred => format!(
            "Deferred `for` expression `{header}` at line {line} cannot use {body}: its iterable \
             is still unresolved after module expansion, and only a managed resource body can be \
             deferred until apply. Iterate over a value known before module expansion, or put \
             managed resources directly in the loop body"
        ),
        DeferredForIterableResolution::DependsOnEnclosingModuleArgument => format!(
            "Deferred `for` expression `{header}` at line {line} cannot use {body}: its iterable \
             still depends on an enclosing module argument at this module-expansion boundary, \
             and replaying {body} after nested module expansion is not supported. Move the loop \
             to the caller where that argument is supplied, or put managed resources directly \
             in the loop body"
        ),
        DeferredForIterableResolution::ConcreteAfterModuleArgumentSubstitution => format!(
            "Deferred `for` expression `{header}` at line {line} cannot use {body}: its iterable \
             is concrete after module argument substitution, but replaying {body} after module \
             expansion is not supported. Move the loop to the caller where the iterable is known \
             during parsing, or put managed resources directly in the loop body"
        ),
    }
}

/// Render the `UndefinedIdentifier` message. When a close match exists
/// (`suggestion`), lead with `Did you mean ...?`. Otherwise list the
/// concrete in-scope names so the reader learns what is available,
/// followed by the abstract list of binding kinds as a trailing aside.
/// See #2038.
fn format_undefined_identifier(
    name: &str,
    suggestion: Option<&str>,
    in_scope: &[String],
) -> String {
    if let Some(s) = suggestion {
        return format!("Undefined identifier `{}`. Did you mean `{}`?", name, s);
    }
    let kinds = "let / upstream_state / read / module / function / for / fn / arguments";
    if in_scope.is_empty() {
        format!(
            "Undefined identifier `{}`: no bindings are in scope ({})",
            name, kinds,
        )
    } else {
        format!(
            "Undefined identifier `{}`. In-scope names: {} ({})",
            name,
            in_scope.join(", "),
            kinds,
        )
    }
}

/// A structured warning emitted during parsing (non-fatal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarningKind {
    DeferredFor,
    NonLiteralPreventDestroy,
    SingleQuotedInterpolation,
    UnusedForBinding,
}

/// 1-indexed source span for a parse warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseWarningSpan {
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    /// 1-indexed exclusive end column for single-line spans.
    pub end_column: usize,
}

/// A structured warning emitted during parsing (non-fatal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseWarning {
    /// Full source path of the file this warning originated from
    /// (stamped by `config_loader` after parsing). `None` at parse time;
    /// always `Some` once the warning reaches CLI/LSP callers.
    pub file: Option<String>,
    pub line: usize,
    pub kind: WarningKind,
    pub span: Option<ParseWarningSpan>,
    pub message: String,
}

impl ParseError {
    pub(crate) fn in_file(self, path: impl Into<PathBuf>) -> Self {
        if matches!(self, Self::Located { .. }) {
            self
        } else {
            Self::Located {
                path: path.into(),
                error: Box::new(self),
            }
        }
    }

    pub fn source_file(&self) -> Option<&Path> {
        match self {
            Self::Located { path, .. } => Some(path),
            _ => None,
        }
    }

    pub(crate) fn from_eval_value(error: EvalValueError, line: usize) -> Self {
        ParseError::InvalidExpression {
            line,
            message: error.to_string(),
        }
    }

    pub(crate) fn unresolved_declaration_type(
        name: impl Into<String>,
        span: SourceSpan,
        message: String,
    ) -> Self {
        Self::UnresolvedDeclarationType {
            token: UnresolvedTypeToken::new(name, span),
            line: span.start_line(),
            message,
        }
    }

    /// Construct an `UndefinedIdentifier` error with did-you-mean suggestion
    /// and sorted in-scope binding list. `known_bindings` is the set of
    /// names that are in scope at the check site — they will be sorted
    /// and used to compute the close-match suggestion. See #2038.
    pub fn undefined_identifier(
        name: String,
        line: usize,
        known_bindings: Vec<String>,
    ) -> ParseError {
        Self::undefined_identifier_at(name, line, known_bindings, None)
    }

    pub(crate) fn undefined_identifier_at(
        name: String,
        line: usize,
        known_bindings: Vec<String>,
        span: Option<SourceSpan>,
    ) -> ParseError {
        let known_refs: Vec<&str> = known_bindings.iter().map(String::as_str).collect();
        let suggestion = crate::schema::suggest_similar_name(&name, &known_refs);
        let mut in_scope = known_bindings;
        in_scope.sort();
        ParseError::UndefinedIdentifier {
            name,
            line,
            span: span.map(Box::new),
            suggestion,
            in_scope,
        }
    }

    pub(crate) fn undefined_deferred_iterable(
        name: String,
        line: usize,
        known_bindings: Vec<String>,
        span: SourceSpan,
    ) -> ParseError {
        let known_refs: Vec<&str> = known_bindings.iter().map(String::as_str).collect();
        let suggestion = crate::schema::suggest_similar_name(&name, &known_refs);
        let mut in_scope = known_bindings;
        in_scope.sort();
        ParseError::UndefinedDeferredIterable {
            name,
            line,
            span: Box::new(span),
            suggestion,
            in_scope,
        }
    }
}

pub(super) fn undefined_identifier_error(
    known: &std::collections::HashSet<&str>,
    name: String,
    line: usize,
) -> ParseError {
    ParseError::undefined_identifier(name, line, known.iter().map(|s| s.to_string()).collect())
}
