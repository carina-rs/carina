//! Block parsers — one submodule per top-level block form.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

pub(super) mod attributes;
pub(super) mod backend;
pub(super) mod module_call;
pub(super) mod provider;
pub(super) mod resource;
pub(super) mod state;
pub(super) mod use_stmt;
