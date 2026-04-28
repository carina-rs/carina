//! Carina Core
//!
//! Core library for an infrastructure management tool that treats side effects as values

pub mod binding_index;
pub mod builtins;
pub mod config_loader;
pub mod deps;
pub mod detail_rows;
pub mod diff_helpers;
pub mod differ;
pub mod effect;
pub mod executor;
pub mod formatter;
pub mod heredoc;
pub mod identifier;
pub mod keywords;
pub mod lint;
pub mod module;
pub mod module_resolver;
pub mod parser;
pub mod plan;
pub mod plan_tree;
pub mod provider;
pub mod resolver;
pub mod resource;
pub mod schema;
pub mod upstream_exports;
pub mod utils;
pub mod validation;
pub mod value;
pub mod version_constraint;
