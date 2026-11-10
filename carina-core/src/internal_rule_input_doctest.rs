// Production root modules for crate-visible compile-fail doctests.
// The positive control catches source-map and visibility failures.

#[cfg(all(feature = "provider-host", target_arch = "wasm32"))]
compile_error!(
    "carina-core host plumbing is not available to wasm32 guest providers; remove the `provider-host` feature from guest dependencies"
);

pub use carina_provider_protocol::{FiniteF64, NonFiniteFloatError};

mod environment;
mod user_dirs;
mod value_description;

pub use environment::EnvironmentSnapshot;
#[cfg(any(test, feature = "test-utils"))]
pub use user_dirs::TestUserDirs;
pub use user_dirs::{UserDirs, UserDirsError};
pub use value_description::ValueDescription;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

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
pub mod execution_authority;
pub mod executor;
pub mod explicit;
pub mod formatter;
pub mod heredoc;
pub mod hint;
pub mod identifier;
pub mod input_rules;
pub mod integer_arithmetic;
#[doc(hidden)]
pub mod json_field_order;
pub mod keywords;
pub mod lint;
pub mod module;
pub mod module_resolver;
pub mod name_override;
pub(crate) mod non_empty;
pub(crate) mod numeric;
pub mod parser;
pub mod paths;
pub mod plan;
pub mod plan_tree;
pub mod planning_state;
pub mod protocol_convert;
pub mod provider;
pub mod provider_source;
pub mod provider_text;
pub mod provider_wire;
pub mod references;
pub use plan::replacement;
pub mod resolved_graph;
pub mod resolver;
#[cfg(test)]
mod resolver_split_tests;
pub mod resource;
pub mod saved_replay;
pub mod schema;
pub mod shutdown;
pub mod static_type;
pub mod upstream_exports;
pub mod utils;
pub mod validation;
pub mod value;
pub mod value_walk;
pub mod version_constraint;
pub mod wait;
