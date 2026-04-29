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

    #[error("Invalid argument type for '{argument}' in module '{module}': expected {expected}")]
    InvalidArgumentType {
        module: String,
        argument: String,
        expected: String,
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
