//! Rhs-driven type inference for `exports { }` declarations whose
//! `type_expr` annotation is omitted.
//!
//! [`apply_inference`] is the bridge from `ParsedFile` to `InferredFile`.
//! Each inferred export retains its declared/inferred origin and any typed
//! unchecked reason directly on the parameter. The loader runs this once per directory load so
//! downstream consumers (CLI validate, LSP diagnostics, LSP
//! completion) see a definitive type per export instead of having to
//! re-derive it. See `notes/specs/2026-05-03-typeexpr-stage2-design.md`.
//!
//! Export types come exclusively from the authored expression report produced
//! by the loader/expander.

use std::collections::HashMap;

use crate::parser::{ExportParamLike, TypeExpr};
use crate::provider::ProviderLoadedSchemaRegistry;
use crate::resource::{BindingAddress, PlaceholderKind, RelativeBindingName};
use crate::schema::{
    AttrTypeKind, CompiledAttributeType as AttributeType, CompiledResourceSchema as ResourceSchema,
    SchemaRegistry,
};
pub use crate::static_type::InferenceError;
use crate::static_type::{
    ExportStaticType, InferenceIssue, InferredUncheckedReason, NotInferableReason, StaticType,
};

/// Binding classification for authored expression judgement in
/// [`crate::parser::retained`]. Resource bindings identify the provider and
/// resource schema used for attribute lookup; external and module-call
/// bindings keep their names in scope before their values are available.
///
/// `Virtual` records the attributes exposed by an expanded module call's
/// composition resource. The retained judge checks attribute names against
/// this set and derives their types from authored module-output evidence
/// (#2493).
#[derive(Debug, Clone)]
pub enum InferenceBinding {
    Resource {
        provider: String,
        resource_type: String,
    },
    UpstreamState,
    Virtual {
        attributes: indexmap::IndexSet<String>,
    },
    /// Module-call binding registered before expansion. The retained judge
    /// recognizes the name but needs authored module-output evidence to
    /// derive attribute types. After expansion, [`bindings_from_parts`]
    /// classifies the composition as `Virtual`. #2493.
    ModuleCall,
    /// A value parameter declared by an `arguments {}` block. Parser-side
    /// expressions carry a `Placeholder(ModuleArgument)` until the
    /// caller value is substituted; retaining the declaration here lets
    /// postfix access be checked before expansion.
    Argument {
        type_expr: TypeExpr,
    },
    /// A user-defined function binding. Authored syntax retains its signature.
    UserFunction,
}

pub type InferenceBindings = HashMap<String, InferenceBinding>;

/// Build an [`InferenceBindings`] map from a `ParsedFile`. Convenience
/// over [`bindings_from_parts`] for the common case of having a parsed
/// file in hand.
pub fn bindings_from_parsed<E, P, T, D, B>(
    parsed: &crate::parser::File<E, P, T, D, B>,
) -> InferenceBindings
where
    P: PlaceholderKind + PartialEq,
    T: PlaceholderKind,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
{
    // carina#3181: pass the typed top-level slices directly so
    // `bindings_from_parts` classifies managed resources, data sources,
    // and (post-expansion) composition rows from their own kinds.
    let mut out = bindings_from_parts(
        &parsed.resources,
        &parsed.data_sources,
        &parsed.compositions,
        &parsed.upstream_states,
    );
    // Before module expansion, calls have no composition entries. Register
    // their names as `ModuleCall` so the retained judge recognizes them
    // before output evidence is available. After expansion,
    // `bindings_from_parts` classifies the compositions as `Virtual`, and
    // authored module-output judgements supply their attribute types. #2493.
    for call in &parsed.module_calls {
        if let Some(name) = &call.binding_name
            && !out.contains_key(name.as_str())
        {
            out.insert(name.to_string(), InferenceBinding::ModuleCall);
        }
    }
    for argument in &parsed.arguments {
        out.entry(argument.name.clone())
            .or_insert_with(|| InferenceBinding::Argument {
                type_expr: argument.static_type.clone(),
            });
    }
    for function in parsed.user_functions.values() {
        out.entry(function.name.clone())
            .or_insert(InferenceBinding::UserFunction);
    }
    out
}

/// Build an [`InferenceBindings`] map from already-extracted resource
/// and upstream-state slices. Used by callers that already work in
/// terms of the slice arguments (e.g. `validate_export_param_ref_types_with_bindings`)
/// so the binding-collection logic is in one place.
///
/// Both resource bindings (`let <name> = <provider>.<resource> { ... }`)
/// and `upstream_state` bindings (`let <name> = upstream_state { ... }`)
/// are included, so the retained judge can distinguish a known external
/// binding from an unknown name.
///
/// Insert order is upstream-states first, resources second, so a
/// resource binding wins on collision with an `upstream_state` of the
/// same name. The directory loader's duplicate-declaration channel is
/// the validation gate for these cross-file collisions, but during a
/// mid-edit the LSP deliberately keeps the merged parse alive — preferring
/// the resource means downstream inference still gets the precise type
/// instead of degrading to `NonInferableBinding`. Two resource bindings
/// with the same name likewise remain deterministic here while the loader
/// diagnostic reports the collision to the user.
pub fn bindings_from_parts<S, P: PlaceholderKind + PartialEq, B: BindingAddress>(
    resources: &[crate::resource::Resource<S, P, B>],
    data_sources: &[crate::resource::DataSource<S, P, B>],
    compositions: &[crate::resource::LiveComposition<P, B>],
    upstream_states: &[crate::parser::UpstreamState<B>],
) -> InferenceBindings {
    let mut out = InferenceBindings::new();
    for us in upstream_states {
        out.insert(us.binding.to_string(), InferenceBinding::UpstreamState);
    }
    // carina#3181: managed resources and data sources have a provider
    // identity and use `InferenceBinding::Resource` for schema attribute
    // lookup. Expanded module-call compositions have no provider identity;
    // their `Virtual` binding records exposed attributes. `parser::retained`
    // resolves their types from authored module-output judgements. #2493.
    for resource in resources {
        let Some(name) = &resource.binding else {
            continue;
        };
        out.insert(
            name.to_string(),
            InferenceBinding::Resource {
                provider: resource.id.provider.clone(),
                resource_type: resource.id.resource_type.clone(),
            },
        );
    }
    for data_source in data_sources {
        let Some(name) = &data_source.binding else {
            continue;
        };
        out.insert(
            name.to_string(),
            InferenceBinding::Resource {
                provider: data_source.id.provider.clone(),
                resource_type: data_source.id.resource_type.clone(),
            },
        );
    }
    for composition in compositions {
        let Some(name) = &composition.binding else {
            continue;
        };
        out.insert(
            name.to_string(),
            InferenceBinding::Virtual {
                attributes: composition
                    .signature
                    .attributes
                    .iter()
                    .filter(|(_, attr)| attr.handle().is_none())
                    .map(|(name, _)| name.clone())
                    .collect(),
            },
        );
    }
    out
}

#[derive(Clone, Copy)]
struct InferenceSchemaContext<'a> {
    schemas: &'a SchemaRegistry,
    provider_context: &'a crate::parser::ProviderContext,
}

fn require_proven_integer_operand(
    inferred: Result<TypeExpr, InferenceIssue>,
    op: crate::integer_arithmetic::IntegerOp,
    side: crate::integer_arithmetic::IntegerOperandSide,
    operator_span: crate::integer_arithmetic::IntegerOperatorSpan,
) -> Result<TypeExpr, InferenceIssue> {
    match inferred {
        Ok(inferred) => Ok(inferred),
        Err(
            error @ InferenceIssue::Failed(InferenceError::InvalidIntegerArithmeticOperand {
                ..
            }),
        ) => Err(error),
        Err(
            error @ InferenceIssue::NotInferable(NotInferableReason::CustomTypeSchemaUnavailable {
                ..
            }),
        ) => Err(error),
        Err(error) => Err(InferenceError::InvalidIntegerArithmeticOperand {
            op,
            side,
            actual: format!("unproven type ({error})"),
            operator_span,
        }
        .into()),
    }
}

pub(crate) fn judge_integer_operands(
    op: crate::integer_arithmetic::IntegerOp,
    location: &crate::parser::DeclarationLocation,
    operands: &[crate::parser::retained::JudgedExpr; 2],
    schemas: &SchemaRegistry,
    provider_context: &crate::parser::ProviderContext,
) -> Result<TypeExpr, InferenceIssue> {
    for operand in operands {
        if let Err(InferenceIssue::NotInferable(reason)) = operand.static_type() {
            return Err(InferenceIssue::NotInferable(reason.clone()));
        }
    }
    let span = crate::integer_arithmetic::IntegerOperatorSpan::authored(location.clone());
    let lhs = require_proven_integer_operand(
        operands[0].static_type().cloned(),
        op,
        crate::integer_arithmetic::IntegerOperandSide::Left,
        span.clone(),
    )?;
    let rhs = require_proven_integer_operand(
        operands[1].static_type().cloned(),
        op,
        crate::integer_arithmetic::IntegerOperandSide::Right,
        span.clone(),
    )?;
    require_integer_arithmetic_operands(
        op,
        span,
        &lhs,
        &rhs,
        InferenceSchemaContext {
            schemas,
            provider_context,
        },
    )
}

fn require_integer_arithmetic_operands(
    op: crate::integer_arithmetic::IntegerOp,
    operator_span: crate::integer_arithmetic::IntegerOperatorSpan,
    lhs: &TypeExpr,
    rhs: &TypeExpr,
    context: InferenceSchemaContext<'_>,
) -> Result<TypeExpr, InferenceIssue> {
    let schemas = context.schemas;
    for (side, operand) in [
        (crate::integer_arithmetic::IntegerOperandSide::Left, lhs),
        (crate::integer_arithmetic::IntegerOperandSide::Right, rhs),
    ] {
        let operand = crate::parser::resolve_type_expr_from_schemas(
            operand,
            schemas,
            context.provider_context,
        )
        .map_err(|error| {
            InferenceIssue::from(InferenceError::UnknownType {
                reason: error.to_string(),
            })
        })?;
        if inferred_scalar_kind(&operand).map_err(InferenceIssue::NotInferable)?
            != Some(crate::schema::RefinedScalarKind::Int)
        {
            return Err(InferenceError::InvalidIntegerArithmeticOperand {
                op,
                side,
                actual: crate::schema::MismatchTypeNames::for_type_exprs(&TypeExpr::Int, &operand)
                    .actual()
                    .to_string(),
                operator_span,
            }
            .into());
        }
    }
    Ok(TypeExpr::Int)
}

fn inferred_scalar_kind(
    inferred: &TypeExpr,
) -> Result<Option<crate::schema::RefinedScalarKind>, NotInferableReason> {
    use crate::schema::RefinedScalarKind;

    Ok(match inferred {
        TypeExpr::UnloadedSchemaType(unloaded) => {
            return Err(NotInferableReason::CustomTypeSchemaUnavailable {
                identity: unloaded.identity().clone(),
            });
        }
        TypeExpr::String | TypeExpr::StringLiteral(_) => Some(RefinedScalarKind::String),
        TypeExpr::Int => Some(RefinedScalarKind::Int),
        TypeExpr::Float => Some(RefinedScalarKind::Float),
        TypeExpr::SchemaType { owned } => Some(owned.scalar_kind()),
        TypeExpr::Simple(name) => crate::provider::identified_custom_type_scalar_kind(
            name.declaration(),
        )
        .map_err(|unloaded| NotInferableReason::CustomTypeSchemaUnavailable {
            identity: unloaded.identity().clone(),
        })?,
        TypeExpr::Bool
        | TypeExpr::Duration
        | TypeExpr::List(_)
        | TypeExpr::Map(_)
        | TypeExpr::Ref(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::Struct { .. }
        | TypeExpr::Union(_) => None,
    })
}

pub(crate) fn infer_declared_reference_type(
    binding: &str,
    attribute: &str,
    segments: &[crate::resource::PathSegment],
    bindings: &InferenceBindings,
    schemas: &SchemaRegistry,
) -> Result<TypeExpr, InferenceIssue> {
    match bindings.get(binding) {
        Some(InferenceBinding::Resource {
            provider,
            resource_type,
        }) => infer_schema_reference(
            binding,
            attribute,
            segments,
            provider,
            resource_type,
            schemas,
        ),
        Some(_) => Err(InferenceIssue::NotInferable(
            NotInferableReason::NonInferableBinding {
                binding: binding.to_string(),
            },
        )),
        None => Err(InferenceError::UnknownBinding {
            binding: binding.to_string(),
        }
        .into()),
    }
}

fn infer_schema_reference(
    binding: &str,
    attribute: &str,
    segments: &[crate::resource::PathSegment],
    provider: &str,
    resource_type: &str,
    schemas: &SchemaRegistry,
) -> Result<TypeExpr, InferenceIssue> {
    let Some(schema) = lookup_schema(schemas, provider, resource_type) else {
        return Err(InferenceIssue::NotInferable(
            NotInferableReason::SchemaUnavailable {
                binding: binding.to_string(),
            },
        ));
    };
    let Some(attr_schema) = schema.attributes.get(attribute) else {
        return Err(InferenceError::UnknownAttribute {
            binding: binding.to_string(),
            attribute: attribute.to_string(),
        }
        .into());
    };
    // Walk the path's ordered segments — a free mix of `.field` and
    // `[idx]` continuations (carina#3025). A `.field` descends into the
    // current struct; an `[idx]` peels one collection layer (`list(T)
    // → T`, `map(_, V) → V`). Any mismatch surfaces a typed error
    // rather than silently dropping the path.
    use crate::resource::{PathSegment, Subscript};
    let mut current = &attr_schema.attr_type;
    for seg in segments {
        // Resolve a `Ref` chain at each step so Subscript arms below
        // see the underlying List / Map / Struct shape instead of the
        // opaque `Ref` (carina#3340). `descend_struct_field` already
        // resolves internally for the Field arm; pre-resolving here
        // also keeps the Subscript arms shape-matched.
        let resolved = current.resolve_refs_with_defs(&schema.defs).as_attr();
        current = match (seg, &resolved.kind) {
            (PathSegment::Field { name }, AttrTypeKind::Struct { .. }) => {
                match descend_struct_field(resolved, name, &schema.defs) {
                    Some(t) => t,
                    None => {
                        return Err(InferenceError::UnknownAttribute {
                            binding: binding.to_string(),
                            attribute: format!("{}.{}", attribute, name),
                        }
                        .into());
                    }
                }
            }
            (PathSegment::Field { .. }, AttrTypeKind::Map { value, .. }) => value,
            (
                PathSegment::Subscript {
                    index: Subscript::Int { .. },
                },
                AttrTypeKind::List {
                    element_type: inner,
                    ..
                },
            ) => inner,
            (
                PathSegment::Subscript {
                    index: Subscript::Str { .. },
                },
                AttrTypeKind::Map { value, .. },
            ) => value,
            _ => {
                return Err(InferenceError::InvalidAccess {
                    base_type: attribute_type_to_type_expr(resolved, provider).to_string(),
                    step: seg.clone(),
                }
                .into());
            }
        };
    }
    // A `Union` at the *resolved leaf* of the access path carries
    // multiple alternatives — projecting it to a single `TypeExpr`
    // would silently let a value satisfy branches it shouldn't.
    // Demand the user write an annotation so the intended branch is
    // explicit.
    //
    // The check is outermost-only on purpose: real provider schemas
    // bury `Union` inside larger structs (e.g. IAM policy documents:
    // `policy_document` is a `Struct{statement: List<Struct{principal:
    // Union<...>, action: Union<...>, resource: Union<...>, ...}>}`).
    // Flagging every nested union would block legitimate
    // `policy_document = main.policy.policy_document` references that
    // forward the whole struct without naming the inner union branch.
    // The user can only "pick a branch" for a union they're directly
    // accessing, not for one buried inside a transferred value.
    // Resolve any trailing `Ref` so the Union check and the
    // TypeExpr projection see the underlying shape (carina#3340).
    let current = current.resolve_refs_with_defs(&schema.defs).as_attr();
    if matches!(&current.kind, AttrTypeKind::Union(_)) {
        return Err(InferenceError::UnknownType {
            reason: format!(
                "rhs `{}.{}` resolves to a union type; annotate the export to pick a branch",
                binding, attribute
            ),
        }
        .into());
    }
    Ok(attribute_type_to_type_expr(current, provider))
}

fn lookup_schema<'a>(
    schemas: &'a SchemaRegistry,
    provider: &str,
    resource_type: &str,
) -> Option<&'a ResourceSchema> {
    schemas
        .get_kindless(provider, resource_type)
        .map(|schema| schema.schema().as_schema())
}

fn descend_struct_field<'a>(
    attr_type: &'a AttributeType,
    field: &str,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    // Resolve any leading `Ref` chain so a reference path inside a
    // cyclic CFN struct (`WebACL.Statement -> AndStatement -> ...`)
    // can be followed across the cycle (carina#3340).
    match attr_type.shape_with_defs(defs) {
        crate::schema::Shape::Struct { .. } => {
            crate::schema::struct_fields_with_defs(attr_type, defs)
                .expect("Shape::Struct must expose struct fields internally")
                .iter()
                .find(|f| f.name == field)
                .map(|f| &f.field_type)
        }
        _ => None,
    }
}

/// Convert an `AttributeType` (schema side) into a `TypeExpr`
/// (declaration side). Used both by `ResourceRef` inference and by
/// recursive descent into `List`/`Map`/`Struct` schema receivers.
///
/// Notable mappings:
/// - `Custom { identity: Some(id), .. }` → `TypeExpr::SchemaType`, including
///   bare semantic identities. The schema owner must survive inference so a
///   downstream sink never falls back to identity-only routing.
/// - `Custom { identity: None, .. }` → falls through to the base
///   type; an anonymous Custom is structurally a wrapper and carries
///   no identity to project.
/// - closed `Enum` (`values: Some`) → `TypeExpr::String`
/// - dynamic `Enum` (`values: None` with provider transform or host
///   validator) → `TypeExpr::SchemaType`, preserving the typed
///   identity old CustomEnum-origin schemas carried before enum
///   unification.
pub(super) fn attribute_type_to_type_expr(
    attr_type: &AttributeType,
    schema_owner: &str,
) -> TypeExpr {
    if let AttrTypeKind::String {
        identity_layer:
            crate::schema::StringIdentityLayer {
                binding: crate::schema::RefinementBinding::Annotation(witness),
                ..
            },
        ..
    }
    | AttrTypeKind::Int {
        identity_layer:
            crate::schema::IntIdentityLayer {
                binding: crate::schema::RefinementBinding::Annotation(witness),
                ..
            },
        ..
    }
    | AttrTypeKind::Float {
        binding: crate::schema::RefinementBinding::Annotation(witness),
        ..
    } = attr_type.kind()
    {
        return TypeExpr::SchemaType {
            owned: witness.clone(),
        };
    }
    let scalar_identity = match attr_type.kind() {
        AttrTypeKind::UnloadedSchemaType(unloaded) => {
            return TypeExpr::UnloadedSchemaType(unloaded.clone());
        }
        AttrTypeKind::Reference { identity, .. } => Some(identity),
        AttrTypeKind::String {
            identity_layer: crate::schema::StringIdentityLayer { binding, .. },
            ..
        }
        | AttrTypeKind::Int {
            identity_layer: crate::schema::IntIdentityLayer { binding, .. },
            ..
        }
        | AttrTypeKind::Float { binding, .. } => binding.identity(),
        AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. }
        | AttrTypeKind::List { .. }
        | AttrTypeKind::Map { .. }
        | AttrTypeKind::Struct { .. }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => None,
    };
    if scalar_identity.is_some() {
        return TypeExpr::SchemaType {
            owned: crate::parser::ResolvedSchemaType::from_schema_declaration(
                schema_owner,
                attr_type,
            )
            .expect("identified scalar declaration"),
        };
    }

    match attr_type.kind() {
        AttrTypeKind::UnloadedSchemaType(unloaded) => {
            TypeExpr::UnloadedSchemaType(unloaded.clone())
        }
        AttrTypeKind::Reference { .. } | AttrTypeKind::String { .. } => TypeExpr::String,
        AttrTypeKind::ResourceHandle {
            provider,
            resource_type,
            ..
        } => TypeExpr::Ref(crate::parser::ResourceTypePath::new(
            provider.clone(),
            resource_type.clone(),
        )),
        AttrTypeKind::StringLiteralSet { values, .. } => match values.as_slice() {
            [value] => TypeExpr::StringLiteral(value.clone()),
            values => TypeExpr::Union(
                values
                    .iter()
                    .cloned()
                    .map(TypeExpr::StringLiteral)
                    .collect(),
            ),
        },
        AttrTypeKind::Int { .. } => TypeExpr::Int,
        AttrTypeKind::Float { .. } => TypeExpr::Float,
        AttrTypeKind::Bool => TypeExpr::Bool,
        AttrTypeKind::Duration => TypeExpr::Duration,
        AttrTypeKind::Enum {
            values,
            validate,
            to_dsl,
            ..
        } if values.is_none() && (validate.is_some() || to_dsl.is_some()) => TypeExpr::SchemaType {
            owned: crate::parser::ResolvedSchemaType::from_declaration(
                schema_owner,
                crate::parser::IdentifiedCustomTypeDeclaration::new(attr_type)
                    .expect("identified enum declaration"),
            ),
        },
        AttrTypeKind::Enum { .. } => TypeExpr::String,
        AttrTypeKind::List {
            element_type: inner,
            ..
        } => TypeExpr::List(Box::new(attribute_type_to_type_expr(inner, schema_owner))),
        AttrTypeKind::Map { value, .. } => {
            TypeExpr::Map(Box::new(attribute_type_to_type_expr(value, schema_owner)))
        }
        AttrTypeKind::Struct { fields, .. } => TypeExpr::Struct {
            fields: fields
                .iter()
                .map(|f| {
                    (
                        f.name.clone(),
                        attribute_type_to_type_expr(&f.field_type, schema_owner),
                    )
                })
                .collect(),
        },
        // A union receiver has no single TypeExpr that captures it —
        // collapsing to one member would silently let a value satisfy
        // alternatives it actually doesn't. Returning `String` as a
        // sentinel matches today's `attribute_type_to_type_expr`
        // contract (must return a TypeExpr) but consumers that care
        // about precision should detect the union upstream and emit
        // a "type annotation required" error instead of trusting this
        // result.
        AttrTypeKind::Union(_) => TypeExpr::String,
        // `Ref` requires the enclosing `Schema` to resolve — this pure
        // attribute-type-to-type-expr conversion does not have one in
        // scope. Returning `String` as a sentinel matches the
        // `Union(_)` arm: callers that need precision must resolve the
        // `Ref` via `Schema::resolve` upstream and convert the result.
        // The carina#3340 model emits `Ref` only at cycle points in
        // generated schemas, so this fallback path is unreachable for
        // hand-written or codegen-produced attribute types reached
        // through normal validation flows.
        AttrTypeKind::Ref(_) => TypeExpr::String,
    }
}

/// Phase transition (#2360 stage 2): attach declared/inferred origin and any
/// typed unchecked reason to every export.
pub fn apply_inference<D, B: BindingAddress>(
    parsed: crate::parser::File<
        crate::parser::ParsedExportParam<crate::resource::ParserPlaceholder, B>,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schemas: &SchemaRegistry,
    provider_context: &crate::parser::ProviderContext,
) -> crate::parser::File<
    crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>,
    crate::resource::ParserPlaceholder,
    crate::resource::ParserPlaceholder,
    D,
    B,
>
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    let judged = crate::parser::retained::JudgedFile::new(parsed, schemas, provider_context);
    let inferred_exports = infer_export_params(&judged).into_parameters();
    // Single phase-axis surface (carina#3126): only `export_params`
    // changes phase; every other field moves through `map_export_params`
    // so a new `File<E>` field cannot be silently dropped on the
    // parser→inferred relabel.
    judged.into_file().map_export_params(|_| inferred_exports)
}

/// Export parameters inferred with a caller-supplied schema registry.
#[derive(Debug, Clone)]
pub struct SchemaCheckedExportParams<B: BindingAddress = RelativeBindingName> {
    parameters: Vec<crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>>,
}

impl<B: BindingAddress> SchemaCheckedExportParams<B> {
    pub fn parameters(
        &self,
    ) -> &[crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>] {
        &self.parameters
    }

    fn into_parameters(
        self,
    ) -> Vec<crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>> {
        self.parameters
    }
}

/// Infer export types from a schema-judged configuration without cloning its file.
pub fn infer_export_params<D, B: BindingAddress>(
    judged: &crate::parser::retained::JudgedFile<
        '_,
        crate::parser::ParsedExportParam<crate::resource::ParserPlaceholder, B>,
        D,
        B,
    >,
) -> SchemaCheckedExportParams<B>
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    let parsed = judged.file();
    let report = judged.report();
    let parameters = parsed
        .export_params
        .iter()
        .map(|parameter| {
            let static_type = match parameter
                .judgement()
                .map(|expression| expression.type_projection())
            {
                Ok(crate::parser::retained::TypeProjection::NoProjection(_)) => {
                    ExportStaticType::Inferred(StaticType::Unchecked(
                        InferredUncheckedReason::NotInferable(NotInferableReason::ValueDependent {
                            reason: "type unavailable for a conversion-carrying metadata query"
                                .into(),
                        }),
                    ))
                }
                Err(missing) => ExportStaticType::Inferred(StaticType::Unchecked(
                    InferredUncheckedReason::InferenceFailed(missing.clone().into()),
                )),
                Ok(crate::parser::retained::TypeProjection::Available(_)) => {
                    match (&parameter.static_type, &parameter.value) {
                        (
                            Some(type_expr @ (TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_))),
                            _,
                        ) => ExportStaticType::Declared(type_expr.clone()),
                        (_, Some(crate::parser::BoundaryExpression::Handle(handle))) => {
                            ExportStaticType::Inferred(StaticType::Known(TypeExpr::Ref(
                                handle.resource_type().clone(),
                            )))
                        }
                        (Some(type_expr), _) => ExportStaticType::Declared(type_expr.clone()),
                        (None, value) => match value {
                            None => ExportStaticType::Inferred(StaticType::Unchecked(
                                InferredUncheckedReason::NotInferable(
                                    NotInferableReason::MissingValue,
                                ),
                            )),
                            Some(crate::parser::BoundaryExpression::Value(_value)) => {
                                let inferred = parameter
                                    .value_location()
                                    .ok_or_else(|| {
                                        crate::parser::retained::MissingEvidence::new(
                                            "export value location",
                                        )
                                    })
                                    .and_then(|location| report.expression(location))
                                    .map_err(|error| InferenceIssue::Failed(error.into()))
                                    .and_then(|expression| expression.static_type().cloned());
                                match inferred {
                                    Ok(type_expr) => {
                                        ExportStaticType::Inferred(StaticType::Known(type_expr))
                                    }
                                    // The expression itself is valid. An unannotated
                                    // export additionally requires a static type, so
                                    // this boundary owns the annotation failure.
                                    Err(InferenceIssue::NotInferable(
                                        NotInferableReason::ValueDependent { reason },
                                    )) => ExportStaticType::Inferred(StaticType::Unchecked(
                                        InferredUncheckedReason::InferenceFailed(
                                            InferenceError::UnknownType { reason },
                                        ),
                                    )),
                                    Err(issue) => ExportStaticType::Inferred(
                                        StaticType::Unchecked(issue.into()),
                                    ),
                                }
                            }
                            Some(crate::parser::BoundaryExpression::Handle(_handle)) => {
                                unreachable!(
                                    "handle expressions are classified before value inference"
                                )
                            }
                        },
                    }
                }
            };
            let value = parameter
                .value
                .as_ref()
                .and_then(|expression| match expression {
                    crate::parser::BoundaryExpression::Value(value) => Some(value.clone()),
                    crate::parser::BoundaryExpression::Handle(_handle) => None,
                });
            crate::parser::InferredExportParam::from_parsed(parameter, static_type, value)
        })
        .collect();
    SchemaCheckedExportParams { parameters }
}

/// Export parameters whose inference used a registry minted by provider loading.
#[derive(Debug, Clone)]
pub struct ExportParamsInferredWithLoadedProviderSchemas<B: BindingAddress = RelativeBindingName> {
    inferred: SchemaCheckedExportParams<B>,
}

impl<B: BindingAddress> ExportParamsInferredWithLoadedProviderSchemas<B> {
    pub fn parameters(
        &self,
    ) -> &[crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>] {
        self.inferred.parameters()
    }

    pub fn checked_exports(&self) -> ExportsInferredWithLoadedProviderSchemas<'_, B> {
        ExportsInferredWithLoadedProviderSchemas {
            parameters: self.parameters(),
        }
    }

    pub fn into_inferred_export_params(self) -> SchemaCheckedExportParams<B> {
        self.inferred
    }
}

/// Infer export parameters using schemas proven to come from provider loading.
/// The input has already crossed the parser provenance boundary.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::{ParsedFile, ProviderContext}, provider::ProviderLoadedSchemaRegistry, validation::inference::infer_export_params_with_loaded_provider_schemas};
/// fn infer(file: &ParsedFile, schemas: &ProviderLoadedSchemaRegistry, context: &ProviderContext) {
///     infer_export_params_with_loaded_provider_schemas(file, schemas, context);
/// }
/// ```
pub fn infer_export_params_with_loaded_provider_schemas<D, B: BindingAddress>(
    tree: &crate::parser::AnalysisTree<
        '_,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schemas: &ProviderLoadedSchemaRegistry,
    provider_context: &crate::parser::ProviderContext,
) -> ExportParamsInferredWithLoadedProviderSchemas<B>
where
    D: Clone
        + crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    ExportParamsInferredWithLoadedProviderSchemas {
        inferred: infer_export_params(&crate::parser::retained::JudgedFile::new(
            tree.file().clone(),
            schemas.schemas(),
            provider_context,
        )),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AttachedInferenceFailure<'a> {
    pub name: &'a str,
    pub error: &'a InferenceError,
}

impl std::fmt::Display for AttachedInferenceFailure<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&format_inference_error(self.name, self.error))
    }
}

/// Proof-bearing export view whose inference ran against loaded provider schemas.
#[derive(Debug, Clone)]
pub struct ExportsInferredWithLoadedProviderSchemas<'a, B: BindingAddress = RelativeBindingName> {
    parameters: &'a [crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>],
}

pub fn attached_inference_failures<'a, B: BindingAddress>(
    checked: ExportsInferredWithLoadedProviderSchemas<'a, B>,
) -> impl Iterator<Item = crate::parser::Located<AttachedInferenceFailure<'a>>> + 'a {
    checked
        .parameters
        .iter()
        .filter_map(move |parameter| match &parameter.static_type {
            ExportStaticType::Declared(_type_expr) => None,
            ExportStaticType::Inferred(StaticType::Known(_type_expr)) => None,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(_reason),
            )) => None,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::InferenceFailed(error),
            )) => Some(crate::parser::Located::new(
                parameter.name_location().clone(),
                AttachedInferenceFailure {
                    name: &parameter.name,
                    error,
                },
            )),
        })
}

/// Retry inferred exports whose original reason was not having enough schema
/// authority. A newly reportable failure replaces that reason; successful or
/// still-deferred probes retain the original provenance.
pub fn retry_unchecked_export_inference<'a, D, B: BindingAddress>(
    parsed: &'a mut crate::parser::File<
        crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schemas: &ProviderLoadedSchemaRegistry,
    provider_context: &crate::parser::ProviderContext,
) -> Result<ExportsInferredWithLoadedProviderSchemas<'a, B>, crate::parser::AnalysisTreeError>
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    crate::parser::retained::judge_configuration(parsed, schemas.schemas(), provider_context);
    drop(crate::parser::AnalysisTree::try_from_file(parsed)?);
    let report = parsed.expression_report().clone();

    for parameter in &mut parsed.export_params {
        let retry = match &parameter.static_type {
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(
                    NotInferableReason::MissingValue | NotInferableReason::ValueDependent { .. },
                ),
            )) => false,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(
                    NotInferableReason::NonInferableBinding { .. }
                    | NotInferableReason::SchemaUnavailable { .. }
                    | NotInferableReason::CustomTypeSchemaUnavailable { .. },
                ),
            )) => true,
            ExportStaticType::Declared(_type_expr) => false,
            ExportStaticType::Inferred(StaticType::Known(_type_expr)) => false,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::InferenceFailed(_error),
            )) => false,
        };
        if !retry {
            continue;
        }
        let Some(_value) = &parameter.value else {
            continue;
        };

        let expression = parameter
            .value_location()
            .ok_or_else(|| crate::parser::retained::MissingEvidence::new("export value location"))
            .and_then(|location| report.expression(location))
            .map_err(|error| InferenceIssue::Failed(error.into()))
            .and_then(|expression| expression.static_type().cloned());
        match expression {
            Ok(_type_expr) => {}
            Err(InferenceIssue::NotInferable(_reason)) => {}
            Err(InferenceIssue::Failed(error)) => {
                parameter.static_type = ExportStaticType::Inferred(StaticType::Unchecked(
                    InferredUncheckedReason::InferenceFailed(error),
                ));
            }
        }
    }

    Ok(ExportsInferredWithLoadedProviderSchemas {
        parameters: &parsed.export_params,
    })
}

/// Format an inference error pointing at the offending export. The shared CLI
/// validation pipeline and the LSP
/// (`carina-lsp/src/diagnostics/checks.rs`) both surface this same
/// "type annotation required" wording so editor and command line stay
/// in parity (the existing e2e parity tests pin this).
pub fn format_inference_error(name: &str, err: &InferenceError) -> String {
    format!("export '{}': type annotation required: {}", name, err)
}

#[cfg(test)]
mod tests {
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };

    use super::*;

    #[test]
    fn r16_missing_export_judgement_is_a_typed_error() {
        let mut parsed = crate::parser::parse_and_resolve("let x = 1\n").unwrap();
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::new_in_source(
                "unregistered".into(),
                Some(TypeExpr::String),
                Some(crate::parser::BoundaryExpression::Value(Value::Concrete(
                    crate::resource::ConcreteValue::String("ok".into()),
                ))),
                crate::parser::SourceSpan::dummy(),
                crate::parser::ModuleCallId::test_dummy_in("unregistered.crn")
                    .declaration_location()
                    .clone(),
            ));
        let inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert!(
            matches!(
                inferred.export_params[0].static_type,
                ExportStaticType::Inferred(StaticType::Unchecked(
                    InferredUncheckedReason::InferenceFailed(InferenceError::MissingEvidence(_))
                ))
            ),
            "{:?}",
            inferred.export_params[0].static_type
        );
    }

    #[test]
    fn r5c_metadata_export_projection_has_no_type_for_a_carried_value() {
        let parsed =
            crate::parser::parse_and_resolve("exports { answer = 'ready' as mock.Name }\n")
                .unwrap();
        let tree = crate::parser::AnalysisTree::try_from_file(&parsed).unwrap();
        let schemas = ProviderLoadedSchemaRegistry::for_test(SchemaRegistry::new());
        let inferred = infer_export_params_with_loaded_provider_schemas(
            &tree,
            &schemas,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert!(
            matches!(
                inferred.parameters()[0].static_type,
                ExportStaticType::Inferred(StaticType::Unchecked(_))
            ),
            "{:?}",
            inferred.parameters()[0]
        );
    }

    #[test]
    fn export_inference_returns_inferred_type() {
        let parsed = crate::parser::parse_and_resolve("exports { answer = 'ready' }\n").unwrap();
        let tree = crate::parser::AnalysisTree::try_from_file(&parsed).unwrap();
        let schemas = ProviderLoadedSchemaRegistry::for_test(SchemaRegistry::new());
        let inferred = infer_export_params_with_loaded_provider_schemas(
            &tree,
            &schemas,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(inferred.parameters().len(), 1);
        assert_eq!(inferred.parameters()[0].name, "answer");
        assert_eq!(
            inferred.parameters()[0].static_type,
            ExportStaticType::Inferred(StaticType::Known(TypeExpr::String))
        );
    }

    #[test]
    fn loaded_export_inference_rejects_missing_provenance_without_panicking() {
        let mut parsed =
            crate::parser::parse_and_resolve("let source = mock.test.Resource {}\n").unwrap();
        parsed.resources[0].declaration = Default::default();
        let error = match crate::parser::AnalysisTree::try_from_file(&parsed) {
            Ok(_) => panic!("reject missing provenance before inference"),
            Err(error) => error,
        };
        assert!(matches!(
            error.kind(),
            crate::parser::AnalysisTreeErrorKind::MissingResourceProvenance { .. }
        ));
    }

    #[test]
    fn export_inference_retry_rejects_missing_provenance_without_panicking() {
        let mut parsed =
            crate::parser::parse_and_resolve("let source = mock.test.Resource {}\n").unwrap();
        parsed.resources[0].declaration = Default::default();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let mut inferred = apply_inference(parsed, &SchemaRegistry::new(), &context);
        let schemas = ProviderLoadedSchemaRegistry::for_test(SchemaRegistry::new());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            retry_unchecked_export_inference(&mut inferred, &schemas, &context).map(|_| ())
        }));
        let error = outcome
            .expect("must not panic")
            .expect_err("reject missing provenance");
        assert!(matches!(
            error.kind(),
            crate::parser::AnalysisTreeErrorKind::MissingResourceProvenance { .. }
        ));
    }
    use crate::parser::ProviderContext;
    use crate::resource::{AuthoredValue as Value, ParserPlaceholder, RelativeBindingName};
    use crate::schema::{
        CompiledAttributeType as AttributeType, SchemaRegistry, TypeIdentity, legacy_validator,
    };

    fn judged_binding_type(
        source: &str,
        schemas: &SchemaRegistry,
        context: &ProviderContext,
    ) -> Result<TypeExpr, InferenceIssue> {
        let parsed =
            crate::parser::parse_file(source, std::path::Path::new("inference.crn"), context)
                .unwrap();
        let report = crate::parser::retained::judge_expressions(&parsed, schemas, context);
        report
            .binding(&RelativeBindingName::new("result"))
            .unwrap()
            .static_type()
            .cloned()
    }

    #[test]
    fn empty_list_judgement_requires_annotation() {
        assert_eq!(
            judged_binding_type(
                "let result = []",
                &SchemaRegistry::new(),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty())
            ),
            Err(InferenceIssue::NotInferable(
                NotInferableReason::MissingValue
            ))
        );
    }

    #[test]
    fn empty_interpolation_access_keeps_invalid_access() {
        let inferred = judged_binding_type(
            "let result = \"${}\"[0]",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert!(matches!(
            inferred,
            Err(InferenceIssue::Failed(InferenceError::InvalidAccess { .. }))
        ));
        assert_eq!(
            inferred.unwrap_err().to_string(),
            "cannot apply access [0] to String"
        );
    }

    fn noop(_v: &crate::resource::ExpandedValue) -> Result<(), String> {
        Ok(())
    }

    fn vpc_id_custom<R: crate::schema::PatternForm>() -> crate::schema::PatternAttributeType<R> {
        PatternAttributeType::refined_string_with_validator(
            TypeIdentity::bare("VpcId"),
            None,
            None,
            legacy_validator(move |value| noop(value).map_err(Into::into)),
            None,
        )
    }

    fn vpc_schema<R: crate::schema::PatternForm>() -> crate::schema::PatternResourceSchema<R> {
        PatternResourceSchema::new("ec2.Vpc")
            .attribute(
                PatternAttributeSchema::optional("cidr_block", PatternAttributeType::string())
                    .build(),
            )
            .attribute(PatternAttributeSchema::optional("vpc_id", vpc_id_custom()).build())
    }

    #[test]
    fn unloaded_inferred_scalar_kind_defers_without_panicking() {
        let annotation = crate::parser::parse_type_expr_str(
            "Count",
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        assert!(
            matches!(inferred_scalar_kind(&annotation), Err(NotInferableReason::CustomTypeSchemaUnavailable { identity }) if identity == crate::schema::TypeIdentity::bare("Count"))
        );
    }

    #[test]
    fn integer_arithmetic_accepts_declared_and_resource_int_operands() {
        assert_eq!(
            judged_binding_type(
                "arguments { source: Int }\nlet value = source + 1\nlet result = value",
                &SchemaRegistry::new(),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty())
            ),
            Ok(TypeExpr::Int)
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            PatternResourceSchema::new("clock.Source")
                .attribute(
                    PatternAttributeSchema::provider_populated(
                        "seconds",
                        PatternAttributeType::int(),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        assert_eq!(
            judged_binding_type(
                "let source = test.clock.Source {}\nlet result = source.seconds / 60",
                &schemas,
                &ProviderContext::new(crate::EnvironmentSnapshot::empty())
            ),
            Ok(TypeExpr::Int)
        );
    }

    #[test]
    fn self_referential_resource_attribute_uses_schema_type() {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            PatternResourceSchema::new("clock.Source")
                .attribute(
                    PatternAttributeSchema::optional("seconds", PatternAttributeType::int())
                        .build(),
                )
                .validate()
                .unwrap(),
        );
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            "let source = test.clock.Source { seconds = source.seconds }\nlet result = source.seconds",
            std::path::Path::new("inference.crn"),
            &context,
        )
        .unwrap();
        let report = crate::parser::retained::judge_expressions(&parsed, &schemas, &context);

        assert_eq!(
            report
                .binding(&RelativeBindingName::new("result"))
                .unwrap()
                .static_type(),
            Ok(&TypeExpr::Int),
        );
        assert!(report.findings().is_empty());
    }

    #[test]
    fn refined_module_arguments_use_their_declared_scalar_base_for_arithmetic() {
        let port_identity = TypeIdentity::bare("Port");
        let label_identity = TypeIdentity::bare("Label");
        let schema = PatternResourceSchema::new("clock.Source")
            .attribute(
                PatternAttributeSchema::provider_populated(
                    "port",
                    PatternAttributeType::refined_int(Some(port_identity.clone()), None),
                )
                .build(),
            )
            .attribute(
                PatternAttributeSchema::provider_populated(
                    "label",
                    PatternAttributeType::refined_string(
                        Some(label_identity.clone()),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            schema.validate().expect("clock schema must be valid"),
        );

        let snapshot = crate::test_utils::loaded_fixture_snapshot(schemas);
        let context = snapshot.provider_context().as_provider_context();
        assert_eq!(
            judged_binding_type(
                "arguments { port: Port }\nlet result = port + 1",
                snapshot.schema_registry(),
                context
            ),
            Ok(TypeExpr::Int),
            "an identity-bound refined Int argument remains an Int operand"
        );
        assert!(matches!(
            judged_binding_type(
                "arguments { label: Label }\nlet result = label + 1",
                snapshot.schema_registry(),
                context
            ),
            Err(InferenceIssue::Failed(
                InferenceError::InvalidIntegerArithmeticOperand {
                    side: crate::integer_arithmetic::IntegerOperandSide::Left,
                    ..
                }
            ))
        ));
    }

    fn schemas_with_vpc() -> SchemaRegistry {
        let mut s = SchemaRegistry::new();
        s.insert(
            "awscc",
            vpc_schema()
                .validate()
                .expect("VPC inference schema must be valid"),
        );
        s
    }

    fn owned_vpc_id_type() -> TypeExpr {
        let schema = vpc_schema().validate().unwrap();
        let attribute = schema.effective_attribute("vpc_id").unwrap();
        TypeExpr::SchemaType {
            owned: crate::parser::ResolvedSchemaType::from_schema_declaration(
                "awscc",
                attribute.value_type().as_type(),
            )
            .unwrap(),
        }
    }

    #[test]
    fn literal_int_inferred_as_int() {
        let r = judged_binding_type(
            "let result = 42",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(r, Ok(TypeExpr::Int));
    }

    #[test]
    fn literal_bool_inferred_as_bool() {
        let r = judged_binding_type(
            "let result = true",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(r, Ok(TypeExpr::Bool));
    }

    #[test]
    fn secret_inferred_as_string() {
        let r = judged_binding_type(
            "let result = secret('hidden')",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn lookup_and_string_list_is_not_inferable() {
        let inferred = judged_binding_type(
            "let result = [lookup({ a = 'one' }, 'a', 'default'), 'extra']",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            inferred,
            Err(InferenceIssue::NotInferable(
                NotInferableReason::ValueDependent {
                    reason: "builtin return type depends on arguments".into(),
                }
            ))
        );
    }

    #[test]
    fn list_of_strings_inferred_as_list_string() {
        let r = judged_binding_type(
            "let result = ['a','b']",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(r, Ok(TypeExpr::List(Box::new(TypeExpr::String))));
    }

    #[test]
    fn list_with_mixed_types_fails() {
        let r = judged_binding_type(
            "let result = ['a',1]",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(
                InferenceError::HeterogeneousCollection { .. }
            ))
        ));
    }

    #[test]
    fn production_collection_inference_retains_mismatch_names() {
        let inferred = judged_binding_type(
            "let result = ['a',1]",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            inferred.as_ref().unwrap_err().to_string(),
            "collection element types disagree: `String` vs `Int`",
        );
        assert_eq!(
            inferred,
            Err(InferenceIssue::Failed(
                InferenceError::HeterogeneousCollection {
                    types: crate::schema::MismatchTypeNames::for_type_exprs(
                        &TypeExpr::String,
                        &TypeExpr::Int
                    ),
                }
            ))
        );
    }

    #[test]
    fn map_of_strings_inferred_as_map_string() {
        let r = judged_binding_type(
            "let result = { a = 'x' b = 'y' }",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(r, Ok(TypeExpr::Map(Box::new(TypeExpr::String))));
    }

    #[test]
    fn heterogeneous_map_judgement_remains_value_dependent() {
        let r = judged_binding_type(
            "let result = { a = 'x' b = 1 }",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            r,
            Err(InferenceIssue::NotInferable(
                NotInferableReason::ValueDependent {
                    reason: "collection element types disagree: `String` vs `Int`".into()
                }
            ))
        );
    }

    #[test]
    fn resource_ref_descends_struct_field_path() {
        // `vpc.tags.environment` against a `Struct{environment: String}`.
        // Locks the descent loop so a future refactor cannot silently
        // break field-path traversal.

        let tags_struct = PatternAttributeType::struct_(
            "Tags",
            vec![
                PatternStructField::optional("environment", PatternAttributeType::string()).build(),
            ],
        );
        let schema = PatternResourceSchema::new("ec2.Vpc")
            .attribute(PatternAttributeSchema::optional("tags", tags_struct).build());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );

        let r = judged_binding_type(
            "let vpc = awscc.ec2.Vpc {}\nlet result = vpc.tags.environment",
            &schemas,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn coalesce_uses_left_list_type_for_empty_default() {
        assert_eq!(
            judged_binding_type(
                "let up = { accounts = ['a'] }\nlet result = up.accounts ?? []",
                &SchemaRegistry::new(),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty())
            ),
            Ok(TypeExpr::List(Box::new(TypeExpr::String)))
        );
    }

    #[test]
    fn coalesce_rejects_default_that_conflicts_with_left_type() {
        let error = judged_binding_type(
            "let up = { accounts = ['a'] }\nlet result = up.accounts ?? 'not-a-list'",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect_err("coalesce default must match the left type");
        assert!(matches!(
            &error,
            InferenceIssue::Failed(InferenceError::IncompatibleCoalesceTypes { .. })
        ));
        assert!(error.to_string().contains("does not match left type"));
    }

    #[test]
    fn resource_ref_to_string_attr_inferred_as_string() {
        let r = judged_binding_type(
            "let vpc = awscc.ec2.Vpc {}\nlet result = vpc.cidr_block",
            &schemas_with_vpc(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn dynamic_enum_attr_type_projects_to_schema_type() {
        let region = AttributeType::enum_(
            TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
            None,
            vec![],
            None,
            Some(crate::schema::DslTransform::HyphenToUnderscore),
        );

        assert_eq!(
            attribute_type_to_type_expr(&region, "aws"),
            TypeExpr::SchemaType {
                owned: crate::parser::ResolvedSchemaType::from_schema_declaration("aws", &region)
                    .unwrap(),
            }
        );
    }

    #[test]
    fn closed_enum_attr_type_still_projects_to_string() {
        let effect = AttributeType::enum_(
            crate::schema::enum_identity("Effect", Some("aws.iam.PolicyDocument")),
            Some(vec!["Allow".to_string(), "Deny".to_string()]),
            vec![
                ("Allow".to_string(), "allow".to_string()),
                ("Deny".to_string(), "deny".to_string()),
            ],
            None,
            None,
        );

        assert_eq!(
            attribute_type_to_type_expr(&effect, "aws"),
            TypeExpr::String
        );
    }

    #[test]
    fn resource_ref_unknown_attribute_errors() {
        let r = judged_binding_type(
            "let vpc = awscc.ec2.Vpc {}\nlet result = vpc.missing",
            &schemas_with_vpc(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(
                InferenceError::UnknownAttribute { .. }
            ))
        ));
    }

    #[test]
    fn postfix_access_on_list_result_infers_element_type() {
        let inferred = judged_binding_type(
            "let result = ['item'][0]",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(inferred, Ok(TypeExpr::String));
    }

    #[test]
    fn postfix_access_on_map_result_infers_value_type() {
        let inferred = judged_binding_type(
            "let result = { x = 1 }['x']",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(inferred, Ok(TypeExpr::Int));
    }

    #[test]
    fn resource_ref_subscript_on_list_peels_one_layer() {
        // Retained judgement follows `[0]` into the schema collection:
        // `vpc.subnets[0]` against `subnets: List<String>` yields `String`.
        let list_str = PatternAttributeType::list(PatternAttributeType::string());
        let schema = PatternResourceSchema::new("ec2.Vpc")
            .attribute(PatternAttributeSchema::optional("subnets", list_str).build());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );

        let r = judged_binding_type(
            "let vpc = awscc.ec2.Vpc {}\nlet result = vpc.subnets[0]",
            &schemas,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn resource_ref_to_union_typed_attr_demands_annotation() {
        // A `Union` receiver carries multiple alternatives; projecting
        // it to one branch silently lets the value satisfy alternatives
        // it doesn't. Demand annotation instead.
        let union_attr = PatternAttributeType::union(vec![
            PatternAttributeType::string(),
            PatternAttributeType::int(),
        ]);
        let schema = PatternResourceSchema::new("ec2.Vpc")
            .attribute(PatternAttributeSchema::optional("polymorphic", union_attr).build());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );

        let r = judged_binding_type(
            "let vpc = awscc.ec2.Vpc {}\nlet result = vpc.polymorphic",
            &schemas,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
        ));
    }

    #[test]
    fn bindings_from_parts_resource_wins_over_upstream_state_collision() {
        // Mid-edit shape: same binding name appears in both
        // `let x = upstream_state {...}` and `let x = aws.ec2.Vpc {...}`.
        // Insert order (upstream first, resources second) means the
        // resource wins, so `x.attr` infers precisely instead of
        // degrading to `NonInferableBinding`.
        use crate::parser::UpstreamState;
        use crate::resource::AuthoredResource as Resource;
        let resource: Resource =
            Resource::with_provider("awscc", "ec2.Vpc", "main", None).with_binding("x");
        let upstream: UpstreamState = UpstreamState {
            binding: "x".into(),
            source: std::path::PathBuf::from("../other"),
        };
        let bindings = bindings_from_parts(&[resource], &[], &[], &[upstream]);
        assert!(matches!(
            bindings.get("x"),
            Some(InferenceBinding::Resource { .. })
        ));
    }

    #[test]
    fn bindings_from_parts_collects_pure_upstream_state() {
        use crate::parser::UpstreamState;
        let upstream: UpstreamState = UpstreamState {
            binding: "network".into(),
            source: std::path::PathBuf::from("../network"),
        };
        let bindings = bindings_from_parts::<
            crate::resource::ModuleSource,
            ParserPlaceholder,
            RelativeBindingName,
        >(&[], &[], &[], &[upstream]);
        assert!(matches!(
            bindings.get("network"),
            Some(InferenceBinding::UpstreamState)
        ));
    }

    #[test]
    fn list_inside_struct_field_with_union_does_not_demand_annotation() {
        // Real-world IAM-policy shape: outer Struct, inner field is a
        // List<Struct{principal: Union<...>}>. The user's
        // `policy_document = main.policy.policy_document` reference is
        // forwarding the whole struct, not picking a union branch, and
        // must not be blocked. Inference accepts this and projects the
        // outer Struct into a TypeExpr::Struct.

        let inner_struct = PatternAttributeType::struct_(
            "Statement",
            vec![
                PatternStructField::optional(
                    "principal",
                    PatternAttributeType::union(vec![
                        PatternAttributeType::string(),
                        PatternAttributeType::int(),
                    ]),
                )
                .build(),
            ],
        );
        let outer_struct = PatternAttributeType::struct_(
            "PolicyDocument",
            vec![
                PatternStructField::optional("statement", PatternAttributeType::list(inner_struct))
                    .build(),
            ],
        );
        let schema = PatternResourceSchema::new("iam.Policy")
            .attribute(PatternAttributeSchema::optional("policy_document", outer_struct).build());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );
        let r = judged_binding_type(
            "let policy = awscc.iam.Policy {}\nlet result = policy.policy_document",
            &schemas,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        // Pin the inferred shape, not just `is_ok`: the outer Struct
        // is preserved with its `statement` field, and the buried
        // Union inside the inner Statement struct collapses to the
        // sentinel `String` at the leaf (per `attribute_type_to_type_expr`'s
        // documented behavior). A future refactor that changes how
        // nested unions are rendered will trip this assertion and
        // force a deliberate decision rather than a silent drift.
        let inferred = r.expect("inference should accept policy_document");
        let TypeExpr::Struct { fields } = inferred else {
            panic!("expected Struct, got {:?}", inferred);
        };
        assert_eq!(fields.len(), 1, "outer struct must have one field");
        assert_eq!(fields[0].0, "statement");
        let TypeExpr::List(elem) = &fields[0].1 else {
            panic!("statement must project to a List, got {:?}", fields[0].1);
        };
        let TypeExpr::Struct {
            fields: inner_fields,
        } = elem.as_ref()
        else {
            panic!("List element must be a Struct, got {:?}", elem);
        };
        assert_eq!(inner_fields.len(), 1);
        assert_eq!(inner_fields[0].0, "principal");
        // The buried Union projects to the sentinel `String`. This is
        // the documented contract — see `attribute_type_to_type_expr`'s
        // doc-comment on Union. Locking the assertion here prevents
        // silent drift to e.g. `Int` if the first-member fallback ever
        // gets reintroduced.
        assert_eq!(inner_fields[0].1, TypeExpr::String);
    }

    #[test]
    fn deferred_builtin_identity_rejects_unknown_names() {
        assert!(crate::builtins::BuiltinFunction::from_name("nonexistent").is_none());
    }

    #[test]
    fn apply_inference_fills_inferable_export_with_inferred_type() {
        let parsed = crate::parser::parse_and_resolve(
            "let main = awscc.ec2.Vpc {}\nexports { vpc_id = main.vpc_id }",
        )
        .unwrap();

        let inferred = apply_inference(
            parsed,
            &schemas_with_vpc(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(inferred.export_params.len(), 1);
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Known(owned_vpc_id_type()))
        );
    }

    #[test]
    fn apply_inference_attaches_failure_to_inferred_export() {
        use crate::static_type::{ExportStaticType, InferredUncheckedReason, StaticType};

        let parsed = crate::parser::parse_and_resolve("arguments { entries: String }\nexports { zone_id = lookup(entries, 'key', 'fallback') }").unwrap();

        let inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );

        assert!(matches!(
            &inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::InferenceFailed(InferenceError::UnknownType { .. })
            ))
        ));
    }

    #[test]
    fn apply_inference_preserves_declared_and_inferred_origins() {
        use crate::static_type::{ExportStaticType, StaticType};

        let parsed = crate::parser::parse_and_resolve(
            "exports { declared: String = 'declared' inferred = 'inferred' }",
        )
        .unwrap();

        let inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );

        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Declared(TypeExpr::String)
        );
        assert_eq!(
            inferred.export_params[1].static_type,
            ExportStaticType::Inferred(StaticType::Known(TypeExpr::String))
        );
    }

    #[test]
    fn handle_exports_retain_provenance_until_the_state_boundary_rejects_them() {
        let mut context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        context
            .resource_types
            .insert(("mock".to_string(), "test.Record".to_string()));
        let mut parsed = crate::parser::parse(
            r#"
let record = mock.test.Record { name = "record" }
exports {
  inferred = record
  declared: mock.test.Record = "record-id"
  annotated_value: String = record
}
"#,
            &context,
        )
        .expect("handle export fixture parses");
        crate::parser::resolve_resource_refs_with_config(&mut parsed, &context)
            .expect("handle export fixture resolves declared resource types");
        assert!(
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                crate::validation::resolve_file_type_exprs(&mut parsed, session).unwrap()
            })
            .is_empty(),
            "handle export fixture resolves its declaration types",
        );

        let inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        let handle_type =
            TypeExpr::Ref(crate::parser::ResourceTypePath::new("mock", "test.Record"));
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Known(handle_type.clone())),
        );
        assert_eq!(
            inferred.export_params[1].static_type,
            ExportStaticType::Declared(handle_type),
        );
        assert!(
            matches!(
                inferred.export_params[2].static_type,
                ExportStaticType::Inferred(StaticType::Known(TypeExpr::Ref(_)))
            ),
            "a handle RHS must not disappear behind a value annotation",
        );

        let errors = crate::validation::validate_export_handle_boundaries(&inferred.export_params)
            .expect_err("resource handles cannot cross state boundaries");
        assert_eq!(
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            "export 'inferred': inferred resource handle mock.test.Record cannot cross a state boundary; export an identity value such as mock.test.Record.Id instead\n\
export 'declared': resource handle mock.test.Record cannot cross a state boundary; export an identity value such as mock.test.Record.Id instead\n\
export 'annotated_value': inferred resource handle mock.test.Record cannot cross a state boundary; export an identity value such as mock.test.Record.Id instead",
        );
    }

    #[test]
    fn apply_inference_keeps_not_inferable_reasons_distinct() {
        use crate::static_type::{
            ExportStaticType, InferredUncheckedReason, NotInferableReason, StaticType,
        };

        let mut parsed = crate::parser::parse_and_resolve("let upstream = upstream_state { source = '../upstream' }\nlet main = awscc.ec2.Vpc {}\nexports { missing = 'removed' upstream = upstream.value schema = main.vpc_id }").unwrap();
        // A removed export value retains its parser-owned declaration identity.
        parsed.export_params[0].value = None;

        let inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );

        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::MissingValue)
            ))
        );
        assert_eq!(
            inferred.export_params[1].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::NonInferableBinding {
                    binding: "upstream".to_string()
                })
            ))
        );
        assert_eq!(
            inferred.export_params[2].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::SchemaUnavailable {
                    binding: "main".to_string()
                })
            ))
        );
    }

    #[test]
    fn retry_unchecked_export_inference_attaches_reportable_failure() {
        use crate::static_type::{
            ExportStaticType, InferredUncheckedReason, NotInferableReason, StaticType,
        };

        let parsed = crate::parser::parse_file(
            "let main = awscc.ec2.Vpc {}\nexports { vpc_id = main.vpc_i }\n",
            std::path::Path::new("test.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();

        let mut inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::SchemaUnavailable {
                    binding: "main".to_string()
                })
            ))
        );

        let loaded_schemas = ProviderLoadedSchemaRegistry::for_test(schemas_with_vpc());
        let schema_checked = retry_unchecked_export_inference(
            &mut inferred,
            &loaded_schemas,
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let failures = attached_inference_failures(schema_checked)
            .map(|failure| (failure.name.to_string(), failure.error.clone()))
            .collect::<Vec<_>>();

        assert!(matches!(
            failures.as_slice(),
            [(name, InferenceError::UnknownAttribute { binding, attribute })]
                if name == "vpc_id" && binding == "main" && attribute == "vpc_i"
        ));
    }

    #[test]
    fn retry_unchecked_export_inference_preserves_reason_after_success() {
        use crate::static_type::{
            ExportStaticType, InferredUncheckedReason, NotInferableReason, StaticType,
        };

        let parsed = crate::parser::parse_file(
            "let main = awscc.ec2.Vpc {}\nexports { vpc_id = main.vpc_id }\n",
            std::path::Path::new("test.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();

        let mut inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        let loaded_schemas = ProviderLoadedSchemaRegistry::for_test(schemas_with_vpc());
        let schema_checked = retry_unchecked_export_inference(
            &mut inferred,
            &loaded_schemas,
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        assert_eq!(attached_inference_failures(schema_checked).count(), 0);

        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::SchemaUnavailable {
                    binding: "main".to_string()
                })
            )),
            "retry probes for reportable errors without replacing unchecked provenance",
        );
    }

    #[test]
    fn apply_inference_preserves_explicit_annotation() {
        let parsed =
            crate::parser::parse_and_resolve("exports { vpc_id: VpcId = 'vpc-abc' }").unwrap();

        let inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Declared(crate::parser::bare_type_for_test("vpc_id".to_string()))
        );
    }

    #[test]
    fn apply_inference_resolves_module_call_attribute_via_virtual_binding() {
        // #2493: expanded module-output judgements must retain the inner
        // schema attribute's exact type and callback provenance for exports.
        // The composition registers its attribute names; retained judgement
        // follows the authored module output to `role.vpc_id`.
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(
            directory.path().join("child/main.crn"),
            "let role = awscc.ec2.Vpc {}\nattributes { role_id = role.vpc_id }",
        )
        .unwrap();
        std::fs::write(directory.path().join("main.crn"), "let github_module = use { source = './child' }\nlet github_actions_carina = github_module {}\nexports { role_id = github_actions_carina.role_id }").unwrap();
        let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::config_loader::parse_directory(&path, &context).unwrap();
        let parsed =
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                crate::module_resolver::resolve_modules_with_providerless_config(
                    parsed,
                    &path,
                    session,
                    &schemas_with_vpc(),
                )
            })
            .unwrap();

        let inferred = apply_inference(
            parsed,
            &schemas_with_vpc(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(inferred.export_params.len(), 1);
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Known(owned_vpc_id_type())),
            "expected the inner attribute's type to flow through the composition binding"
        );
    }

    #[test]
    fn apply_inference_treats_pre_expansion_module_call_binding_as_non_inferable() {
        // #2493: before expansion the call is registered as `ModuleCall`
        // and has no authored output evidence. Export inference retains
        // `NonInferableBinding` without an unknown-binding diagnostic.
        // The sibling test exercises authored output judgement after
        // expansion; loaded-schema retry preserves unchecked provenance.
        let parsed = crate::parser::parse_and_resolve("let github_module = use { source = './child' }\nlet github_actions_carina = github_module {}\nexports { role_arn = github_actions_carina.role_arn }").unwrap();

        let inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::NonInferableBinding {
                    binding: "github_actions_carina".to_string()
                })
            ))
        );
    }

    #[test]
    fn apply_inference_attaches_missing_value_reason() {
        // Export with no value and no annotation: nothing to infer from
        // → a typed non-failure reason (the value-less shape is the
        // parser-tombstoned state, not an inference failure).
        let mut parsed =
            crate::parser::parse_and_resolve("exports { vpc_id = 'removed' }").unwrap();
        parsed.export_params[0].value = None;

        let inferred = apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::MissingValue)
            ))
        );
    }

    #[test]
    fn sibling_map_field_references_infer_list_string() {
        let r = judged_binding_type(
            "let a = { x = 'hi' }\nlet b = { y = 'there' }\nlet result = [a.x,b.y]",
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            r,
            Ok(TypeExpr::List(Box::new(TypeExpr::String))),
            "sibling map field references must infer list(String)"
        );
    }
}
