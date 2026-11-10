//! Pending local checks retain their admitted destination declarations and scope.

use super::*;
use std::sync::Arc;

#[derive(Debug, Clone)]
enum DestinationSchema {
    Draft {
        root: PatternAttributeType<Draft>,
        defs: BTreeMap<String, PatternAttributeType<Draft>>,
    },
    Compiled(Schema),
}

#[derive(Debug, Clone)]
enum DestinationChecks {
    Schema(Box<DestinationSchema>),
    All(Vec<DestinationObligation>),
    Any(Vec<DestinationObligation>),
}

#[derive(Clone, Copy)]
pub(super) enum DestinationPosition<'a> {
    ListElement,
    MapKey,
    MapValue,
    Field(&'a str),
}

/// The admitted destination whose local constraints must still be checked.
/// Declarations and definitions survive temporary type lifting and recursive
/// assignment. This is never evidence that a value has passed those checks.
#[derive(Debug, Clone)]
#[must_use]
pub struct DestinationObligation(Arc<DestinationChecks>);

impl Eq for DestinationObligation {}
impl PartialEq for DestinationObligation {
    fn eq(&self, other: &Self) -> bool {
        // Executable provenance belongs to this check, not merely its type name.
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl DestinationObligation {
    pub(super) fn draft(ty: TypeInSchema<'_, Draft>) -> Self {
        Self(Arc::new(DestinationChecks::Schema(Box::new(
            DestinationSchema::Draft {
                root: ty.attr.clone(),
                defs: ty.defs.clone(),
            },
        ))))
    }
    pub(super) fn compiled(ty: TypeInSchema<'_, Compiled>) -> Self {
        Self(Arc::new(DestinationChecks::Schema(Box::new(
            DestinationSchema::Compiled(Schema {
                root: ty.attr.clone(),
                defs: ty.defs.clone(),
            }),
        ))))
    }
    pub(crate) fn merge(self, other: Self) -> Self {
        Self(Arc::new(DestinationChecks::All(vec![self, other])))
    }
    pub(super) fn alternative(self, other: Self) -> Self {
        Self(Arc::new(DestinationChecks::Any(vec![self, other])))
    }

    pub(super) fn within<R: PatternForm>(
        self,
        parent: TypeInSchema<'_, R>,
        position: DestinationPosition<'_>,
    ) -> Self {
        self.within_schema(&R::assignment_destination(parent), position)
    }

    fn within_schema(&self, parent: &Self, position: DestinationPosition<'_>) -> Self {
        let checks = match self.0.as_ref() {
            DestinationChecks::All(checks) => DestinationChecks::All(
                checks
                    .iter()
                    .map(|check| check.within_schema(parent, position))
                    .collect(),
            ),
            DestinationChecks::Any(checks) => DestinationChecks::Any(
                checks
                    .iter()
                    .map(|check| check.within_schema(parent, position))
                    .collect(),
            ),
            DestinationChecks::Schema(child) => {
                // Both snapshots were created by the same sealed PatternForm.
                let DestinationChecks::Schema(parent) = parent.0.as_ref() else {
                    unreachable!("a scope is one declaration")
                };
                DestinationChecks::Schema(Box::new(match (parent.as_ref(), child.as_ref()) {
                    (
                        DestinationSchema::Draft { root, defs },
                        DestinationSchema::Draft {
                            root: child,
                            defs: _,
                        },
                    ) => DestinationSchema::Draft {
                        root: scoped_root(root, child, position),
                        defs: defs.clone(),
                    },
                    (DestinationSchema::Compiled(parent), DestinationSchema::Compiled(child)) => {
                        DestinationSchema::Compiled(Schema {
                            root: scoped_root(&parent.root, &child.root, position),
                            defs: parent.defs.clone(),
                        })
                    }
                    (DestinationSchema::Draft { .. }, DestinationSchema::Compiled(_))
                    | (DestinationSchema::Compiled(_), DestinationSchema::Draft { .. }) => {
                        unreachable!("assignment keeps its PatternForm")
                    }
                }))
            }
        };
        Self(Arc::new(checks))
    }

    /// Read-only consumers can describe a candidate without conferring proof.
    pub fn destination_name(&self) -> String {
        match self.0.as_ref() {
            DestinationChecks::Schema(schema) => match schema.as_ref() {
                DestinationSchema::Draft { root, defs: _ } => root.type_name(),
                DestinationSchema::Compiled(schema) => schema.root.type_name(),
            },
            DestinationChecks::All(checks) => checks
                .iter()
                .map(Self::destination_name)
                .collect::<Vec<_>>()
                .join(" and "),
            DestinationChecks::Any(checks) => checks
                .iter()
                .map(Self::destination_name)
                .collect::<Vec<_>>()
                .join(" | "),
        }
    }

    pub(crate) fn check_or_defer<P: PlaceholderKind, B: BindingAddress>(
        &self,
        value: &Value<P, B>,
        ancestry: FieldAncestry,
    ) -> Result<(), TypeError> {
        match self.0.as_ref() {
            DestinationChecks::All(checks) => checks
                .iter()
                .try_for_each(|check| check.check_or_defer(value, ancestry)),
            DestinationChecks::Any(checks) => {
                let mut errors = Vec::new();
                for check in checks {
                    match check.check_or_defer(value, ancestry) {
                        Ok(()) => return Ok(()),
                        Err(error) => errors.push(error),
                    }
                }
                let mut errors = errors.into_iter();
                Err(TypeError::from_union_arm_errors(
                    errors.next().expect("assignment alternatives are nonempty"),
                    errors,
                ))
            }
            DestinationChecks::Schema(schema) => match schema.as_ref() {
                DestinationSchema::Compiled(schema) => {
                    super::validate_attr_with_defs(&schema.root, value, &schema.defs, ancestry)
                }
                DestinationSchema::Draft { root, defs } => {
                    // Draft queries enter value validation through ordinary admission.
                    let mut schema = ResourceSchema::new("assignment")
                        .attribute(AttributeSchema::optional("value", root.clone()).build());
                    for (name, definition) in defs {
                        schema = schema.with_def(name.clone(), definition.clone());
                    }
                    let schema =
                        schema
                            .validate()
                            .map_err(|error| TypeError::ValidationFailed {
                                message: error.to_string().into(),
                            })?;
                    schema
                        .effective_attribute("value")
                        .expect("the admitted destination exists")
                        .value_type()
                        .validate(value)
                }
            },
        }
    }

    /// Consume only after registration with this destination's existing value check.
    pub(crate) fn defer_to_value_check<T>(self, check: impl FnOnce() -> T) -> T {
        check()
    }
}

fn scoped_root<R: PatternForm>(
    parent: &PatternAttributeType<R>,
    child: &PatternAttributeType<R>,
    position: DestinationPosition<'_>,
) -> PatternAttributeType<R> {
    let mut root = parent.clone();
    match (position, &mut root.kind) {
        (DestinationPosition::ListElement, AttrTypeKind::List { element_type, .. }) => {
            **element_type = child.clone()
        }
        (DestinationPosition::MapKey, AttrTypeKind::Map { key, .. }) => **key = child.clone(),
        (DestinationPosition::MapValue, AttrTypeKind::Map { value, .. }) => **value = child.clone(),
        (DestinationPosition::Field(name), AttrTypeKind::Struct { fields, .. }) => {
            fields
                .iter_mut()
                .find(|field| field.name == name)
                .expect("the assignment descended into this field")
                .field_type = child.clone()
        }
        _ => unreachable!("assignment scope follows the destination shape"),
    }
    root
}

/// A loaded assignment may be proven, rejected, or still owe a value check.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub enum LoadedTypeAssignment {
    Assignable,
    Mismatch,
    NeedsLocal(DestinationObligation),
}

/// Successful reference/type admission preserves any outstanding local check.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub enum AdmittedTypeAssignment {
    Assignable,
    NeedsLocal(DestinationObligation),
}

impl LoadedTypeAssignment {
    /// A boolean is available only when no local destination check remains.
    /// Pending constraints are returned to the caller, never mapped to false.
    pub fn require_proven(self) -> Result<bool, DestinationObligation> {
        match self {
            Self::Assignable => Ok(true),
            Self::Mismatch => Ok(false),
            Self::NeedsLocal(obligation) => Err(obligation),
        }
    }
}
impl AdmittedTypeAssignment {
    pub(crate) fn check_or_defer<P: PlaceholderKind, B: BindingAddress>(
        self,
        value: &Value<P, B>,
        ancestry: FieldAncestry,
    ) -> Result<(), TypeError> {
        match self {
            Self::Assignable => Ok(()),
            Self::NeedsLocal(obligation) => obligation.check_or_defer(value, ancestry),
        }
    }
}
