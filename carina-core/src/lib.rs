//! Carina Core
//!
//! Core library for an infrastructure management tool that treats side effects as values

pub mod config_loader;
pub mod deps;
pub mod differ;
pub mod effect;
pub mod executor;
pub mod formatter;
pub mod identifier;
pub mod lint;
pub mod module;
pub mod module_resolver;
pub mod parser;
pub mod plan;
pub mod provider;
pub mod resolver;
pub mod resource;
pub mod schema;
pub mod utils;
pub mod validation;
pub mod value;
