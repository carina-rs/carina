//! Type evidence shared by strict boundaries and destinations that retain
//! local checks. No result here grants placement authority.

use crate::binding_index::{BindingIndex, ResolvedRefType};
use crate::parser::{ProviderContext, TypeExpr};
use crate::resource::BindingAddress;
use crate::schema::{
    TypeInSchema,
    assignability::{AssignmentRelation, ReferenceAssignment},
};

pub(crate) enum StaticAssignmentSource<'a> {
    Reference(&'a ResolvedRefType),
    Element(&'a ResolvedRefType),
    Expression(&'a TypeExpr),
    Judged(&'a crate::parser::retained::JudgedExpr),
    /// Authored interpolation or a string-producing builtin, without identity evidence.
    ComputedString,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaticAssignmentVerdict {
    Proven,
    NeedsLocal,
    NeedsSchema,
}

#[derive(Debug)]
pub(crate) enum StaticAssignmentError {
    Mismatch,
    Resolution,
}

pub(crate) fn type_needs_schema(ty: &TypeExpr, config: &ProviderContext) -> bool {
    match ty {
        TypeExpr::DottedUnresolved(path) | TypeExpr::Ref(path) => {
            config.custom_types.may_defer(Some(&path.provider))
        }
        TypeExpr::UnloadedSchemaType(ty) => config
            .custom_types
            .may_defer(ty.identity().provider.as_deref()),
        TypeExpr::SchemaType { owned } => owned
            .provider_names()
            .any(|owner| config.custom_types.may_defer(Some(owner))),
        TypeExpr::List(inner) | TypeExpr::Map(inner) => type_needs_schema(inner, config),
        TypeExpr::Union(members) => members.iter().any(|ty| type_needs_schema(ty, config)),
        TypeExpr::Struct { fields } => fields.iter().any(|(_, ty)| type_needs_schema(ty, config)),
        _ => false,
    }
}

pub(crate) fn judge_static_assignment<B: BindingAddress>(
    source: StaticAssignmentSource<'_>,
    expected: &TypeExpr,
    bindings: &BindingIndex<'_, B>,
    known: &impl Fn(&crate::parser::retained::JudgedExpr) -> bool,
) -> Result<StaticAssignmentVerdict, StaticAssignmentError> {
    use StaticAssignmentError::{Mismatch, Resolution};
    use StaticAssignmentVerdict::{NeedsLocal, NeedsSchema};
    let config = bindings.provider_context();
    if type_needs_schema(expected, config) {
        return Ok(NeedsSchema);
    }
    let expected = super::super::resolve_type_expr(expected, config).map_err(|_| Resolution)?;
    if let StaticAssignmentSource::Judged(expression) = source {
        if known(expression) {
            return Ok(StaticAssignmentVerdict::Proven);
        }
        let children: Vec<_> = match &expected {
            TypeExpr::Struct { fields } if expression.fields().next().is_some() => {
                let mut children = Vec::new();
                for (name, ty) in fields {
                    let Ok(field) = expression.field(name) else {
                        return Ok(NeedsLocal);
                    };
                    children.push((field, ty));
                }
                children
            }
            TypeExpr::List(ty) if expression.elements().next().is_some() => expression
                .elements()
                .map(|field| (field, ty.as_ref()))
                .collect(),
            TypeExpr::Map(ty) if expression.fields().next().is_some() => expression
                .fields()
                .map(|(_, field)| (field, ty.as_ref()))
                .collect(),
            _ => {
                return judge_static_assignment(
                    match expression.static_type() {
                        Ok(TypeExpr::String)
                            if expression
                                .for_sink(&TypeExpr::String, config)
                                .is_single_string_construction() =>
                        {
                            StaticAssignmentSource::ComputedString
                        }
                        Ok(ty) => StaticAssignmentSource::Expression(ty),
                        Err(_) => StaticAssignmentSource::Unknown,
                    },
                    &expected,
                    bindings,
                    known,
                );
            }
        };
        let mut verdict = StaticAssignmentVerdict::Proven;
        for (field, ty) in children {
            match judge_static_assignment(
                StaticAssignmentSource::Judged(field),
                ty,
                bindings,
                known,
            )? {
                NeedsSchema => return Ok(NeedsSchema),
                NeedsLocal => verdict = NeedsLocal,
                StaticAssignmentVerdict::Proven => {}
            }
        }
        return Ok(verdict);
    }
    let constructed_string_sink = expected.is_string_shaped() && expected != TypeExpr::String;
    let Some(expected) = super::super::lift_type_expr(&expected) else {
        return Ok(NeedsLocal);
    };
    let sink = expected.assignment_sink(&config.custom_types);
    let relation = match source {
        kind @ (StaticAssignmentSource::Reference(source)
        | StaticAssignmentSource::Element(source)) => {
            let mut relations = Vec::new();
            let elements = matches!(kind, StaticAssignmentSource::Element(_));
            for ty in source.constraints() {
                let ty = if elements {
                    let Some(ty) = ty.iterable_element() else {
                        return Ok(NeedsLocal);
                    };
                    ty
                } else {
                    ty
                };
                let relation = ty.reference_relation_to(sink);
                if relation.is_mismatch() {
                    return Err(Mismatch);
                }
                relations.push(relation);
            }
            ReferenceAssignment::all(relations)
        }
        source @ (StaticAssignmentSource::Expression(_)
        | StaticAssignmentSource::ComputedString) => {
            // Constructed strings have the same deferred value admission used
            // at attribute and module boundaries, including named string sinks.
            if matches!(source, StaticAssignmentSource::ComputedString) && constructed_string_sink {
                return Ok(NeedsLocal);
            }
            let source = match source {
                StaticAssignmentSource::Expression(source) => source,
                _ => &TypeExpr::String,
            };
            if type_needs_schema(source, config) {
                return Ok(NeedsSchema);
            }
            let source = super::super::resolve_type_expr(source, config).map_err(|_| Resolution)?;
            let Some(source) = super::super::lift_type_expr(&source) else {
                return Ok(NeedsLocal);
            };
            let source = TypeInSchema::schemaless(&source);
            return classify(source.reference_relation_to(sink));
        }
        StaticAssignmentSource::Judged(_) => unreachable!("authored children were consumed"),
        StaticAssignmentSource::Unknown => return Ok(NeedsLocal),
    };
    classify(relation)
}

fn classify(
    relation: ReferenceAssignment<'_>,
) -> Result<StaticAssignmentVerdict, StaticAssignmentError> {
    use StaticAssignmentVerdict::{NeedsLocal, NeedsSchema, Proven};
    let proven = relation.type_proven();
    match relation.for_destination() {
        AssignmentRelation::Assignable | AssignmentRelation::NeedsLocal(None) => {
            Ok(if proven { Proven } else { NeedsLocal })
        }
        AssignmentRelation::NeedsLocal(Some(_)) | AssignmentRelation::Unchecked(_) => {
            Ok(NeedsSchema)
        }
        AssignmentRelation::Mismatch => Err(StaticAssignmentError::Mismatch),
    }
}
