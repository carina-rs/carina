//! Boundary assignability when provider declarations may not be loaded.

use super::{AttrTypeKind, PatternForm, TypeIdentity, TypeInSchema, resolve_refs_on_path};
use crate::binding_index::{
    BindingIndex, DeclaredValueAssignment, RefTypeAssignabilityError, RefTypeMismatch,
    ResolvedRefType,
};
use crate::parser::CustomTypeValidationError;

/// Missing provider evidence is distinct from a proven type mismatch.
/// This declaration-compatibility API cannot return an admission obligation.
///
/// ```compile_fail,E0599
/// use carina_core::schema::TypeAssignability;
/// let _ = TypeAssignability::NeedsLocal(None);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum TypeAssignability<'s> {
    /// The declarations admit this flow; this grants no resolved-value proof.
    Assignable,
    Mismatch,
    Unchecked(TypeAssignmentDeferral<'s>),
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

impl<'s> TypeAssignability<'s> {
    /// Loaded validation must prove assignment using the provider schemas.
    pub fn require_loaded(self) -> Result<bool, CustomTypeValidationError> {
        match self {
            Self::Assignable => Ok(true),
            Self::Mismatch => Ok(false),
            Self::Unchecked(_) => Err(CustomTypeValidationError::ProvidersNotLoaded),
        }
    }

    /// Record missing evidence before permitting a provisional assignment.
    pub fn record_unchecked(
        self,
        sink: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> bool {
        match self {
            Self::Assignable => true,
            Self::Unchecked(evidence) => {
                evidence.record(sink);
                true
            }
            Self::Mismatch => false,
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
            input_rules: _,
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
                input_rules: _,
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

/// Structural length/range evidence for scalars and enum bases. Identity
/// admission stays in the catalog-and-authority-bound judgement.
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
            input_rules: _,
        }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => None,
    };
    refs.truncate(resolved.restore_len);
    constraints
}

#[derive(Clone, Copy)]
struct ScalarValueConstraints {
    bounds: NominalScalarConstraints,
    constrained: bool,
    has_member_constraints: bool,
}

/// Concrete values owe both layers, including for anonymous source evidence.
fn scalar_value_constraints<'a, R: PatternForm>(
    ty: TypeInSchema<'a, R>,
    refs: &mut Vec<&'a str>,
) -> Option<ScalarValueConstraints> {
    let resolved = resolve_refs_on_path(ty.attr, ty.defs, refs).ok()?;
    let constraints = match &resolved.attr.kind {
        AttrTypeKind::String {
            identity_layer,
            member_layer,
        } => {
            let has_member_constraints =
                member_layer.pattern.is_some() || member_layer.length.is_some();
            Some(ScalarValueConstraints {
                bounds: NominalScalarConstraints::String(super::intersect_bounds(
                    identity_layer.length,
                    member_layer.length,
                )),
                constrained: identity_layer.pattern.is_some()
                    || identity_layer.length.is_some()
                    || has_member_constraints,
                has_member_constraints,
            })
        }
        AttrTypeKind::Int {
            identity_layer,
            member_layer,
        } => {
            let has_member_constraints = member_layer.range.is_some();
            Some(ScalarValueConstraints {
                bounds: NominalScalarConstraints::Int(super::intersect_bounds(
                    identity_layer.range,
                    member_layer.range,
                )),
                constrained: identity_layer.range.is_some() || has_member_constraints,
                has_member_constraints,
            })
        }
        AttrTypeKind::Float { binding: _, range } => Some(ScalarValueConstraints {
            bounds: NominalScalarConstraints::Float(*range),
            constrained: range.is_some(),
            has_member_constraints: false,
        }),
        AttrTypeKind::Enum {
            identity: _,
            base,
            values: _,
            dsl_aliases: _,
            validate: _,
            to_dsl: _,
        } => scalar_value_constraints(base.in_schema(ty.defs), refs),
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
            input_rules: _,
        }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => None,
    };
    refs.truncate(resolved.restore_len);
    constraints
}

/// An Enum value may still be written to a scalar destination slot. Enum
/// bases themselves cannot have member constraints after schema admission.
fn scalar_member_obligation<'result, R: PatternForm, C: AssignmentCheck>(
    source: Option<NominalScalarConstraints>,
    sink: TypeInSchema<'_, R>,
    check: C,
) -> C::Result<'result> {
    let constraints = scalar_value_constraints(sink, &mut Vec::new())
        .filter(|constraints| constraints.has_member_constraints);
    scalar_value_obligation(source, constraints, check)
}

/// Shared structural admission proof, including empty bounds on either side.
fn scalar_bounds_overlap(
    source: Option<NominalScalarConstraints>,
    sink: Option<NominalScalarConstraints>,
) -> bool {
    !(source.is_some_and(|bounds| bounds.provably_disjoint(bounds))
        || sink.is_some_and(|bounds| bounds.provably_disjoint(bounds))
        || source
            .zip(sink)
            .is_some_and(|(source, sink)| source.provably_disjoint(sink)))
}

/// Admit scalar overlap and record `NeedsLocal` in reference mode. Provider
/// schema slots validate resolved values against the full destination declaration
/// before dispatch; declaration compatibility alone grants no value proof.
fn scalar_value_obligation<'result, C: AssignmentCheck>(
    source: Option<NominalScalarConstraints>,
    sink: Option<ScalarValueConstraints>,
    check: C,
) -> C::Result<'result> {
    if !scalar_bounds_overlap(source, sink.map(|constraints| constraints.bounds)) {
        TypeAssignability::Mismatch.into()
    } else if sink.is_some_and(|constraints| constraints.constrained) {
        check
            .destination_obligation(true, false)
            .expect("scalar constraints have a destination obligation")
    } else {
        TypeAssignability::Assignable.into()
    }
}

/// Convert kind evidence while retaining the reference-mode value obligation.
/// Identified reads carry identity bounds; anonymous reads carry both layers.
fn known_assignment_result<'source, 'sink, R: PatternForm, C: AssignmentCheck>(
    source: TypeInSchema<'source, R>,
    sink: AssignmentSink<'sink, R>,
    verdict: super::KnownAssignment,
    check: C,
    source_refs: &mut Vec<&'source str>,
    sink_refs: &mut Vec<&'sink str>,
) -> C::Result<'source>
where
    'sink: 'source,
{
    match verdict {
        super::KnownAssignment::Assignable => TypeAssignability::Assignable.into(),
        super::KnownAssignment::Mismatch => TypeAssignability::Mismatch.into(),
        super::KnownAssignment::DeferToSinkValue => {
            let source_bounds = if identity_leaf(&source.attr.kind).is_some() {
                identity_scalar_constraints(source, source_refs)
            } else {
                scalar_value_constraints(source, source_refs).map(|constraints| constraints.bounds)
            };
            scalar_value_obligation(
                source_bounds,
                scalar_value_constraints(sink.ty, sink_refs),
                check,
            )
        }
    }
}

/// The reference-mode relation retains local obligations. Strict walks return
/// `TypeAssignability` directly; this relation has no conversion back to it.
///
/// ```compile_fail,E0277
/// use carina_core::schema::assignability::{AssignmentRelation, TypeAssignability};
/// fn bypass(reference: AssignmentRelation<'_>) -> TypeAssignability<'_> {
///     reference.into()
/// }
/// ```
#[derive(Debug)]
pub enum AssignmentRelation<'s> {
    Assignable,
    Mismatch,
    Unchecked(TypeAssignmentDeferral<'s>),
    NeedsLocal(Option<TypeAssignmentDeferral<'s>>),
}
impl<'s> From<TypeAssignability<'s>> for AssignmentRelation<'s> {
    fn from(value: TypeAssignability<'s>) -> Self {
        match value {
            TypeAssignability::Assignable => Self::Assignable,
            TypeAssignability::Mismatch => Self::Mismatch,
            TypeAssignability::Unchecked(schema) => Self::Unchecked(schema),
        }
    }
}
impl<'s> AssignmentRelation<'s> {
    pub(crate) fn all(results: impl IntoIterator<Item = Self>) -> Self {
        let mut schema: Option<TypeAssignmentDeferral<'s>> = None;
        let mut local = false;
        for result in results {
            let next = match result {
                Self::Mismatch => return Self::Mismatch,
                Self::Assignable => None,
                Self::Unchecked(schema) => Some(schema),
                Self::NeedsLocal(schema) => {
                    local = true;
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
        if local {
            Self::NeedsLocal(schema)
        } else {
            match schema {
                Some(schema) => Self::Unchecked(schema),
                None => Self::Assignable,
            }
        }
    }
    fn any(results: impl IntoIterator<Item = Self>) -> Self {
        let mut pending = Vec::new();
        for result in results {
            match result {
                Self::Assignable => return Self::Assignable,
                Self::NeedsLocal(None) => return Self::NeedsLocal(None),
                result @ (Self::Unchecked(_) | Self::NeedsLocal(Some(_))) => pending.push(result),
                Self::Mismatch => {}
            }
        }
        if pending.is_empty() {
            Self::Mismatch
        } else {
            Self::all(pending)
        }
    }
}

/// Each mode selects its result before entering the shared recursive walk.
trait AssignmentCheck: Copy {
    type Result<'s>: AssignmentResult<'s>;
    fn identified_only(self) -> bool;
    /// Identified scalar declarations and widening retain identity-bound
    /// containment; reference admission can discharge same-identity overlap.
    /// Anonymous scalar pairs use overlap-or-defer in both Full check modes.
    fn requires_identity_containment(self) -> bool;
    fn destination_obligation<'s>(
        self,
        scalar: bool,
        same_identity: bool,
    ) -> Option<Self::Result<'s>>;
}

trait AssignmentResult<'s>: From<TypeAssignability<'s>> {
    fn from_bool(value: bool) -> Self {
        if value {
            TypeAssignability::Assignable.into()
        } else {
            TypeAssignability::Mismatch.into()
        }
    }
    fn all(results: impl IntoIterator<Item = Self>) -> Self;
    fn any(results: impl IntoIterator<Item = Self>) -> Self;
}

impl<'s> AssignmentResult<'s> for TypeAssignability<'s> {
    fn all(results: impl IntoIterator<Item = Self>) -> Self {
        let mut schema: Option<TypeAssignmentDeferral<'s>> = None;
        for result in results {
            match result {
                Self::Mismatch => return Self::Mismatch,
                Self::Assignable => {}
                Self::Unchecked(next) => {
                    schema = Some(match schema {
                        Some(previous) => previous.merge(next),
                        None => next,
                    });
                }
            }
        }
        match schema {
            Some(schema) => Self::Unchecked(schema),
            None => Self::Assignable,
        }
    }

    fn any(results: impl IntoIterator<Item = Self>) -> Self {
        let mut pending = Vec::new();
        for result in results {
            match result {
                Self::Assignable => return Self::Assignable,
                result @ Self::Unchecked(_) => pending.push(result),
                Self::Mismatch => {}
            }
        }
        if pending.is_empty() {
            Self::Mismatch
        } else {
            Self::all(pending)
        }
    }
}

impl<'s> AssignmentResult<'s> for AssignmentRelation<'s> {
    fn all(results: impl IntoIterator<Item = Self>) -> Self {
        Self::all(results)
    }
    fn any(results: impl IntoIterator<Item = Self>) -> Self {
        Self::any(results)
    }
}

#[derive(Clone, Copy)]
enum DeclaredAssignmentCheck {
    Full,
    /// Re-admit only original identified leaves at a declared value's later sink.
    /// This identity check grants no resolved-value proof.
    IdentifiedLeaves,
}

impl AssignmentCheck for DeclaredAssignmentCheck {
    type Result<'s> = TypeAssignability<'s>;
    fn identified_only(self) -> bool {
        matches!(self, Self::IdentifiedLeaves)
    }
    fn requires_identity_containment(self) -> bool {
        true
    }
    fn destination_obligation<'s>(
        self,
        scalar: bool,
        _same_identity: bool,
    ) -> Option<Self::Result<'s>> {
        // Declaration compatibility admits types, not resolved values. Anonymous
        // refinements occur in provider schema slots, whose full declarations
        // are checked on resolved values before provider dispatch.
        scalar.then_some(TypeAssignability::Assignable)
    }
}

#[derive(Clone, Copy)]
enum ReferenceAssignmentCheck {
    Full,
    IdentifiedLeaves,
}

impl AssignmentCheck for ReferenceAssignmentCheck {
    type Result<'s> = AssignmentRelation<'s>;
    fn identified_only(self) -> bool {
        matches!(self, Self::IdentifiedLeaves)
    }
    fn requires_identity_containment(self) -> bool {
        false
    }
    fn destination_obligation<'s>(
        self,
        scalar: bool,
        same_identity: bool,
    ) -> Option<Self::Result<'s>> {
        (scalar || same_identity).then_some(AssignmentRelation::NeedsLocal(None))
    }
}

impl TypeInSchema<'_, super::Compiled> {
    /// Fully judge the declaration and recheck its original identified leaves.
    /// Provider slots validate resolved values separately before dispatch.
    pub(crate) fn declared_assignment_to<B: crate::resource::BindingAddress>(
        self,
        original: &ResolvedRefType,
        incoming: AssignmentSink<'_, super::Compiled>,
        sink: AssignmentSink<'_, super::Compiled>,
        bindings: &BindingIndex<'_, B>,
    ) -> DeclaredValueAssignment {
        // A rejected declaration owns the mismatch. Its original source is
        // relevant only after the declaration passes at this sink.
        match bindings.check_type_assignment(self, sink) {
            Err(error) => {
                return DeclaredValueAssignment::Rejected(RefTypeAssignabilityError::Unchecked(
                    Box::new(error),
                ));
            }
            Ok(false) => {
                return DeclaredValueAssignment::Rejected(RefTypeAssignabilityError::Mismatch(
                    RefTypeMismatch::direct(super::MismatchTypeNames::for_types(*sink, self)),
                ));
            }
            Ok(true) => {}
        }
        match original.check_assignment(*sink, |source| {
            bindings.consume_type_assignment(source.identity_assignment_to(sink))
        }) {
            Ok(()) => DeclaredValueAssignment::Assignable,
            Err(error @ RefTypeAssignabilityError::Mismatch(_)) => {
                // The incoming boundary already owns a proven rejection.
                // Inspect that relation without recording another deferral:
                // unknown incoming edges cannot explain this sink's mismatch.
                if original.has_proven_mismatch_at(incoming) {
                    DeclaredValueAssignment::IncomingBoundaryMismatch
                } else {
                    DeclaredValueAssignment::Rejected(error)
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
    /// Identity-less leaves are skipped; this identity check grants no
    /// resolved-value proof.
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
    ) -> C::Result<'source>
    where
        'sink: 'source,
    {
        let Ok(source) = resolve_refs_on_path(self.attr, self.defs, source_refs) else {
            return TypeAssignability::Mismatch.into();
        };
        let Ok(target) = resolve_refs_on_path(sink.attr, sink.defs, sink_refs) else {
            source_refs.truncate(source.restore_len);
            return TypeAssignability::Mismatch.into();
        };
        let left = source.attr.in_schema(self.defs);
        let right = sink.rebind(target.attr);
        let source_constraints = identity_scalar_constraints(left, source_refs);
        let target_constraints = identity_scalar_constraints(right.ty, sink_refs);
        use AttrTypeKind::*;
        use TypeAssignability::*;
        // Same identity is the zero-edge case of nominal admission. Reference
        // checks may retain a resolved-value obligation. Same-identity value
        // admission defers overlapping bounds; identified widening retains the
        // containment proof. Strict
        // declarations check identified identity bounds; anonymous scalar pairs
        // use overlap-or-defer in both declaration and reference Full checks.
        let scalar = matches!(&source.attr.kind, String { .. } | Int { .. })
            && matches!(&target.attr.kind, String { .. } | Int { .. } | Float { .. });
        let same_identity = identity_leaf(&source.attr.kind)
            .and_then(|leaf| leaf.identity())
            .zip(identity_leaf(&target.attr.kind).and_then(|leaf| leaf.identity()))
            .is_some_and(|(source, target)| source == target);
        if let Some(obligation) = check.destination_obligation(scalar, same_identity)
            && let Some(source_identity) =
                identity_leaf(&source.attr.kind).and_then(|leaf| leaf.identity())
            && let Some(target_identity) = identity_leaf(&target.attr.kind)
        {
            if !scalar {
                // Keep main's same-identity reference deferral for Float,
                // Enum and unloaded leaves. Strict declaration checks still
                // use their existing kind/base/range rules below.
                let empty = source_constraints
                    .is_some_and(|bounds| bounds.provably_disjoint(bounds))
                    || target.attr.has_empty_scalar_intersection();
                let result = if empty { Mismatch.into() } else { obligation };
                source_refs.truncate(source.restore_len);
                sink_refs.truncate(target.restore_len);
                return result;
            }
            let identity_fits = source_constraints
                .zip(target_constraints)
                .is_some_and(|(source, target)| source.contained_by(target));
            let disjoint = target.attr.has_empty_scalar_intersection()
                || source_constraints
                    .zip(scalar_value_constraints(right.ty, sink_refs))
                    .is_some_and(|(source, target)| source.provably_disjoint(target.bounds));
            let result = C::Result::all([
                right
                    .identity_assignment(source_identity, target_identity)
                    .into(),
                if disjoint
                    || ((!same_identity || check.requires_identity_containment()) && !identity_fits)
                {
                    Mismatch.into()
                } else {
                    obligation
                },
            ]);
            source_refs.truncate(source.restore_len);
            sink_refs.truncate(target.restore_len);
            return result;
        }
        let result = match (&source.attr.kind, &target.attr.kind) {
            (_, _)
                if source_constraints.is_some_and(|bounds| bounds.provably_disjoint(bounds))
                    || target.attr.has_empty_scalar_intersection() =>
            {
                Mismatch.into()
            }
            (_, kind)
                if check.identified_only()
                    && identity_leaf(kind).is_none()
                    && !matches!(kind, List { .. } | Map { .. } | Struct { .. } | Union(_)) =>
            {
                Assignable.into()
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
                    input_rules: _,
                }
                | UnloadedSchemaType(_)
                | Bool
                | Duration
                | Union(_)
                | Ref(_),
            ) => C::Result::all(members.as_raw_slice().iter().map(|member| {
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
                    input_rules: _,
                }
                | UnloadedSchemaType(_)
                | Bool
                | Duration
                | Ref(_),
                Union(members),
            ) => C::Result::any(members.as_raw_slice().iter().map(|member| {
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
                    input_rules: _,
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
                    input_rules: _,
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
                        C::Result::from_bool(
                            right
                                .identity()
                                .is_some_and(|sink| left.structurally_assignable_to(sink)),
                        )
                    } else {
                        sink.rebind(target.attr)
                            .identity_assignment(left, right)
                            .into()
                    }
                }
                (None, Some(_)) => Mismatch.into(),
                (Some(_) | None, None) => Unchecked(TypeAssignmentDeferral::new(
                    crate::parser::TypeExpr::UnloadedSchemaType(unloaded.clone()),
                ))
                .into(),
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
            ) => left.in_schema(self.defs).assignment_on_paths(
                sink.rebind(right),
                check,
                source_refs,
                sink_refs,
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
            ) => C::Result::all([
                lk.in_schema(self.defs).assignment_on_paths(
                    sink.rebind(rk),
                    check,
                    source_refs,
                    sink_refs,
                ),
                lv.in_schema(self.defs).assignment_on_paths(
                    sink.rebind(rv),
                    check,
                    source_refs,
                    sink_refs,
                ),
            ]),
            (
                Struct {
                    fields: left,
                    name: _,
                    field_groups: _,
                    input_rules: _,
                },
                Struct {
                    fields: right,
                    name: _,
                    field_groups: _,
                    input_rules: _,
                },
            ) => {
                if left.len() != right.len() {
                    Mismatch.into()
                } else {
                    C::Result::all(right.iter().map(|field| {
                        match left.iter().find(|candidate| candidate.name == field.name) {
                            Some(candidate) => candidate
                                .field_type
                                .in_schema(self.defs)
                                .assignment_on_paths(
                                    sink.rebind(&field.field_type),
                                    check,
                                    source_refs,
                                    sink_refs,
                                ),
                            None => Mismatch.into(),
                        }
                    }))
                }
            }
            (
                Struct {
                    fields,
                    name: _,
                    field_groups: _,
                    input_rules: _,
                },
                Map {
                    value,
                    key: _,
                    constraints: _,
                },
            ) => C::Result::all(fields.iter().map(|field| {
                field.field_type.in_schema(self.defs).assignment_on_paths(
                    sink.rebind(value),
                    check,
                    source_refs,
                    sink_refs,
                )
            })),
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
                C::Result::all([
                    right
                        .identity_assignment(source, IdentityLeaf::Scalar(target))
                        .into(),
                    C::Result::from_bool(super::f64_range_contains(
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
            ) if target.identity().is_some() => Mismatch.into(),
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
            ) if let Some(source) = source.identity() => C::Result::all([
                C::Result::from_bool(source.structurally_assignable_to(target)),
                C::Result::from_bool(match (source_constraints, target_constraints) {
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
            ) if target.identity().is_some() => C::Result::all([
                C::Result::from_bool(
                    target
                        .identity()
                        .is_some_and(|target| source.structurally_assignable_to(target)),
                ),
                C::Result::from_bool(match (source_constraints, target_constraints) {
                    (Some(source), Some(sink)) => source.contained_by(sink),
                    (None, Some(_) | None) | (Some(_), None) => false,
                }),
                scalar_member_obligation(source_constraints, right.ty, check),
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
            ) => C::Result::all([
                C::Result::from_bool(source.structurally_assignable_to(target)),
                // Enum identities stay nominal; identity-less bases recurse
                // through overlap-or-defer. Destination values enforce membership.
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
                    input_rules: _,
                }
                | Bool
                | Duration
                | Ref(_),
            ) => known_assignment_result(
                left,
                right,
                left.is_assignable_to_known(right),
                check,
                source_refs,
                sink_refs,
            ),
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
                    input_rules: _,
                }
                | Bool
                | Duration
                | Ref(_),
            ) => known_assignment_result(
                left,
                right,
                left.is_assignable_to_known(right),
                check,
                source_refs,
                sink_refs,
            ),
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
                    input_rules: _,
                }
                | Bool
                | Duration
                | Ref(_),
            ) => known_assignment_result(
                left,
                right,
                left.is_assignable_to_known(right),
                check,
                source_refs,
                sink_refs,
            ),
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
                    input_rules: _,
                }
                | Bool
                | Duration
                | Ref(_),
            ) => known_assignment_result(
                left,
                right,
                left.is_assignable_to_known(right),
                check,
                source_refs,
                sink_refs,
            ),
            (
                Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                    input_rules: _,
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
            ) => known_assignment_result(
                left,
                right,
                left.is_assignable_to_known(right),
                check,
                source_refs,
                sink_refs,
            ),
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
            AssignmentRelation::NeedsLocal(None)
        ));
    }

    #[test]
    fn enum_declaration_and_readmission_admit_overlapping_bases() {
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
            TypeAssignability::Assignable,
            "existing enum base recursion admits overlapping anonymous bounds"
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
    fn enum_admission_defers_anonymous_base_bounds_and_values() {
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
            (enum_type(16, Some(vec!["a".into()])), true),
            (enum_type(8, Some(vec!["a".into(), "b".into()])), true),
            (destination.clone(), true),
        ] {
            assert_eq!(
                TypeInSchema::schemaless(&source)
                    .assignment_to(sink)
                    .require_loaded()
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
                AssignmentRelation::NeedsLocal(None)
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
            assert_eq!(assignment.clone().require_loaded(), Ok(expected));
            assert_eq!(
                crate::validation::deferred_types::with_session_for_test(
                    &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                    |session| assignment.record_unchecked(session)
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
            unchecked.clone().require_loaded(),
            Err(CustomTypeValidationError::ProvidersNotLoaded)
        );
        assert!(crate::validation::deferred_types::with_session_for_test(
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| unchecked.record_unchecked(session)
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
            AssignmentRelation::NeedsLocal(None)
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
            AssignmentRelation::NeedsLocal(None)
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
            AssignmentRelation::NeedsLocal(None)
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
                    AssignmentRelation::NeedsLocal(None)
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
    fn issue_4362_identified_sources_defer_to_identityless_sinks() {
        use crate::schema::{
            AttributeType, IntIdentityLayer, IntMemberConstraints, StringIdentityLayer,
            StringMemberConstraints,
        };
        let identity = Some(TypeIdentity::bare("Source"));
        let string = AttributeType::string_member(
            StringIdentityLayer::new(identity.clone(), None, Some((Some(1), Some(100))), None),
            StringMemberConstraints::new(None, Some((Some(50), Some(60)))),
        );
        let int = AttributeType::int_member(
            IntIdentityLayer::new(identity.clone(), Some((Some(1), Some(100)))),
            IntMemberConstraints::new(Some((Some(50), Some(60)))),
        );
        let float = AttributeType::refined_float(
            identity,
            Some((Some(crate::finite(1.0)), Some(crate::finite(100.0)))),
        );
        let float_sink = AttributeType::refined_float(
            None,
            Some((Some(crate::finite(1.0)), Some(crate::finite(47.0)))),
        );
        for (source, target, unconstrained) in [
            (
                string,
                AttributeType::refined_string(None, None, Some((Some(1), Some(47))), None),
                AttributeType::string(),
            ),
            (
                int.clone(),
                AttributeType::refined_int(None, Some((Some(1), Some(47)))),
                AttributeType::int(),
            ),
            (float, float_sink.clone(), AttributeType::float()),
            (int, float_sink, AttributeType::float()),
        ] {
            // Identified reads carry identity bounds, never source slot members.
            assert_anonymous_scalar_assignment(&source, &target, Expect::NeedsLocal);
            assert_anonymous_scalar_assignment(&source, &unconstrained, Expect::Assignable);
        }
    }

    #[test]
    fn issue_4362_identified_disjoint_identity_bounds_reject() {
        use crate::schema::AttributeType;
        let identity = Some(TypeIdentity::bare("Source"));
        let float_sink = AttributeType::refined_float(
            None,
            Some((Some(crate::finite(1.0)), Some(crate::finite(47.0)))),
        );
        let int = AttributeType::refined_int(identity.clone(), Some((Some(50), Some(60))));
        for (source, target) in [
            (
                AttributeType::refined_string(
                    identity.clone(),
                    None,
                    Some((Some(50), Some(60))),
                    None,
                ),
                AttributeType::refined_string(None, None, Some((Some(1), Some(47))), None),
            ),
            (
                int.clone(),
                AttributeType::refined_int(None, Some((Some(1), Some(47)))),
            ),
            (
                AttributeType::refined_float(
                    identity,
                    Some((Some(crate::finite(50.0)), Some(crate::finite(60.0)))),
                ),
                float_sink.clone(),
            ),
            (int, float_sink),
        ] {
            assert_anonymous_scalar_assignment(&source, &target, Expect::Mismatch);
        }
    }

    #[test]
    fn issue_4362_identified_leaves_skip_anonymous_bounds() {
        use crate::schema::AttributeType;
        let source = AttributeType::refined_string(None, None, Some((Some(50), Some(60))), None);
        let target = AttributeType::refined_string(None, None, Some((Some(1), Some(47))), None);
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let sink = TypeInSchema::schemaless(&target)
            .with_authority(&catalog, SinkAuthority::Owner(&owner));
        let source = TypeInSchema::schemaless(&source);
        assert_eq!(source.assignment_to(sink), TypeAssignability::Mismatch);
        assert!(matches!(
            source.assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            AssignmentRelation::Mismatch
        ));
        assert_eq!(
            source.identity_assignment_to(sink),
            TypeAssignability::Assignable,
            "original evidence rechecks must skip anonymous bounds"
        );
        assert!(matches!(
            source.assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::IdentifiedLeaves,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            AssignmentRelation::Assignable
        ));
    }

    #[test]
    fn issue_4362_known_scalar_verdict_preserves_destination_obligation() {
        use crate::schema::{
            AttributeType, IntIdentityLayer, IntMemberConstraints, StringIdentityLayer,
            StringMemberConstraints,
        };
        let string = |min, max| {
            AttributeType::refined_string(None, None, Some((Some(min), Some(max))), None)
        };
        let int = |min, max| AttributeType::refined_int(None, Some((Some(min), Some(max))));
        let float = |min, max| {
            AttributeType::refined_float(
                None,
                Some((Some(crate::finite(min)), Some(crate::finite(max)))),
            )
        };
        let disjoint_string = AttributeType::string_member(
            StringIdentityLayer::new(None, None, Some((Some(1), Some(100))), None),
            StringMemberConstraints::new(None, Some((Some(50), Some(60)))),
        );
        let disjoint_int = || {
            AttributeType::int_member(
                IntIdentityLayer::new(None, Some((Some(1), Some(100)))),
                IntMemberConstraints::new(Some((Some(50), Some(60)))),
            )
        };
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        for (overlap, disjoint, target) in [
            (string(1, 100), disjoint_string, string(1, 47)),
            (int(1, 100), disjoint_int(), int(1, 47)),
            (float(1.0, 100.0), float(50.0, 60.0), float(1.0, 47.0)),
            (int(1, 100), disjoint_int(), float(1.0, 47.0)),
        ] {
            let sink = TypeInSchema::schemaless(&target)
                .with_authority(&catalog, SinkAuthority::Owner(&owner));
            for source in [&overlap, &disjoint] {
                assert_eq!(
                    TypeInSchema::schemaless(source).is_assignable_to_known(sink),
                    super::super::KnownAssignment::DeferToSinkValue
                );
            }
            assert_anonymous_scalar_assignment(&overlap, &target, Expect::NeedsLocal);
            assert_anonymous_scalar_assignment(&disjoint, &target, Expect::Mismatch);
        }
    }

    #[test]
    fn issue_4362_anonymous_string_overlap_defers_and_disjoint_rejects() {
        use crate::schema::{AttributeType, StringIdentityLayer, StringMemberConstraints};
        let string = |min, max, pattern: &str| {
            AttributeType::string_member(
                StringIdentityLayer::new(None, None, None, None),
                StringMemberConstraints::new(
                    Some(crate::schema::PatternSpec::new(
                        pattern,
                        crate::schema::PatternMatch::Full,
                    )),
                    Some((Some(min), Some(max))),
                ),
            )
        };
        let sink = string(1, 47, "^[a-z]+$");
        assert_anonymous_scalar_assignment(&string(1, 100, "^[A-Z]+$"), &sink, Expect::NeedsLocal);
        assert_anonymous_scalar_assignment(&string(50, 60, "^[a-z]+$"), &sink, Expect::Mismatch);
        // Both layers contribute bounds, including when their intersection is empty.
        for (bounds, unconstrained) in [
            ((50, 60), Expect::Assignable),
            ((101, 110), Expect::Mismatch),
        ] {
            let source = AttributeType::string_member(
                StringIdentityLayer::new(None, None, Some((Some(1), Some(100))), None),
                StringMemberConstraints::new(None, Some((Some(bounds.0), Some(bounds.1)))),
            );
            assert_anonymous_scalar_assignment(&source, &sink, Expect::Mismatch);
            assert_anonymous_scalar_assignment(&source, &AttributeType::string(), unconstrained);
        }
    }

    #[test]
    fn issue_4362_anonymous_int_overlap_defers_and_disjoint_rejects() {
        use crate::schema::{AttributeType, IntIdentityLayer, IntMemberConstraints};
        let int = |min, max| AttributeType::refined_int(None, Some((Some(min), Some(max))));
        let sink = int(1, 47);
        assert_anonymous_scalar_assignment(&int(1, 100), &sink, Expect::NeedsLocal);
        assert_anonymous_scalar_assignment(&int(50, 60), &sink, Expect::Mismatch);
        for (bounds, unconstrained) in [
            ((50, 60), Expect::Assignable),
            ((101, 110), Expect::Mismatch),
        ] {
            let source = AttributeType::int_member(
                IntIdentityLayer::new(None, Some((Some(1), Some(100)))),
                IntMemberConstraints::new(Some((Some(bounds.0), Some(bounds.1)))),
            );
            assert_anonymous_scalar_assignment(&source, &sink, Expect::Mismatch);
            assert_anonymous_scalar_assignment(&source, &AttributeType::int(), unconstrained);
        }
    }

    #[test]
    fn issue_4362_anonymous_float_overlap_defers_and_disjoint_rejects() {
        use crate::schema::AttributeType;
        let float = |min, max| {
            AttributeType::refined_float(
                None,
                Some((Some(crate::finite(min)), Some(crate::finite(max)))),
            )
        };
        let sink = float(1.0, 47.0);
        assert_anonymous_scalar_assignment(&float(1.0, 100.0), &sink, Expect::NeedsLocal);
        assert_anonymous_scalar_assignment(&float(50.0, 60.0), &sink, Expect::Mismatch);
    }

    #[test]
    fn issue_4362_anonymous_int_to_float_overlap_defers_and_disjoint_rejects() {
        use crate::schema::AttributeType;
        let int = |min, max| AttributeType::refined_int(None, Some((Some(min), Some(max))));
        let sink = AttributeType::refined_float(
            None,
            Some((Some(crate::finite(1.0)), Some(crate::finite(47.0)))),
        );
        assert_anonymous_scalar_assignment(&int(1, 100), &sink, Expect::NeedsLocal);
        assert_anonymous_scalar_assignment(&int(50, 60), &sink, Expect::Mismatch);
        // Compare integer endpoints without rounding them through f64.
        let edge = 9_007_199_254_740_992;
        let sink = AttributeType::refined_float(
            None,
            Some((None, Some(crate::finite(9_007_199_254_740_992.0)))),
        );
        assert_anonymous_scalar_assignment(&int(edge, edge + 1), &sink, Expect::NeedsLocal);
        assert_anonymous_scalar_assignment(&int(edge + 1, edge + 2), &sink, Expect::Mismatch);
    }

    #[derive(Clone, Copy, Debug)]
    enum Expect {
        Assignable,
        NeedsLocal,
        Mismatch,
    }

    fn assert_anonymous_scalar_assignment(
        source: &crate::schema::AttributeType,
        target: &crate::schema::AttributeType,
        expected: Expect,
    ) {
        let catalog = crate::parser::ProviderCustomTypes::default();
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let sink =
            TypeInSchema::schemaless(target).with_authority(&catalog, SinkAuthority::Owner(&owner));
        let declared = TypeInSchema::schemaless(source).assignment_to(sink);
        assert_eq!(
            declared,
            match expected {
                Expect::Assignable | Expect::NeedsLocal => TypeAssignability::Assignable,
                Expect::Mismatch => TypeAssignability::Mismatch,
            },
            "declared {} -> {}",
            source.scalar_diagnostic_name(),
            target.scalar_diagnostic_name(),
        );
        assert_eq!(
            TypeInSchema::schemaless(source).assignment_on_paths(
                sink,
                DeclaredAssignmentCheck::IdentifiedLeaves,
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            TypeAssignability::Assignable,
            "original evidence rechecks must skip anonymous leaves"
        );
        let reference = TypeInSchema::schemaless(source).assignment_on_paths(
            sink,
            ReferenceAssignmentCheck::Full,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(
            matches!(
                (&reference, expected),
                (AssignmentRelation::Assignable, Expect::Assignable)
                    | (AssignmentRelation::NeedsLocal(None), Expect::NeedsLocal)
                    | (AssignmentRelation::Mismatch, Expect::Mismatch)
            ),
            "reference {} -> {}: {reference:?}, expected {expected:?}",
            source.scalar_diagnostic_name(),
            target.scalar_diagnostic_name(),
        );
        assert!(matches!(
            TypeInSchema::schemaless(source).assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::IdentifiedLeaves,
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            AssignmentRelation::Assignable
        ));
    }

    #[test]
    fn issue_4362_anonymous_scalar_refinements_are_monotonic() {
        use crate::schema::{
            AttributeType, IntIdentityLayer, IntMemberConstraints, StringIdentityLayer,
            StringMemberConstraints,
        };
        for member in [false, true] {
            let string = |bounded: bool, patterned: bool| {
                let bounds = bounded.then_some((Some(1), Some(100)));
                let pattern = patterned.then(|| {
                    crate::schema::PatternSpec::new("^[A-Z]+$", crate::schema::PatternMatch::Full)
                });
                if member {
                    AttributeType::string_member(
                        StringIdentityLayer::new(None, None, None, None),
                        StringMemberConstraints::new(pattern, bounds),
                    )
                } else {
                    AttributeType::refined_string(None, pattern, bounds, None)
                }
            };
            let int = |bounds| {
                if member {
                    AttributeType::int_member(
                        IntIdentityLayer::new(None, None),
                        IntMemberConstraints::new(bounds),
                    )
                } else {
                    AttributeType::refined_int(None, bounds)
                }
            };
            let float = |bounds| AttributeType::refined_float(None, bounds);
            let integers = vec![AttributeType::int(), int(Some((Some(1), Some(100))))];
            let floats = vec![
                AttributeType::float(),
                float(Some((Some(crate::finite(1.0)), Some(crate::finite(100.0))))),
            ];
            let strings = vec![
                string(false, false),
                string(true, false),
                string(false, true),
                string(true, true),
            ];
            let mut sinks = vec![
                (AttributeType::string(), Expect::Assignable),
                (string(false, true), Expect::NeedsLocal),
                (
                    AttributeType::refined_string(None, None, Some((None, None)), None),
                    Expect::NeedsLocal,
                ),
            ];
            // Exercise each destination layer independently, as well as both together.
            for pattern in [false, true] {
                let pattern = pattern.then(|| {
                    crate::schema::PatternSpec::new("^[a-z]+$", crate::schema::PatternMatch::Full)
                });
                sinks.push((
                    AttributeType::string_member(
                        StringIdentityLayer::new(
                            None,
                            pattern.clone(),
                            Some((Some(1), Some(80))),
                            None,
                        ),
                        StringMemberConstraints::new(pattern, Some((Some(1), Some(47)))),
                    ),
                    Expect::NeedsLocal,
                ));
            }
            let int_sinks = vec![
                (AttributeType::int(), Expect::Assignable),
                (int(Some((None, None))), Expect::NeedsLocal),
                (int(Some((Some(1), Some(47)))), Expect::NeedsLocal),
                (int(Some((None, Some(47)))), Expect::NeedsLocal),
                (int(Some((Some(100), None))), Expect::NeedsLocal),
            ];
            let float_sinks = vec![
                (AttributeType::float(), Expect::Assignable),
                (float(Some((None, None))), Expect::NeedsLocal),
                (
                    float(Some((Some(crate::finite(1.0)), Some(crate::finite(47.0))))),
                    Expect::NeedsLocal,
                ),
                (
                    float(Some((None, Some(crate::finite(47.0))))),
                    Expect::NeedsLocal,
                ),
                (
                    float(Some((Some(crate::finite(100.0)), None))),
                    Expect::NeedsLocal,
                ),
            ];
            for (sources, sinks) in [
                (&strings, &sinks),
                (&integers, &int_sinks),
                (&floats, &float_sinks),
                (&integers, &float_sinks),
            ] {
                // The first source is unrefined; every later source overlaps every sink.
                for (sink, expected) in sinks {
                    for source in sources {
                        assert_anonymous_scalar_assignment(source, sink, *expected);
                    }
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
}
