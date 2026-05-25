//! Shared read-only accessor trait across the resource typestate
//! arms (#3174).
//!
//! Lets read-only consumers — plan tree builders, formatters,
//! diagnostics — stay generic over the three sibling types
//! ([`Resource`](super::Resource),
//! [`Composition`](super::Composition),
//! [`DataSource`](super::DataSource)). Write-side callers (resolver,
//! effect-executor, writeback) continue to take a concrete type and
//! benefit from the typed dispatch.

use std::collections::BTreeSet;

use indexmap::IndexMap;

use super::{Composition, DataSource, Resource, ResourceId, Value};

/// Read-only accessors shared by all resource representations.
///
/// Object-safe: `&dyn ResourceLike` is a legal type.
pub trait ResourceLike {
    /// Stable identifier of this resource.
    fn id(&self) -> &ResourceId;

    /// Source-order preserving attribute map.
    fn attributes(&self) -> &IndexMap<String, Value>;

    /// `let` binding name if any.
    fn binding(&self) -> Option<&str>;

    /// Binding names this resource depends on.
    fn dependency_bindings(&self) -> &BTreeSet<String>;
}

/// Blanket impl so a `&T` where `T: ResourceLike` is itself
/// `ResourceLike`. Lets generic callers — `fn f<R: ResourceLike>(r: R)` —
/// accept both an owned receiver and a borrowed one without forcing
/// every downstream site to spell `?Sized` bounds.
impl<T: ResourceLike + ?Sized> ResourceLike for &T {
    fn id(&self) -> &ResourceId {
        (**self).id()
    }
    fn attributes(&self) -> &IndexMap<String, Value> {
        (**self).attributes()
    }
    fn binding(&self) -> Option<&str> {
        (**self).binding()
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        (**self).dependency_bindings()
    }
}

impl ResourceLike for Resource {
    fn id(&self) -> &ResourceId {
        &self.id
    }
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependency_bindings
    }
}

impl ResourceLike for Composition {
    fn id(&self) -> &ResourceId {
        &self.id
    }
    /// Reads the I/O surface's `attributes` half.
    ///
    /// `Composition` is the only sibling that carries a [`Signature`]
    /// (#3292). The `arguments` half is not exposed through
    /// `ResourceLike` — the trait abstracts over the *output* surface
    /// that `Resource` and `DataSource` also expose, while
    /// `arguments` is composition-only and reached via
    /// `c.signature.arguments` directly.
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.signature.attributes
    }
    fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependency_bindings
    }
}

impl ResourceLike for DataSource {
    fn id(&self) -> &ResourceId {
        &self.id
    }
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependency_bindings
    }
}
