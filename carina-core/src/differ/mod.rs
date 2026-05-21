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

/// Split a legacy `[Resource]` mix into the typed slices that
/// [`create_plan`] now requires (carina#3179).
///
/// Virtuals are dropped — they are post-apply attribute containers and
/// never participate in differ logic. Used by tests and other callers
/// that still hold an unsorted `Vec<Resource>` while wiring/parser
/// migration to typed inputs proceeds.
pub fn split_resources_by_kind(
    resources: &[Resource],
) -> (
    Vec<crate::resource::ManagedResource>,
    Vec<crate::resource::DataSource>,
) {
    let (managed, data_sources, _virtuals) = split_resources_by_kind_with_virtuals(resources);
    (managed, data_sources)
}

/// Like [`split_resources_by_kind`], but also returns the virtual slice
/// instead of dropping it. Used by callers that need to handle all
/// three kinds (inference binding map, validation diagnostics, etc.)
/// so the `match resource.kind` per-iteration check is replaced by a
/// single typed split at the top of the function (carina#3180).
pub fn split_resources_by_kind_with_virtuals(
    resources: &[Resource],
) -> (
    Vec<crate::resource::ManagedResource>,
    Vec<crate::resource::DataSource>,
    Vec<crate::resource::VirtualResource>,
) {
    let mut managed = Vec::new();
    let mut data_sources = Vec::new();
    let mut virtuals = Vec::new();
    for r in resources {
        if let Ok(ds) = crate::resource::DataSource::try_from(r) {
            data_sources.push(ds);
        } else if let Ok(v) = crate::resource::VirtualResource::try_from(r) {
            virtuals.push(v);
        } else if let Ok(m) = crate::resource::ManagedResource::try_from(r) {
            managed.push(m);
        }
    }
    (managed, data_sources, virtuals)
}

#[cfg(test)]
mod cascade_tests;
#[cfg(test)]
mod comparison_tests;
#[cfg(test)]
mod diff_tests;
#[cfg(test)]
mod plan_tests;
