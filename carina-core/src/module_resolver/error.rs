//! Module resolution error type.

use crate::parser::ParseError;
use crate::validation::typecheck::TypeExprValueError;
use std::path::{Path, PathBuf};

use super::validation::ModuleConstraintDiagnostic;

/// Module resolution error
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    /// Resolver failure owned by a particular import alias in the containing
    /// configuration. Nested wrappers preserve the full import chain; editor
    /// diagnostics anchor at the outermost owner.
    #[error("{error}")]
    OwnedByImport {
        import: String,
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

    #[error("Reference type validation failed: {0}")]
    ReferenceType(String),

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
    pub(crate) fn with_owning_import(self, import: impl Into<String>) -> Self {
        Self::OwnedByImport {
            import: import.into(),
            error: Box::new(self),
        }
    }

    /// Outermost import alias whose graph produced this failure.
    pub fn owning_import(&self) -> Option<&str> {
        match self {
            Self::OwnedByImport { import, .. } => Some(import),
            _ => None,
        }
    }

    /// Semantic leaf after removing typed import ownership wrappers.
    pub fn root_cause(&self) -> &Self {
        match self {
            Self::OwnedByImport { error, .. } => error.root_cause(),
            error => error,
        }
    }

    /// Render paths owned by an imported module relative to the root
    /// configuration directory. The resolver may receive an absolute root
    /// (notably from the LSP) or `.` (from the CLI); diagnostics should expose
    /// the same stable, project-relative path in either case.
    pub fn display_relative_to(&self, root: &Path) -> String {
        match self.root_cause() {
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
            _ => self.to_string(),
        }
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
