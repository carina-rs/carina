//! `PersistentId` / `EphemeralId` — typed wrappers over [`ResourceId`]
//! that record whether the id is allowed to enter state lookups.
//!
//! ## What problem these solve
//!
//! Pre-#3293 every node in the IR carried a `ResourceId`. The state
//! layer's lookup APIs (`current_states: HashMap<ResourceId, State>`,
//! `StateBackend::load`, etc.) accepted any `&ResourceId`, including
//! one from a [`Composition`](super::Composition) — which never
//! persists. A composition id passed to a state lookup is guaranteed
//! to miss, but the type system gave no static signal of that.
//!
//! The post-#3169 typestate split already encodes "compositions do
//! not enter the pre-apply differ" structurally (PR D). This PR
//! mirrors that for state-load callers: a `PersistentId` can only
//! be constructed from a leaf node ([`Resource`](super::Resource) or
//! [`DataSource`](super::DataSource)); a `Composition` exposes only
//! `EphemeralId`. No `From<EphemeralId> for PersistentId` impl
//! exists, and no `Deref<Target = ResourceId>` is provided, so a
//! state API that takes `&PersistentId` cannot be handed a
//! composition id by accident.
//!
//! ## Migration scope
//!
//! This PR is **boundary-only**: the wrappers exist, accessor
//! methods on [`Resource`](super::Resource) / [`DataSource`](super::DataSource) /
//! [`Composition`](super::Composition) make them reachable, but the
//! existing `current_states: HashMap<ResourceId, State>` maps and
//! the ~1130 sites that touch `ResourceId` are *not* migrated in
//! this PR. The wrappers become the canonical type at every new
//! state-touching boundary going forward; existing boundaries can
//! be widened one at a time without large mechanical diffs.
//!
//! `into_inner()` is provided as an explicit escape hatch (the wrapper
//! around `ResourceId` is content-equal, so peeling it is safe — the
//! caller is just opting out of the static guarantee on that site).

use serde::{Deserialize, Serialize};

use super::ResourceId;

/// An id of a node that **persists** in state — i.e., a leaf node
/// ([`Resource`](super::Resource) or
/// [`DataSource`](super::DataSource)).
///
/// State-load APIs that take `&PersistentId` are statically prevented
/// from being passed an [`EphemeralId`] (composition id), which would
/// always miss. Construct via [`Resource::persistent_id`](super::Resource::persistent_id)
/// or [`DataSource::persistent_id`](super::DataSource::persistent_id),
/// or via [`PersistentId::new`] when you already have an owned
/// `ResourceId` and know the node it came from is leaf-kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct PersistentId(ResourceId);

impl PersistentId {
    /// Wrap an owned [`ResourceId`] as a `PersistentId`.
    ///
    /// The caller asserts the id came from a leaf node. The wrapper
    /// is content-equal to its inner `ResourceId`, so this is purely
    /// a type-system declaration.
    pub fn new(id: ResourceId) -> Self {
        Self(id)
    }

    /// Borrow the wrapped [`ResourceId`].
    ///
    /// Returned as a borrow so callers cannot accidentally clone the
    /// inner id and pass it to APIs that would otherwise reject a
    /// `PersistentId` — there is only one path back to a bare
    /// `ResourceId`, and that is [`into_inner`](Self::into_inner).
    pub fn inner(&self) -> &ResourceId {
        &self.0
    }

    /// Unwrap into the inner [`ResourceId`].
    ///
    /// Explicit escape hatch for boundaries that have not yet
    /// migrated to typed-id APIs. Each call site that uses this is a
    /// candidate for a follow-up that takes `&PersistentId` directly.
    pub fn into_inner(self) -> ResourceId {
        self.0
    }
}

/// An id of a node that **never persists** in state — i.e., a
/// [`Composition`](super::Composition).
///
/// `EphemeralId` cannot convert to a [`PersistentId`] (no `From` impl
/// exists), so passing it to a state-load API that takes
/// `&PersistentId` is a compile error. Construct via
/// [`Composition::ephemeral_id`](super::Composition::ephemeral_id) or
/// via [`EphemeralId::new`] when you already have the id and know it
/// came from a composition.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct EphemeralId(ResourceId);

impl EphemeralId {
    /// Wrap an owned [`ResourceId`] as an `EphemeralId`.
    ///
    /// The caller asserts the id came from a composition.
    pub fn new(id: ResourceId) -> Self {
        Self(id)
    }

    /// Borrow the wrapped [`ResourceId`].
    pub fn inner(&self) -> &ResourceId {
        &self.0
    }

    /// Unwrap into the inner [`ResourceId`].
    ///
    /// Explicit escape hatch for boundaries that have not yet
    /// migrated to typed-id APIs.
    pub fn into_inner(self) -> ResourceId {
        self.0
    }
}

/// A node identifier in either flavour — persistent (leaf) or
/// ephemeral (composition).
///
/// Used by APIs that need to refer to **any** node by id without
/// committing to leaf or composition statically. The chief consumer
/// is the upcoming `CompositionAttribute::Forwarded(NodeId, AttrPath)`
/// (#3294), where a forwarded composition attribute may alias either
/// a leaf attribute (`PersistentId`) or another composition's attribute
/// (`EphemeralId`).
///
/// `NodeId` is **not** a substitute for the typed-id newtypes at
/// state-load boundaries. APIs that genuinely require leaf-only ids
/// should still take `&PersistentId` directly so that handing them an
/// `EphemeralId` is a compile error rather than a runtime branch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeId {
    Persistent(PersistentId),
    Ephemeral(EphemeralId),
}

impl NodeId {
    /// Borrow the underlying [`ResourceId`].
    ///
    /// Discriminates the variant internally; callers that care about
    /// which kind they have should pattern-match instead of using this.
    pub fn inner(&self) -> &ResourceId {
        match self {
            NodeId::Persistent(p) => p.inner(),
            NodeId::Ephemeral(e) => e.inner(),
        }
    }
}

impl From<PersistentId> for NodeId {
    fn from(p: PersistentId) -> Self {
        NodeId::Persistent(p)
    }
}

impl From<EphemeralId> for NodeId {
    fn from(e: EphemeralId) -> Self {
        NodeId::Ephemeral(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_id_round_trip() {
        let id = ResourceId::with_identity("aws.s3.Bucket", "b");
        let pid = PersistentId::new(id.clone());
        assert_eq!(pid.inner(), &id);
        assert_eq!(pid.into_inner(), id);
    }

    #[test]
    fn ephemeral_id_round_trip() {
        let id = ResourceId::with_identity("_virtual", "m");
        let eid = EphemeralId::new(id.clone());
        assert_eq!(eid.inner(), &id);
        assert_eq!(eid.into_inner(), id);
    }

    /// The whole point of the typestate split: `EphemeralId` and
    /// `PersistentId` are not interconvertible. A function that takes
    /// `&PersistentId` cannot accept an `&EphemeralId`.
    ///
    /// ```compile_fail
    /// use carina_core::resource::{EphemeralId, PersistentId, ResourceId};
    /// fn loads_from_state(_pid: &PersistentId) {}
    /// let eid = EphemeralId::new(ResourceId::with_identity("_virtual", "m"));
    /// loads_from_state(&eid);
    /// ```
    ///
    /// ```compile_fail
    /// use carina_core::resource::{EphemeralId, PersistentId, ResourceId};
    /// let eid = EphemeralId::new(ResourceId::with_identity("_virtual", "m"));
    /// let _pid: PersistentId = eid.into();
    /// ```
    #[allow(dead_code)]
    fn _doctest_anchor() {}

    #[test]
    fn node_id_from_persistent_id() {
        let pid = PersistentId::new(ResourceId::with_identity("aws.s3.Bucket", "b"));
        let nid: NodeId = pid.clone().into();
        assert!(matches!(nid, NodeId::Persistent(_)));
        assert_eq!(nid.inner(), pid.inner());
    }

    #[test]
    fn node_id_from_ephemeral_id() {
        let eid = EphemeralId::new(ResourceId::with_identity("_virtual", "m"));
        let nid: NodeId = eid.clone().into();
        assert!(matches!(nid, NodeId::Ephemeral(_)));
        assert_eq!(nid.inner(), eid.inner());
    }
}
