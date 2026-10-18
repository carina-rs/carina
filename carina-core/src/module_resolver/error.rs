//! Module resolution error type.

use crate::parser::ParseError;
use crate::validation::typecheck::TypeExprValueError;

use super::validation::ModuleConstraintDiagnostic;

/// Module resolution error
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
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
