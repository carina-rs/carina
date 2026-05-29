//! `ResolvedAttrType` — a private wrapper that proves an
//! `&AttributeType` is not `AttributeType::Ref`.
//!
//! Lives in its own submodule so the tuple-struct constructor
//! `ResolvedAttrType(...)` is **not callable** from anywhere outside
//! this file. The only public path to construct a value is
//! [`new_after_peel`], which is itself `pub(super)` and is invoked
//! exclusively by [`super::AttributeType::resolve_refs`] after
//! peeling every `Ref` hop. A future contributor adding code in
//! `schema/mod.rs` (or any other carina-core file) cannot synthesise a
//! `ResolvedAttrType(&AttributeType::Ref(...))` because the
//! constructor is module-private to *this* file, not module-private to
//! `schema/`.
//!
//! See [`super::AttributeType::resolve_refs`] and the
//! `ResolvedAttrType` doc-comment for the carina#3340 / carina#3349
//! invariant being enforced.

use super::AttributeType;

/// A reference to an [`AttributeType`] that is **guaranteed not to be**
/// [`AttributeType::Ref`].
///
/// Constructed exclusively by [`super::AttributeType::resolve_refs`].
/// The tuple field is private to this submodule, so no code outside
/// `resolved_attr_type.rs` can synthesise a `ResolvedAttrType` from a
/// raw `&AttributeType` (including from a `Ref`). Callers `match` on
/// the wrapper's [`Self::as_attr`] result; the wildcard arm is safe
/// because `Ref` is unreachable at the value level inside the wrapper.
///
/// Carina#3349 was the concrete prior failure: a wildcard arm in
/// `resolve_block_names` over a raw `&AttributeType` silently dropped
/// `Ref`-typed attributes, so `awscc.s3.Bucket.lifecycle_configuration`
/// (typed `Ref("LifecycleConfiguration")`) rejected the documented
/// `rule { }` block syntax. The carina#3340 chain documents the
/// broader walk-site invariant.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedAttrType<'a>(&'a AttributeType);

impl<'a> ResolvedAttrType<'a> {
    /// **Internal** — only invoke after every `Ref` hop has been
    /// peeled.  Calling this with `&AttributeType::Ref(_)` violates
    /// the type's invariant; the only legitimate caller is
    /// [`super::AttributeType::resolve_refs`].
    pub(super) fn new_after_peel(inner: &'a AttributeType) -> Self {
        debug_assert!(
            !matches!(inner, AttributeType::Ref(_)),
            "ResolvedAttrType constructor reached with an unpeeled Ref; \
             only AttributeType::resolve_refs may produce this type"
        );
        ResolvedAttrType(inner)
    }

    /// Borrow the underlying [`AttributeType`]. Guaranteed not to be
    /// the [`AttributeType::Ref`] variant; the wildcard arm in any
    /// `match self.as_attr() { ... }` is therefore safe.
    #[inline]
    pub fn as_attr(self) -> &'a AttributeType {
        self.0
    }
}
