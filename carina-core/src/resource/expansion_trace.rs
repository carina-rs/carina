//! `ExpansionTrace` — plan-scoped sidecar carrying leaf-to-composition
//! lineage.
//!
//! When the module resolver expands a module call into leaf resources,
//! each leaf carries an instance-prefixed `ResourceId` but no
//! structural record of which composition (or chain of nested
//! compositions) produced it. The display layer wants that lineage so
//! it can fold leaf rows under their originating composition row:
//!
//! ```text
//! + Composition "cluster"
//!     + aws.eks.Cluster      cluster/inner
//!     + aws.iam.Role         cluster/inner-role
//! + aws.s3.Bucket            logs
//! ```
//!
//! `ExpansionTrace` records that relationship as a sidecar map keyed
//! by leaf [`PersistentId`] → an outermost-first chain of
//! [`EphemeralId`]s for the composition(s) that nest the leaf.
//!
//! The trace is **plan-scoped, not persisted**. State files do not
//! carry it; every plan rebuilds it from DSL at parse time. This keeps
//! the persistence layer free of composition concerns (PR D's
//! `LeafNode` guarantee remains intact: state only sees leaves) while
//! still letting the display layer fold leaf rows under their
//! composition's call site.
//!
//! Map iteration order is not stable; consumers that render in a
//! stable order should sort the iterator output by the leaf id's
//! `name` themselves.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{EphemeralId, PersistentId};

// NOTE: keyed by `PersistentId` via `HashMap` (not `BTreeMap`) because
// the inner `ResourceId` does not implement `Ord` workspace-wide.
// Display code that wants a stable render order should sort the
// iterator output by `leaf.inner().name.as_str()` itself rather than
// relying on map iteration order.

/// Plan-scoped lineage of leaf nodes back to the composition call
/// sites that produced them.
///
/// Built during module-call expansion: every leaf resource added to
/// the expanded `ParsedFile` records, in `leaf_to_call_sites`, the
/// composition(s) that nest it — outermost first. A leaf at the root
/// of the DSL (not inside any composition) maps to an empty `Vec`.
///
/// `ExpansionTrace` is **not** serialized to the state file. The
/// state layer only ever sees [`LeafNode`](super::LeafNode)s with
/// their [`PersistentId`]s; the trace is consumed by the display
/// layer, which folds the leaf list under composition rows using this
/// sidecar.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExpansionTrace {
    /// For each persistent (leaf) id, the chain of composition call
    /// sites that nest it — outermost first. An empty `Vec` means the
    /// leaf was declared at the DSL root, outside any composition.
    pub leaf_to_call_sites: HashMap<PersistentId, Vec<EphemeralId>>,
}

impl ExpansionTrace {
    /// Create an empty trace.
    pub fn new() -> Self {
        Self {
            leaf_to_call_sites: HashMap::new(),
        }
    }

    /// Record that `leaf` is produced inside the composition call
    /// chain `call_sites` (outermost first).
    ///
    /// Calling `record` again with the same `leaf` overwrites the
    /// previous chain — this matches the expander's contract that
    /// each leaf has exactly one originating call chain.
    pub fn record(&mut self, leaf: PersistentId, call_sites: Vec<EphemeralId>) {
        self.leaf_to_call_sites.insert(leaf, call_sites);
    }

    /// Look up the composition chain for `leaf`.
    ///
    /// Returns `None` if the leaf was never recorded (treat as "root-
    /// level leaf, no composition nesting"). Callers that always want
    /// a slice can use [`call_sites_of`](Self::call_sites_of) which
    /// returns an empty slice for the not-recorded case.
    pub fn get(&self, leaf: &PersistentId) -> Option<&[EphemeralId]> {
        self.leaf_to_call_sites.get(leaf).map(Vec::as_slice)
    }

    /// Look up the composition chain for `leaf`, returning an empty
    /// slice if the leaf has no recorded chain (root-level).
    pub fn call_sites_of(&self, leaf: &PersistentId) -> &[EphemeralId] {
        self.leaf_to_call_sites
            .get(leaf)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Number of recorded leaves.
    pub fn len(&self) -> usize {
        self.leaf_to_call_sites.len()
    }

    /// Whether no leaves have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.leaf_to_call_sites.is_empty()
    }

    /// Iterate `(leaf, call_sites)` pairs in `HashMap` order.
    pub fn iter(&self) -> impl Iterator<Item = (&PersistentId, &Vec<EphemeralId>)> {
        self.leaf_to_call_sites.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ResourceId;

    fn pid(name: &str) -> PersistentId {
        PersistentId::new(ResourceId::new("aws.s3.Bucket", name))
    }

    fn eid(name: &str) -> EphemeralId {
        EphemeralId::new(ResourceId::new("_virtual", name))
    }

    #[test]
    fn new_is_empty() {
        let t = ExpansionTrace::new();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn record_and_get_round_trip() {
        let mut t = ExpansionTrace::new();
        let leaf = pid("inner");
        let chain = vec![eid("outer"), eid("inner_comp")];
        t.record(leaf.clone(), chain.clone());
        assert_eq!(t.get(&leaf), Some(chain.as_slice()));
        assert_eq!(t.call_sites_of(&leaf), chain.as_slice());
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn call_sites_of_unrecorded_leaf_is_empty() {
        let t = ExpansionTrace::new();
        let leaf = pid("never_recorded");
        assert_eq!(t.call_sites_of(&leaf), &[] as &[EphemeralId]);
        assert!(t.get(&leaf).is_none());
    }

    #[test]
    fn record_overwrites() {
        // The expander's contract: each leaf has exactly one
        // originating chain. A second `record` for the same leaf
        // replaces, not appends.
        let mut t = ExpansionTrace::new();
        let leaf = pid("inner");
        t.record(leaf.clone(), vec![eid("outer1")]);
        t.record(leaf.clone(), vec![eid("outer2")]);
        assert_eq!(t.call_sites_of(&leaf), &[eid("outer2")]);
    }

    #[test]
    fn iter_yields_every_recorded_leaf_exactly_once() {
        let mut t = ExpansionTrace::new();
        t.record(pid("b_leaf"), vec![eid("c1")]);
        t.record(pid("a_leaf"), vec![eid("c2")]);
        // `HashMap`-backed iteration order is not guaranteed; only
        // the *set* of keys is observable. Verify every recorded leaf
        // shows up exactly once.
        let mut names: Vec<&str> = t.iter().map(|(p, _)| p.inner().name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a_leaf", "b_leaf"]);
    }

    #[test]
    fn root_level_leaves_can_be_represented_by_empty_chain() {
        let mut t = ExpansionTrace::new();
        let leaf = pid("logs");
        // A leaf declared at the DSL root has no composition nesting.
        // The convention is "absent from the map", but recording an
        // empty Vec is also legal and useful for display passes that
        // want every leaf accounted for.
        t.record(leaf.clone(), vec![]);
        assert_eq!(t.call_sites_of(&leaf), &[] as &[EphemeralId]);
        // `get` returns `Some(&[])` here, not `None` — both forms are
        // semantically equivalent for the display layer.
        assert_eq!(t.get(&leaf), Some(&[] as &[EphemeralId]));
    }
}
