//! Module resolution error type.

use crate::parser::ParseError;

/// Module resolution error
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("Module not found: {0}")]
    NotFound(String),

    #[error("Circular import detected: {0}")]
    CircularImport(String),

    #[error("Missing required argument '{argument}' for module '{module}'")]
    MissingArgument { module: String, argument: String },

    #[error(
        "Invalid argument type for '{argument}' in module '{module}': expected {expected}, got {actual}"
    )]
    InvalidArgumentType {
        module: String,
        argument: String,
        expected: String,
        /// Short, human-readable description of the value shape that was
        /// passed (e.g. `string`, `int`, `list`, `map`, `resource reference`).
        /// Surfacing the actual shape avoids the misleading
        /// `expected list(...)` reading that sent issue #3238's reporter
        /// hunting for an element-type mismatch when the real cause was
        /// a value-shape mismatch.
        actual: String,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(#[from] ParseError),

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

    #[error(
        "Validation failed for argument '{argument}' in module '{module}': {message} (got {actual})"
    )]
    ArgumentValidationFailed {
        module: String,
        argument: String,
        message: String,
        actual: String,
    },

    #[error("Require constraint failed in module '{module}': {message}")]
    RequireConstraintFailed { module: String, message: String },

    #[error(
        "Module path '{path}' must be a directory. Single-file modules are not supported; put the module's .crn files in a directory and import the directory."
    )]
    NotADirectory { path: String },
}
