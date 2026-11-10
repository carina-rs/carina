//! Boundary assignability when provider declarations may not be loaded.

use super::{AttrTypeKind, PatternForm, TypeIdentity, TypeInSchema, resolve_refs_on_path};
use crate::binding_index::{
    BindingIndex, DeclaredValueAssignment, RefTypeAssignabilityError, RefTypeMismatch,
    ResolvedRefType,
};
use crate::parser::CustomTypeValidationError;

/// Missing provider evidence is distinct from a proven type mismatch.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub enum TypeAssignability<'s> {
    /// Every structural destination member constraint is statically proven.
    Assignable,
    Mismatch,
    Unchecked(TypeAssignmentDeferral<'s>),
    NeedsLocal(LocalAssignmentObligation<'s>),
}

/// The actual annotation identities whose scalar evidence is missing.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeAssignmentDeferral<'s> {
    first: crate::parser::TypeExpr,
    additional: Vec<crate::parser::TypeExpr>,
    brand: std::marker::PhantomData<fn(&'s ()) -> &'s ()>,
}

impl<'s> TypeAssignmentDeferral<'s> {
    pub(crate) fn new(expected: crate::parser::TypeExpr) -> Self {
        Self {
            first: expected,
            additional: Vec::new(),
            brand: std::marker::PhantomData,
        }
    }
    pub(crate) fn with_additional(
        mut self,
        types: impl IntoIterator<Item = crate::parser::TypeExpr>,
    ) -> Self {
        self.additional.extend(types);
        self
    }
    fn merge(mut self, other: Self) -> Self {
        self.additional.push(other.first);
        self.additional.extend(other.additional);
        self
    }
    pub(crate) fn into_types(self) -> impl Iterator<Item = crate::parser::TypeExpr> {
        std::iter::once(self.first).chain(self.additional)
    }
    fn record(self, sink: &dyn crate::validation::deferred_types::ProviderTypeCheckContext) {
        for expected in self.into_types() {
            sink.record_type(&expected, "assignment");
        }
    }
}

/// Local and unavailable-schema obligations travel together through recursive
/// assignment. Consumers must choose a concrete value check or explicit deferral.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub struct LocalAssignmentObligation<'s> {
    destination: super::DestinationObligation,
    schema: Option<TypeAssignmentDeferral<'s>>,
}

impl<'s> LocalAssignmentObligation<'s> {
    fn new<R: PatternForm>(destination: TypeInSchema<'_, R>) -> Self {
        Self {
            destination: R::assignment_destination(destination),
            schema: None,
        }
    }
    pub fn missing_schema(&self) -> Option<&TypeAssignmentDeferral<'s>> {
        self.schema.as_ref()
    }
    pub(crate) fn into_parts(
        self,
    ) -> (
        super::DestinationObligation,
        Option<TypeAssignmentDeferral<'s>>,
    ) {
        (self.destination, self.schema)
    }
}

impl<'s> TypeAssignability<'s> {
    /// Loading schemas preserves the distinction between proof and a remaining
    /// destination check; it never converts a pending member into a boolean.
    pub fn require_loaded(self) -> Result<super::LoadedTypeAssignment, CustomTypeValidationError> {
        use super::LoadedTypeAssignment;
        match self {
            Self::Assignable => Ok(LoadedTypeAssignment::Assignable),
            Self::Mismatch => Ok(LoadedTypeAssignment::Mismatch),
            Self::Unchecked(_) => Err(CustomTypeValidationError::ProvidersNotLoaded),
            Self::NeedsLocal(LocalAssignmentObligation {
                destination,
                schema,
            }) => match schema {
                Some(missing) => {
                    drop(missing);
                    Err(CustomTypeValidationError::ProvidersNotLoaded)
                }
                None => Ok(LoadedTypeAssignment::NeedsLocal(destination)),
            },
        }
    }

    /// Record unavailable schemas while retaining a separate local obligation.
    pub fn record_unchecked(
        self,
        sink: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> super::LoadedTypeAssignment {
        use super::LoadedTypeAssignment;
        match self {
            Self::Assignable => LoadedTypeAssignment::Assignable,
            Self::Mismatch => LoadedTypeAssignment::Mismatch,
            Self::Unchecked(evidence) => {
                evidence.record(sink);
                LoadedTypeAssignment::Assignable
            }
            Self::NeedsLocal(LocalAssignmentObligation {
                destination,
                schema,
            }) => {
                if let Some(missing) = schema {
                    missing.record(sink);
                }
                LoadedTypeAssignment::NeedsLocal(destination)
            }
        }
    }

    pub(crate) fn into_owned<'out>(self) -> TypeAssignability<'out> {
        let own = |evidence: TypeAssignmentDeferral<'s>| TypeAssignmentDeferral {
            first: evidence.first,
            additional: evidence.additional,
            brand: std::marker::PhantomData,
        };
        match self {
            Self::Assignable => TypeAssignability::Assignable,
            Self::Mismatch => TypeAssignability::Mismatch,
            Self::Unchecked(evidence) => TypeAssignability::Unchecked(own(evidence)),
            Self::NeedsLocal(LocalAssignmentObligation {
                destination,
                schema,
            }) => TypeAssignability::NeedsLocal(LocalAssignmentObligation {
                destination,
                schema: schema.map(own),
            }),
        }
    }

    fn within_destination<R: PatternForm>(
        self,
        destination: TypeInSchema<'_, R>,
        position: super::assignment_obligation::DestinationPosition<'_>,
    ) -> Self {
        match self {
            Self::Assignable => Self::Assignable,
            Self::Mismatch => Self::Mismatch,
            Self::Unchecked(evidence) => Self::Unchecked(evidence),
            Self::NeedsLocal(LocalAssignmentObligation {
                destination: child,
                schema,
            }) => Self::NeedsLocal(LocalAssignmentObligation {
                destination: child.within(destination, position),
                schema,
            }),
        }
    }
}

#[derive(Clone, Copy)]
enum IdentityLeaf<'a> {
    Scalar(&'a super::RefinementBinding),
    Enum(&'a TypeIdentity),
    Unloaded(&'a crate::parser::UnloadedSchemaType),
}

impl<'a> IdentityLeaf<'a> {
    fn identity(self) -> Option<&'a TypeIdentity> {
        match self {
            Self::Scalar(binding) => binding.identity(),
            Self::Enum(identity) => Some(identity),
            Self::Unloaded(unloaded) => Some(unloaded.identity()),
        }
    }
}

fn identity_leaf<R: PatternForm>(kind: &AttrTypeKind<R>) -> Option<IdentityLeaf<'_>> {
    match kind {
        AttrTypeKind::UnloadedSchemaType(unloaded) => Some(IdentityLeaf::Unloaded(unloaded)),
        AttrTypeKind::String {
            identity_layer: crate::schema::StringIdentityLayer { binding, .. },
            member_layer: _,
        }
        | AttrTypeKind::Int {
            identity_layer: crate::schema::IntIdentityLayer { binding, .. },
            member_layer: _,
        }
        | AttrTypeKind::Float { binding, range: _ } => {
            binding.identity().map(|_| IdentityLeaf::Scalar(binding))
        }
        AttrTypeKind::Enum {
            identity,
            base: _,
            values: _,
            dsl_aliases: _,
            validate: _,
            to_dsl: _,
        } => Some(IdentityLeaf::Enum(identity)),
        AttrTypeKind::Reference {
            identity: _,
            pattern: _,
            length: _,
        }
        | AttrTypeKind::ResourceHandle {
            provider: _,
            resource_type: _,
        }
        | AttrTypeKind::StringLiteralSet { values: _ }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::List {
            element_type: _,
            ordered: _,
            constraints: _,
        }
        | AttrTypeKind::Map {
            key: _,
            value: _,
            constraints: _,
        }
        | AttrTypeKind::Struct {
            name: _,
            fields: _,
            field_groups: _,
        }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => None,
    }
}

/// Authority allowed to admit an edge at a sink. Semantic namespaces never
/// select the provider that owns a resource or data-source attribute.
#[derive(Clone, Copy)]
pub enum SinkAuthority<'a> {
    Owner(&'a crate::parser::SchemaTypeOwner),
    Owners(&'a crate::parser::NonEmptySchemaTypeOwners),
}

#[derive(Clone, Copy)]
enum SinkScope<'a> {
    Authority(SinkAuthority<'a>),
    Annotation,
}

/// A sink's type, edge catalog, and authority travel together through every
/// recursive comparison. Only a lifted annotation may create annotation scope;
/// its identified leaves retain their complete owner sets.
///
/// Arbitrary types cannot publicly choose a sink authority.
/// ```compile_fail,E0624
/// use carina_core::parser::{ProviderCustomTypes, SchemaTypeOwner};
/// use carina_core::schema::{AttributeType, SinkAuthority, TypeInSchema};
/// let ty: AttributeType = AttributeType::string();
/// let catalog = ProviderCustomTypes::default();
/// let owner = SchemaTypeOwner::Provider("test".into());
/// let _ = TypeInSchema::schemaless(&ty).with_authority(&catalog, SinkAuthority::Owner(&owner));
/// ```
#[derive(Clone, Copy)]
pub struct AssignmentSink<'a, R: PatternForm = super::Draft> {
    pub(super) ty: TypeInSchema<'a, R>,
    catalog: &'a crate::parser::ProviderCustomTypes,
    scope: SinkScope<'a>,
}

impl<'a, R: PatternForm> std::ops::Deref for AssignmentSink<'a, R> {
    type Target = TypeInSchema<'a, R>;
    fn deref(&self) -> &Self::Target {
        &self.ty
    }
}

impl<'a, R: PatternForm> AssignmentSink<'a, R> {
    pub fn as_type(self) -> &'a super::PatternAttributeType<R> {
        self.ty.attr
    }
    pub fn defs(self) -> &'a std::collections::BTreeMap<String, super::PatternAttributeType<R>> {
        self.ty.defs
    }

    /// Descend into a List element or Map value while retaining the catalog
    /// and authority. Arbitrary types cannot replace an annotation's leaves.
    pub fn collection_element(self) -> Option<Self> {
        let Ok(resolved) = resolve_refs_on_path(self.ty.attr, self.ty.defs, &mut Vec::new()) else {
            return None;
        };
        match &resolved.attr.kind {
            AttrTypeKind::List {
                element_type,
                ordered: _,
                constraints: _,
            }
            | AttrTypeKind::Map {
                key: _,
                value: element_type,
                constraints: _,
            } => Some(self.rebind(element_type)),
            AttrTypeKind::UnloadedSchemaType(_)
            | AttrTypeKind::Reference {
                identity: _,
                pattern: _,
                length: _,
            }
            | AttrTypeKind::String {
                identity_layer: _,
                member_layer: _,
            }
            | AttrTypeKind::ResourceHandle {
                provider: _,
                resource_type: _,
            }
            | AttrTypeKind::StringLiteralSet { values: _ }
            | AttrTypeKind::Int {
                identity_layer: _,
                member_layer: _,
            }
            | AttrTypeKind::Float {
                binding: _,
                range: _,
            }
            | AttrTypeKind::Bool
            | AttrTypeKind::Duration
            | AttrTypeKind::Enum {
                identity: _,
                base: _,
                values: _,
                dsl_aliases: _,
                validate: _,
                to_dsl: _,
            }
            | AttrTypeKind::Struct {
                name: _,
                fields: _,
                field_groups: _,
            }
            | AttrTypeKind::Union(_)
            | AttrTypeKind::Ref(_) => None,
        }
    }

    fn rebind(self, attr: &'a super::PatternAttributeType<R>) -> Self {
        Self {
            ty: attr.in_schema(self.ty.defs),
            catalog: self.catalog,
            scope: self.scope,
        }
    }

    fn identity_assignment(
        self,
        source: &TypeIdentity,
        leaf: IdentityLeaf<'a>,
    ) -> TypeAssignability<'a> {
        use TypeAssignability::{Assignable, Mismatch, Unchecked};
        let Some(sink) = leaf.identity() else {
            return Mismatch;
        };
        if source.structurally_assignable_to(sink) {
            return Assignable;
        }
        // Enums retain structural identity matching, including at bootstrap.
        if matches!(leaf, IdentityLeaf::Enum(_)) {
            return Mismatch;
        }
        let authority = match (self.scope, leaf) {
            (SinkScope::Authority(authority), _) => authority,
            (
                SinkScope::Annotation,
                IdentityLeaf::Scalar(super::RefinementBinding::Annotation(owned)),
            ) => SinkAuthority::Owners(owned.owners()),
            (SinkScope::Annotation, IdentityLeaf::Unloaded(unloaded)) => {
                return Unchecked(TypeAssignmentDeferral::new(
                    crate::parser::TypeExpr::UnloadedSchemaType(unloaded.clone()),
                ));
            }
            (
                SinkScope::Annotation,
                IdentityLeaf::Scalar(
                    super::RefinementBinding::MetadataOnly(_)
                    | super::RefinementBinding::Validator {
                        identity: _,
                        callback: _,
                    },
                )
                | IdentityLeaf::Enum(_),
            ) => return Mismatch,
        };
        let (reachable, unavailable) = match authority {
            SinkAuthority::Owner(owner) => (
                self.catalog.reachable(owner, source, sink),
                !self.catalog.assignability_edges_available(owner),
            ),
            SinkAuthority::Owners(owners) => (
                self.catalog.reachable_for_all(owners, source, sink),
                owners
                    .iter()
                    .any(|owner| !self.catalog.assignability_edges_available(owner)),
            ),
        };
        if reachable {
            Assignable
        } else if unavailable {
            match crate::parser::TypeExpr::try_from(sink.clone()) {
                Ok(expected) => Unchecked(TypeAssignmentDeferral::new(expected)),
                Err(_) => Mismatch,
            }
        } else {
            Mismatch
        }
    }
}

impl<'a> AssignmentSink<'a, super::Compiled> {
    pub(crate) fn annotation(
        lifted: &'a crate::validation::LiftedType,
        catalog: &'a crate::parser::ProviderCustomTypes,
    ) -> Self {
        Self {
            ty: TypeInSchema::schemaless(lifted),
            catalog,
            scope: SinkScope::Annotation,
        }
    }
}

impl<'a, R: PatternForm> TypeInSchema<'a, R> {
    /// Bind the mandatory edge catalog and sink authority before assignment.
    pub(crate) fn with_authority(
        self,
        catalog: &'a crate::parser::ProviderCustomTypes,
        authority: SinkAuthority<'a>,
    ) -> AssignmentSink<'a, R> {
        AssignmentSink {
            ty: self,
            catalog,
            scope: SinkScope::Authority(authority),
        }
    }
}

/// Structural evidence below a nominal scalar or enum wrapper. This helper
/// compares only length/range containment; identity admission stays exclusively
/// in the catalog-and-authority-bound judgement.
#[derive(Clone, Copy)]
enum NominalScalarConstraints {
    String(Option<(Option<u64>, Option<u64>)>),
    Int(Option<(Option<i64>, Option<i64>)>),
    Float(Option<(Option<crate::FiniteF64>, Option<crate::FiniteF64>)>),
}

impl NominalScalarConstraints {
    fn provably_disjoint(self, sink: Self) -> bool {
        fn disjoint<T: Copy + Ord>(
            source: Option<(Option<T>, Option<T>)>,
            sink: Option<(Option<T>, Option<T>)>,
        ) -> bool {
            super::bounds_are_empty(super::intersect_bounds(source, sink))
        }
        match (self, sink) {
            (Self::String(source), Self::String(sink)) => disjoint(source, sink),
            (Self::Int(source), Self::Int(sink)) => disjoint(source, sink),
            (Self::Float(source), Self::Float(sink)) => disjoint(source, sink),
            (Self::Int(source), Self::Float(sink)) => {
                let (source_min, source_max) = source.unwrap_or((None, None));
                let (sink_min, sink_max) = sink.unwrap_or((None, None));
                sink_min.is_some_and(|min| {
                    super::compare_int_float(source_max.unwrap_or(i64::MAX), min).is_lt()
                }) || sink_max.is_some_and(|max| {
                    super::compare_int_float(source_min.unwrap_or(i64::MIN), max).is_gt()
                })
            }
            (Self::String(_), Self::Int(_) | Self::Float(_))
            | (Self::Int(_), Self::String(_))
            | (Self::Float(_), Self::String(_) | Self::Int(_)) => true,
        }
    }

    fn contained_by(self, sink: Self) -> bool {
        match (self, sink) {
            (Self::String(source), Self::String(sink)) => {
                super::length_contains(source.as_ref(), sink.as_ref())
            }
            (Self::Int(source), Self::Int(sink)) => {
                super::i64_range_contains(source.as_ref(), sink.as_ref())
            }
            (Self::Float(source), Self::Float(sink)) => {
                super::f64_range_contains(source.as_ref(), sink.as_ref())
            }
            (Self::Int(source), Self::Float(sink)) => {
                super::i64_range_assignable_to_f64(source.as_ref(), sink.as_ref())
            }
            (Self::String(_), Self::Int(_) | Self::Float(_))
            | (Self::Int(_), Self::String(_))
            | (Self::Float(_), Self::String(_) | Self::Int(_)) => false,
        }
    }
}

fn identity_scalar_constraints<'a, R: PatternForm>(
    ty: TypeInSchema<'a, R>,
    refs: &mut Vec<&'a str>,
) -> Option<NominalScalarConstraints> {
    let resolved = resolve_refs_on_path(ty.attr, ty.defs, refs).ok()?;
    let constraints = match &resolved.attr.kind {
        AttrTypeKind::String {
            identity_layer,
            member_layer: _,
        } => Some(NominalScalarConstraints::String(identity_layer.length)),
        AttrTypeKind::Int {
            identity_layer,
            member_layer: _,
        } => Some(NominalScalarConstraints::Int(identity_layer.range)),
        AttrTypeKind::Float { binding: _, range } => Some(NominalScalarConstraints::Float(*range)),
        AttrTypeKind::Enum {
            identity: _,
            base,
            values: _,
            dsl_aliases: _,
            validate: _,
            to_dsl: _,
        } => identity_scalar_constraints(base.in_schema(ty.defs), refs),
        AttrTypeKind::UnloadedSchemaType(_)
        | AttrTypeKind::Reference {
            identity: _,
            pattern: _,
            length: _,
        }
        | AttrTypeKind::ResourceHandle {
            provider: _,
            resource_type: _,
        }
        | AttrTypeKind::StringLiteralSet { values: _ }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::List {
            element_type: _,
            ordered: _,
            constraints: _,
        }
        | AttrTypeKind::Map {
            key: _,
            value: _,
            constraints: _,
        }
        | AttrTypeKind::Struct {
            name: _,
            fields: _,
            field_groups: _,
        }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => None,
    };
    refs.truncate(resolved.restore_len);
    constraints
}

fn sink_scalar_constraints<'a, R: PatternForm>(
    ty: TypeInSchema<'a, R>,
    refs: &mut Vec<&'a str>,
) -> Option<NominalScalarConstraints> {
    let resolved = resolve_refs_on_path(ty.attr, ty.defs, refs).ok()?;
    let constraints = match &resolved.attr.kind {
        AttrTypeKind::String {
            identity_layer,
            member_layer,
        } => Some(NominalScalarConstraints::String(super::intersect_bounds(
            identity_layer.length,
            member_layer.length,
        ))),
        AttrTypeKind::Int {
            identity_layer,
            member_layer,
        } => Some(NominalScalarConstraints::Int(super::intersect_bounds(
            identity_layer.range,
            member_layer.range,
        ))),
        AttrTypeKind::Float { binding: _, range } => Some(NominalScalarConstraints::Float(*range)),
        AttrTypeKind::Enum {
            identity: _,
            base,
            values: _,
            dsl_aliases: _,
            validate: _,
            to_dsl: _,
        } => sink_scalar_constraints(base.in_schema(ty.defs), refs),
        AttrTypeKind::UnloadedSchemaType(_)
        | AttrTypeKind::Reference {
            identity: _,
            pattern: _,
            length: _,
        }
        | AttrTypeKind::ResourceHandle {
            provider: _,
            resource_type: _,
        }
        | AttrTypeKind::StringLiteralSet { values: _ }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::List {
            element_type: _,
            ordered: _,
            constraints: _,
        }
        | AttrTypeKind::Map {
            key: _,
            value: _,
            constraints: _,
        }
        | AttrTypeKind::Struct {
            name: _,
            fields: _,
            field_groups: _,
        }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => None,
    };
    refs.truncate(resolved.restore_len);
    constraints
}

fn effective_source_constraints<R: PatternForm>(
    source: TypeInSchema<'_, R>,
    identity: Option<NominalScalarConstraints>,
) -> Option<NominalScalarConstraints> {
    match &source.attr.kind {
        AttrTypeKind::String {
            identity_layer,
            member_layer,
        } if identity_layer.binding.identity().is_none() => Some(NominalScalarConstraints::String(
            super::intersect_bounds(identity_layer.length, member_layer.length),
        )),
        AttrTypeKind::Int {
            identity_layer,
            member_layer,
        } if identity_layer.binding.identity().is_none() => Some(NominalScalarConstraints::Int(
            super::intersect_bounds(identity_layer.range, member_layer.range),
        )),
        _ => identity,
    }
}

fn member_constraints_are_contained<R: PatternForm>(
    source: TypeInSchema<'_, R>,
    sink: TypeInSchema<'_, R>,
    source_bounds: Option<NominalScalarConstraints>,
) -> bool {
    let source_bounds = effective_source_constraints(source, source_bounds);
    let mut source_patterns = source.attr.identity_patterns();
    if let AttrTypeKind::String {
        identity_layer,
        member_layer,
    } = &source.attr.kind
        && identity_layer.binding.identity().is_none()
    {
        source_patterns.extend(member_layer.pattern.as_ref().map(AsRef::as_ref));
    }
    match &sink.attr.kind {
        AttrTypeKind::String {
            identity_layer: _,
            member_layer: super::StringMemberConstraints { pattern, length },
        } => {
            let pattern_fits = match pattern {
                None => true,
                Some(pattern) => source_patterns
                    .into_iter()
                    .any(|source| super::pattern_compatible(Some(source), Some(pattern.as_ref()))),
            };
            pattern_fits
                && match source_bounds {
                    Some(NominalScalarConstraints::String(source)) => {
                        super::length_contains(source.as_ref(), length.as_ref())
                    }
                    Some(NominalScalarConstraints::Int(_) | NominalScalarConstraints::Float(_))
                    | None => pattern.is_none() && length.is_none(),
                }
        }
        AttrTypeKind::Int {
            identity_layer: _,
            member_layer: super::IntMemberConstraints { range },
        } => match source_bounds {
            Some(NominalScalarConstraints::Int(source)) => {
                super::i64_range_contains(source.as_ref(), range.as_ref())
            }
            Some(NominalScalarConstraints::String(_) | NominalScalarConstraints::Float(_))
            | None => range.is_none(),
        },
        _ => true,
    }
}

/// An Enum value may still be written to a scalar destination slot. Enum
/// bases themselves cannot have member constraints after schema admission.
fn scalar_member_obligation<'result, R: PatternForm, C: AssignmentCheck>(
    source: Option<NominalScalarConstraints>,
    source_type: TypeInSchema<'_, R>,
    sink: TypeInSchema<'_, R>,
    check: C,
) -> TypeAssignability<'result> {
    let member_bounds = match &sink.attr.kind {
        AttrTypeKind::String {
            identity_layer,
            member_layer: super::StringMemberConstraints { pattern, length },
        } if pattern.is_some() || length.is_some() => Some(NominalScalarConstraints::String(
            super::intersect_bounds(identity_layer.length, *length),
        )),
        AttrTypeKind::Int {
            identity_layer,
            member_layer: super::IntMemberConstraints { range },
        } if range.is_some() => Some(NominalScalarConstraints::Int(super::intersect_bounds(
            identity_layer.range,
            *range,
        ))),
        _ => None,
    };
    match member_bounds {
        Some(target) if source.is_some_and(|source| source.provably_disjoint(target)) => {
            TypeAssignability::Mismatch
        }
        Some(_) if member_constraints_are_contained(source_type, sink, source) => {
            TypeAssignability::Assignable
        }
        Some(_) => destination_obligation(check, true, sink),
        None => TypeAssignability::Assignable,
    }
}

/// Reference and declared assignment preserve the same obligation-bearing result.
pub type AssignmentRelation<'s> = TypeAssignability<'s>;

impl<'s> TypeAssignability<'s> {
    fn from_bool(value: bool) -> Self {
        if value {
            Self::Assignable
        } else {
            Self::Mismatch
        }
    }
    pub(crate) fn all(results: impl IntoIterator<Item = Self>) -> Self {
        let mut schema: Option<TypeAssignmentDeferral<'s>> = None;
        let mut local: Option<super::DestinationObligation> = None;
        for result in results {
            let next = match result {
                Self::Mismatch => return Self::Mismatch,
                Self::Assignable => None,
                Self::Unchecked(schema) => Some(schema),
                Self::NeedsLocal(LocalAssignmentObligation {
                    destination,
                    schema,
                }) => {
                    local = Some(match local {
                        Some(previous) => previous.merge(destination),
                        None => destination,
                    });
                    schema
                }
            };
            if let Some(next) = next {
                schema = Some(match schema {
                    Some(previous) => previous.merge(next),
                    None => next,
                });
            }
        }
        match local {
            Some(destination) => Self::NeedsLocal(LocalAssignmentObligation {
                destination,
                schema,
            }),
            None => match schema {
                Some(schema) => Self::Unchecked(schema),
                None => Self::Assignable,
            },
        }
    }
    fn any(results: impl IntoIterator<Item = Self>) -> Self {
        let mut pending: Option<LocalAssignmentObligation<'s>> = None;
        let mut missing: Option<TypeAssignmentDeferral<'s>> = None;
        for result in results {
            match result {
                Self::Assignable => return Self::Assignable,
                Self::NeedsLocal(local) => {
                    pending = Some(match pending {
                        None => local,
                        Some(previous) => LocalAssignmentObligation {
                            destination: previous.destination.alternative(local.destination),
                            schema: match (previous.schema, local.schema) {
                                (Some(a), Some(b)) => Some(a.merge(b)),
                                (Some(a), None) | (None, Some(a)) => Some(a),
                                (None, None) => None,
                            },
                        },
                    });
                }
                Self::Unchecked(next) => {
                    missing = Some(match missing {
                        Some(previous) => previous.merge(next),
                        None => next,
                    })
                }
                Self::Mismatch => {}
            }
        }
        match (pending, missing) {
            (Some(mut local), missing) => {
                if let Some(missing) = missing {
                    local.schema = Some(match local.schema {
                        Some(previous) => previous.merge(missing),
                        None => missing,
                    });
                }
                Self::NeedsLocal(local)
            }
            (None, Some(missing)) => Self::Unchecked(missing),
            (None, None) => Self::Mismatch,
        }
    }
}

/// Modes choose identity proof policy, never whether to retain member checks.
trait AssignmentCheck: Copy {
    fn identified_only(self) -> bool;
    fn requires_identity_containment(self) -> bool;
    fn defers_identity_value(self) -> bool;
}

fn destination_obligation<'s, R: PatternForm, C: AssignmentCheck>(
    check: C,
    member_unproven: bool,
    destination: TypeInSchema<'_, R>,
) -> TypeAssignability<'s> {
    if member_unproven || check.defers_identity_value() {
        TypeAssignability::NeedsLocal(LocalAssignmentObligation::new(destination))
    } else {
        TypeAssignability::Assignable
    }
}

#[derive(Clone, Copy)]
enum DeclaredAssignmentCheck {
    Full,
    /// Re-admit original identities at a declared value's later sink. Only
    /// identified leaves need this proof; their destination value checks remain.
    IdentifiedLeaves,
}

impl AssignmentCheck for DeclaredAssignmentCheck {
    fn identified_only(self) -> bool {
        matches!(self, Self::IdentifiedLeaves)
    }
    fn requires_identity_containment(self) -> bool {
        true
    }
    fn defers_identity_value(self) -> bool {
        false
    }
}

#[derive(Clone, Copy)]
enum ReferenceAssignmentCheck {
    Full,
    IdentifiedLeaves,
}

impl AssignmentCheck for ReferenceAssignmentCheck {
    fn identified_only(self) -> bool {
        matches!(self, Self::IdentifiedLeaves)
    }
    fn requires_identity_containment(self) -> bool {
        false
    }
    fn defers_identity_value(self) -> bool {
        true
    }
}

impl TypeInSchema<'_, super::Compiled> {
    /// Judge a declaration and its original sources together at the same sink.
    /// The originals' identity-less leaves may retain deferred value checks
    /// only after the declaration has passed full assignment at this sink.
    pub(crate) fn declared_assignment_to<B: crate::resource::BindingAddress>(
        self,
        original: &ResolvedRefType,
        incoming: AssignmentSink<'_, super::Compiled>,
        sink: AssignmentSink<'_, super::Compiled>,
        bindings: &BindingIndex<'_, B>,
    ) -> DeclaredValueAssignment {
        use super::{AdmittedTypeAssignment, LoadedTypeAssignment};
        let declaration_local = match bindings.check_type_assignment(self, sink) {
            Err(error) => {
                return DeclaredValueAssignment::Rejected(RefTypeAssignabilityError::Unchecked(
                    Box::new(error),
                ));
            }
            Ok(LoadedTypeAssignment::Mismatch) => {
                return DeclaredValueAssignment::Rejected(RefTypeAssignabilityError::Mismatch(
                    RefTypeMismatch::direct(super::MismatchTypeNames::for_types(*sink, self)),
                ));
            }
            Ok(LoadedTypeAssignment::Assignable) => None,
            Ok(LoadedTypeAssignment::NeedsLocal(obligation)) => Some(obligation),
        };
        match original.check_assignment(*sink, |source| {
            bindings.consume_type_assignment(source.identity_assignment_to(sink))
        }) {
            Ok(AdmittedTypeAssignment::Assignable) => match declaration_local {
                Some(obligation) => DeclaredValueAssignment::NeedsLocal(obligation),
                None => DeclaredValueAssignment::Assignable,
            },
            Ok(AdmittedTypeAssignment::NeedsLocal(obligation)) => {
                DeclaredValueAssignment::NeedsLocal(match declaration_local {
                    Some(previous) => previous.merge(obligation),
                    None => obligation,
                })
            }
            Err(error @ RefTypeAssignabilityError::Mismatch(_)) => {
                // A pending incoming check is not a proven incoming rejection.
                // The already-proven original mismatch owns this diagnostic.
                match original.assignment_at(incoming) {
                    TypeAssignability::Mismatch => {
                        DeclaredValueAssignment::IncomingBoundaryMismatch
                    }
                    TypeAssignability::Assignable => DeclaredValueAssignment::Rejected(error),
                    TypeAssignability::Unchecked(missing) => {
                        drop(missing);
                        DeclaredValueAssignment::Rejected(error)
                    }
                    TypeAssignability::NeedsLocal(obligation) => {
                        drop(obligation);
                        DeclaredValueAssignment::Rejected(error)
                    }
                }
            }
            Err(error @ RefTypeAssignabilityError::Unchecked(_)) => {
                DeclaredValueAssignment::Rejected(error)
            }
        }
    }
}

impl<'source, R: PatternForm> TypeInSchema<'source, R> {
    /// Prove assignment, prove a mismatch, or defer for the existing
    /// `ProvidersNotLoaded` reason. Identified types first compare structural
    /// identities; a mismatch also needs the sink authority's edge declaration.
    /// Unloaded catalogs and failed owners in partial catalogs defer those
    /// identity mismatches because their edges are unknown. Comparisons needing
    /// an unavailable scalar kind are also unchecked. A source without identity
    /// cannot satisfy a nominal sink.
    pub fn assignment_to<'query>(self, sink: AssignmentSink<'query, R>) -> TypeAssignability<'query>
    where
        'source: 'query,
    {
        let source: TypeInSchema<'query, R> = self;
        source.assignment_on_paths(
            sink,
            DeclaredAssignmentCheck::Full,
            &mut Vec::new(),
            &mut Vec::new(),
        )
    }

    /// Recheck original identity evidence after a value declaration has passed
    /// full assignment. An earlier owner's edge is not proof for this sink.
    /// Identity-less leaves keep the declaration's deferred value checks;
    /// identified leaves retain the same identity admission and value obligations.
    pub(crate) fn identity_assignment_to<'query>(
        self,
        sink: AssignmentSink<'query, R>,
    ) -> TypeAssignability<'query>
    where
        'source: 'query,
    {
        let source: TypeInSchema<'query, R> = self;
        source.assignment_on_paths(
            sink,
            DeclaredAssignmentCheck::IdentifiedLeaves,
            &mut Vec::new(),
            &mut Vec::new(),
        )
    }

    pub(crate) fn reference_assignment_to<'query, B: crate::resource::BindingAddress>(
        self,
        sink: AssignmentSink<'query, R>,
        _reference: &crate::validation::admission::ReferenceCandidate<B>,
    ) -> AssignmentRelation<'query>
    where
        'source: 'query,
    {
        let source: TypeInSchema<'query, R> = self;
        source.assignment_on_paths(
            sink,
            ReferenceAssignmentCheck::Full,
            &mut Vec::new(),
            &mut Vec::new(),
        )
    }

    pub(crate) fn identified_reference_assignment_to<'query, B: crate::resource::BindingAddress>(
        self,
        sink: AssignmentSink<'query, R>,
        _reference: &crate::validation::admission::ReferenceCandidate<B>,
    ) -> AssignmentRelation<'query>
    where
        'source: 'query,
    {
        let source: TypeInSchema<'query, R> = self;
        source.assignment_on_paths(
            sink,
            ReferenceAssignmentCheck::IdentifiedLeaves,
            &mut Vec::new(),
            &mut Vec::new(),
        )
    }

    fn assignment_on_paths<'sink, C: AssignmentCheck>(
        self,
        sink: AssignmentSink<'sink, R>,
        check: C,
        source_refs: &mut Vec<&'source str>,
        sink_refs: &mut Vec<&'sink str>,
    ) -> TypeAssignability<'source>
    where
        'sink: 'source,
    {
        let Ok(source) = resolve_refs_on_path(self.attr, self.defs, source_refs) else {
            return TypeAssignability::Mismatch;
        };
        let Ok(target) = resolve_refs_on_path(sink.attr, sink.defs, sink_refs) else {
            source_refs.truncate(source.restore_len);
            return TypeAssignability::Mismatch;
        };
        let left = source.attr.in_schema(self.defs);
        let right = sink.rebind(target.attr);
        let source_constraints = identity_scalar_constraints(left, source_refs);
        let target_constraints = identity_scalar_constraints(right.ty, sink_refs);
        use super::assignment_obligation::DestinationPosition;
        use AttrTypeKind::*;
        use TypeAssignability::*;
        // Same identity is the zero-edge case of nominal admission. Reference
        // destinations retain their existing resolved-value obligation for any
        // admitted identity. Same-identity value admission defers overlapping
        // bounds; different-identity widening retains the containment proof.
        // Strict declaration compatibility preserves main's containment rules.
        let scalar = matches!(&source.attr.kind, String { .. } | Int { .. })
            && matches!(&target.attr.kind, String { .. } | Int { .. } | Float { .. });
        let same_identity = identity_leaf(&source.attr.kind)
            .and_then(|leaf| leaf.identity())
            .zip(identity_leaf(&target.attr.kind).and_then(|leaf| leaf.identity()))
            .is_some_and(|(source, target)| source == target);
        let member_unproven = !member_constraints_are_contained(left, right.ty, source_constraints);
        if let Some(source_identity) =
            identity_leaf(&source.attr.kind).and_then(|leaf| leaf.identity())
            && let Some(target_identity) = identity_leaf(&target.attr.kind)
            && (scalar || (same_identity && check.defers_identity_value()))
        {
            if !scalar {
                // Keep main's same-identity reference deferral for Float,
                // Enum and unloaded leaves. Strict declaration checks still
                // use their existing kind/base/range rules below.
                let empty = source_constraints
                    .is_some_and(|bounds| bounds.provably_disjoint(bounds))
                    || target.attr.has_empty_scalar_intersection();
                let result = if empty {
                    Mismatch
                } else {
                    destination_obligation(check, member_unproven, right.ty)
                };
                source_refs.truncate(source.restore_len);
                sink_refs.truncate(target.restore_len);
                return result;
            }
            let identity_fits = source_constraints
                .zip(target_constraints)
                .is_some_and(|(source, target)| source.contained_by(target));
            let disjoint = target.attr.has_empty_scalar_intersection()
                || source_constraints
                    .zip(sink_scalar_constraints(right.ty, sink_refs))
                    .is_some_and(|(source, target)| source.provably_disjoint(target));
            let result = TypeAssignability::all(
                std::iter::once(right.identity_assignment(source_identity, target_identity)).chain(
                    std::iter::once_with(|| {
                        if disjoint
                            || ((!same_identity || check.requires_identity_containment())
                                && !identity_fits)
                        {
                            Mismatch
                        } else {
                            destination_obligation(check, member_unproven, right.ty)
                        }
                    }),
                ),
            );
            source_refs.truncate(source.restore_len);
            sink_refs.truncate(target.restore_len);
            return result;
        }
        let result = match (&source.attr.kind, &target.attr.kind) {
            (_, _)
                if source_constraints.is_some_and(|bounds| bounds.provably_disjoint(bounds))
                    || target.attr.has_empty_scalar_intersection() =>
            {
                Mismatch
            }
            (_, kind)
                if check.identified_only()
                    && identity_leaf(kind).is_none()
                    && !matches!(kind, List { .. } | Map { .. } | Struct { .. } | Union(_)) =>
            {
                Assignable
            }
            (
                Union(members),
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                }
                | Map {
                    key: _,
                    value: _,
                    constraints: _,
                }
                | Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                }
                | UnloadedSchemaType(_)
                | Bool
                | Duration
                | Union(_)
                | Ref(_),
            ) => TypeAssignability::all(members.as_raw_slice().iter().map(|member| {
                member.in_schema(self.defs).assignment_on_paths(
                    right,
                    check,
                    source_refs,
                    sink_refs,
                )
            })),
            (
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                }
                | Map {
                    key: _,
                    value: _,
                    constraints: _,
                }
                | Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                }
                | UnloadedSchemaType(_)
                | Bool
                | Duration
                | Ref(_),
                Union(members),
            ) => TypeAssignability::any(members.as_raw_slice().iter().map(|member| {
                left.assignment_on_paths(sink.rebind(member), check, source_refs, sink_refs)
            })),
            (
                UnloadedSchemaType(unloaded),
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                }
                | Map {
                    key: _,
                    value: _,
                    constraints: _,
                }
                | Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                }
                | UnloadedSchemaType(_)
                | Bool
                | Duration
                | Ref(_),
            )
            | (
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                }
                | Map {
                    key: _,
                    value: _,
                    constraints: _,
                }
                | Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                }
                | Bool
                | Duration
                | Ref(_),
                UnloadedSchemaType(unloaded),
            ) => match (
                identity_leaf(&source.attr.kind).and_then(IdentityLeaf::identity),
                identity_leaf(&target.attr.kind),
            ) {
                (Some(left), Some(right)) => {
                    if matches!(source.attr.kind, Enum { .. }) {
                        TypeAssignability::from_bool(
                            right
                                .identity()
                                .is_some_and(|sink| left.structurally_assignable_to(sink)),
                        )
                    } else {
                        sink.rebind(target.attr).identity_assignment(left, right)
                    }
                }
                (None, Some(_)) => Mismatch,
                (Some(_) | None, None) => Unchecked(TypeAssignmentDeferral::new(
                    crate::parser::TypeExpr::UnloadedSchemaType(unloaded.clone()),
                )),
            },
            (
                List {
                    element_type: left,
                    ordered: _,
                    constraints: _,
                },
                List {
                    element_type: right,
                    ordered: _,
                    constraints: _,
                },
            ) => left
                .in_schema(self.defs)
                .assignment_on_paths(sink.rebind(right), check, source_refs, sink_refs)
                .within_destination(
                    target.attr.in_schema(sink.defs),
                    DestinationPosition::ListElement,
                ),
            (
                Map {
                    key: lk,
                    value: lv,
                    constraints: _,
                },
                Map {
                    key: rk,
                    value: rv,
                    constraints: _,
                },
            ) => TypeAssignability::all([
                lk.in_schema(self.defs)
                    .assignment_on_paths(sink.rebind(rk), check, source_refs, sink_refs)
                    .within_destination(
                        target.attr.in_schema(sink.defs),
                        DestinationPosition::MapKey,
                    ),
                lv.in_schema(self.defs)
                    .assignment_on_paths(sink.rebind(rv), check, source_refs, sink_refs)
                    .within_destination(
                        target.attr.in_schema(sink.defs),
                        DestinationPosition::MapValue,
                    ),
            ]),
            (
                Struct {
                    fields: left,
                    name: _,
                    field_groups: _,
                },
                Struct {
                    fields: right,
                    name: _,
                    field_groups: _,
                },
            ) => {
                if left.len() != right.len() {
                    Mismatch
                } else {
                    TypeAssignability::all(right.iter().map(|field| {
                        match left.iter().find(|candidate| candidate.name == field.name) {
                            Some(candidate) => candidate
                                .field_type
                                .in_schema(self.defs)
                                .assignment_on_paths(
                                    sink.rebind(&field.field_type),
                                    check,
                                    source_refs,
                                    sink_refs,
                                )
                                .within_destination(
                                    target.attr.in_schema(sink.defs),
                                    DestinationPosition::Field(&field.name),
                                ),
                            None => Mismatch,
                        }
                    }))
                }
            }
            (
                Struct {
                    fields,
                    name: _,
                    field_groups: _,
                },
                Map {
                    value,
                    key: _,
                    constraints: _,
                },
            ) => {
                // A structural source supplies fixed map keys. Preserve each
                // field's admitted union branch at its own value position.
                let scope = super::PatternAttributeType::struct_(
                    "assignment",
                    fields
                        .iter()
                        .map(|field| {
                            super::PatternStructField::optional(&field.name, (**value).clone())
                                .build()
                        })
                        .collect(),
                );
                TypeAssignability::all(fields.iter().map(|field| {
                    field
                        .field_type
                        .in_schema(self.defs)
                        .assignment_on_paths(sink.rebind(value), check, source_refs, sink_refs)
                        .within_destination(
                            scope.in_schema(sink.defs),
                            DestinationPosition::Field(&field.name),
                        )
                }))
            }
            (
                Float {
                    binding: source,
                    range: source_range,
                },
                Float {
                    binding: target,
                    range: sink_range,
                },
            ) if let (Some(source), Some(_)) = (source.identity(), target.identity()) => {
                TypeAssignability::all([
                    right.identity_assignment(source, IdentityLeaf::Scalar(target)),
                    TypeAssignability::from_bool(super::f64_range_contains(
                        source_range.as_ref(),
                        sink_range.as_ref(),
                    )),
                ])
            }
            // Identified Int values were admitted above using their identity
            // bounds. An anonymous Int cannot prove a named Float identity.
            (
                Int {
                    identity_layer: _,
                    member_layer: _,
                },
                Float {
                    binding: target,
                    range: _,
                },
            ) if target.identity().is_some() => Mismatch,
            (
                String {
                    identity_layer:
                        crate::schema::StringIdentityLayer {
                            binding: source, ..
                        },
                    member_layer: _,
                }
                | Int {
                    identity_layer:
                        crate::schema::IntIdentityLayer {
                            binding: source, ..
                        },
                    member_layer: _,
                }
                | Float {
                    binding: source,
                    range: _,
                },
                Enum {
                    identity: target,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                },
            ) if let Some(source) = source.identity() => TypeAssignability::all([
                TypeAssignability::from_bool(source.structurally_assignable_to(target)),
                TypeAssignability::from_bool(match (source_constraints, target_constraints) {
                    (Some(source), Some(sink)) => source.contained_by(sink),
                    (None, Some(_) | None) | (Some(_), None) => false,
                }),
            ]),
            (
                Enum {
                    identity: source,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                },
                String {
                    identity_layer:
                        crate::schema::StringIdentityLayer {
                            binding: target, ..
                        },
                    member_layer: _,
                }
                | Int {
                    identity_layer:
                        crate::schema::IntIdentityLayer {
                            binding: target, ..
                        },
                    member_layer: _,
                }
                | Float {
                    binding: target,
                    range: _,
                },
            ) if target.identity().is_some() => TypeAssignability::all([
                TypeAssignability::from_bool(
                    target
                        .identity()
                        .is_some_and(|target| source.structurally_assignable_to(target)),
                ),
                TypeAssignability::from_bool(match (source_constraints, target_constraints) {
                    (Some(source), Some(sink)) => source.contained_by(sink),
                    (None, Some(_) | None) | (Some(_), None) => false,
                }),
                scalar_member_obligation(source_constraints, left, right.ty, check),
            ]),
            (
                Enum {
                    identity: source,
                    base: source_base,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                },
                Enum {
                    identity: target,
                    base: sink_base,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                },
            ) => TypeAssignability::all([
                TypeAssignability::from_bool(source.structurally_assignable_to(target)),
                source_base.in_schema(self.defs).assignment_on_paths(
                    sink.rebind(sink_base),
                    check,
                    source_refs,
                    sink_refs,
                ),
            ]),
            (
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | Bool
                | Duration
                | Ref(_),
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                }
                | Map {
                    key: _,
                    value: _,
                    constraints: _,
                }
                | Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                }
                | Bool
                | Duration
                | Ref(_),
            ) => TypeAssignability::from_bool(left.is_assignable_to_known(right)),
            (
                Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                },
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                }
                | Map {
                    key: _,
                    value: _,
                    constraints: _,
                }
                | Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                }
                | Bool
                | Duration
                | Ref(_),
            ) => TypeAssignability::from_bool(left.is_assignable_to_known(right)),
            (
                List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                },
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | Map {
                    key: _,
                    value: _,
                    constraints: _,
                }
                | Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                }
                | Bool
                | Duration
                | Ref(_),
            ) => TypeAssignability::from_bool(left.is_assignable_to_known(right)),
            (
                Map {
                    key: _,
                    value: _,
                    constraints: _,
                },
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                }
                | Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                }
                | Bool
                | Duration
                | Ref(_),
            ) => TypeAssignability::from_bool(left.is_assignable_to_known(right)),
            (
                Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                },
                String {
                    identity_layer: _,
                    member_layer: _,
                }
                | Int {
                    identity_layer: _,
                    member_layer: _,
                }
                | Float {
                    binding: _,
                    range: _,
                }
                | Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | ResourceHandle {
                    provider: _,
                    resource_type: _,
                }
                | StringLiteralSet { values: _ }
                | List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                }
                | Bool
                | Duration
                | Ref(_),
            ) => TypeAssignability::from_bool(left.is_assignable_to_known(right)),
        };
        let result = if member_unproven && matches!(&target.attr.kind, String { .. } | Int { .. }) {
            let source_bounds = effective_source_constraints(left, source_constraints);
            let disjoint = source_bounds
                .zip(sink_scalar_constraints(right.ty, sink_refs))
                .is_some_and(|(source, target)| source.provably_disjoint(target));
            TypeAssignability::all(std::iter::once(result).chain(std::iter::once_with(|| {
                if disjoint {
                    Mismatch
                } else {
                    destination_obligation(check, true, right.ty)
                }
            })))
        } else {
            result
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
    fn enum_ref_base_probes_preserve_the_path_stacks() {
        use crate::schema::AttributeType;
        let enumeration = AttributeType::enum_with_base(
            TypeIdentity::bare("Mode"),
            AttributeType::ref_("Base"),
            Some(vec!["a".into()]),
            Vec::new(),
            None,
            None,
        );
        let defs = std::collections::BTreeMap::from([
            (
                "Base".into(),
                AttributeType::refined_string(None, None, Some((Some(1), Some(8))), None),
            ),
            ("Mode".into(), enumeration.clone()),
        ]);
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        for source in [&enumeration, &AttributeType::ref_("Mode")] {
            let sink = enumeration
                .in_schema(&defs)
                .with_authority(&catalog, SinkAuthority::Owner(&owner));
            let mut source_refs = vec!["Outer"];
            let mut sink_refs = vec!["Outer"];
            assert_eq!(
                source.in_schema(&defs).assignment_on_paths(
                    sink,
                    DeclaredAssignmentCheck::Full,
                    &mut source_refs,
                    &mut sink_refs,
                ),
                TypeAssignability::Assignable
            );
            assert_eq!(source_refs, ["Outer"]);
            assert_eq!(sink_refs, ["Outer"]);
        }
    }

    #[test]
    fn nominal_float_sinks_preserve_main_assignment_modes() {
        use crate::schema::AttributeType;
        let float = |max| {
            AttributeType::refined_float(
                Some(TypeIdentity::bare("Count")),
                Some((Some(crate::finite(0.0)), Some(crate::finite(max)))),
            )
        };
        let source = float(100.0);
        let target = float(10.0);
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let sink = TypeInSchema::schemaless(&target)
            .with_authority(&catalog, SinkAuthority::Owner(&owner));
        assert_eq!(
            TypeInSchema::schemaless(&source).assignment_to(sink),
            TypeAssignability::Mismatch
        );
        assert!(matches!(
            TypeInSchema::schemaless(&source).assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            AssignmentRelation::NeedsLocal(ref obligation) if obligation.missing_schema().is_none()
        ));
    }

    #[test]
    fn enum_identity_readmission_preserves_the_assignment_check_mode() {
        use crate::schema::AttributeType;

        let enum_type = |base| {
            AttributeType::enum_with_base(
                TypeIdentity::bare("Mode"),
                base,
                None,
                Vec::new(),
                None,
                None,
            )
        };
        let original = enum_type(AttributeType::refined_string(
            None,
            None,
            Some((Some(1), Some(16))),
            None,
        ));
        let declaration = enum_type(AttributeType::refined_string(
            None,
            None,
            Some((Some(1), Some(8))),
            None,
        ));
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let sink = TypeInSchema::schemaless(&declaration)
            .with_authority(&catalog, SinkAuthority::Owner(&owner));

        assert_eq!(
            TypeInSchema::schemaless(&original).assignment_to(sink),
            TypeAssignability::Mismatch
        );
        assert_eq!(
            TypeInSchema::schemaless(&declaration).assignment_to(sink),
            TypeAssignability::Assignable
        );
        assert_eq!(
            TypeInSchema::schemaless(&original).identity_assignment_to(sink),
            TypeAssignability::Assignable,
            "identity-less enum bases retain the declaration's deferred value checks"
        );
    }

    #[test]
    fn source_members_do_not_prove_disjointness_at_a_destination() {
        use crate::schema::{
            AttributeType, IntIdentityLayer, IntMemberConstraints, StringIdentityLayer,
            StringMemberConstraints,
        };
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let string = |min, max| {
            AttributeType::string_member(
                StringIdentityLayer::new(
                    Some(TypeIdentity::from_dotted("test.Name")),
                    None,
                    None,
                    None,
                ),
                StringMemberConstraints::new(None, Some((Some(min), Some(max)))),
            )
        };
        let int = |min, max| {
            AttributeType::int_member(
                IntIdentityLayer::new(Some(TypeIdentity::from_dotted("test.Count")), None),
                IntMemberConstraints::new(Some((Some(min), Some(max)))),
            )
        };
        for (source, destination) in [(string(20, 30), string(1, 10)), (int(20, 30), int(1, 10))] {
            let sink = TypeInSchema::schemaless(&destination)
                .with_authority(&catalog, SinkAuthority::Owner(&owner));
            assert!(
                matches!(
                    TypeInSchema::schemaless(&source).assignment_on_paths(
                        sink,
                        ReferenceAssignmentCheck::Full,
                        &mut Vec::new(),
                        &mut Vec::new(),
                    ),
                    AssignmentRelation::NeedsLocal(_)
                ),
                "source member bounds describe its slot, not the read value"
            );
        }
    }

    #[test]
    fn nominal_int_to_float_admission_uses_identity_bound_containment() {
        use crate::schema::{AttributeType, IntIdentityLayer, IntMemberConstraints};
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let sink = AttributeType::refined_float(
            Some(TypeIdentity::from_dotted("aws.Count")),
            Some((
                Some(crate::FiniteF64::new(0.0).unwrap()),
                Some(crate::FiniteF64::new(100.0).unwrap()),
            )),
        );
        let sink =
            TypeInSchema::schemaless(&sink).with_authority(&catalog, SinkAuthority::Owner(&owner));
        for (range, expected) in [
            (None, true),
            (Some((Some(5), Some(10))), true),
            (Some((Some(20), Some(30))), true),
        ] {
            let source = AttributeType::int_member(
                IntIdentityLayer::new(
                    Some(TypeIdentity::from_dotted("aws.service.Counter.Count")),
                    Some((Some(0), Some(100))),
                ),
                IntMemberConstraints::new(range),
            );
            assert_eq!(
                TypeInSchema::schemaless(&source)
                    .assignment_to(sink)
                    .require_loaded()
                    .map(|assignment| assignment
                        .require_proven()
                        .expect("test expects a static assignment proof"))
                    .unwrap(),
                expected,
                "member range {range:?}"
            );
            let reference = TypeInSchema::schemaless(&source).assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new(),
            );
            assert_eq!(
                matches!(reference, AssignmentRelation::NeedsLocal(_)),
                expected
            );
            if !expected {
                assert!(matches!(reference, AssignmentRelation::Mismatch));
            }
        }
        let disjoint = AttributeType::refined_int(
            Some(TypeIdentity::from_dotted("aws.service.Counter.Count")),
            Some((Some(200), Some(300))),
        );
        assert!(matches!(
            TypeInSchema::schemaless(&disjoint).assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            AssignmentRelation::Mismatch
        ));
        let overlapping = AttributeType::refined_int(
            Some(TypeIdentity::from_dotted("aws.service.Counter.Count")),
            Some((Some(0), Some(2048))),
        );
        assert_eq!(
            TypeInSchema::schemaless(&overlapping).assignment_to(sink),
            TypeAssignability::Mismatch
        );
        // With no source identity, the deleted nominal arm's rejection still
        // belongs to ordinary scalar compatibility, never the nominal seam.
        assert_eq!(
            TypeInSchema::schemaless(&AttributeType::int()).assignment_to(sink),
            TypeAssignability::Mismatch
        );
    }

    #[test]
    fn same_nominal_name_mismatch_identifies_the_scalar_kinds() {
        use crate::schema::{AttributeType, MismatchTypeNames};
        let source = AttributeType::refined_float(Some(TypeIdentity::bare("Count")), None);
        let sink = AttributeType::refined_int(Some(TypeIdentity::bare("Count")), None);
        let names = MismatchTypeNames::for_types(
            TypeInSchema::schemaless(&sink),
            TypeInSchema::schemaless(&source),
        );
        assert_ne!(names.expected(), names.actual());
        assert!(
            names.expected().contains("Int") && names.actual().contains("Float"),
            "{names}"
        );
    }

    #[test]
    fn disjoint_int_members_report_their_distinct_ranges() {
        use crate::schema::{
            AttributeType, IntIdentityLayer, IntMemberConstraints, MismatchTypeNames,
        };
        let declaration = |min, max| {
            AttributeType::int_member(
                IntIdentityLayer::new(Some(TypeIdentity::bare("Count")), None),
                IntMemberConstraints::new(Some((Some(min), Some(max)))),
            )
        };
        let source = declaration(100, 128);
        let sink = declaration(1, 64);
        let names = MismatchTypeNames::for_types(
            TypeInSchema::schemaless(&sink),
            TypeInSchema::schemaless(&source),
        );
        assert!(names.expected().contains("member range 1..=64"), "{names}");
        assert!(names.actual().contains("member range 100..=128"), "{names}");
        assert_ne!(names.expected(), names.actual());
    }

    #[test]
    fn enum_admission_preserves_main_reference_and_declared_modes() {
        use crate::schema::AttributeType;
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let identity = TypeIdentity::from_dotted("test.Mode");
        let scalar = AttributeType::refined_string(
            Some(identity.clone()),
            None,
            Some((Some(1), Some(100))),
            None,
        );
        let enum_type = |max, values: Option<Vec<String>>| {
            AttributeType::enum_with_base(
                identity.clone(),
                AttributeType::refined_string(None, None, Some((Some(1), Some(max))), None),
                values,
                Vec::new(),
                None,
                None,
            )
        };
        let destination = enum_type(8, Some(vec!["a".into()]));
        let sink = TypeInSchema::schemaless(&destination)
            .with_authority(&catalog, SinkAuthority::Owner(&owner));
        for (source, declared) in [
            (scalar, false),
            (enum_type(16, Some(vec!["a".into()])), false),
            (enum_type(8, Some(vec!["a".into(), "b".into()])), true),
            (destination.clone(), true),
        ] {
            assert_eq!(
                TypeInSchema::schemaless(&source)
                    .assignment_to(sink)
                    .require_loaded()
                    .map(|assignment| assignment
                        .require_proven()
                        .expect("test expects a static assignment proof"))
                    .unwrap(),
                declared
            );
            assert!(matches!(
                TypeInSchema::schemaless(&source).assignment_on_paths(
                    sink,
                    ReferenceAssignmentCheck::Full,
                    &mut Vec::new(),
                    &mut Vec::new()
                ),
                AssignmentRelation::NeedsLocal(ref obligation) if obligation.missing_schema().is_none()
            ));
        }
    }

    #[test]
    fn enum_value_mismatch_names_the_different_allowed_values() {
        use crate::schema::{AttributeType, MismatchTypeNames};
        let enumeration = |values| {
            AttributeType::enum_(
                TypeIdentity::from_dotted("test.Mode"),
                Some(values),
                Vec::new(),
                None,
                None,
            )
        };
        let source = enumeration(vec!["a".into(), "b".into()]);
        let sink = enumeration(vec!["a".into()]);
        let names = MismatchTypeNames::for_types(
            TypeInSchema::schemaless(&sink),
            TypeInSchema::schemaless(&source),
        );
        assert_ne!(names.expected(), names.actual(), "{names}");
        assert!(names.actual().contains("b"), "{names}");
        assert!(names.expected().contains("values"), "{names}");
    }

    #[test]
    fn assignment_consumers_preserve_the_unchecked_policy() {
        for (assignment, expected) in [
            (TypeAssignability::Assignable, true),
            (TypeAssignability::Mismatch, false),
        ] {
            assert_eq!(
                assignment
                    .clone()
                    .require_loaded()
                    .map(|assignment| assignment
                        .require_proven()
                        .expect("test expects a static assignment proof")),
                Ok(expected)
            );
            assert_eq!(
                crate::validation::deferred_types::with_session_for_test(
                    &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                    |session| assignment
                        .record_unchecked(session)
                        .require_proven()
                        .expect("test expects a static assignment proof")
                ),
                expected
            );
        }
        let unchecked = TypeAssignability::Unchecked(TypeAssignmentDeferral::new(
            crate::parser::parse_type_expr_str(
                "Arn",
                &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap(),
        ));
        assert_eq!(
            unchecked
                .clone()
                .require_loaded()
                .map(|assignment| assignment
                    .require_proven()
                    .expect("test expects a static assignment proof")),
            Err(CustomTypeValidationError::ProvidersNotLoaded)
        );
        assert!(crate::validation::deferred_types::with_session_for_test(
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| unchecked
                .record_unchecked(session)
                .require_proven()
                .expect("test expects a static assignment proof")
        ));
    }

    #[test]
    fn round12_same_identity_float_references_keep_main_deferral() {
        use crate::schema::AttributeType;
        let source = AttributeType::refined_float(
            Some(TypeIdentity::bare("Scalar")),
            Some((Some(crate::finite(0.0)), Some(crate::finite(100.0)))),
        );
        let target = AttributeType::refined_float(
            Some(TypeIdentity::bare("Scalar")),
            Some((Some(crate::finite(0.0)), Some(crate::finite(10.0)))),
        );
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let sink = TypeInSchema::schemaless(&target)
            .with_authority(&catalog, SinkAuthority::Owner(&owner));
        assert!(matches!(
            TypeInSchema::schemaless(&source).assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            AssignmentRelation::NeedsLocal(ref obligation) if obligation.missing_schema().is_none()
        ));
    }

    #[test]
    fn round12_same_identity_enum_references_keep_main_deferral() {
        use crate::schema::AttributeType;
        let source = AttributeType::enum_(TypeIdentity::bare("Choice"), None, vec![], None, None);
        let target = AttributeType::enum_(TypeIdentity::bare("Choice"), None, vec![], None, None);
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let sink = TypeInSchema::schemaless(&target)
            .with_authority(&catalog, SinkAuthority::Owner(&owner));
        assert!(matches!(
            TypeInSchema::schemaless(&source).assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            AssignmentRelation::NeedsLocal(ref obligation) if obligation.missing_schema().is_none()
        ));
    }

    #[test]
    fn round12_same_identity_unloaded_references_keep_main_deferral() {
        let source = super::super::PatternAttributeType::<super::super::Draft> {
            kind: AttrTypeKind::UnloadedSchemaType(
                TypeIdentity::from_dotted("test.X").try_into().unwrap(),
            ),
        };
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let sink = TypeInSchema::schemaless(&source)
            .with_authority(&catalog, SinkAuthority::Owner(&owner));
        assert!(matches!(
            TypeInSchema::schemaless(&source).assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            AssignmentRelation::NeedsLocal(ref obligation) if obligation.missing_schema().is_none()
        ));
    }

    #[test]
    fn round12_empty_source_annotation_is_rejected_before_admission() {
        use crate::parser::ResolvedSchemaType;
        use crate::schema::CompiledAttributeType;
        let id = TypeIdentity::from_dotted("test.X");
        for (first, second, target) in [
            (
                CompiledAttributeType::refined_string(
                    Some(id.clone()),
                    None,
                    Some((Some(1), Some(4))),
                    None,
                ),
                CompiledAttributeType::refined_string(
                    Some(id.clone()),
                    None,
                    Some((Some(8), Some(12))),
                    None,
                ),
                CompiledAttributeType::refined_string(Some(id.clone()), None, None, None),
            ),
            (
                CompiledAttributeType::refined_int(Some(id.clone()), Some((Some(1), Some(4)))),
                CompiledAttributeType::refined_int(Some(id.clone()), Some((Some(8), Some(12)))),
                CompiledAttributeType::refined_int(Some(id.clone()), None),
            ),
        ] {
            let first = ResolvedSchemaType::from_annotation_declaration("a", &first).unwrap();
            let second = ResolvedSchemaType::from_annotation_declaration("b", &second).unwrap();
            let source = first.conjoin(&second).materialize();
            assert!(source.has_empty_scalar_intersection());
            let catalog = crate::parser::ProviderCustomTypes::default();
            let owner = crate::parser::SchemaTypeOwner::Provider("b".into());
            let sink = TypeInSchema::schemaless(&target)
                .with_authority(&catalog, SinkAuthority::Owner(&owner));
            assert_eq!(
                TypeInSchema::schemaless(&source).assignment_to(sink),
                TypeAssignability::Mismatch
            );
            assert!(matches!(
                TypeInSchema::schemaless(&source).assignment_on_paths(
                    sink,
                    ReferenceAssignmentCheck::Full,
                    &mut Vec::new(),
                    &mut Vec::new()
                ),
                AssignmentRelation::Mismatch
            ));
        }
    }

    #[test]
    fn round13_same_identity_overlap_defers_but_disjoint_and_widening_do_not() {
        use crate::schema::AttributeType;
        let source_id = TypeIdentity::from_dotted("aws.iam.Role.Arn");
        let source_string = AttributeType::refined_string(
            Some(source_id.clone()),
            None,
            Some((Some(1), Some(2048))),
            None,
        );
        let source_int =
            AttributeType::refined_int(Some(source_id.clone()), Some((Some(1), Some(2048))));
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("aws".into());
        for (source, overlap, disjoint, widening) in [
            (
                source_string,
                AttributeType::refined_string(
                    Some(source_id.clone()),
                    None,
                    Some((Some(20), Some(2048))),
                    None,
                ),
                AttributeType::refined_string(
                    Some(source_id.clone()),
                    None,
                    Some((Some(3000), Some(4000))),
                    None,
                ),
                AttributeType::refined_string(
                    Some(TypeIdentity::from_dotted("aws.Arn")),
                    None,
                    Some((Some(1), Some(10))),
                    None,
                ),
            ),
            (
                source_int,
                AttributeType::refined_int(Some(source_id.clone()), Some((Some(20), Some(2048)))),
                AttributeType::refined_int(Some(source_id.clone()), Some((Some(3000), Some(4000)))),
                AttributeType::refined_int(
                    Some(TypeIdentity::from_dotted("aws.Arn")),
                    Some((Some(1), Some(10))),
                ),
            ),
        ] {
            for mode in [
                ReferenceAssignmentCheck::Full,
                ReferenceAssignmentCheck::IdentifiedLeaves,
            ] {
                let sink = |ty| {
                    TypeInSchema::schemaless(ty)
                        .with_authority(&catalog, SinkAuthority::Owner(&owner))
                };
                assert!(matches!(
                    TypeInSchema::schemaless(&source).assignment_on_paths(
                        sink(&overlap),
                        mode,
                        &mut Vec::new(),
                        &mut Vec::new()
                    ),
                    AssignmentRelation::NeedsLocal(ref obligation) if obligation.missing_schema().is_none()
                ));
                for destination in [&disjoint, &widening] {
                    assert!(matches!(
                        TypeInSchema::schemaless(&source).assignment_on_paths(
                            sink(destination),
                            mode,
                            &mut Vec::new(),
                            &mut Vec::new()
                        ),
                        AssignmentRelation::Mismatch
                    ));
                    assert_eq!(
                        TypeInSchema::schemaless(&source).assignment_to(sink(destination)),
                        TypeAssignability::Mismatch
                    );
                }
            }
        }
    }

    #[test]
    fn round14_declared_assignment_retains_destination_member_obligations() {
        use crate::schema::{AttributeType, StringIdentityLayer, StringMemberConstraints};
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("aws".into());
        for (source_id, sink_id, source_max, member_max, expected) in [
            ("aws.Name", "aws.Name", 100, 10, "NeedsLocal"),
            ("aws.Name", "aws.Name", 100, 200, "Assignable"),
            ("aws.iam.Role.Arn", "aws.Arn", 2048, 20, "NeedsLocal"),
        ] {
            let source = AttributeType::refined_string(
                Some(TypeIdentity::from_dotted(source_id)),
                None,
                Some((Some(1), Some(source_max))),
                None,
            );
            let sink = AttributeType::string_member(
                StringIdentityLayer::new(
                    Some(TypeIdentity::from_dotted(sink_id)),
                    None,
                    None,
                    None,
                ),
                StringMemberConstraints::new(None, Some((Some(1), Some(member_max)))),
            );
            let outcome = TypeInSchema::schemaless(&source).assignment_to(
                TypeInSchema::schemaless(&sink)
                    .with_authority(&catalog, SinkAuthority::Owner(&owner)),
            );
            match outcome {
                TypeAssignability::Assignable => assert_eq!(expected, "Assignable"),
                TypeAssignability::NeedsLocal(obligation) => {
                    assert_eq!(expected, "NeedsLocal");
                    let (destination, missing) = obligation.into_parts();
                    assert!(missing.is_none());
                    for length in [5, 50] {
                        let value: crate::resource::Value = crate::resource::Value::Concrete(
                            crate::resource::ConcreteValue::String("A".repeat(length).into()),
                        );
                        let checked = destination
                            .check_or_defer(&value, crate::schema::FieldAncestry::WRITABLE);
                        assert_eq!(
                            checked.is_ok(),
                            length <= member_max as usize,
                            "{checked:?}"
                        );
                        if let Err(error) = checked {
                            assert_eq!(
                                error.to_string().matches("outside allowed range").count(),
                                1
                            );
                        }
                    }
                }
                TypeAssignability::Mismatch => panic!("overlapping member bounds must defer"),
                TypeAssignability::Unchecked(missing) => {
                    panic!("fixture has structural identity authority: {missing:?}")
                }
            }
        }
    }

    #[test]
    fn round14_enum_assignment_uses_the_base_recursion_without_an_extra_bound_proof() {
        use crate::schema::AttributeType;
        let id = TypeIdentity::from_dotted("test.Count");
        let source = AttributeType::enum_with_base(
            id.clone(),
            AttributeType::ref_("Base"),
            None,
            vec![],
            None,
            None,
        );
        let target = AttributeType::enum_with_base(
            id,
            AttributeType::ref_("Base"),
            None,
            vec![],
            None,
            None,
        );
        let source_defs = std::collections::BTreeMap::from([("Base".into(), AttributeType::int())]);
        let target_defs = std::collections::BTreeMap::from([(
            "Base".into(),
            AttributeType::refined_int(None, Some((Some(1), Some(10)))),
        )]);
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        assert_eq!(
            source.in_schema(&source_defs).assignment_to(
                target
                    .in_schema(&target_defs)
                    .with_authority(&catalog, SinkAuthority::Owner(&owner))
            ),
            TypeAssignability::Assignable
        );
    }

    #[test]
    fn round14_anonymous_member_assignment_preserves_a_local_obligation() {
        use crate::schema::{AttributeType, StringIdentityLayer, StringMemberConstraints};
        let source = AttributeType::string();
        let target = AttributeType::string_member(
            StringIdentityLayer::new(None, None, None, None),
            StringMemberConstraints::new(None, Some((Some(1), Some(10)))),
        );
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let result = TypeInSchema::schemaless(&source).assignment_to(
            TypeInSchema::schemaless(&target)
                .with_authority(&catalog, SinkAuthority::Owner(&owner)),
        );
        assert!(
            format!("{result:?}").starts_with("NeedsLocal"),
            "{result:?}"
        );
    }

    #[test]
    fn round14_nested_obligation_keeps_the_complete_destination_scope() {
        use crate::schema::{
            CompiledAttributeType as Ty, CompiledStructField as Field, StringIdentityLayer,
            StringMemberConstraints,
        };
        let name = || {
            StringIdentityLayer::new(
                Some(TypeIdentity::from_dotted("aws.Name")),
                None,
                Some((Some(1), Some(100))),
                None,
            )
        };
        let source = Ty::struct_(
            "Pair",
            vec![
                Field::optional("first", Ty::string_member(name(), Default::default())).build(),
                Field::optional("second", Ty::string_member(name(), Default::default())).build(),
            ],
        );
        let target = Ty::struct_(
            "Pair",
            vec![
                Field::optional(
                    "first",
                    Ty::string_member(
                        name(),
                        StringMemberConstraints::new(None, Some((Some(1), Some(10)))),
                    ),
                )
                .build(),
                Field::optional(
                    "second",
                    Ty::string_member(
                        name(),
                        StringMemberConstraints::new(None, Some((Some(1), Some(20)))),
                    ),
                )
                .build(),
            ],
        );
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let result = TypeInSchema::schemaless(&source)
            .assignment_to(
                TypeInSchema::schemaless(&target)
                    .with_authority(&catalog, SinkAuthority::Owner(&owner)),
            )
            .require_loaded()
            .unwrap();
        let crate::schema::LoadedTypeAssignment::NeedsLocal(obligation) = result else {
            panic!("member checks must survive parent folding")
        };
        for length in [5, 50] {
            let text = |length| {
                crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                    "A".repeat(length).into(),
                ))
            };
            let value: crate::resource::Value = crate::resource::Value::Concrete(
                crate::resource::ConcreteValue::Map(
                    indexmap::indexmap! { "first".into() => text(5), "second".into() => text(length) },
                ),
            );
            let checked = obligation.check_or_defer(&value, crate::schema::FieldAncestry::WRITABLE);
            assert_eq!(checked.is_ok(), length == 5, "{checked:?}");
            if let Err(error) = checked {
                let message = error.to_string();
                assert!(message.contains("second"), "{message}");
                assert_eq!(
                    message.matches("outside allowed range").count(),
                    1,
                    "{message}"
                );
            }
        }
    }

    #[test]
    fn round14_union_obligation_cannot_escape_through_an_unadmitted_identity() {
        use crate::schema::{
            CompiledAttributeType as Ty, StringIdentityLayer, StringMemberConstraints,
        };
        let name = |identity| {
            StringIdentityLayer::new(
                Some(TypeIdentity::from_dotted(identity)),
                None,
                Some((Some(1), Some(100))),
                None,
            )
        };
        let source = Ty::string_member(name("aws.Name"), Default::default());
        let target = Ty::union(vec![
            Ty::string_member(
                name("aws.Name"),
                StringMemberConstraints::new(None, Some((Some(1), Some(10)))),
            ),
            Ty::string_member(name("aws.UnrelatedName"), Default::default()),
        ]);
        let catalog =
            crate::parser::ProviderCustomTypes::loaded(Default::default(), Box::new(|_, _| Ok(())));
        let owner = crate::parser::SchemaTypeOwner::Provider("aws".into());
        let result = TypeInSchema::schemaless(&source)
            .assignment_to(
                TypeInSchema::schemaless(&target)
                    .with_authority(&catalog, SinkAuthority::Owner(&owner)),
            )
            .require_loaded()
            .unwrap();
        let crate::schema::LoadedTypeAssignment::NeedsLocal(obligation) = result else {
            panic!("only the bounded Name arm is nominally admitted")
        };
        for length in [5, 50] {
            let value: crate::resource::Value = crate::resource::Value::Concrete(
                crate::resource::ConcreteValue::String("A".repeat(length).into()),
            );
            assert_eq!(
                obligation
                    .check_or_defer(&value, crate::schema::FieldAncestry::WRITABLE)
                    .is_ok(),
                length == 5
            );
        }
    }
}
