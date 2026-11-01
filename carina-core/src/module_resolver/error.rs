//! Module resolution error type.

use crate::parser::{ModuleCallId, ParseError};
use crate::validation::typecheck::TypeExprValueError;
use std::path::{Path, PathBuf};

use super::validation::ModuleConstraintDiagnostic;

/// Two or more resolver failures retained by a non-collecting API.
///
/// The field is private and construction is centralized in
/// [`ModuleError::from_errors`], so `ModuleError::Multiple` cannot represent
/// an empty or needlessly singleton failure.
#[derive(Debug)]
pub struct MultipleModuleErrors(Vec<ModuleError>);

impl MultipleModuleErrors {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &ModuleError> {
        self.0.iter()
    }

    pub(crate) fn into_vec(self) -> Vec<ModuleError> {
        self.0
    }
}

impl std::fmt::Display for MultipleModuleErrors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            write!(formatter, "{error}")?;
        }
        Ok(())
    }
}

/// Source ownership attached to a resolver error.
#[derive(Debug, Clone, Copy)]
pub enum ModuleErrorCallSource<'a> {
    /// One concrete expansion failed.
    Failing(&'a ModuleCallId),
    /// An imported graph failed before one of its calls could be singled out.
    Owners(&'a [ModuleCallId]),
    /// The error has no meaningful source-local module-call anchor.
    Unavailable,
}

/// Module resolution error
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("{0}")]
    Multiple(MultipleModuleErrors),

    /// Resolver failure owned by concrete call sites in the containing
    /// configuration. Nested wrappers preserve the full import chain; editor
    /// diagnostics anchor at the outermost source identities.
    #[error("{error}")]
    OwnedByCalls {
        calls: Vec<ModuleCallId>,
        error: Box<ModuleError>,
    },

    /// Failure raised while expanding one concrete call site.
    #[error("{error}")]
    AtCall {
        call: ModuleCallId,
        error: Box<ModuleError>,
    },

    #[error("Module not found: {0}")]
    NotFound(String),

    #[error("Circular import detected: {0}")]
    CircularImport(String),

    #[error("Missing required argument '{argument}' for module '{module}'")]
    MissingArgument { module: String, argument: String },

    #[error("Invalid argument type for '{argument}' in module '{module}': {mismatch}")]
    InvalidArgumentType {
        module: String,
        argument: String,
        mismatch: TypeExprValueError,
    },

    #[error(
        "module call '{module}' argument '{argument}' in {source_path} at line {line}: {message}"
    )]
    BareStructuralHandleArgument {
        module: String,
        argument: String,
        source_path: String,
        line: usize,
        message: String,
    },

    #[error("module '{module}' ({source_path}) at line {line}: {message}")]
    ArgumentValueAccess {
        module: String,
        source_path: String,
        line: usize,
        message: String,
    },

    #[error(
        "binding '{binding}' is defined only in a dropped branch of condition `{condition}` in {source_path} at line {line}"
    )]
    DroppedConditionalBinding {
        binding: String,
        condition: String,
        source_path: PathBuf,
        line: usize,
    },

    #[error(
        "cannot publish a reusable module template with an unresolved argument-dependent `if` at {context}; instantiate the module with concrete arguments before plan or apply"
    )]
    UnexpandedModuleTemplate { context: String },

    #[error("Reference type validation failed: {0}")]
    ReferenceType(crate::validation::ExpansionBoundaryReferenceErrors),

    // An I/O error is a terminal leaf here. Keep its diagnostic in Display
    // and do not expose it a second time through Error::source.
    #[error("IO error: {0}")]
    Io(std::io::Error),

    #[error("Parse error: {0}")]
    Parse(ParseError),

    #[error("Parse error in {path}: {error}")]
    ParseInFile {
        path: PathBuf,
        error: Box<ParseError>,
    },

    /// Declaration type resolution remains typed through recursive module
    /// loading so the CLI/LSP finding can cover this resolver backstop without
    /// comparing rendered parse-error text.
    #[error(transparent)]
    TypeResolution(Box<crate::validation::TypeResolutionDiagnostic>),

    #[error("Unknown argument '{argument}' for module '{module}'")]
    UnknownArgument { module: String, argument: String },

    #[error("Unknown module: {0}")]
    UnknownModule(String),

    #[error(
        "provider blocks are not allowed inside modules. Define providers at the root configuration level."
    )]
    ProviderInModule,

    #[error(
        "state blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level."
    )]
    StateBlockInModule,

    #[error(
        "backend blocks are not allowed inside modules. Define the backend at the root configuration level."
    )]
    BackendInModule,

    #[error(
        "upstream_state declarations are not allowed inside modules. Define upstream_state declarations at the root configuration level."
    )]
    UpstreamStateInModule,

    #[error(
        "exports blocks are not allowed inside modules. Define exports at the root configuration level."
    )]
    ExportsInModule,

    #[error(transparent)]
    Constraint(Box<ModuleConstraintDiagnostic>),

    #[error(
        "Module path '{path}' must be a directory. Single-file modules are not supported; put the module's .crn files in a directory and import the directory."
    )]
    NotADirectory { path: String },
}

impl From<std::io::Error> for ModuleError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ParseError> for ModuleError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

impl ModuleError {
    pub(crate) fn from_errors(errors: Vec<Self>) -> Option<Self> {
        let mut errors = errors.into_iter();
        let first = errors.next()?;
        let rest = errors.collect::<Vec<_>>();
        if rest.is_empty() {
            Some(first)
        } else {
            Some(Self::Multiple(MultipleModuleErrors(
                std::iter::once(first).chain(rest).collect(),
            )))
        }
    }

    pub(crate) fn with_owning_calls(self, calls: Vec<ModuleCallId>) -> Self {
        if calls.is_empty() {
            self
        } else {
            Self::OwnedByCalls {
                calls,
                error: Box::new(self),
            }
        }
    }

    pub(crate) fn with_failing_call(self, call: ModuleCallId) -> Self {
        Self::AtCall {
            call,
            error: Box::new(self),
        }
    }

    /// Outermost typed source ownership for this failure.
    pub fn call_source(&self) -> ModuleErrorCallSource<'_> {
        match self {
            Self::Multiple(_errors) => ModuleErrorCallSource::Unavailable,
            Self::OwnedByCalls { calls, .. } => ModuleErrorCallSource::Owners(calls),
            Self::AtCall { call, .. } => ModuleErrorCallSource::Failing(call),
            Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument { .. }
            | Self::InvalidArgumentType { .. }
            | Self::BareStructuralHandleArgument { .. }
            | Self::ArgumentValueAccess { .. }
            | Self::DroppedConditionalBinding { .. }
            | Self::UnexpandedModuleTemplate { .. }
            | Self::ReferenceType(_)
            | Self::TypeResolution(_)
            | Self::Io(_)
            | Self::Parse(_)
            | Self::ParseInFile { .. }
            | Self::UnknownArgument { .. }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::NotADirectory { .. } => ModuleErrorCallSource::Unavailable,
        }
    }

    pub(crate) fn failing_call(&self) -> Option<&ModuleCallId> {
        match self {
            Self::Multiple(_errors) => None,
            Self::OwnedByCalls { calls: _, error } => error.failing_call(),
            Self::AtCall { call, error } => error.failing_call().or(Some(call)),
            Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument {
                module: _,
                argument: _,
            }
            | Self::InvalidArgumentType {
                module: _,
                argument: _,
                mismatch: _,
            }
            | Self::BareStructuralHandleArgument {
                module: _,
                argument: _,
                source_path: _,
                line: _,
                message: _,
            }
            | Self::ArgumentValueAccess {
                module: _,
                source_path: _,
                line: _,
                message: _,
            }
            | Self::DroppedConditionalBinding {
                binding: _,
                condition: _,
                source_path: _,
                line: _,
            }
            | Self::UnexpandedModuleTemplate { context: _ }
            | Self::ReferenceType(_)
            | Self::TypeResolution(_)
            | Self::Io(_)
            | Self::Parse(_)
            | Self::ParseInFile { path: _, error: _ }
            | Self::UnknownArgument {
                module: _,
                argument: _,
            }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::NotADirectory { path: _ } => None,
        }
    }

    /// Semantic leaf after removing typed import ownership wrappers.
    pub fn root_cause(&self) -> &Self {
        match self {
            Self::OwnedByCalls { error, .. } | Self::AtCall { error, .. } => error.root_cause(),
            error @ (Self::Multiple(_)
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument { .. }
            | Self::InvalidArgumentType { .. }
            | Self::BareStructuralHandleArgument { .. }
            | Self::ArgumentValueAccess { .. }
            | Self::DroppedConditionalBinding { .. }
            | Self::UnexpandedModuleTemplate { .. }
            | Self::ReferenceType(_)
            | Self::TypeResolution(_)
            | Self::Io(_)
            | Self::Parse(_)
            | Self::ParseInFile { .. }
            | Self::UnknownArgument { .. }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::NotADirectory { .. }) => error,
        }
    }

    /// Render paths owned by an imported module relative to the root
    /// configuration directory. The resolver may receive an absolute root
    /// (notably from the LSP) or `.` (from the CLI); diagnostics should expose
    /// the same stable, project-relative path in either case.
    pub fn display_relative_to(&self, root: &Path) -> String {
        match self {
            Self::Multiple(errors) => errors
                .iter()
                .map(|error| error.display_relative_to(root))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::AtCall { call, error } if error.is_deferred_for_body_rejection() => format!(
                "module call in {} at line {}: {}",
                display_source_path_relative_to(call.source_file(), root),
                call.call_span().start_line(),
                error.display_relative_to(root),
            ),
            Self::OwnedByCalls { calls, error }
                if !calls.is_empty() && error.is_deferred_for_body_rejection() =>
            {
                let owners = calls
                    .iter()
                    .map(|call| {
                        format!(
                            "{} at line {}",
                            display_source_path_relative_to(call.source_file(), root),
                            call.call_span().start_line(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "module call in {owners}: {}",
                    error.display_relative_to(root)
                )
            }
            Self::OwnedByCalls { calls: _, error } | Self::AtCall { call: _, error } => {
                error.display_relative_to(root)
            }
            Self::BareStructuralHandleArgument {
                module,
                argument,
                source_path,
                line,
                message,
            } => format!(
                "module call '{module}' argument '{argument}' in {} at line {line}: {message}",
                display_source_path_relative_to(Path::new(source_path), root),
            ),
            Self::ArgumentValueAccess {
                module,
                source_path,
                line,
                message,
            } => format!(
                "module '{module}' ({}) at line {line}: {message}",
                display_source_path_relative_to(Path::new(source_path), root),
            ),
            Self::DroppedConditionalBinding {
                binding,
                condition,
                source_path,
                line,
            } => format!(
                "binding '{binding}' is defined only in a dropped branch of condition `{condition}` in {} at line {line}",
                display_source_path_relative_to(source_path, root),
            ),
            Self::UnexpandedModuleTemplate { context } => format!(
                "cannot publish a reusable module template with an unresolved argument-dependent `if` at {context}; instantiate the module with concrete arguments before plan or apply"
            ),
            Self::Parse(ParseError::Located { path, error }) => format!(
                "Parse error in {}: {}",
                display_source_path_relative_to(path, root),
                display_parse_error_relative_to(error, root),
            ),
            Self::Parse(error) => format!("Parse error: {error}"),
            Self::ParseInFile { path, error } => format!(
                "Parse error in {}: {}",
                display_source_path_relative_to(path, root),
                display_parse_error_relative_to(error, root),
            ),
            Self::TypeResolution(diagnostic) => diagnostic.source_location().map_or_else(
                || diagnostic.to_string(),
                |source| {
                    format!(
                        "{}: {diagnostic}",
                        display_source_path_relative_to(source.source_file().path(), root),
                    )
                },
            ),
            Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument { .. }
            | Self::InvalidArgumentType { .. }
            | Self::ReferenceType(_)
            | Self::Io(_)
            | Self::UnknownArgument { .. }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::NotADirectory { .. } => self.to_string(),
        }
    }

    fn is_deferred_for_body_rejection(&self) -> bool {
        match self.root_cause() {
            Self::Multiple(errors) => errors
                .iter()
                .all(ModuleError::is_deferred_for_body_rejection),
            Self::Parse(error) => parse_error_is_deferred_for_body_rejection(error),
            Self::ParseInFile { path: _, error } => {
                parse_error_is_deferred_for_body_rejection(error)
            }
            Self::OwnedByCalls { .. }
            | Self::AtCall { .. }
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument { .. }
            | Self::InvalidArgumentType { .. }
            | Self::BareStructuralHandleArgument { .. }
            | Self::ArgumentValueAccess { .. }
            | Self::DroppedConditionalBinding { .. }
            | Self::UnexpandedModuleTemplate { .. }
            | Self::ReferenceType(_)
            | Self::TypeResolution(_)
            | Self::Io(_)
            | Self::UnknownArgument { .. }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::NotADirectory { .. } => false,
        }
    }
}

fn parse_error_is_deferred_for_body_rejection(error: &ParseError) -> bool {
    match error {
        ParseError::DeferredForBodyUnsupported(_)
        | ParseError::UndefinedDeferredIterable { .. } => true,
        ParseError::Located { path: _, error } => parse_error_is_deferred_for_body_rejection(error),
        ParseError::DeferredForBodyTemplate {
            line: _,
            header: _,
            span: _,
            error: _,
        }
        | ParseError::Syntax(_)
        | ParseError::InvalidExpression { .. }
        | ParseError::IntegerArithmetic { .. }
        | ParseError::UnboundLoopRead { .. }
        | ParseError::UnresolvedDeclarationType { .. }
        | ParseError::UnknownFunction { .. }
        | ParseError::ReservedResourceAttribute { .. }
        | ParseError::SecretDerivedBackendAttribute { .. }
        | ParseError::InterpolationNotAllowedInLiteralString { .. }
        | ParseError::InvalidResourceIdentity { .. }
        | ParseError::UndefinedVariable(_)
        | ParseError::InvalidResourceType(_)
        | ParseError::DuplicateModule(_)
        | ParseError::DuplicateBinding { .. }
        | ParseError::UndefinedIdentifier { .. }
        | ParseError::ModuleNotFound(_)
        | ParseError::InternalError { .. }
        | ParseError::RecursiveFunction(_)
        | ParseError::CyclicLetBinding { .. }
        | ParseError::UserFunctionError(_) => false,
    }
}

fn display_parse_error_relative_to(error: &ParseError, root: &Path) -> String {
    match error {
        ParseError::Located { path, error } => format!(
            "{}: {}",
            display_source_path_relative_to(path, root),
            display_parse_error_relative_to(error, root),
        ),
        _ => error.to_string(),
    }
}

fn display_source_path_relative_to(path: &Path, root: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    crate::utils::normalize_source_path(relative)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ProviderContext;

    #[test]
    fn spanned_generic_undefined_identifier_is_not_a_deferred_for_rejection() {
        let parsed = crate::parser::parse_file(
            "let child = use { source = './child' }\nchild {}\n",
            Path::new("main.crn"),
            &ProviderContext::default(),
        )
        .expect("module-call fixture parses");
        let call = parsed.module_calls[0].clone();
        let error = ParseError::undefined_identifier_at(
            "missing".to_string(),
            2,
            Vec::new(),
            Some(call.source_span()),
        );
        let rendered = ModuleError::Parse(error)
            .with_failing_call(call.id().clone())
            .display_relative_to(Path::new("."));

        assert!(
            !rendered.starts_with("module call in "),
            "a source span does not make a generic undefined identifier a deferred-for body rejection: {rendered}",
        );
    }
}
