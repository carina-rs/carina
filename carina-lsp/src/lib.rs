#![deny(clippy::iter_over_hash_type)]

#[cfg(feature = "test-utils")]
pub mod admission_coverage;
pub mod backend;
pub mod code_action;
pub mod completion;
pub mod diagnostics;
pub mod document;
pub(crate) mod enclosing_block;
pub mod factories;
pub mod hover;
pub(crate) mod let_parse;
pub(crate) mod lex;
pub mod position;
pub mod semantic_tokens;
#[cfg(feature = "test-utils")]
pub mod test_support;
pub mod workspace;

pub use backend::Backend;
