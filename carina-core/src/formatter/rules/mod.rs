//! Per-rule `impl Formatter` blocks.
//!
//! Each submodule contributes additional methods to the [`Formatter`](super::format::Formatter)
//! struct, grouped by which DSL constructs they format. There is no logic
//! split — Rust allows multiple `impl` blocks for the same type across
//! files, and the public API is bit-identical to the pre-split layout.

mod attributes;
mod expressions;
mod functions;
mod module;
mod provider;
mod resource;
mod values;
