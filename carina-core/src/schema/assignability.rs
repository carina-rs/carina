//! Boundary assignability when provider declarations may not be loaded.

use super::{AttrTypeKind, PatternForm, TypeIdentity, TypeInSchema, resolve_refs_on_path};
use crate::parser::CustomTypeValidationError;

/// Missing provider evidence is distinct from a proven type mismatch.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub enum TypeAssignability {
    Assignable,
    Mismatch,
    Unchecked(CustomTypeValidationError),
}

impl TypeAssignability {
    /// Loaded validation must prove assignment using the provider schemas.
    pub fn require_loaded(self) -> Result<bool, CustomTypeValidationError> {
        match self {
            Self::Assignable => Ok(true),
            Self::Mismatch => Ok(false),
            Self::Unchecked(reason) => Err(reason),
        }
    }

    /// Bootstrap/editor consumers explicitly permit missing scalar evidence.
    pub fn allow_unchecked(self) -> bool {
        match self {
            Self::Assignable | Self::Unchecked(_) => true,
            Self::Mismatch => false,
        }
    }

    fn all(results: impl IntoIterator<Item = Self>) -> Self {
        let mut result = Self::Assignable;
        for next in results {
            match next {
                Self::Mismatch => return Self::Mismatch,
                Self::Unchecked(reason) => result = Self::Unchecked(reason),
                Self::Assignable => {}
            }
        }
        result
    }

    fn any(results: impl IntoIterator<Item = Self>) -> Self {
        let mut result = Self::Mismatch;
        for next in results {
            match next {
                Self::Assignable => return Self::Assignable,
                Self::Unchecked(reason) => result = Self::Unchecked(reason),
                Self::Mismatch => {}
            }
        }
        result
    }
}

fn identity<R: PatternForm>(kind: &AttrTypeKind<R>) -> Option<&TypeIdentity> {
    match kind {
        AttrTypeKind::UnloadedSchemaType(unloaded) => Some(unloaded.identity()),
        AttrTypeKind::String { binding, .. }
        | AttrTypeKind::Int { binding, .. }
        | AttrTypeKind::Float { binding, .. } => binding.identity(),
        AttrTypeKind::Enum { identity, .. } => Some(identity),
        AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::List { .. }
        | AttrTypeKind::Map { .. }
        | AttrTypeKind::Struct { .. }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => None,
    }
}

impl<'source, R: PatternForm> TypeInSchema<'source, R> {
    /// Prove assignment, prove a mismatch, or defer for the existing
    /// `ProvidersNotLoaded` reason. Identified types use the shared directional
    /// identity relation, including provider and segments, even when a schema
    /// is unloaded. A source without identity cannot satisfy a nominal sink.
    /// Only comparisons that require the unavailable scalar kind are unchecked.
    pub fn assignment_to(self, sink: TypeInSchema<'_, R>) -> TypeAssignability {
        self.assignment_on_paths(sink, &mut Vec::new(), &mut Vec::new())
    }

    fn assignment_on_paths<'sink>(
        self,
        sink: TypeInSchema<'sink, R>,
        source_refs: &mut Vec<&'source str>,
        sink_refs: &mut Vec<&'sink str>,
    ) -> TypeAssignability {
        let Ok(source) = resolve_refs_on_path(self.attr, self.defs, source_refs) else {
            return TypeAssignability::Mismatch;
        };
        let Ok(target) = resolve_refs_on_path(sink.attr, sink.defs, sink_refs) else {
            source_refs.truncate(source.restore_len);
            return TypeAssignability::Mismatch;
        };
        let left = source.attr.in_schema(self.defs);
        let right = target.attr.in_schema(sink.defs);
        use AttrTypeKind::*;
        use TypeAssignability::*;
        let result = match (&source.attr.kind, &target.attr.kind) {
            (Union(members), _) => {
                TypeAssignability::all(members.as_raw_slice().iter().map(|member| {
                    member
                        .in_schema(self.defs)
                        .assignment_on_paths(right, source_refs, sink_refs)
                }))
            }
            (_, Union(members)) => {
                TypeAssignability::any(members.as_raw_slice().iter().map(|member| {
                    left.assignment_on_paths(member.in_schema(sink.defs), source_refs, sink_refs)
                }))
            }
            (UnloadedSchemaType(_), _) | (_, UnloadedSchemaType(_)) => {
                match (identity(&source.attr.kind), identity(&target.attr.kind)) {
                    (Some(left), Some(right)) => {
                        if left.assignable_to(right) {
                            Assignable
                        } else {
                            Mismatch
                        }
                    }
                    (None, Some(_)) => Mismatch,
                    (_, None) => Unchecked(CustomTypeValidationError::ProvidersNotLoaded),
                }
            }
            (
                List {
                    element_type: left, ..
                },
                List {
                    element_type: right,
                    ..
                },
            ) => left.in_schema(self.defs).assignment_on_paths(
                right.in_schema(sink.defs),
                source_refs,
                sink_refs,
            ),
            (Map { key: lk, value: lv }, Map { key: rk, value: rv }) => TypeAssignability::all([
                lk.in_schema(self.defs).assignment_on_paths(
                    rk.in_schema(sink.defs),
                    source_refs,
                    sink_refs,
                ),
                lv.in_schema(self.defs).assignment_on_paths(
                    rv.in_schema(sink.defs),
                    source_refs,
                    sink_refs,
                ),
            ]),
            (Struct { fields: left, .. }, Struct { fields: right, .. }) => {
                if left.len() != right.len() {
                    Mismatch
                } else {
                    TypeAssignability::all(right.iter().map(|field| {
                        match left.iter().find(|candidate| candidate.name == field.name) {
                            Some(candidate) => candidate
                                .field_type
                                .in_schema(self.defs)
                                .assignment_on_paths(
                                    field.field_type.in_schema(sink.defs),
                                    source_refs,
                                    sink_refs,
                                ),
                            None => Mismatch,
                        }
                    }))
                }
            }
            (Struct { fields, .. }, Map { value, .. }) => {
                TypeAssignability::all(fields.iter().map(|field| {
                    field.field_type.in_schema(self.defs).assignment_on_paths(
                        value.in_schema(sink.defs),
                        source_refs,
                        sink_refs,
                    )
                }))
            }
            _ => {
                if left.is_assignable_to_known(right) {
                    Assignable
                } else {
                    Mismatch
                }
            }
        };
        source_refs.truncate(source.restore_len);
        sink_refs.truncate(target.restore_len);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignment_consumers_preserve_the_unchecked_policy() {
        for (assignment, expected) in [
            (TypeAssignability::Assignable, true),
            (TypeAssignability::Mismatch, false),
        ] {
            assert_eq!(assignment.clone().require_loaded(), Ok(expected));
            assert_eq!(assignment.allow_unchecked(), expected);
        }
        let unchecked = TypeAssignability::Unchecked(CustomTypeValidationError::ProvidersNotLoaded);
        assert_eq!(
            unchecked.clone().require_loaded(),
            Err(CustomTypeValidationError::ProvidersNotLoaded)
        );
        assert!(unchecked.allow_unchecked());
    }
}
