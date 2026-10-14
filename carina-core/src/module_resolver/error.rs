//! Module resolution error type.

use crate::ValueDescription;
use crate::parser::ParseError;

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

    #[error(
        "Invalid argument type for '{argument}' in module '{module}': expected {expected}, got {actual}"
    )]
    InvalidArgumentType {
        module: String,
        argument: String,
        expected: String,
        /// Value-derived description of the argument. Concrete scalar values
        /// except `Duration` include their literal, while duration, structural,
        /// and deferred values expose only their shape (e.g. `list`, `map`,
        /// `resource reference`, or `secret`). Nested mismatches use
        /// `<outer shape> containing <inner>`, such as
        /// `list containing string "production"`. For a `ValidationError`, the
        /// outer value is described because the validator message carries the
        /// mismatch detail.
        actual: ValueDescription,
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

    #[error(transparent)]
    Constraint(Box<ModuleConstraintDiagnostic>),

    #[error(
        "Module path '{path}' must be a directory. Single-file modules are not supported; put the module's .crn files in a directory and import the directory."
    )]
    NotADirectory { path: String },
}
