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

use std::borrow::Cow;
use std::collections::BTreeSet;

use indexmap::IndexMap;

use super::{Composition, DataSource, Resource, ResourceId, Value};

/// Read-only accessors shared by all resource representations.
///
/// Object-safe: `&dyn ResourceLike` is a legal type.
pub trait ResourceLike {
    /// Stable identifier of this resource.
    fn id(&self) -> &ResourceId;

    /// Source-order preserving attribute map as `Value`s.
    ///
    /// Returned as a [`Cow`] because [`Composition`](super::Composition)
    /// stores its attributes as
    /// [`CompositionAttribute`](super::CompositionAttribute) (#3294)
    /// and must materialize a `Value`-typed view on demand; the other
    /// two siblings return a borrowed reference into their owned
    /// `IndexMap<String, Value>`. Callers can `.iter()` /
    /// `.contains_key()` / `.get()` through the `Cow` directly via
    /// `Deref`.
    fn attributes(&self) -> Cow<'_, IndexMap<String, Value>>;

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
    fn attributes(&self) -> Cow<'_, IndexMap<String, Value>> {
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
    fn attributes(&self) -> Cow<'_, IndexMap<String, Value>> {
        Cow::Borrowed(&self.attributes)
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
    /// Materializes the I/O surface's `attributes` half as a
    /// `Value`-typed map.
    ///
    /// `Composition.signature.attributes` is
    /// `IndexMap<String, CompositionAttribute>` since #3294 — the
    /// typed variant carries the same information as the pre-#3294
    /// `Value` form (cf. `CompositionAttribute::to_value`), so this
    /// materialization is lossless and used only by callers that
    /// haven't yet been ported to dispatch on the variant. Direct
    /// consumers can read `c.signature.attributes` instead.
    ///
    /// `arguments` is composition-only and reached via
    /// `c.signature.arguments` directly.
    fn attributes(&self) -> Cow<'_, IndexMap<String, Value>> {
        Cow::Owned(
            self.signature
                .attributes
                .iter()
                .map(|(k, attr)| (k.clone(), attr.to_value()))
                .collect(),
        )
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
    fn attributes(&self) -> Cow<'_, IndexMap<String, Value>> {
        Cow::Borrowed(&self.attributes)
    }
    fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependency_bindings
    }
}
