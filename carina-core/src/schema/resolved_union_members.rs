//! Lazy, definition-aware view over Union members.
//!
//! Natural semantic APIs expose Union members through [`UnionMembers::iter`],
//! which peels them through the enclosing definition map. [`super::RawShape`]
//! remains the documented escape hatch for structural declaration relays.

use std::collections::BTreeMap;

use super::{
    AttributeType, RefPathResolution, ResolvedAttrType, UnionCanonicalRank,
    union_member_judgement_on_path,
};
use crate::resource::Value;

/// Opaque storage for raw Union declarations.
///
/// Matching [`super::AttrTypeKind::Union`] does not expose definition-blind
/// iteration to semantic consumers; they use [`UnionMembers`]. Structural
/// relays can deliberately obtain the declaration through [`super::RawShape`].
#[derive(Clone)]
pub(crate) struct UnionMemberTypes(Vec<AttributeType>);

impl UnionMemberTypes {
    pub(super) fn new(members: Vec<AttributeType>) -> Self {
        Self(members)
    }

    pub(super) fn as_raw_slice(&self) -> &[AttributeType] {
        &self.0
    }
}

impl std::fmt::Debug for UnionMemberTypes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A lazy, declaration-ordered view of one Union's members together with the
/// definition map required to interpret them.
///
/// The raw `&[AttributeType]` is deliberately private. Iteration yields only
/// members whose top-level Ref chain has been peeled, making it impossible for
/// semantic consumers to forget definition-aware resolution.
///
/// ```compile_fail
/// use carina_core::schema::UnionMembers;
///
/// fn bypass_resolution(members: UnionMembers<'_>) {
///     for _ in members.members {}
/// }
/// ```
#[derive(Debug, Clone)]
pub struct UnionMembers<'a> {
    members: &'a [AttributeType],
    defs: &'a BTreeMap<String, AttributeType>,
    visited_refs: Vec<&'a str>,
}

impl<'a> UnionMembers<'a> {
    pub(super) fn new_after_union(
        members: &'a [AttributeType],
        defs: &'a BTreeMap<String, AttributeType>,
    ) -> Self {
        Self::new_after_union_on_path(members, defs, Vec::new())
    }

    pub(super) fn new_after_union_on_path(
        members: &'a [AttributeType],
        defs: &'a BTreeMap<String, AttributeType>,
        visited_refs: Vec<&'a str>,
    ) -> Self {
        Self {
            members,
            defs,
            visited_refs,
        }
    }

    /// Number of declared members, including members whose Ref chain cannot
    /// be resolved. This preserves declaration-shape checks such as the exact
    /// two-arm `string_or_list_of_strings` predicate.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the Union declares no members.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Iterate in declaration order, gracefully yielding `None` for a member
    /// whose top-level Ref chain is missing, cyclic, or exceeds the hop bound.
    /// Each sibling starts from the view's retained path; consumers performing
    /// a non-consuming recursive selection use [`Self::select_on_path`]
    /// instead so only the chosen sibling extends that path.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Option<ResolvedUnionMember<'a>>> + '_ {
        self.members.iter().map(move |member| {
            let mut visited_refs = self.visited_refs.clone();
            super::resolve_refs_on_path(member, self.defs, &mut visited_refs)
                .ok()
                .map(|resolution| ResolvedUnionMember::new(resolution.attr))
        })
    }

    /// Return whether any declaration-ordered member satisfies `predicate`
    /// while retaining that member's peeled Ref names for the duration of the
    /// callback. Each sibling starts from the same path and resolution
    /// failures are skipped.
    ///
    /// Non-consuming recursive walkers must use this instead of [`Self::iter`]
    /// so a member that peels to another Union cannot lose its cycle guard
    /// before the recursive call.
    pub(crate) fn any_on_path(
        &self,
        visited_refs: &mut Vec<&'a str>,
        mut predicate: impl FnMut(ResolvedUnionMember<'a>, &mut Vec<&'a str>) -> bool,
    ) -> bool {
        let restore_len = visited_refs.len();
        for name in &self.visited_refs {
            if !visited_refs.contains(name) {
                visited_refs.push(name);
            }
        }

        for member in self.members {
            let Ok(resolution) = super::resolve_refs_on_path(member, self.defs, visited_refs)
            else {
                continue;
            };
            let matched = predicate(ResolvedUnionMember::new(resolution.attr), visited_refs);
            visited_refs.truncate(resolution.restore_len);
            if matched {
                visited_refs.truncate(restore_len);
                return true;
            }
        }

        visited_refs.truncate(restore_len);
        false
    }

    /// Find the first declaration-ordered member satisfying `predicate`.
    /// Resolution failures are skipped. On success, the caller retains only
    /// the selected member's own Ref chain beyond its entry path; Ref names
    /// stored on this view are used for cycle detection but never leaked.
    pub(crate) fn find_on_path(
        &self,
        visited_refs: &mut Vec<&'a str>,
        mut predicate: impl FnMut(ResolvedUnionMember<'a>, &mut Vec<&'a str>) -> bool,
    ) -> Option<ResolvedUnionMember<'a>> {
        let restore_len = visited_refs.len();
        for name in &self.visited_refs {
            if !visited_refs.contains(name) {
                visited_refs.push(name);
            }
        }

        let mut selected = None;
        for member in self.members {
            let Ok(resolution) = super::resolve_refs_on_path(member, self.defs, visited_refs)
            else {
                continue;
            };
            let matches = predicate(ResolvedUnionMember::new(resolution.attr), visited_refs);
            visited_refs.truncate(resolution.restore_len);
            if matches {
                selected = Some(member);
                break;
            }
        }

        visited_refs.truncate(restore_len);
        let selected = selected?;
        let resolution = super::resolve_refs_on_path(selected, self.defs, visited_refs).ok()?;
        Some(ResolvedUnionMember::new(resolution.attr))
    }

    /// Select the structurally best member through the same judgement used for
    /// validation failure attribution. The strict maximum rank wins, leaving
    /// declaration order as the tie-break; no structural match returns `None`.
    /// Only the chosen member's Ref names remain on the caller's current
    /// non-consuming path.
    pub(crate) fn select_on_path(
        &self,
        value: &Value,
        visited_refs: &mut Vec<&'a str>,
    ) -> Option<ResolvedUnionMember<'a>> {
        let projected = value.as_concrete()?;
        let restore_len = visited_refs.len();
        for name in &self.visited_refs {
            if !visited_refs.contains(name) {
                visited_refs.push(name);
            }
        }
        let mut best: Option<(UnionCanonicalRank, &AttributeType)> = None;
        for member in self.members {
            let judgement =
                union_member_judgement_on_path(member, projected, self.defs, visited_refs);
            if judgement.canonical_rank.is_match()
                && best
                    .as_ref()
                    .is_none_or(|(previous, _)| judgement.canonical_rank > *previous)
            {
                best = Some((judgement.canonical_rank, member));
            }
        }

        let Some((_, selected)) = best else {
            visited_refs.truncate(restore_len);
            return None;
        };
        visited_refs.truncate(restore_len);
        let Ok(RefPathResolution { attr, .. }) =
            super::resolve_refs_on_path(selected, self.defs, visited_refs)
        else {
            return None;
        };
        Some(ResolvedUnionMember::new(attr))
    }
}

/// One Union member whose top-level Ref chain is proven peeled.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedUnionMember<'a>(ResolvedAttrType<'a>);

impl<'a> ResolvedUnionMember<'a> {
    fn new(attr: &'a AttributeType) -> Self {
        Self(ResolvedAttrType::new_after_peel(attr))
    }

    /// Borrow the member after its top-level Ref chain has been peeled.
    pub fn as_attr(self) -> &'a AttributeType {
        self.0.as_attr()
    }
}
