//! Differ - Compare desired state with current state to generate a Plan
//!
//! Compares the "desired state" declared in DSL with the "current state" fetched
//! from the Provider, and generates a list of required Effects (Plan).

mod comparison;
mod plan;

use std::collections::HashMap;

use crate::resource::{Resource, ResourceId, State, Value};
use crate::schema::ResourceSchema;

pub use plan::{create_plan, create_plan_with_cascades};

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

// Re-export comparison primitives for consumers that must agree with
// `find_changed_attributes`: detail rows render with the same equality,
// and executor patch construction uses the same key/value gate.
pub(crate) use comparison::{
    AttrComparison, TypedAttr, key_should_enter_patch, secret_grafted_comparison_view,
    type_aware_equal,
};

/// Returns true when `binding` is `template_binding_name` with one numeric
/// deferred-for index suffix, e.g. `validation_records[0]`.
pub fn binding_matches_deferred_template(binding: &str, template_binding_name: &str) -> bool {
    let Some(suffix) = binding.strip_prefix(template_binding_name) else {
        return false;
    };
    let Some(inner) = suffix.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return false;
    };
    !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit())
}

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
