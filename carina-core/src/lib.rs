//! Carina Core
//!
//! Core library for an infrastructure management tool that treats side effects as values

mod user_dirs;
mod value_description;

#[cfg(any(test, feature = "test-utils"))]
pub use user_dirs::TestUserDirs;
pub use user_dirs::{UserDirs, UserDirsError};
pub use value_description::ValueDescription;

pub mod binding_index;
#[cfg(test)]
mod binding_index_split_tests;
pub mod builtins;
pub mod config_loader;
pub mod data_source_value;
pub mod deps;
pub mod desired;
pub mod detail_rows;
pub mod diff_helpers;
pub mod differ;
pub mod effect;
pub(crate) mod eval_value;
pub mod executor;
pub mod explicit;
pub mod formatter;
pub mod heredoc;
pub mod hint;
pub mod identifier;
pub mod integer_arithmetic;
pub mod keywords;
pub mod lint;
pub mod module;
pub mod module_resolver;
pub mod name_override;
pub(crate) mod non_empty;
pub mod parser;
pub mod plan;
pub mod plan_tree;
pub mod protocol_convert;
pub mod provider;
pub mod provider_source;
pub mod provider_wire;
pub mod references;
pub mod resolver;
#[cfg(test)]
mod resolver_split_tests;
pub mod resource;
pub mod schema;
pub mod shutdown;
pub mod static_type;
pub mod upstream_exports;
pub mod utils;
pub mod validation;
pub mod value;
pub mod version_constraint;
pub mod wait;
