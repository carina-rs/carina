//! Code formatter for Carina DSL
//!
//! Provides formatting similar to `terraform fmt` for .crn files.
//! Preserves comments and normalizes whitespace.
//!
//! # Example
//!
//! ```
//! use carina_core::formatter::{format, FormatConfig};
//!
//! let source = "provider aws {\nregion=aws.Region.ap_northeast_1\n}";
//! let config = FormatConfig::default();
//! let formatted = format(source, &config).unwrap();
//!
//! assert!(formatted.contains("    region = aws.Region.ap_northeast_1"));
//! ```

mod config;
mod cst;
mod cst_builder;
mod format;
mod parser;

pub use config::FormatConfig;
pub use format::{format, needs_format};
pub use parser::FormatParseError;
