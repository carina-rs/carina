//! `LeafNode` — owned enum over post-expansion top-level node kinds.
//!
//! `LeafNode` is the [`GraphNode`](super::GraphNode) **subset** that
//! contains only the variants which survive composition expansion:
//! resources (CRUD) and data sources (read). A
//! [`Composition`](super::Composition) is, by definition, the
//! intermediate-only node that expands away into leaves; it is
//! therefore deliberately not representable by `LeafNode`.
//!
//! ## Why a separate enum
//!
//! The `#3287` design splits the IR pipeline so that
//! `Composition` is a parse-time construct, while the differ /
//! executor / state layers only ever see leaves. Encoding the
//! "no `Composition` past this point" invariant in the type system —
//! rather than in a hand-maintained doctest or a runtime guard —
//! means a stale or misguided caller cannot pass a composition into
//! the post-expansion path: the code does not type-check.
//!
//! Today the practical guarantee is structural: `LeafNode` has two
//! variants and no `Composition` arm. As callers migrate from
//! threading `&[Resource], &[DataSource]` pairs to receiving
//! `&[LeafNode]` (or iterating `into_leaf_nodes()`), the type guard
//! propagates outward; future PRs in the series tighten the
//! `expand` boundary so that signatures explicitly hand off
//! `Vec<GraphNode>` upstream and `Vec<LeafNode>` downstream.
//!
//! ## Conversion from `GraphNode`
//!
//! - `GraphNode::Resource(r)` / `GraphNode::DataSource(d)` → matching
//!   `LeafNode` variant.
//! - `GraphNode::Composition(_)` → `None`. Use [`expand_to_leaves`] to
//!   collect the leaf nodes from a `Vec<GraphNode>` while dropping
//!   compositions, or [`TryFrom<GraphNode>`](LeafNode#impl-TryFrom<GraphNode>-for-LeafNode)
//!   for single-node conversion that surfaces the failure.

use serde::{Deserialize, Serialize};

use super::{Composition, DataSource, GraphNode, Resource, ResourceId};

/// An owned post-expansion IR node — either a resource (CRUD) or a
/// data source (read-only).
///
/// `LeafNode` is the type-level guarantee that
/// [`Composition`](super::Composition) cannot enter post-expansion
/// paths. Convert from [`GraphNode`](super::GraphNode) via
/// [`TryFrom<GraphNode>`](#impl-TryFrom<GraphNode>-for-LeafNode) (for
/// a single node where the caller wants to handle the composition
/// case) or [`expand_to_leaves`] (to drop compositions in bulk).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LeafNode {
    Resource(Resource),
    DataSource(DataSource),
}

impl LeafNode {
    /// The id of the underlying node.
    pub fn id(&self) -> &ResourceId {
        match self {
            LeafNode::Resource(r) => &r.id,
            LeafNode::DataSource(d) => &d.id,
        }
    }

    /// The id of the underlying node wrapped as a [`PersistentId`](super::PersistentId).
    ///
    /// `LeafNode` is by construction a leaf — never a composition —
    /// so its id is always `Persistent`. APIs that take
    /// `&PersistentId` can be fed from a `LeafNode` without further
    /// discrimination.
    pub fn persistent_id(&self) -> super::PersistentId {
        super::PersistentId::new(self.id().clone())
    }

    /// Whether this leaf is a [`Resource`] (CRUD variant).
    pub fn is_resource(&self) -> bool {
        matches!(self, LeafNode::Resource(_))
    }

    /// Whether this leaf is a [`DataSource`] (read-only variant).
    pub fn is_data_source(&self) -> bool {
        matches!(self, LeafNode::DataSource(_))
    }

    /// Borrow the underlying resource if this is a `Resource` variant.
    pub fn as_resource(&self) -> Option<&Resource> {
        match self {
            LeafNode::Resource(r) => Some(r),
            _ => None,
        }
    }

    /// Borrow the underlying data source if this is a `DataSource` variant.
    pub fn as_data_source(&self) -> Option<&DataSource> {
        match self {
            LeafNode::DataSource(d) => Some(d),
            _ => None,
        }
    }
}

impl From<Resource> for LeafNode {
    fn from(r: Resource) -> Self {
        LeafNode::Resource(r)
    }
}

impl From<DataSource> for LeafNode {
    fn from(d: DataSource) -> Self {
        LeafNode::DataSource(d)
    }
}

impl From<LeafNode> for GraphNode {
    fn from(leaf: LeafNode) -> Self {
        match leaf {
            LeafNode::Resource(r) => GraphNode::Resource(r),
            LeafNode::DataSource(d) => GraphNode::DataSource(d),
        }
    }
}

/// A `GraphNode` was a `Composition`, so it cannot become a `LeafNode`.
///
/// Returned by [`TryFrom<GraphNode> for LeafNode`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompositionNotALeaf(pub Composition);

impl TryFrom<GraphNode> for LeafNode {
    type Error = CompositionNotALeaf;

    fn try_from(node: GraphNode) -> Result<Self, Self::Error> {
        match node {
            GraphNode::Resource(r) => Ok(LeafNode::Resource(r)),
            GraphNode::DataSource(d) => Ok(LeafNode::DataSource(d)),
            GraphNode::Composition(c) => Err(CompositionNotALeaf(c)),
        }
    }
}

/// Expand a `Vec<GraphNode>` into leaves, discarding compositions.
///
/// This is the simple form of the expansion boundary: every
/// `Composition` is dropped from the output. A later PR in the series
/// will return an `ExpansionTrace` alongside the leaves so the display
/// layer can fold leaf rows under their originating composition; for
/// now the trace would be a placeholder unit and is therefore omitted
/// (#3295 adds it).
///
/// Use this at the post-parse / pre-differ boundary to obtain a
/// `Vec<LeafNode>` whose type prevents downstream callers from
/// accidentally handing them a composition.
pub fn expand_to_leaves(nodes: Vec<GraphNode>) -> Vec<LeafNode> {
    nodes
        .into_iter()
        .filter_map(|n| LeafNode::try_from(n).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_resource() -> Resource {
        Resource::new("aws.s3.Bucket", "b")
    }

    fn sample_data_source() -> DataSource {
        DataSource::new("aws.ec2.Ami", "ami")
    }

    fn sample_composition() -> Composition {
        use indexmap::IndexMap;
        use std::collections::{BTreeSet, HashSet};
        Composition {
            id: ResourceId::with_identity("_virtual", "m"),
            signature: super::super::Signature {
                arguments: IndexMap::new(),
                attributes: IndexMap::new(),
            },
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_name: "m".to_string(),
            instance: "i".to_string(),
            quoted_string_attrs: HashSet::new(),
        }
    }

    #[test]
    fn leaf_node_from_resource() {
        let r = sample_resource();
        let leaf: LeafNode = r.clone().into();
        assert!(leaf.is_resource());
        assert_eq!(leaf.id(), &r.id);
    }

    #[test]
    fn leaf_node_from_data_source() {
        let d = sample_data_source();
        let leaf: LeafNode = d.clone().into();
        assert!(leaf.is_data_source());
        assert_eq!(leaf.id(), &d.id);
    }

    #[test]
    fn try_from_graph_node_resource_ok() {
        let node = GraphNode::Resource(sample_resource());
        let leaf = LeafNode::try_from(node).expect("resource is a leaf");
        assert!(leaf.is_resource());
    }

    #[test]
    fn try_from_graph_node_data_source_ok() {
        let node = GraphNode::DataSource(sample_data_source());
        let leaf = LeafNode::try_from(node).expect("data source is a leaf");
        assert!(leaf.is_data_source());
    }

    #[test]
    fn try_from_graph_node_composition_err() {
        let c = sample_composition();
        let node = GraphNode::Composition(c.clone());
        let err = LeafNode::try_from(node).expect_err("composition is not a leaf");
        assert_eq!(err.0.id, c.id);
    }

    #[test]
    fn expand_to_leaves_drops_compositions() {
        let nodes = vec![
            GraphNode::Resource(sample_resource()),
            GraphNode::Composition(sample_composition()),
            GraphNode::DataSource(sample_data_source()),
        ];
        let leaves = expand_to_leaves(nodes);
        assert_eq!(leaves.len(), 2);
        assert!(leaves[0].is_resource());
        assert!(leaves[1].is_data_source());
    }

    #[test]
    fn leaf_node_round_trip_through_graph_node() {
        let leaf: LeafNode = sample_resource().into();
        let node: GraphNode = leaf.clone().into();
        let back: LeafNode = LeafNode::try_from(node).expect("round-trips");
        assert_eq!(leaf, back);
    }

    /// Composition cannot be constructed as a `LeafNode`. Pinning this
    /// at the type level (no `LeafNode::Composition` variant exists)
    /// is the core guarantee of this enum.
    ///
    /// ```compile_fail
    /// use carina_core::resource::{LeafNode, Composition, ResourceId, Signature};
    /// use indexmap::IndexMap;
    /// use std::collections::{BTreeSet, HashSet};
    /// let c = Composition {
    ///     id: ResourceId::with_identity("_virtual", "m"),
    ///     signature: Signature {
    ///         arguments: IndexMap::new(),
    ///         attributes: IndexMap::new(),
    ///     },
    ///     binding: None,
    ///     dependency_bindings: BTreeSet::new(),
    ///     module_name: "m".to_string(),
    ///     instance: "i".to_string(),
    ///     quoted_string_attrs: HashSet::new(),
    /// };
    /// // No `From<Composition> for LeafNode` impl exists. This must
    /// // fail to type-check.
    /// let _leaf: LeafNode = c.into();
    /// ```
    #[allow(dead_code)]
    fn _doctest_anchor() {}
}
