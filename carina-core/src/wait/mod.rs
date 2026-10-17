//! `wait` construct: typed AST and (later phases) executor logic.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md`.

pub mod augment;
mod observation;
pub mod predicate;
pub use observation::WaitObservation;
pub use predicate::{AttrPath, AttrPathError, WaitPredicate};

use crate::resource::ResourceId;
use crate::schema::SchemaKind;

/// Find a wait target while preserving whether the selected candidate is a
/// managed resource or data source. This is the schema-kind-carrying form used
/// by both wiring and plan construction, so a same-type managed/data-source
/// schema pair cannot be resolved by registration order.
pub fn find_wait_target_with_schema_kind<'a>(
    target: &str,
    candidates: impl IntoIterator<Item = (&'a ResourceId, Option<&'a str>, SchemaKind)>,
) -> Option<(&'a ResourceId, SchemaKind)> {
    let mut identity_match = None;
    for (id, binding, kind) in candidates {
        if binding == Some(target) {
            return Some((id, kind));
        }
        if identity_match.is_none() && id.identity_str() == Some(target) {
            identity_match = Some((id, kind));
        }
    }
    identity_match
}

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
