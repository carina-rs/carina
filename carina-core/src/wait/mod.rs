//! `wait` construct: typed AST and (later phases) executor logic.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md`.

pub mod augment;
mod observation;
pub mod predicate;
pub use observation::WaitObservation;
pub use predicate::{AttrPath, AttrPathError, WaitPredicate};

use serde::{Deserialize, Serialize};

use crate::parser::UntilPredicateAst;
#[cfg(any(test, feature = "test-utils"))]
use crate::resource::ResolvedResource;
use crate::resource::{ResolvedDependencies, ResolvedResourceId};

/// A managed resource that can be polled by an [`crate::effect::Effect::Wait`].
///
/// Production code can construct this only through the differ's wait-target
/// resolver. The wrapper serializes transparently as its resolved id. On saved
/// plan reload, `carina-cli`'s `PlanFile::validate_resource_identities` check
/// requires every deserialized wait target id to appear in `sorted_resources`,
/// the plan's desired managed-resource set.
///
/// ```compile_fail
/// use carina_core::resource::{ResolvedResource, Resource};
/// use carina_core::wait::ManagedWaitTarget;
///
/// let resource = ResolvedResource::new(Resource::new("test.Resource", "managed"));
/// let _ = ManagedWaitTarget::new(&resource);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManagedWaitTarget(ResolvedResourceId);

impl ManagedWaitTarget {
    /// Construct a wait target selected by the differ's managed-resource
    /// resolver.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn new(resource: &ResolvedResource) -> Self {
        Self(resource.resolved_id())
    }

    pub(crate) fn from_resolved_id(id: ResolvedResourceId) -> Self {
        Self(id)
    }

    /// Construct a managed wait target in tests that assemble effects without
    /// running the differ.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn for_test(resource: &ResolvedResource) -> Self {
        Self::new(resource)
    }

    /// Borrow the target's resolved resource id.
    pub fn id(&self) -> &ResolvedResourceId {
        &self.0
    }
}

/// A wait whose target and explicit ordering operands have crossed graph resolution.
#[derive(Debug, Clone)]
pub struct ResolvedWaitBinding {
    pub id: ResolvedResourceId,
    pub target_id: ManagedWaitTarget,
    pub until_raw: String,
    pub until_predicate: UntilPredicateAst<crate::resource::AbsoluteBindingAddress>,
    pub timeout_secs: Option<u64>,
    pub explicit_dependencies: ResolvedDependencies,
    pub line: usize,
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
