//! `GraphNode` — owned umbrella enum over the three top-level
//! IR node kinds.
//!
//! Carina's IR has three sibling node types — [`Resource`],
//! [`DataSource`], [`Composition`] — that the `#3287` series splits by
//! lifecycle (CRUD vs read-only vs module-call expansion). `GraphNode`
//! is the owned single-type umbrella over them. It carries an instance
//! of *one* variant; functions that want to receive any kind of node
//! by value take `GraphNode` instead of threading three slices.
//!
//! ## Owned vs borrowed
//!
//! - **Borrowed view**: [`ResourceRef<'a>`](crate::parser::ResourceRef)
//!   already exists and is what call sites use when iterating over a
//!   `ParsedFile`. It includes a fourth `Deferred` arm for deferred
//!   for-expression templates.
//! - **Owned form**: [`GraphNode`] is for storage and ownership
//!   transfer (e.g. plan-engine intermediate representations,
//!   post-expansion graphs). It has no `Deferred` arm because deferred
//!   templates are only meaningful while the parser still holds them.
//!
//! Conversion: see [`GraphNode::as_ref`] and the
//! `From<Resource>` / `From<DataSource>` / `From<Composition>` impls.

use serde::{Deserialize, Serialize};

use super::{Composition, DataSource, Resource, ResourceId};

/// An owned IR node — either a resource (CRUD), data source (read), or
/// composition (module-call expansion).
///
/// `GraphNode` is the **owned** umbrella over the three sibling node
/// types established by the `#3169` typestate split. Carrying any of
/// the three as `GraphNode` lets a function expose a single argument
/// type for "any top-level node" without resorting to dyn trait
/// objects or threading three parallel slices.
///
/// To inspect a `GraphNode` without taking ownership, call
/// [`GraphNode::as_ref`] which returns a borrowing
/// [`ResourceRef`](crate::parser::ResourceRef).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GraphNode {
    Resource(Resource),
    DataSource(DataSource),
    Composition(Composition),
}

impl GraphNode {
    /// The id of the underlying node.
    pub fn id(&self) -> &ResourceId {
        match self {
            GraphNode::Resource(r) => &r.id,
            GraphNode::DataSource(d) => &d.id,
            GraphNode::Composition(c) => &c.id,
        }
    }

    /// Whether this node is a [`Resource`] (CRUD variant).
    pub fn is_resource(&self) -> bool {
        matches!(self, GraphNode::Resource(_))
    }

    /// Whether this node is a [`DataSource`] (read-only variant).
    pub fn is_data_source(&self) -> bool {
        matches!(self, GraphNode::DataSource(_))
    }

    /// Whether this node is a [`Composition`] (module-call expansion variant).
    pub fn is_composition(&self) -> bool {
        matches!(self, GraphNode::Composition(_))
    }

    /// Borrow the underlying resource if this is a `Resource` variant.
    pub fn as_resource(&self) -> Option<&Resource> {
        match self {
            GraphNode::Resource(r) => Some(r),
            _ => None,
        }
    }

    /// Borrow the underlying data source if this is a `DataSource` variant.
    pub fn as_data_source(&self) -> Option<&DataSource> {
        match self {
            GraphNode::DataSource(d) => Some(d),
            _ => None,
        }
    }

    /// Borrow the underlying composition if this is a `Composition` variant.
    pub fn as_composition(&self) -> Option<&Composition> {
        match self {
            GraphNode::Composition(c) => Some(c),
            _ => None,
        }
    }
}

impl From<Resource> for GraphNode {
    fn from(r: Resource) -> Self {
        GraphNode::Resource(r)
    }
}

impl From<DataSource> for GraphNode {
    fn from(d: DataSource) -> Self {
        GraphNode::DataSource(d)
    }
}

impl From<Composition> for GraphNode {
    fn from(c: Composition) -> Self {
        GraphNode::Composition(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_node_from_resource_round_trip() {
        let r = Resource::new("aws.s3.Bucket", "b");
        let id = r.id.clone();
        let node: GraphNode = r.into();
        assert!(node.is_resource());
        assert_eq!(node.id(), &id);
        assert!(node.as_resource().is_some());
        assert!(node.as_data_source().is_none());
        assert!(node.as_composition().is_none());
    }

    #[test]
    fn graph_node_from_data_source_round_trip() {
        let d = DataSource::new("aws.ec2.Ami", "ami");
        let id = d.id.clone();
        let node: GraphNode = d.into();
        assert!(node.is_data_source());
        assert_eq!(node.id(), &id);
        assert!(node.as_data_source().is_some());
        assert!(node.as_resource().is_none());
    }

    #[test]
    fn graph_node_from_composition_round_trip() {
        use indexmap::IndexMap;
        use std::collections::{BTreeSet, HashSet};
        let c = Composition {
            id: ResourceId::new("_virtual", "m"),
            attributes: IndexMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_name: "m".to_string(),
            instance: "i".to_string(),
            quoted_string_attrs: HashSet::new(),
        };
        let id = c.id.clone();
        let node: GraphNode = c.into();
        assert!(node.is_composition());
        assert_eq!(node.id(), &id);
        assert!(node.as_composition().is_some());
        assert!(node.as_resource().is_none());
    }
}
