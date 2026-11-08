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
            binding,
            pattern: _,
            length: _,
            to_dsl: _,
        }
        | AttrTypeKind::Int { binding, range: _ }
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
                binding: _,
                pattern: _,
                length: _,
                to_dsl: _,
            }
            | AttrTypeKind::ResourceHandle {
                provider: _,
                resource_type: _,
            }
            | AttrTypeKind::StringLiteralSet { values: _ }
            | AttrTypeKind::Int {
                binding: _,
                range: _,
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

fn nominal_scalar_constraints<'a, R: PatternForm>(
    ty: TypeInSchema<'a, R>,
    refs: &mut Vec<&'a str>,
) -> Option<NominalScalarConstraints> {
    let resolved = resolve_refs_on_path(ty.attr, ty.defs, refs).ok()?;
    let constraints = match &resolved.attr.kind {
        AttrTypeKind::String {
            binding: _,
            pattern: _,
            length,
            to_dsl: _,
        } => Some(NominalScalarConstraints::String(*length)),
        AttrTypeKind::Int { binding: _, range } => Some(NominalScalarConstraints::Int(*range)),
        AttrTypeKind::Float { binding: _, range } => Some(NominalScalarConstraints::Float(*range)),
        AttrTypeKind::Enum {
            identity: _,
            base,
            values: _,
            dsl_aliases: _,
            validate: _,
            to_dsl: _,
        } => nominal_scalar_constraints(base.in_schema(ty.defs), refs),
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

/// The shared relation retains local obligations without allowing them into
/// the strict boolean/record_unchecked adapters.
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
    fn strict(self) -> TypeAssignability<'s> {
        match self {
            Self::Assignable => TypeAssignability::Assignable,
            Self::Mismatch => TypeAssignability::Mismatch,
            Self::Unchecked(schema) => TypeAssignability::Unchecked(schema),
            Self::NeedsLocal(_) => {
                unreachable!("strict assignment never creates local obligations")
            }
        }
    }
    fn from_bool(value: bool) -> Self {
        if value {
            Self::Assignable
        } else {
            Self::Mismatch
        }
    }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum AssignmentCheck {
    Full,
    /// Re-admit original identities at a declared value's later sink. Only
    /// identified leaves need this proof; their rule-7 containment still applies.
    IdentifiedLeaves,
    Reference,
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
        source
            .assignment_on_paths(
                sink,
                AssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .strict()
    }

    /// Recheck original identity evidence after a value declaration has passed
    /// full assignment. An earlier owner's edge is not proof for this sink.
    /// Identity-less leaves keep the declaration's deferred value checks;
    /// identified leaves retain ordinary identity admission and containment.
    fn identity_assignment_to<'query>(
        self,
        sink: AssignmentSink<'query, R>,
    ) -> TypeAssignability<'query>
    where
        'source: 'query,
    {
        let source: TypeInSchema<'query, R> = self;
        source
            .assignment_on_paths(
                sink,
                AssignmentCheck::IdentifiedLeaves,
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .strict()
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
            AssignmentCheck::Reference,
            &mut Vec::new(),
            &mut Vec::new(),
        )
    }

    fn assignment_on_paths<'sink>(
        self,
        sink: AssignmentSink<'sink, R>,
        check: AssignmentCheck,
        source_refs: &mut Vec<&'source str>,
        sink_refs: &mut Vec<&'sink str>,
    ) -> AssignmentRelation<'source>
    where
        'sink: 'source,
    {
        let Ok(source) = resolve_refs_on_path(self.attr, self.defs, source_refs) else {
            return AssignmentRelation::Mismatch;
        };
        let Ok(target) = resolve_refs_on_path(sink.attr, sink.defs, sink_refs) else {
            source_refs.truncate(source.restore_len);
            return AssignmentRelation::Mismatch;
        };
        let left = source.attr.in_schema(self.defs);
        let right = sink.rebind(target.attr);
        use AssignmentRelation::*;
        use AttrTypeKind::*;
        let same_identity = check == AssignmentCheck::Reference
            && identity_leaf(&source.attr.kind)
                .and_then(|leaf| leaf.identity())
                .zip(identity_leaf(&target.attr.kind).and_then(|leaf| leaf.identity()))
                .is_some_and(|(source, target)| source == target);
        if same_identity {
            source_refs.truncate(source.restore_len);
            sink_refs.truncate(target.restore_len);
            return NeedsLocal(None);
        }
        let result = match (&source.attr.kind, &target.attr.kind) {
            (_, _)
                if source.attr.has_empty_scalar_intersection()
                    || target.attr.has_empty_scalar_intersection() =>
            {
                Mismatch
            }
            (_, kind)
                if check == AssignmentCheck::IdentifiedLeaves
                    && identity_leaf(kind).is_none()
                    && !matches!(kind, List { .. } | Map { .. } | Struct { .. } | Union(_)) =>
            {
                Assignable
            }
            (
                Union(members),
                String {
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
            ) => AssignmentRelation::all(members.as_raw_slice().iter().map(|member| {
                member.in_schema(self.defs).assignment_on_paths(
                    right,
                    check,
                    source_refs,
                    sink_refs,
                )
            })),
            (
                String {
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
            ) => AssignmentRelation::any(members.as_raw_slice().iter().map(|member| {
                left.assignment_on_paths(sink.rebind(member), check, source_refs, sink_refs)
            })),
            (
                UnloadedSchemaType(unloaded),
                String {
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
                        AssignmentRelation::from_bool(
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
            ) => AssignmentRelation::all([
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
                    AssignmentRelation::all(right.iter().map(|field| {
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
            ) => AssignmentRelation::all(fields.iter().map(|field| {
                field.field_type.in_schema(self.defs).assignment_on_paths(
                    sink.rebind(value),
                    check,
                    source_refs,
                    sink_refs,
                )
            })),
            (
                String {
                    binding: source,
                    pattern: _,
                    length: source_length,
                    to_dsl: _,
                },
                String {
                    binding: target,
                    pattern: _,
                    length: sink_length,
                    to_dsl: _,
                },
            ) if let (Some(source), Some(_)) = (source.identity(), target.identity()) => {
                AssignmentRelation::all([
                    right
                        .identity_assignment(source, IdentityLeaf::Scalar(target))
                        .into(),
                    AssignmentRelation::from_bool(super::length_contains(
                        source_length.as_ref(),
                        sink_length.as_ref(),
                    )),
                ])
            }
            (
                Int {
                    binding: source,
                    range: source_range,
                },
                Int {
                    binding: target,
                    range: sink_range,
                },
            ) if let (Some(source), Some(_)) = (source.identity(), target.identity()) => {
                AssignmentRelation::all([
                    right
                        .identity_assignment(source, IdentityLeaf::Scalar(target))
                        .into(),
                    AssignmentRelation::from_bool(super::i64_range_contains(
                        source_range.as_ref(),
                        sink_range.as_ref(),
                    )),
                ])
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
                AssignmentRelation::all([
                    right
                        .identity_assignment(source, IdentityLeaf::Scalar(target))
                        .into(),
                    AssignmentRelation::from_bool(super::f64_range_contains(
                        source_range.as_ref(),
                        sink_range.as_ref(),
                    )),
                ])
            }
            (
                Int {
                    binding: source,
                    range: source_range,
                },
                Float {
                    binding: target,
                    range: sink_range,
                },
            ) if target.identity().is_some() => match source.identity() {
                Some(source) => AssignmentRelation::all([
                    right
                        .identity_assignment(source, IdentityLeaf::Scalar(target))
                        .into(),
                    AssignmentRelation::from_bool(super::i64_range_assignable_to_f64(
                        source_range.as_ref(),
                        sink_range.as_ref(),
                    )),
                ]),
                None => Mismatch,
            },
            (
                String {
                    binding: source,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: source,
                    range: _,
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
            ) if let Some(source) = source.identity() => AssignmentRelation::all([
                AssignmentRelation::from_bool(source.structurally_assignable_to(target)),
                AssignmentRelation::from_bool(
                    match (
                        nominal_scalar_constraints(left, source_refs),
                        nominal_scalar_constraints(right.ty, sink_refs),
                    ) {
                        (Some(source), Some(sink)) => source.contained_by(sink),
                        (None, Some(_) | None) | (Some(_), None) => false,
                    },
                ),
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
                    binding: target,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: target,
                    range: _,
                }
                | Float {
                    binding: target,
                    range: _,
                },
            ) if target.identity().is_some() => AssignmentRelation::all([
                AssignmentRelation::from_bool(
                    target
                        .identity()
                        .is_some_and(|target| source.structurally_assignable_to(target)),
                ),
                AssignmentRelation::from_bool(
                    match (
                        nominal_scalar_constraints(left, source_refs),
                        nominal_scalar_constraints(right.ty, sink_refs),
                    ) {
                        (Some(source), Some(sink)) => source.contained_by(sink),
                        (None, Some(_) | None) | (Some(_), None) => false,
                    },
                ),
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
            ) => AssignmentRelation::all([
                AssignmentRelation::from_bool(source.structurally_assignable_to(target)),
                source_base.in_schema(self.defs).assignment_on_paths(
                    sink.rebind(sink_base),
                    check,
                    source_refs,
                    sink_refs,
                ),
            ]),
            (
                String {
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
            ) => AssignmentRelation::from_bool(left.is_assignable_to_known(right)),
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
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
            ) => AssignmentRelation::from_bool(left.is_assignable_to_known(right)),
            (
                List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                },
                String {
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
            ) => AssignmentRelation::from_bool(left.is_assignable_to_known(right)),
            (
                Map {
                    key: _,
                    value: _,
                    constraints: _,
                },
                String {
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
            ) => AssignmentRelation::from_bool(left.is_assignable_to_known(right)),
            (
                Struct {
                    name: _,
                    fields: _,
                    field_groups: _,
                },
                String {
                    binding: _,
                    pattern: _,
                    length: _,
                    to_dsl: _,
                }
                | Int {
                    binding: _,
                    range: _,
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
            ) => AssignmentRelation::from_bool(left.is_assignable_to_known(right)),
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
}
