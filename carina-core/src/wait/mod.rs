//! `wait` construct: typed AST and (later phases) executor logic.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md`.

pub mod augment;
mod observation;
pub mod predicate;
pub use observation::WaitObservation;
pub use predicate::{AttrPath, AttrPathError, WaitPredicate};

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
