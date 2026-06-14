//! `wait` construct: typed AST and (later phases) executor logic.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md`.

pub mod predicate;

use predicate::AttrPath;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BindingPattern {
    Exact(String),
    ForLoopChildren {
        base: String,
    },
    AttributeMatch {
        resource_type: String,
        attr: AttrPath,
        from: AttrPath,
    },
}

#[cfg(test)]
mod tests;
