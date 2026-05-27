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
//! + module "cluster" (./modules/cluster)
//!     + aws.eks.Cluster      cluster/inner
//!     + aws.iam.Role         cluster/inner-role
//! + aws.s3.Bucket            logs
//! ```
//!
//! `ExpansionTrace` records that relationship as a sidecar map keyed
//! by leaf [`PersistentId`] → an outermost-first chain of [`CallSite`]
//! entries for the composition(s) that nest the leaf. Each entry
//! carries the call-site id (binding name) plus the module's
//! `use { source = "..." }` path so the renderer can label the group
//! with a DSL-visible name (carina#3322).
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

/// One composition call site in an expansion chain: the `EphemeralId`
/// that identifies the call (its instance binding) plus the module's
/// `use { source = "..." }` path, so the display layer can label the
/// group with a DSL-visible name like `module "r" (./modules/infra)`.
///
/// `source_path` is the path the user wrote in the `use` statement
/// (verbatim, not canonicalized) — the same surface form that appears
/// in the DSL. `None` means "no recorded path" (hand-built test
/// traces, or a synthesized call site that never went through
/// `process_imports`); the renderer drops the parenthesized suffix
/// in that case. Using `Option` here, not an empty `String`, keeps
/// "absent" syntactically distinct from "an empty user-written path"
/// — making the broken state of accidentally treating one as the
/// other unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallSite {
    pub id: EphemeralId,
    #[serde(default)]
    pub source_path: Option<String>,
}

impl CallSite {
    /// Create a call site with a recorded source path (the typical
    /// production shape: `use { source = "..." }` resolved into the
    /// expander).
    pub fn new(id: EphemeralId, source_path: impl Into<String>) -> Self {
        Self {
            id,
            source_path: Some(source_path.into()),
        }
    }

    /// Create a call site without a source path. Used by test
    /// fixtures and any caller that synthesizes a `CallSite` outside
    /// the `process_imports` → `expand_module_call` flow.
    pub fn without_source(id: EphemeralId) -> Self {
        Self {
            id,
            source_path: None,
        }
    }

    /// The call site's binding name (instance prefix), e.g. `cluster`.
    pub fn binding(&self) -> &str {
        self.id.inner().name.as_str()
    }
}

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
    pub leaf_to_call_sites: HashMap<PersistentId, Vec<CallSite>>,
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
    pub fn record(&mut self, leaf: PersistentId, call_sites: Vec<CallSite>) {
        self.leaf_to_call_sites.insert(leaf, call_sites);
    }

    /// Look up the composition chain for `leaf`.
    ///
    /// Returns `None` if the leaf was never recorded (treat as "root-
    /// level leaf, no composition nesting"). Callers that always want
    /// a slice can use [`call_sites_of`](Self::call_sites_of) which
    /// returns an empty slice for the not-recorded case.
    pub fn get(&self, leaf: &PersistentId) -> Option<&[CallSite]> {
        self.leaf_to_call_sites.get(leaf).map(Vec::as_slice)
    }

    /// Look up the composition chain for `leaf`, returning an empty
    /// slice if the leaf has no recorded chain (root-level).
    pub fn call_sites_of(&self, leaf: &PersistentId) -> &[CallSite] {
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
    pub fn iter(&self) -> impl Iterator<Item = (&PersistentId, &Vec<CallSite>)> {
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

    fn site(name: &str, source_path: &str) -> CallSite {
        CallSite::new(
            EphemeralId::new(ResourceId::new("_virtual", name)),
            source_path,
        )
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
        let chain = vec![
            site("outer", "./modules/outer"),
            site("inner_comp", "./modules/inner_comp"),
        ];
        t.record(leaf.clone(), chain.clone());
        assert_eq!(t.get(&leaf), Some(chain.as_slice()));
        assert_eq!(t.call_sites_of(&leaf), chain.as_slice());
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn call_site_carries_source_path_for_display() {
        // carina#3322: the renderer needs the `use { source = "..." }`
        // path so it can label a composition group with a
        // DSL-visible name like `module "r" (./modules/infra)`.
        let s = site("r", "./modules/infra");
        assert_eq!(s.binding(), "r");
        assert_eq!(s.source_path.as_deref(), Some("./modules/infra"));
    }

    #[test]
    fn call_site_without_source_has_none_path() {
        // A call site synthesized outside `process_imports` (test
        // harness, hand-built trace) keeps `source_path = None`.
        // The renderer drops the parenthesized `(<path>)` suffix in
        // that case — `None` is syntactically distinct from a real
        // user-supplied empty path.
        let s = CallSite::without_source(EphemeralId::new(ResourceId::new("_virtual", "r")));
        assert_eq!(s.binding(), "r");
        assert_eq!(s.source_path, None);
    }

    #[test]
    fn call_sites_of_unrecorded_leaf_is_empty() {
        let t = ExpansionTrace::new();
        let leaf = pid("never_recorded");
        assert_eq!(t.call_sites_of(&leaf), &[] as &[CallSite]);
        assert!(t.get(&leaf).is_none());
    }

    #[test]
    fn record_overwrites() {
        // The expander's contract: each leaf has exactly one
        // originating chain. A second `record` for the same leaf
        // replaces, not appends.
        let mut t = ExpansionTrace::new();
        let leaf = pid("inner");
        t.record(leaf.clone(), vec![site("outer1", "./a")]);
        t.record(leaf.clone(), vec![site("outer2", "./b")]);
        assert_eq!(t.call_sites_of(&leaf), &[site("outer2", "./b")]);
    }

    #[test]
    fn iter_yields_every_recorded_leaf_exactly_once() {
        let mut t = ExpansionTrace::new();
        t.record(pid("b_leaf"), vec![site("c1", "./c1")]);
        t.record(pid("a_leaf"), vec![site("c2", "./c2")]);
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
        assert_eq!(t.call_sites_of(&leaf), &[] as &[CallSite]);
        // `get` returns `Some(&[])` here, not `None` — both forms are
        // semantically equivalent for the display layer.
        assert_eq!(t.get(&leaf), Some(&[] as &[CallSite]));
    }
}
