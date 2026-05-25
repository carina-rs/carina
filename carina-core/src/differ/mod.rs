//! Differ - Compare desired state with current state to generate a Plan
//!
//! Compares the "desired state" declared in DSL with the "current state" fetched
//! from the Provider, and generates a list of required Effects (Plan).

mod comparison;
mod plan;

use std::collections::HashMap;

use crate::resource::{Resource, ResourceId, State, Value};
use crate::schema::ResourceSchema;

pub use plan::{cascade_dependent_updates, create_plan};

// Imports used by test submodules (accessible via `use super::*;`)
#[cfg(test)]
use crate::effect::Effect;
#[cfg(test)]
use crate::plan::Plan;
#[cfg(test)]
use crate::resource::Directives;
#[cfg(test)]
use crate::schema::{AttributeType, SchemaRegistry};
#[cfg(test)]
use comparison::find_changed_attributes;

// Re-exported for the plan detail-row renderer (carina#3073): the
// renderer must reuse the differ's own type-aware equality so the
// rendered rows agree with `find_changed_attributes`. Also covers
// the differ's own tests, which previously imported it under cfg(test).
pub(crate) use comparison::type_aware_equal;

/// Result of a diff operation
#[derive(Debug, Clone, PartialEq)]
pub enum Diff {
    /// Resource does not exist -> needs creation
    Create(Resource),
    /// Resource exists with differences -> needs update
    Update {
        id: ResourceId,
        from: Box<State>,
        to: Resource,
        changed_attributes: Vec<String>,
    },
    /// Resource exists with no differences -> no action needed
    NoChange(ResourceId),
    /// Resource exists but not in desired state -> needs deletion
    Delete(ResourceId),
}

impl Diff {
    /// Returns whether this Diff involves a change
    pub fn is_change(&self) -> bool {
        !matches!(self, Diff::NoChange(_))
    }
}

/// Compare desired state with current state to compute a Diff.
/// If `saved` is provided, unmanaged nested fields from the saved state are merged
/// into desired before comparison, preventing false diffs when AWS returns extra fields.
/// If `prev_explicit` is provided, the actual-state side is projected through
/// the authoring tree before comparison so server-side default fields the
/// user never wrote do not surface as diffs (refs awscc#206). The same
/// tree drives explicit-removal detection for attributes the user
/// previously wrote but no longer mentions.
/// If `schema` is provided, type-aware comparison is used (e.g., Int/Float coercion,
/// case-insensitive enum matching).
pub fn diff(
    desired: &Resource,
    current: &State,
    saved: Option<&HashMap<String, Value>>,
    prev_explicit: Option<&crate::explicit::ExplicitFields>,
    schema: Option<&ResourceSchema>,
) -> Diff {
    if !current.exists {
        return Diff::Create(desired.clone());
    }

    let changed = comparison::find_changed_attributes(
        &desired.resolved_attributes(),
        &current.attributes,
        saved,
        prev_explicit,
        schema,
        Some(&desired.id),
    );

    if changed.is_empty() {
        Diff::NoChange(desired.id.clone())
    } else {
        Diff::Update {
            id: desired.id.clone(),
            from: Box::new(current.clone()),
            to: desired.clone(),
            changed_attributes: changed,
        }
    }
}

#[cfg(test)]
mod cascade_tests;
#[cfg(test)]
mod comparison_tests;
#[cfg(test)]
mod diff_tests;
#[cfg(test)]
mod plan_tests;
