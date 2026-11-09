//! One bounded, definition-aware walk for refinement metadata consumers.

use super::{AttrTypeKind, EffectiveType, ShapeWalkBudget};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefinementPathSegment {
    Field(String),
    ListElement,
    MapKey,
    MapValue,
    UnionMember(usize),
    EnumBase,
}

#[derive(Debug, Clone)]
pub struct RefinedScalarPosition<'a> {
    path: Vec<RefinementPathSegment>,
    value_type: EffectiveType<'a>,
}

impl<'a> RefinedScalarPosition<'a> {
    pub fn path(&self) -> &[RefinementPathSegment] {
        &self.path
    }

    pub fn value_type(&self) -> &EffectiveType<'a> {
        &self.value_type
    }
}

#[derive(Debug, Clone)]
pub struct RefinementPositions<'a> {
    pub positions: Vec<RefinedScalarPosition<'a>>,
    /// True only when an actual node could not be visited within the budget.
    pub truncated: bool,
}

impl<'a> EffectiveType<'a> {
    /// Visit at most `max_nodes` raw schema nodes, including each Ref and each
    /// nested Union. Empty branches and exactly-fitting walks are complete.
    pub fn refinement_positions(&self, max_nodes: usize) -> RefinementPositions<'a> {
        let mut positions = Vec::new();
        let complete = visit(
            self.clone(),
            &mut Vec::new(),
            &mut ShapeWalkBudget::new(max_nodes),
            &mut positions,
        );
        RefinementPositions {
            positions,
            truncated: !complete,
        }
    }
}

fn visit<'a>(
    ty: EffectiveType<'a>,
    path: &mut Vec<RefinementPathSegment>,
    budget: &mut ShapeWalkBudget,
    positions: &mut Vec<RefinedScalarPosition<'a>>,
) -> bool {
    if !budget.take() {
        return false;
    }
    let mut descend = |attr_type, segment, field_ancestry, field_name: Option<&str>| {
        let mut child = EffectiveType {
            attr_type,
            defs: ty.defs,
            path: ty.path.clone(),
            field_ancestry,
        };
        if let Some(name) = field_name {
            child.path.push(name.to_owned());
        }
        path.push(segment);
        let complete = visit(child, path, budget, positions);
        path.pop();
        complete
    };
    match &ty.attr_type.kind {
        AttrTypeKind::String {
            identity_layer: _,
            member_layer: _,
        }
        | AttrTypeKind::Int {
            identity_layer: _,
            member_layer: _,
        } => {
            positions.push(RefinedScalarPosition {
                path: path.clone(),
                value_type: ty,
            });
            true
        }
        AttrTypeKind::Ref(name) => match ty.defs.get(name) {
            Some(attr_type) => visit(EffectiveType { attr_type, ..ty }, path, budget, positions),
            None => true, // Validated schemas cannot have a missing definition.
        },
        AttrTypeKind::Struct {
            fields,
            name: _,
            field_groups: _,
        } => fields.iter().all(|field| {
            descend(
                &field.field_type,
                RefinementPathSegment::Field(field.name.clone()),
                ty.field_ancestry.descend(field),
                Some(&field.name),
            )
        }),
        AttrTypeKind::Union(members) => {
            members
                .as_raw_slice()
                .iter()
                .enumerate()
                .all(|(index, member)| {
                    descend(
                        member,
                        RefinementPathSegment::UnionMember(index + 1),
                        ty.field_ancestry,
                        None,
                    )
                })
        }
        AttrTypeKind::List {
            element_type,
            ordered: _,
            constraints: _,
        } => descend(
            element_type,
            RefinementPathSegment::ListElement,
            ty.field_ancestry,
            None,
        ),
        AttrTypeKind::Map {
            key,
            value,
            constraints: _,
        } => {
            descend(key, RefinementPathSegment::MapKey, ty.field_ancestry, None)
                && descend(
                    value,
                    RefinementPathSegment::MapValue,
                    ty.field_ancestry,
                    None,
                )
        }
        AttrTypeKind::Enum {
            base,
            identity: _,
            values: _,
            dsl_aliases: _,
            validate: _,
            to_dsl: _,
        } => descend(
            base,
            RefinementPathSegment::EnumBase,
            ty.field_ancestry,
            None,
        ),
        AttrTypeKind::Float {
            binding: _,
            range: _,
        }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Reference {
            identity: _,
            pattern: _,
            length: _,
        }
        | AttrTypeKind::UnloadedSchemaType(_)
        | AttrTypeKind::ResourceHandle {
            provider: _,
            resource_type: _,
        }
        | AttrTypeKind::StringLiteralSet { values: _ } => true,
    }
}

#[cfg(test)]
mod tests {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    #[test]
    fn refinement_walk_marks_only_unvisited_nodes() {
        for (draft, steps, expected, truncated) in [
            (AttributeType::struct_("Empty", vec![]), 1, 0, false),
            (AttributeType::string(), 1, 1, false),
            (AttributeType::string(), 0, 0, true),
            (
                AttributeType::struct_(
                    "One",
                    vec![StructField::optional("value", AttributeType::string()).build()],
                ),
                2,
                1,
                false,
            ),
            (
                AttributeType::struct_(
                    "One",
                    vec![StructField::optional("value", AttributeType::string()).build()],
                ),
                1,
                0,
                true,
            ),
        ] {
            let schema = ResourceSchema::new("Test")
                .attribute(AttributeSchema::optional("value", draft).build())
                .validate()
                .unwrap();
            let result = schema
                .effective_attribute("value")
                .unwrap()
                .value_type()
                .refinement_positions(steps);
            assert_eq!(result.positions.len(), expected);
            assert_eq!(result.truncated, truncated);
        }
    }

    #[test]
    fn refinement_walk_charges_nested_unions_and_refs() {
        let schema = ResourceSchema::new("Test")
            .with_def("leaf", AttributeType::string())
            .with_def(
                "nested",
                AttributeType::union(vec![AttributeType::ref_("leaf")]),
            )
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::union(vec![AttributeType::ref_("nested")]),
                )
                .build(),
            )
            .validate()
            .unwrap();
        let ty = schema.effective_attribute("value").unwrap().value_type();
        let short = ty.refinement_positions(4);
        assert!(short.positions.is_empty());
        assert!(short.truncated);
        let exact = ty.refinement_positions(5);
        assert_eq!(exact.positions.len(), 1);
        assert!(!exact.truncated);
    }
}
