//! Reference traversal separates graph keys from scoped diagnostic rendering.
use std::collections::{HashMap, HashSet};

use super::{
    AccessPath, BindingAddress, DiagnosticPath, DiagnosticScope, InsideSecret, ReferencePath,
    SecretReadResult,
};

/// An opaque graph key. It has equality, ordering and lookup, but no rendering.
///
/// ```compile_fail
/// use carina_core::resource::{DependencyKey, RelativeBindingName};
/// let key = DependencyKey::new(RelativeBindingName::new("private"));
/// let _ = format!("{key}");
/// ```
///
/// ```compile_fail
/// use carina_core::resource::{DependencyKey, RelativeBindingName};
/// let key = DependencyKey::new(RelativeBindingName::new("private"));
/// let _ = key.to_dot_string();
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DependencyKey<K>(K);

impl<K> DependencyKey<K> {
    pub fn new(key: K) -> Self {
        Self(key)
    }
    /// Add an identity to an existing graph's key collection.
    pub(crate) fn record_graph_key(&self, keys: &mut impl Extend<K>)
    where
        K: Clone,
    {
        keys.extend(std::iter::once(self.0.clone()));
    }
    pub fn matches(&self, key: &K) -> bool
    where
        K: PartialEq,
    {
        &self.0 == key
    }
    pub fn lookup_borrowed<'a, V>(&self, keys: &'a HashMap<&K, V>) -> Option<&'a V>
    where
        K: Eq + std::hash::Hash,
    {
        keys.get(&self.0)
    }
    pub fn lookup_indexmap<'a, V>(&self, keys: &'a indexmap::IndexMap<K, V>) -> Option<&'a V>
    where
        K: Eq + std::hash::Hash,
    {
        keys.get(&self.0)
    }
    pub fn is_in(&self, keys: &HashSet<K>) -> bool
    where
        K: Eq + std::hash::Hash,
    {
        keys.contains(&self.0)
    }
}
impl<K> std::fmt::Debug for DependencyKey<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DependencyKey { .. }")
    }
}
impl<B: BindingAddress> DependencyKey<B> {
    pub fn matches_name(&self, name: &str) -> bool {
        self.0.as_str() == name
    }
    pub fn starts_with(&self, prefix: &str) -> bool {
        self.0.as_str().starts_with(prefix)
    }
    pub fn lookup_by_name<'a, V>(&self, keys: &'a HashMap<String, V>) -> Option<&'a V> {
        keys.get(self.0.as_str())
    }
    pub fn is_in_names<Q>(&self, keys: &HashSet<Q>) -> bool
    where
        Q: std::borrow::Borrow<str> + Eq + std::hash::Hash,
    {
        keys.contains(self.0.as_str())
    }
    pub fn ends_with(&self, suffix: &str) -> bool {
        self.0.as_str().ends_with(suffix)
    }
}

impl DependencyKey<String> {
    pub(crate) fn is_in_strings<Q>(&self, keys: &HashSet<Q>) -> bool
    where
        Q: std::borrow::Borrow<str> + Eq + std::hash::Hash,
    {
        keys.contains(self.0.as_str())
    }
}

/// Dependency information for an access reference. Subscripts and path rendering
/// are deliberately absent; use a diagnostic visitor to obtain a scoped path.
#[derive(Clone, Copy)]
pub struct DependencyResourceRef<'a, B: BindingAddress> {
    path: &'a AccessPath<B>,
}
impl<'a, B: BindingAddress> DependencyResourceRef<'a, B> {
    pub(super) fn new(path: &'a AccessPath<B>) -> Self {
        Self { path }
    }
    pub fn binding_key(&self) -> DependencyKey<B> {
        DependencyKey::new(self.path.binding_address().clone())
    }
    pub fn attribute_key(&self) -> DependencyKey<String> {
        DependencyKey::new(self.path.attribute().to_owned())
    }
    pub(crate) fn matches_occurrence(&self, path: &AccessPath<B>) -> bool {
        self.path == path
            && self.path.reference_locations_shared() == path.reference_locations_shared()
    }
    pub(crate) fn source(&self) -> Option<(Option<std::path::PathBuf>, usize)> {
        self.path.diagnostic_source_owned()
    }
    pub fn removed_attribute_was_sensitive<P: super::PlaceholderKind>(
        &self,
        bindings: &crate::binding_index::ResolvedBindings<P, B>,
    ) -> bool {
        bindings.removed_attribute_was_sensitive(self.path.binding(), self.path.attribute())
    }
}

/// A diagnostic reference never exposes its raw path or a detachable scope.
/// All spelling is produced by `DiagnosticPath`.
#[derive(Clone, Copy)]
pub struct ScopedResourceRef<'a, B: BindingAddress> {
    path: &'a AccessPath<B>,
    scope: DiagnosticScope,
}
impl<'a, B: BindingAddress> ScopedResourceRef<'a, B> {
    pub(super) fn new(path: &'a AccessPath<B>, scope: DiagnosticScope) -> Self {
        Self { path, scope }
    }
    pub fn identity(&self) -> DependencyResourceRef<'a, B> {
        DependencyResourceRef::new(self.path)
    }
    pub fn is_secret(&self) -> bool {
        self.scope.is_secret()
    }
    pub fn diagnostic_path(&self) -> DiagnosticPath<B> {
        self.scope.path(ReferencePath::Access(self.path.clone()))
    }
    pub fn binding_path(&self) -> DiagnosticPath<B> {
        self.scope
            .path(ReferencePath::Binding(self.path.binding_address().clone()))
    }
    pub fn head_path(&self) -> DiagnosticPath<B> {
        self.scope
            .path(ReferencePath::Access(AccessPath::from_binding(
                self.path.binding_address().clone(),
                self.path.attribute(),
                Vec::new(),
            )))
    }
    pub(crate) fn root_identifier_key(&self) -> DependencyKey<String> {
        DependencyKey::new(self.root_identifier().to_owned())
    }
    fn root_identifier(&self) -> &str {
        let root = self.path.binding();
        root.split(['[', ']']).next().unwrap_or(root)
    }
    pub(crate) fn root_identifier_path(&self) -> DiagnosticPath<super::RelativeBindingName> {
        self.scope
            .path(ReferencePath::Binding(self.root_identifier().into()))
    }
    /// Core checks must protect every error and cannot return plaintext on success.
    pub(crate) fn check<R: SecretReadResult, E: InsideSecret>(
        &self,
        check: impl FnOnce(&AccessPath<B>) -> Result<R, E>,
    ) -> Result<R, E> {
        check(self.path).map_err(|error| self.scope.protect_error(error))
    }
    pub(crate) fn analysis_reference(&self) -> Option<crate::parser::AnalysisReference<B>> {
        crate::parser::AnalysisReference::from_scoped(self.path, self.scope)
    }
    pub(crate) fn to_origin(&self) -> ScopedResourceOrigin<B> {
        ScopedResourceOrigin {
            path: self.path.clone(),
            scope: self.scope,
        }
    }
}

/// A bare binding can be used as a graph key or rendered through its scope.
#[derive(Clone, Copy)]
pub struct ScopedBindingRef<'a, B: BindingAddress> {
    binding: &'a B,
    scope: DiagnosticScope,
}
impl<'a, B: BindingAddress> ScopedBindingRef<'a, B> {
    pub(super) fn new(binding: &'a B, scope: DiagnosticScope) -> Self {
        Self { binding, scope }
    }
    pub fn identity(&self) -> DependencyKey<B> {
        DependencyKey::new(self.binding.clone())
    }
    pub fn is_secret(&self) -> bool {
        self.scope.is_secret()
    }
    pub fn diagnostic_path(&self) -> DiagnosticPath<B> {
        self.scope
            .path(ReferencePath::Binding(self.binding.clone()))
    }
    pub(crate) fn check<R: SecretReadResult, E: InsideSecret>(
        &self,
        check: impl FnOnce(&B) -> Result<R, E>,
    ) -> Result<R, E> {
        check(self.binding).map_err(|error| self.scope.protect_error(error))
    }
}

/// Owned provenance retains its scope across expansion without a raw-path accessor.
#[derive(Clone)]
pub(crate) struct ScopedResourceOrigin<B: BindingAddress> {
    path: AccessPath<B>,
    scope: DiagnosticScope,
}
impl<B: BindingAddress> ScopedResourceOrigin<B> {
    pub(crate) fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
    ) -> ScopedResourceOrigin<C> {
        ScopedResourceOrigin {
            path: self.path.map_bindings(map),
            scope: self.scope,
        }
    }
    pub(crate) fn as_ref(&self) -> ScopedResourceRef<'_, B> {
        ScopedResourceRef::new(&self.path, self.scope)
    }
    pub(crate) fn resolve<C: BindingAddress, E: InsideSecret, P: super::ValueKind>(
        &self,
        resolve: impl FnOnce(&AccessPath<B>) -> Result<super::Value<P, C>, E>,
    ) -> Result<Vec<ScopedResourceOrigin<C>>, E> {
        let value = resolve(&self.path).map_err(|error| self.scope.protect_error(error))?;
        let mut origins = Vec::new();
        value.visit_resource_refs_for_diagnostics(&mut |reference, _| {
            let mut origin = reference.to_origin();
            if self.scope.is_secret() {
                origin.scope = self.scope;
            }
            origins.push(origin);
        });
        Ok(origins)
    }
}
impl<B: BindingAddress> std::fmt::Debug for ScopedResourceOrigin<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_ref().diagnostic_path().fmt(f)
    }
}
