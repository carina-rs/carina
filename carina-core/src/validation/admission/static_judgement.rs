//! Type evidence shared by strict boundaries and destinations that retain
//! local checks. No result here grants placement authority.

use crate::binding_index::{BindingIndex, ResolvedRefType};
use crate::parser::{ProviderContext, TypeExpr};
use crate::resource::BindingAddress;
use crate::schema::{
    MismatchTypeNames, TypeInSchema,
    assignability::{AssignmentRelation, ReferenceAssignment},
};
use crate::static_type::StaticTypeResolutionError;

pub(crate) enum StaticAssignmentSource<'a> {
    Reference(&'a ResolvedRefType),
    Expression(&'a TypeExpr),
    /// Authored interpolation or a string-producing builtin, without identity evidence.
    ComputedString,
    Unknown,
    SchemaUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaticAssignmentVerdict {
    Proven,
    NeedsLocal,
    NeedsSchema,
}

#[derive(Debug)]
pub(crate) enum StaticAssignmentError {
    Mismatch(MismatchTypeNames),
    Resolution(StaticTypeResolutionError),
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
) -> Result<StaticAssignmentVerdict, StaticAssignmentError> {
    use StaticAssignmentError::{Mismatch, Resolution};
    use StaticAssignmentVerdict::{NeedsLocal, NeedsSchema};
    let config = bindings.provider_context();
    if matches!(source, StaticAssignmentSource::SchemaUnavailable)
        || type_needs_schema(expected, config)
    {
        return Ok(NeedsSchema);
    }
    let expected = super::super::resolve_type_expr(expected, config).map_err(Resolution)?;
    let constructed_string_sink = expected.is_string_shaped() && expected != TypeExpr::String;
    let Some(expected) = super::super::lift_type_expr(&expected) else {
        return Ok(NeedsLocal);
    };
    let sink = expected.assignment_sink(&config.custom_types);
    let (relation, types) = match source {
        StaticAssignmentSource::Reference(source) => {
            let mut relations = Vec::new();
            for ty in source.constraints() {
                let relation = ty.reference_relation_to(sink);
                if relation.is_mismatch() {
                    return Err(Mismatch(MismatchTypeNames::for_types(*sink, ty)));
                }
                relations.push(relation);
            }
            (
                ReferenceAssignment::all(relations),
                source.mismatch_type_names(*sink),
            )
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
            let source = super::super::resolve_type_expr(source, config).map_err(Resolution)?;
            let Some(source) = super::super::lift_type_expr(&source) else {
                return Ok(NeedsLocal);
            };
            let source = TypeInSchema::schemaless(&source);
            return classify(
                source.reference_relation_to(sink),
                MismatchTypeNames::for_types(*sink, source),
            );
        }
        StaticAssignmentSource::Unknown => return Ok(NeedsLocal),
        StaticAssignmentSource::SchemaUnavailable => return Ok(NeedsSchema),
    };
    classify(relation, types)
}

fn classify(
    relation: ReferenceAssignment<'_>,
    types: MismatchTypeNames,
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
        AssignmentRelation::Mismatch => Err(StaticAssignmentError::Mismatch(types)),
    }
}
