use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, Shape, ShapeWalkBudget, StructField,
};

const SHAPE_WALK_BUDGET: usize = 64;

pub(crate) enum ResolvedAttribute<'a> {
    TopLevel(&'a AttributeSchema),
    Field(&'a StructField),
}

impl<'a> ResolvedAttribute<'a> {
    pub(crate) fn name(&self) -> &'a str {
        match self {
            Self::TopLevel(attribute) => attribute.name(),
            Self::Field(field) => field.name(),
        }
    }

    pub(crate) fn description(&self) -> Option<&'a str> {
        match self {
            Self::TopLevel(attribute) => attribute.description(),
            Self::Field(field) => field.description(),
        }
    }

    pub(crate) fn attr_type(&self) -> &'a AttributeType {
        match self {
            Self::TopLevel(attribute) => attribute.attr_type(),
            Self::Field(field) => field.field_type(),
        }
    }

    pub(crate) fn is_required(&self) -> bool {
        match self {
            Self::TopLevel(attribute) => attribute.is_required(),
            Self::Field(field) => field.is_required(),
        }
    }
}

pub(crate) fn struct_fields_of<'a>(
    schema: &'a ResourceSchema,
    attr_type: &'a AttributeType,
) -> Option<&'a [StructField]> {
    let mut budget = ShapeWalkBudget::new(SHAPE_WALK_BUDGET);
    struct_field_candidates_with_budget(schema, attr_type, &mut budget)
        .into_iter()
        .next()
}

fn struct_field_candidates_with_budget<'a>(
    schema: &'a ResourceSchema,
    attr_type: &'a AttributeType,
    budget: &mut ShapeWalkBudget,
) -> Vec<&'a [StructField]> {
    match schema.shape_of(attr_type) {
        Shape::Struct { .. } => schema
            .struct_fields_with_budget(attr_type, budget)
            .into_iter()
            .collect(),
        Shape::List { element_type, .. } => match schema.shape_of(element_type) {
            Shape::Struct { .. } => schema
                .struct_fields_with_budget(element_type, budget)
                .into_iter()
                .collect(),
            Shape::String { .. }
            | Shape::Int { .. }
            | Shape::Float { .. }
            | Shape::Bool
            | Shape::Duration
            | Shape::Enum { .. }
            | Shape::List { .. }
            | Shape::Map { .. }
            | Shape::Union => Vec::new(),
        },
        Shape::Union => {
            let Some(members) = schema.union_members_with_budget(attr_type, budget) else {
                return Vec::new();
            };
            let mut candidates = Vec::new();
            for member in members.iter().flatten() {
                candidates.extend(struct_field_candidates_with_budget(
                    schema,
                    member.as_attr(),
                    budget,
                ));
            }
            candidates
        }
        Shape::String { .. }
        | Shape::Int { .. }
        | Shape::Float { .. }
        | Shape::Bool
        | Shape::Duration
        | Shape::Enum { .. }
        | Shape::Map { .. } => Vec::new(),
    }
}

pub(crate) fn find_attr_schema<'a>(
    schema: &'a ResourceSchema,
    attr_name: &str,
) -> Option<&'a AttributeSchema> {
    schema.attributes().get(attr_name).or_else(|| {
        schema
            .attributes()
            .values()
            .find(|attribute| attribute.block_name() == Some(attr_name))
    })
}

pub(crate) fn resolve_attribute_path<'a>(
    schema: &'a ResourceSchema,
    path: &[String],
) -> Option<ResolvedAttribute<'a>> {
    let mut segments = path.iter();
    let top_level = find_attr_schema(schema, segments.next()?)?;
    let mut resolved = ResolvedAttribute::TopLevel(top_level);

    for segment in segments {
        let field = find_struct_field(schema, resolved.attr_type(), segment)?;
        resolved = ResolvedAttribute::Field(field);
    }

    Some(resolved)
}

fn find_struct_field<'a>(
    schema: &'a ResourceSchema,
    attr_type: &'a AttributeType,
    field_name: &str,
) -> Option<&'a StructField> {
    let mut budget = ShapeWalkBudget::new(SHAPE_WALK_BUDGET);
    let matching_field = |fields: &'a [StructField]| {
        fields
            .iter()
            .find(|field| field.name() == field_name || field.block_name() == Some(field_name))
    };

    struct_field_candidates_with_budget(schema, attr_type, &mut budget)
        .into_iter()
        .find_map(matching_field)
}
