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
mod signature;
mod typecheck;
mod validation;

pub use error::ModuleError;
pub use expander::{instance_prefix_for_call, reconcile_anonymous_module_instances};
pub use loader::{
    LoadedModule, derive_module_name, get_parsed_file, load_directory_module, load_module,
    load_module_from_directory, load_module_with_diagnostics,
};
pub use resolver::{
    ModuleCallConstraintReport, ModuleCallConstraintReports, ModuleCallSystemIdentity,
    ModuleResolutionDiagnosticReport, ModuleResolver, ResolvedModuleConstraintOutcome,
    ResolvedModuleConstraintStatus, resolve_modules, resolve_modules_with_config,
    resolve_modules_with_diagnostics,
};
pub use signature::{
    ResolvedModuleSignature, ResolvedModuleSignatures, load_resolved_module_signatures,
};
pub use validation::{
    ConstraintEvaluation, EvaluatedModuleConstraint, ModuleConstraintCall,
    ModuleConstraintDiagnostic, ModuleConstraintFailure, ModuleConstraintKind,
    ModuleConstraintViolation, ModuleConstraints, evaluate_constraint, evaluate_module_constraints,
    evaluate_pending_constraints, referenced_constraint_arguments,
};

// Bring `pub(super)` helpers into mod.rs scope so the `tests` submodule (which
// uses `super::*`) can call them by their bare names. Production code never
// reaches these through `mod.rs`; it imports them from `expander` directly.
#[cfg(test)]
use expander::{parse_synthetic_instance_prefix, substitute_arguments};

#[cfg(test)]
mod tests;
