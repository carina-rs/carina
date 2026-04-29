//! Expression parsers — extracted from `parser/mod.rs` per #2262 (part 1/2).
//!
//! These submodules implement the per-expression-form parsing functions
//! (primary values, pipe / compose / coalesce, for, if, validate, string
//! literals). They depend back on `super::*` for shared helpers such as
//! `ParseContext`, `parse_expression`, and the AST types — those move in
//! part 2.

pub(super) mod for_expr;
pub(super) mod if_expr;
pub(super) mod pipe;
pub(super) mod primary;
pub(super) mod string_literal;
pub(super) mod validate_expr;
