//! Module Resolver - Resolve module imports and instantiations
//!
//! This module handles:
//! - Resolving import paths to module definitions
//! - Detecting circular dependencies between modules
//! - Validating module argument parameters
//! - Expanding module calls into resources
//!
//! ## Submodules
//!
//! - `error`: `ModuleError` enum for all resolver-layer failures.
//! - `loader`: filesystem helpers — `load_module`, `load_directory_module`,
//!   `load_module_from_directory`, `get_parsed_file`, `derive_module_name`.
//!   All loader entry points are directory-scoped.
//! - `typecheck`: validates module call argument values against declared
//!   `TypeExpr`s.
//! - `expander`: `expand_module_call` plus argument substitution and
//!   intra-module reference rewriting; also hosts the
//!   `reconcile_anonymous_module_instances` post-pass.
//! - `resolver`: the `ModuleResolver` struct/impl driver and the
//!   `resolve_modules*` top-level entry points.
//! - `validation`: expression evaluator for `validate` and `require` blocks.

mod error;
mod expander;
mod loader;
mod resolver;
mod typecheck;
mod validation;

pub use error::ModuleError;
pub use expander::{instance_prefix_for_call, reconcile_anonymous_module_instances};
pub use loader::{
    derive_module_name, get_parsed_file, load_directory_module, load_module,
    load_module_from_directory,
};
pub use resolver::{ModuleResolver, resolve_modules, resolve_modules_with_config};

// Bring `pub(super)` helpers into mod.rs scope so the `tests` submodule (which
// uses `super::*`) can call them by their bare names. Production code never
// reaches these through `mod.rs`; it imports them from `expander` directly.
#[cfg(test)]
use expander::{parse_synthetic_instance_prefix, substitute_arguments};

#[cfg(test)]
mod tests;
