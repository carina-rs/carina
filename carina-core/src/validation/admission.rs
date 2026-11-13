//! Admission to typed destinations. Only an admitted value can fill a slot.
//!
//! The compiling examples accompany each forbidden operation below.
//!
//! ```
//! use carina_core::validation::admission::{kind::ResourceAttribute, PlacedValue, TypedSlot};
//! fn place(slot: &mut TypedSlot<ResourceAttribute>, value: PlacedValue<ResourceAttribute>) {
//!     slot.place(value).unwrap();
//! }
//! ```
//! ```compile_fail,E0308
//! use carina_core::{resource::Value, validation::admission::{kind::ResourceAttribute, TypedSlot}};
//! fn place(slot: &mut TypedSlot<ResourceAttribute>, raw: Value) { slot.place(raw).unwrap(); }
//! ```
//!
//! ```
//! use carina_core::validation::admission::{kind::ResourceAttribute, Admission, TypedSlot};
//! fn place(slot: &mut TypedSlot<ResourceAttribute>, value: Admission<ResourceAttribute>) {
//!     if let Admission::Ready(value) = value { slot.place(value).unwrap(); }
//! }
//! ```
//! ```compile_fail,E0308
//! use carina_core::validation::admission::{kind::ResourceAttribute, PendingAdmission, TypedSlot};
//! fn place(slot: &mut TypedSlot<ResourceAttribute>, pending: PendingAdmission<ResourceAttribute>) {
//!     slot.place(pending).unwrap();
//! }
//! ```
//!
//! ```
//! use carina_core::validation::admission::{kind::ModuleArgument, PlacedValue, TypedSlot};
//! fn place(slot: &mut TypedSlot<ModuleArgument>, value: PlacedValue<ModuleArgument>) {
//!     slot.place(value).unwrap();
//! }
//! ```
//! ```compile_fail,E0308
//! use carina_core::validation::admission::{kind::{ModuleArgument, ResourceAttribute}, PlacedValue, TypedSlot};
//! fn place(slot: &mut TypedSlot<ResourceAttribute>, value: PlacedValue<ModuleArgument>) {
//!     slot.place(value).unwrap();
//! }
//! ```
//!
//! ```
//! use carina_core::validation::admission::{DestinationKind, kind::ResourceAttribute};
//! fn sealed<K: DestinationKind>() {}
//! sealed::<ResourceAttribute>();
//! ```
//! ```compile_fail,E0277
//! use carina_core::validation::admission::DestinationKind;
//! struct External;
//! impl DestinationKind for External { type LocalObligation = std::convert::Infallible; }
//! ```
//!
//! ```
//! use carina_core::validation::admission::{Destination, PlacedValue, TypedSlot, kind::ResourceAttribute};
//! fn use_receipt(destination: Destination<ResourceAttribute>, value: PlacedValue<ResourceAttribute>) {
//!     let mut slot = TypedSlot::new(destination);
//!     slot.place(value).unwrap();
//! }
//! ```
//! ```compile_fail,E0451
//! use carina_core::validation::admission::{PlacedValue, kind::ResourceAttribute};
//! fn forge() -> PlacedValue<ResourceAttribute> { PlacedValue { inner: unreachable!() } }
//! ```
//!
//! ```
//! use carina_core::{parser::ProviderContext, resource::ResourceId, schema::SchemaRegistry};
//! fn descriptor(registry: &SchemaRegistry, id: &ResourceId, catalog: &ProviderContext) {
//!     let _ = registry.resource_attribute_destination(id, "value", catalog);
//! }
//! ```
//! ```compile_fail,E0451
//! use carina_core::validation::admission::{Destination, kind::ResourceAttribute};
//! fn forge() -> Destination<ResourceAttribute> {
//!     Destination { identity: unreachable!(), label: String::new(), contract: None, kind: std::marker::PhantomData }
//! }
//! ```
//!
//! ```
//! use carina_core::validation::admission::{PendingAdmission, DeferredLocal, kind::ResourceAttribute};
//! fn deferred(value: &PendingAdmission<ResourceAttribute>) -> Option<&DeferredLocal<ResourceAttribute>> {
//!     value.local_obligation()
//! }
//! ```
//! ```compile_fail,E0308
//! use carina_core::validation::admission::{PendingAdmission, DeferredLocal, kind::{ResourceAttribute, ModuleArgument}};
//! fn bypass(value: &PendingAdmission<ModuleArgument>) -> Option<&DeferredLocal<ResourceAttribute>> {
//!     value.local_obligation()
//! }
//! ```
//!
//! A reference judgement cannot hand owned deferred work to external callers.
//!
//! ```compile_fail,E0599
//! use carina_core::validation::admission::{ReferenceCandidate, Destination, AdmissionContext, kind::ResourceAttribute};
//! fn bypass(reference: &ReferenceCandidate, destination: &Destination<ResourceAttribute>, context: &AdmissionContext<'_>) {
//!     let _ = reference.judge_assignment(destination, context);
//! }
//! ```
//!
//! Assignment verdicts are internal to admission; callers cannot construct
//! deferred work outside a pending admission.
//!
//! ```compile_fail,E0603
//! use carina_core::validation::admission::{AssignmentVerdict, DeferredLocal, kind::ResourceAttribute};
//! fn bypass(value: DeferredLocal<ResourceAttribute>) -> AssignmentVerdict<ResourceAttribute> {
//!     AssignmentVerdict::NeedsLocal(value)
//! }
//! ```
//!
//! Every destination kind retains the same resolved-value obligation.
//! Discharge has the same generic boundary.
//!
//! ```
//! use carina_core::validation::admission::{DestinationKind, PendingAdmission, kind};
//! fn hold<K: DestinationKind>(_: Option<PendingAdmission<K>>) {}
//! hold::<kind::ResourceAttribute>(None);
//! hold::<kind::DataSourceAttribute>(None);
//! hold::<kind::ProviderConfigAttribute>(None);
//! hold::<kind::BackendAttribute>(None);
//! hold::<kind::ModuleArgument>(None);
//! hold::<kind::AnnotatedAttribute>(None);
//! hold::<kind::Export>(None);
//! hold::<kind::UpstreamExport>(None);
//! ```

use std::marker::PhantomData;

use super::deferred_types::ProviderTypeCheckContext;
use crate::binding_index::{BindingIndex, ResolvedBindings};
use crate::parser::ProviderContext;
use crate::resolver::ResolutionError;
use crate::resource::knowledge::{
    Apply, Knowledge, KnowledgeContext, KnowledgeError, KnownValue, PendingSite, PlanResolved,
};
use crate::resource::{
    AbsoluteBindingAddress, BindingAddress, NoPlaceholder, PlaceholderKind, Value,
};
use crate::schema::{CompiledAttributeType, FieldAncestry, LeafPath, TypeError};

mod assignment;
pub mod attributes;
pub mod configuration;
pub mod final_input;
#[cfg(test)]
mod tests;

pub use crate::schema::value_validation::WalkCoverage;
use assignment::AssignmentVerdict;
pub(crate) use assignment::LocalPath;
pub use assignment::{AssignmentProof, ReferenceCandidate, SchemaObligation};

mod sealed {
    pub trait Sealed: Sized {
        fn local(
            destination: &super::Destination<Self>,
            paths: Vec<super::assignment::LocalPath>,
            schema: super::SchemaObligation,
        ) -> Result<<Self as super::DestinationKind>::LocalObligation, super::SchemaObligation>
        where
            Self: super::DestinationKind;
        fn defer_reference_check(path: &super::LeafPath) -> Option<super::assignment::LocalPath>;
        fn schema(
            obligation: &<Self as super::DestinationKind>::LocalObligation,
        ) -> &super::SchemaObligation
        where
            Self: super::DestinationKind;
        fn reference_assignment<'a, B: super::BindingAddress>(
            source: crate::schema::TypeInSchema<'a, crate::schema::Compiled>,
            sink: crate::schema::AssignmentSink<'a, crate::schema::Compiled>,
            reference: &super::ReferenceCandidate<B>,
        ) -> crate::schema::assignability::AssignmentRelation<'a>;
        fn reference_original_assignment<'a, B: super::BindingAddress>(
            source: crate::schema::TypeInSchema<'a, crate::schema::Compiled>,
            sink: crate::schema::AssignmentSink<'a, crate::schema::Compiled>,
            reference: &super::ReferenceCandidate<B>,
        ) -> crate::schema::assignability::AssignmentRelation<'a>;
    }
    pub trait Rechecks {}
}

pub trait DestinationKind: sealed::Sealed {
    type LocalObligation: std::fmt::Debug + Clone + PartialEq;
}

pub mod kind {
    macro_rules! rechecks {
        ($($(#[$meta:meta])* $name:ident),+ $(,)?) => {$(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub struct $name;
            impl super::sealed::Sealed for $name {
                fn local(destination: &super::Destination<Self>, paths: Vec<super::assignment::LocalPath>, schema: super::SchemaObligation) -> Result<<Self as super::DestinationKind>::LocalObligation, super::SchemaObligation> {
                    if paths.is_empty() {
                        return Err(schema);
                    }
                    Ok(super::DeferredLocal { destination: destination.clone(), paths: paths.into_iter().map(super::assignment::LocalPath::into_path).collect(), schema })
                }
                fn defer_reference_check(path: &super::LeafPath) -> Option<super::assignment::LocalPath> {
                    Some(super::assignment::LocalPath::for_recheck(Self, path.clone()))
                }
                fn schema(obligation: &<Self as super::DestinationKind>::LocalObligation) -> &super::SchemaObligation { &obligation.schema }
                fn reference_assignment<'a, B: super::BindingAddress>(
                    source: crate::schema::TypeInSchema<'a, crate::schema::Compiled>,
                    sink: crate::schema::AssignmentSink<'a, crate::schema::Compiled>,
                    reference: &super::ReferenceCandidate<B>,
                ) -> crate::schema::assignability::AssignmentRelation<'a> {
                    source.reference_assignment_to(sink, reference)
                }
                fn reference_original_assignment<'a, B: super::BindingAddress>(
                    source: crate::schema::TypeInSchema<'a, crate::schema::Compiled>,
                    sink: crate::schema::AssignmentSink<'a, crate::schema::Compiled>,
                    reference: &super::ReferenceCandidate<B>,
                ) -> crate::schema::assignability::AssignmentRelation<'a> { source.identified_reference_assignment_to(sink, reference) }
            }
            impl super::DestinationKind for $name { type LocalObligation = super::DeferredLocal<Self>; }
            impl super::sealed::Rechecks for $name {}
            impl super::RechecksResolved for $name {}
        )+};
    }
    rechecks!(
        ResourceAttribute,
        DataSourceAttribute,
        /// Provider fields consume only provider placements.
        ///
        /// ```
        /// use carina_core::validation::admission::{TypedSlot, PlacedValue, kind::ProviderConfigAttribute};
        /// fn place(slot: &mut TypedSlot<ProviderConfigAttribute>, value: PlacedValue<ProviderConfigAttribute>) { slot.place(value).unwrap(); }
        /// ```
        /// ```compile_fail
        /// use carina_core::validation::admission::{TypedSlot, PlacedValue, kind::{ProviderConfigAttribute, BackendAttribute}};
        /// fn place(slot: &mut TypedSlot<ProviderConfigAttribute>, value: PlacedValue<BackendAttribute>) {
        ///     slot.place(value).unwrap(); // forbidden: provider and backend placements have distinct kinds
        /// }
        /// ```
        /// ```
        /// # use carina_core::validation::admission::{TypedSlot, PlacedValue, kind::{ProviderConfigAttribute, BackendAttribute}};
        /// # fn place(slot: &mut TypedSlot<ProviderConfigAttribute>, value: PlacedValue<BackendAttribute>) {
        /// # }
        /// ```
        ProviderConfigAttribute,
        BackendAttribute,
        ModuleArgument,
        AnnotatedAttribute,
        Export,
        UpstreamExport
    );
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeferredLocal<K: DestinationKind> {
    destination: Destination<K>,
    paths: Vec<LeafPath>,
    schema: SchemaObligation,
}
impl<K: DestinationKind> DeferredLocal<K> {
    pub fn paths(&self) -> &[LeafPath] {
        &self.paths
    }
}

pub trait RechecksResolved:
    DestinationKind<LocalObligation = DeferredLocal<Self>> + sealed::Rechecks
{
}

/// System-assigned declaration identity, never a rendered or authored name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeclarationId(u64);
impl Default for DeclarationId {
    fn default() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        // Exhausting u64 would require over 584 years at a billion declarations
        // per second in one process, so wrapping is unreachable in practice.
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

#[derive(Clone, PartialEq, Eq)]
struct CatalogRevision(CatalogSnapshot);

#[derive(Clone)]
enum CatalogSnapshot {
    Provider(crate::parser::ProviderCustomTypes),
    Backend(DeclarationId),
}
impl CatalogSnapshot {
    // Resource assignment compares only provider snapshots. Backend admission
    // has its own declaration revision and never enters that resource path.
    fn same_snapshot(&self, catalog: &crate::parser::ProviderCustomTypes) -> bool {
        matches!(self, Self::Provider(snapshot) if snapshot.same_snapshot(catalog))
    }
}
impl PartialEq for CatalogSnapshot {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Provider(left), Self::Provider(right)) => left.same_snapshot(right),
            (Self::Backend(left), Self::Backend(right)) => left == right,
            _ => false,
        }
    }
}
impl Eq for CatalogSnapshot {}
impl std::fmt::Debug for CatalogRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CatalogRevision")
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeclarationOwner {
    Provider(crate::parser::SchemaTypeOwner),
    Backend(configuration::backend::BackendKind),
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeclarationIdentity {
    owner: DeclarationOwner,
    schema: DeclarationId,
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum InstanceIdentity {
    Resource(crate::resource::ResourceId),
    Provider(crate::provider::ProviderInstanceKey),
    Backend(configuration::backend::BackendKind),
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct DestinationIdentity {
    declaration: DeclarationIdentity,
    instance: InstanceIdentity,
    // Catalog ordinals identify slots; they are never diagnostic locations.
    path: Vec<usize>,
    revision: CatalogRevision,
}

#[derive(Debug, Clone)]
pub(crate) struct DestinationContract {
    schema: ContractSchema,
    attribute: String,
    owner: DeclarationOwner,
    ancestry: FieldAncestry,
    path: LeafPath,
}
#[derive(Debug, Clone)]
enum ContractSchema {
    Resource(std::sync::Arc<crate::schema::CompiledResourceSchema>),
    Configuration(std::sync::Arc<crate::schema::ValidatedConfigSchema>),
}
impl DestinationContract {
    fn resource(
        schema: std::sync::Arc<crate::schema::CompiledResourceSchema>,
        attribute: String,
        owner: crate::parser::SchemaTypeOwner,
        ancestry: FieldAncestry,
        path: LeafPath,
    ) -> Self {
        Self {
            schema: ContractSchema::Resource(schema),
            attribute,
            owner: DeclarationOwner::Provider(owner),
            ancestry,
            path,
        }
    }

    fn attr(&self) -> &CompiledAttributeType {
        match &self.schema {
            ContractSchema::Resource(schema) => &schema.attributes()[&self.attribute].attr_type,
            ContractSchema::Configuration(schema) => &schema.parts()[&self.attribute],
        }
    }

    fn defs(&self) -> &std::collections::BTreeMap<String, CompiledAttributeType> {
        match &self.schema {
            ContractSchema::Resource(schema) => schema.defs(),
            ContractSchema::Configuration(_) => crate::schema::empty_defs_for_schema_walks(),
        }
    }

    fn lookup<'a>(
        &'a self,
        catalog: &'a ProviderContext,
    ) -> impl crate::schema::ValidatedCustomTypeLookup + 'a {
        let DeclarationOwner::Provider(crate::parser::SchemaTypeOwner::Provider(owner)) =
            &self.owner
        else {
            unreachable!("backend configuration owns its builtin lookup")
        };
        crate::parser::custom_type_lookup_for_owner(catalog.custom_type_resolver(), owner)
    }

    fn sink_type<'a>(
        &'a self,
        attr: &'a CompiledAttributeType,
        catalog: &'a ProviderContext,
    ) -> crate::schema::AssignmentSink<'a, crate::schema::Compiled> {
        let DeclarationOwner::Provider(owner) = &self.owner else {
            // Built-in backends enter configuration admission, which consumes
            // concrete settings and never judges resource references.
            unreachable!("backend configuration has no resource assignment sink")
        };
        attr.in_schema(self.defs()).with_authority(
            &catalog.custom_types,
            crate::schema::SinkAuthority::Owner(owner),
        )
    }
}

/// Minted only by the owning catalog, binding declaration, instance and snapshot.
pub struct Destination<K: DestinationKind> {
    identity: Box<DestinationIdentity>,
    label: String,
    contract: Option<Box<DestinationContract>>,
    kind: PhantomData<K>,
}
impl<K: DestinationKind> PartialEq for Destination<K> {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}
impl<K: DestinationKind> Eq for Destination<K> {}
impl<K: DestinationKind> std::fmt::Debug for Destination<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Destination")
            .field(&self.label)
            .field(&self.identity)
            .finish()
    }
}
impl<K: DestinationKind> Clone for Destination<K> {
    fn clone(&self) -> Self {
        Self {
            identity: self.identity.clone(),
            label: self.label.clone(),
            contract: self.contract.clone(),
            kind: PhantomData,
        }
    }
}
impl<K: DestinationKind> Destination<K> {
    fn catalogued(identity: DestinationIdentity, label: String) -> Self {
        Self {
            identity: Box::new(identity),
            label,
            contract: None,
            kind: PhantomData,
        }
    }
}

/// A candidate is always paired with the exact proof consumed by its factory.
/// There is deliberately no raw-value constructor or expression accessor.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate<P: PlaceholderKind = NoPlaceholder, B: BindingAddress = AbsoluteBindingAddress>
{
    value: Value<P, B>,
    obligations: CandidateObligations,
}

/// Value replacement moves this whole bundle. Future evidence belongs here.
#[derive(Debug, Clone, PartialEq)]
struct CandidateObligations {
    identity: DeclarationId,
    schema: SchemaObligation,
}

enum AdmissionProof<'a, P: PlaceholderKind, B: BindingAddress> {
    Validation(crate::resource::knowledge::Classification<'a, P, B>),
    Plan(PlanResolved<'a, P, B>),
    Apply(KnownValue<'a, P, B>),
}

enum AdmissionLookup<'a> {
    Catalog,
    Configuration(&'a dyn crate::schema::ValidatedCustomTypeLookup),
}

/// Main's ordinary assignment work requires a current, expression-origin value.
/// Authored policy instead uses AssignmentCheck, including discarded positions.
type OrdinaryAssignmentCheck<'a, P, B> =
    &'a dyn Fn(
        &CompiledAttributeType,
        &Value<P, B>,
        &LeafPath,
        crate::parser::retained::AuthoredCursor<'_, B>,
    ) -> Result<crate::schema::value_validation::AssignmentFindings, TypeError>;

struct AssignmentChecks<'a, P: PlaceholderKind, B: BindingAddress> {
    authored: Option<crate::schema::value_validation::AssignmentCheck<'a, P, B>>,
    ordinary: Option<OrdinaryAssignmentCheck<'a, P, B>>,
}

impl<P: PlaceholderKind, B: BindingAddress> Default for AssignmentChecks<'_, P, B> {
    fn default() -> Self {
        Self {
            authored: None,
            ordinary: None,
        }
    }
}

pub struct AdmissionContext<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    identity: DeclarationId,
    proof: AdmissionProof<'a, P, B>,
    bindings: Option<&'a BindingIndex<'a, B>>,
    provider: &'a dyn ProviderTypeCheckContext,
    lookup: AdmissionLookup<'a>,
    assignment: AssignmentChecks<'a, P, B>,
    cursor: crate::parser::retained::AuthoredCursor<'a, B>,
    coverage: std::cell::RefCell<crate::schema::value_validation::WalkCoverage>,
    untyped: Option<&'a dyn Fn() -> Vec<TypeError>>,
    declaration: Option<(&'a str, &'a crate::binding_index::DeclaredValueSource)>,
}

impl<P: PlaceholderKind, B: BindingAddress> Candidate<P, B> {
    pub fn validation_classified<'a>(
        proof: crate::resource::knowledge::Classification<'a, P, B>,
        bindings: Option<&'a BindingIndex<'a, B>>,
        provider: &'a dyn ProviderTypeCheckContext,
    ) -> (Self, AdmissionContext<'a, P, B>) {
        let phase = crate::parser::retained::ProjectedPhase::classified(&proof);
        Self::validation_projected(proof, bindings, provider, phase)
    }

    fn validation_projected<'a>(
        proof: crate::resource::knowledge::Classification<'a, P, B>,
        bindings: Option<&'a BindingIndex<'a, B>>,
        provider: &'a dyn ProviderTypeCheckContext,
        phase: crate::parser::retained::ProjectedPhase,
    ) -> (Self, AdmissionContext<'a, P, B>) {
        let value = crate::resource::knowledge::ClassifiedRef::from(&proof)
            .diagnostic_value()
            .clone();
        Self::paired(
            value,
            AdmissionProof::Validation(proof),
            bindings,
            provider,
            crate::parser::retained::AuthoredCursor::projected(phase),
        )
    }

    pub fn plan<'a>(
        proof: PlanResolved<'a, P, B>,
        provider: &'a dyn ProviderTypeCheckContext,
    ) -> (Self, AdmissionContext<'a, P, B>) {
        let phase = crate::parser::retained::ProjectedPhase::plan(&proof);
        Self::paired(
            crate::resource::knowledge::ClassifiedRef::from(proof.knowledge())
                .diagnostic_value()
                .clone(),
            AdmissionProof::Plan(proof),
            None,
            provider,
            crate::parser::retained::AuthoredCursor::projected(phase),
        )
    }

    pub fn apply<'a>(
        proof: Knowledge<'a, P, B, Apply>,
        provider: &'a dyn ProviderTypeCheckContext,
    ) -> Result<(Self, AdmissionContext<'a, P, B>), AdmissionErrors> {
        let phase = crate::parser::retained::ProjectedPhase::apply(&proof);
        let proof = Self::known_at_apply(proof)?;
        Ok(Self::paired(
            proof.to_value(),
            AdmissionProof::Apply(proof),
            None,
            provider,
            crate::parser::retained::AuthoredCursor::projected(phase),
        ))
    }

    fn known_at_apply(
        proof: Knowledge<'_, P, B, Apply>,
    ) -> Result<KnownValue<'_, P, B>, AdmissionErrors> {
        let sites = proof.sites().collect();
        Ok(KnownValue::from_apply(proof).map_err(|error| match error {
            KnowledgeError::InvalidStage { operation: _ } => {
                KnowledgeError::UnresolvedAtBoundary { sites }
            }
            error @ (KnowledgeError::Unavailable { .. }
            | KnowledgeError::Cycle { .. }
            | KnowledgeError::UnavailableHistoricalSecret(_)
            | KnowledgeError::UnresolvedAtBoundary { .. }) => error,
        })?)
    }

    fn at_apply<'a>(
        self,
        proof: Knowledge<'a, P, B, Apply>,
        provider: &'a dyn ProviderTypeCheckContext,
    ) -> Result<(Self, AdmissionContext<'a, P, B>), AdmissionErrors> {
        let Self {
            value: _,
            obligations,
        } = self;
        let phase = crate::parser::retained::ProjectedPhase::apply(&proof);
        let proof = Self::known_at_apply(proof)?;
        let context = AdmissionContext {
            identity: obligations.identity,
            proof: AdmissionProof::Apply(proof.clone()),
            bindings: None,
            provider,
            lookup: AdmissionLookup::Catalog,
            assignment: Default::default(),
            cursor: crate::parser::retained::AuthoredCursor::projected(phase),
            coverage: Default::default(),
            untyped: None,
            declaration: None,
        };
        Ok((
            Self {
                value: proof.to_value(),
                obligations,
            },
            context,
        ))
    }

    fn paired<'a>(
        value: Value<P, B>,
        proof: AdmissionProof<'a, P, B>,
        bindings: Option<&'a BindingIndex<'a, B>>,
        provider: &'a dyn ProviderTypeCheckContext,
        cursor: crate::parser::retained::AuthoredCursor<'a, B>,
    ) -> (Self, AdmissionContext<'a, P, B>) {
        Self::paired_in(
            value,
            proof,
            bindings,
            provider,
            cursor,
            AdmissionLookup::Catalog,
        )
    }

    fn configuration<'a>(
        value: &'a Value<P, B>,
        provider: &'a dyn ProviderTypeCheckContext,
        lookup: &'a dyn crate::schema::ValidatedCustomTypeLookup,
        proof: crate::resource::knowledge::Classification<'a, P, B>,
    ) -> (Self, AdmissionContext<'a, P, B>) {
        let phase = crate::parser::retained::ProjectedPhase::classified(&proof);
        Self::paired_in(
            value.clone(),
            AdmissionProof::Validation(proof),
            None,
            provider,
            crate::parser::retained::AuthoredCursor::projected(phase),
            AdmissionLookup::Configuration(lookup),
        )
    }

    fn paired_in<'a>(
        value: Value<P, B>,
        proof: AdmissionProof<'a, P, B>,
        bindings: Option<&'a BindingIndex<'a, B>>,
        provider: &'a dyn ProviderTypeCheckContext,
        cursor: crate::parser::retained::AuthoredCursor<'a, B>,
        lookup: AdmissionLookup<'a>,
    ) -> (Self, AdmissionContext<'a, P, B>) {
        #[cfg(test)]
        tests::candidate_constructed();
        let identity = DeclarationId::default();
        (
            Self {
                obligations: CandidateObligations {
                    identity,
                    schema: SchemaObligation::default(),
                },
                value,
            },
            AdmissionContext {
                identity,
                proof,
                bindings,
                provider,
                lookup,
                assignment: Default::default(),
                cursor,
                coverage: Default::default(),
                untyped: None,
                declaration: None,
            },
        )
    }
}

impl<B: BindingAddress> Candidate<crate::resource::ParserPlaceholder, B> {
    /// Candidate, knowledge and authored cursor are sealed from one slot.
    /// The classification must borrow this slot's projection allocation;
    /// an equal value from another slot or generation is not its proof.
    pub fn validation_slot<'a>(
        slot: &'a crate::parser::retained::JudgedSlot<B>,
        proof: crate::resource::knowledge::Classification<
            'a,
            crate::resource::ParserPlaceholder,
            B,
        >,
        bindings: Option<&'a BindingIndex<'a, B>>,
        provider: &'a dyn ProviderTypeCheckContext,
    ) -> Result<
        (
            Self,
            AdmissionContext<'a, crate::resource::ParserPlaceholder, B>,
        ),
        AdmissionErrors,
    > {
        let value = crate::resource::knowledge::ClassifiedRef::from(&proof).diagnostic_value();
        if !slot
            .projection()
            .is_some_and(|projection| std::ptr::eq(projection, value))
        {
            return Err(seam_constraint(
                crate::parser::retained::MissingEvidence::new("slot/projection proof pairing"),
            ));
        }
        let cursor = crate::parser::retained::AuthoredCursor::authored(
            slot.cursor().map_err(seam_constraint)?,
        );
        Ok(Self::paired(
            value.clone(),
            AdmissionProof::Validation(proof),
            bindings,
            provider,
            cursor,
        ))
    }
}

impl<'a, P: PlaceholderKind, B: BindingAddress> AdmissionContext<'a, P, B> {
    fn with_checks<'s>(
        self,
        assignment: AssignmentChecks<'s, P, B>,
        untyped: &'s dyn Fn() -> Vec<TypeError>,
    ) -> AdmissionContext<'s, P, B>
    where
        'a: 's,
    {
        let mut context: AdmissionContext<'s, P, B> = self;
        context.assignment = assignment;
        context.untyped = Some(untyped);
        context
    }

    fn sites(&self) -> Vec<PendingSite<B>> {
        match &self.proof {
            AdmissionProof::Validation(proof) => proof.sites().collect(),
            AdmissionProof::Plan(proof) => proof.knowledge().sites().collect(),
            AdmissionProof::Apply(_) => vec![],
        }
    }
    fn is_apply(&self) -> bool {
        matches!(self.proof, AdmissionProof::Apply(_))
    }
}

pub struct PlacedValue<
    K: DestinationKind,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    inner: Box<PlacedInner<K, P, B>>,
}
struct PlacedInner<K: DestinationKind, P: PlaceholderKind, B: BindingAddress> {
    findings: crate::schema::ValueValidationFindings,
    destination: Destination<K>,
    value: Value<P, B>,
}
impl<K: DestinationKind, P: PlaceholderKind, B: BindingAddress> Clone for PlacedValue<K, P, B> {
    fn clone(&self) -> Self {
        Self {
            inner: Box::new(PlacedInner {
                findings: self.inner.findings.clone(),
                destination: self.inner.destination.clone(),
                value: self.inner.value.clone(),
            }),
        }
    }
}
impl<K: DestinationKind, P: PlaceholderKind, B: BindingAddress> std::fmt::Debug
    for PlacedValue<K, P, B>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlacedValue")
            .field("destination", &self.inner.destination)
            .field("value", &self.inner.value)
            .finish()
    }
}
impl<K: DestinationKind, P: PlaceholderKind, B: BindingAddress> PartialEq for PlacedValue<K, P, B> {
    fn eq(&self, other: &Self) -> bool {
        self.inner.destination == other.inner.destination && self.inner.value == other.inner.value
    }
}
impl<K: DestinationKind, P: PlaceholderKind, B: BindingAddress> PlacedValue<K, P, B> {
    fn admitted(
        destination: &Destination<K>,
        value: Value<P, B>,
        findings: crate::schema::ValueValidationFindings,
    ) -> Self {
        Self {
            inner: Box::new(PlacedInner {
                findings,
                destination: destination.clone(),
                value,
            }),
        }
    }

    pub(crate) fn take_findings(&mut self) -> crate::schema::ValueValidationFindings {
        std::mem::take(&mut self.inner.findings)
    }

    pub fn value(&self) -> &Value<P, B> {
        &self.inner.value
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingAdmission<
    K: DestinationKind,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    destination: Destination<K>,
    inner: Box<PendingCandidate<K, P, B>>,
}
#[derive(Debug, Clone, PartialEq)]
struct PendingCandidate<K: DestinationKind, P: PlaceholderKind, B: BindingAddress> {
    findings: crate::schema::ValueValidationFindings,
    candidate: Candidate<P, B>,
    assignment: AssignmentVerdict<K>,
    sites: Vec<PendingSite<B>>,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Admission<
    K: DestinationKind,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    Ready(PlacedValue<K, P, B>),
    Pending(PendingAdmission<K, P, B>),
}

/// Reference lookup failures retain their original structured information.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionReferenceError {
    #[error("{0}")]
    Authored(Box<crate::validation::ReferenceFinding>),
    #[error("unknown attribute inside secret(...)")]
    ProtectedAttribute,
    #[error("unknown binding inside secret(...)")]
    ProtectedBinding,
    #[error("{0}")]
    Computed(crate::validation::DeferredAccessTypeError),
    #[error("{0}")]
    Arithmetic(crate::parser::Located<crate::validation::IntegerArithmeticTypeFinding>),
    #[error("{0}")]
    Resolution(Box<ResolutionError>),
    #[error("unknown binding '{binding}' in reference {binding}.{attribute}")]
    UnknownBinding { binding: String, attribute: String },
    #[error("{0}")]
    Path(crate::binding_index::RefTypeError),
    #[error("'{reference}' is a resource handle, not a value")]
    ResourceHandle {
        reference: String,
        types: crate::schema::MismatchTypeNames,
    },
}

/// A typed finding retains the original error and a canonical value location.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmissionFinding {
    pub path: LeafPath,
    pub error: TypeError,
}
#[derive(Debug, Clone, PartialEq)]
pub struct AdmissionFindings {
    findings: Vec<AdmissionFinding>,
}
// TypeError carries diagnostic metadata, not floating-point values or schema
// callbacks. Its structural equality is reflexive, including protected errors.
impl Eq for AdmissionFindings {}
impl std::error::Error for AdmissionFindings {}
impl crate::resource::InsideSecret for AdmissionFindings {
    fn inside_secret(self) -> Self {
        Self {
            findings: self
                .findings
                .into_iter()
                .map(|finding| AdmissionFinding {
                    path: finding.path.inside_secret(),
                    error: finding.error.inside_secret(),
                })
                .collect(),
        }
    }
}
impl AdmissionFindings {
    pub fn findings(&self) -> &[AdmissionFinding] {
        &self.findings
    }
}
impl std::fmt::Display for AdmissionFindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, finding) in self.findings.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            // Field-group errors retain their legacy located path for other
            // renderers. This renderer already has the canonical full path.
            if let TypeError::FieldGroupViolation { group, .. } = &finding.error {
                write!(
                    f,
                    "{}: {}",
                    finding.path,
                    TypeError::FieldGroupViolation {
                        path: crate::schema::FieldPath::new(),
                        group: group.clone(),
                    }
                )?;
            } else {
                write!(f, "{}: {}", finding.path, finding.error)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{operation} admission failed")] // error-chain: caller-renders-sources
pub struct AdmissionErrors {
    findings: Option<Box<crate::schema::ValueValidationFindings>>,
    operation: AdmissionOperation,
    #[source]
    source: ResolutionError,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionOperation {
    Value,
    Conversion,
}
impl std::fmt::Display for AdmissionOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Value => "value",
            Self::Conversion => "conversion",
        })
    }
}
impl PartialEq for AdmissionErrors {
    fn eq(&self, other: &Self) -> bool {
        self.operation == other.operation && self.source == other.source
    }
}
impl Eq for AdmissionErrors {}
impl From<ResolutionError> for AdmissionErrors {
    fn from(source: ResolutionError) -> Self {
        Self {
            operation: AdmissionOperation::Value,
            findings: None,
            source,
        }
    }
}
impl<B: BindingAddress> From<KnowledgeError<B>> for AdmissionErrors {
    fn from(error: KnowledgeError<B>) -> Self {
        ResolutionError::from(error).into()
    }
}
impl From<AdmissionErrors> for ResolutionError {
    fn from(error: AdmissionErrors) -> Self {
        error.into_resolution_error()
    }
}
impl crate::resource::InsideSecret for AdmissionErrors {
    fn inside_secret(self) -> Self {
        Self {
            operation: self.operation,
            findings: self
                .findings
                .map(|findings| Box::new((*findings).inside_secret())),
            source: self.source.inside_secret(),
        }
    }
}
pub(crate) fn assignment_seam_error(error: crate::parser::retained::MissingEvidence) -> TypeError {
    assignment::reference_error(AdmissionReferenceError::Computed(
        crate::validation::DeferredAccessTypeError::InvalidAccess {
            error: error.into(),
        },
    ))
}

pub(crate) fn seam_constraint(error: crate::parser::retained::MissingEvidence) -> AdmissionErrors {
    constraints(vec![AdmissionFinding {
        path: LeafPath::from_segments([]),
        error: assignment_seam_error(error),
    }])
}

fn join(root: &LeafPath, relative: &LeafPath) -> LeafPath {
    LeafPath::from_segments(root.segments().iter().chain(relative.segments()).cloned())
}
fn constraints(findings: Vec<AdmissionFinding>) -> AdmissionErrors {
    ResolutionError::AdmissionConstraints(Box::new(AdmissionFindings { findings })).into()
}

impl AdmissionErrors {
    pub(crate) fn into_resolution_error(self) -> ResolutionError {
        self.source
    }

    pub(crate) fn take_findings(&mut self) -> Option<crate::schema::ValueValidationFindings> {
        self.findings.take().map(|findings| *findings)
    }
    /// Preserve #4260's pinned error-chain label when conversion orchestration
    /// rebases onto this shared wrapper. This grants no placement authority.
    pub fn conversion(source: ResolutionError) -> Self {
        Self {
            operation: AdmissionOperation::Conversion,
            findings: None,
            source,
        }
    }
    pub fn resolution_error(&self) -> &ResolutionError {
        &self.source
    }
}

/// Admit a proof-paired candidate. Its actual value determines whether reference
/// assignment applies; a raw value cannot supply reference-origin evidence.
///
/// ```
/// use carina_core::validation::admission::{admit, Candidate, Destination, AdmissionContext, kind::ResourceAttribute};
/// fn inspect(value: Candidate, destination: &Destination<ResourceAttribute>, context: &AdmissionContext<'_>) {
///     let _ = admit(destination, value, context);
/// }
/// ```
/// ```compile_fail,E0308
/// use carina_core::{resource::Value, validation::admission::{admit, Destination, AdmissionContext, kind::ResourceAttribute}};
/// fn bypass(value: Value, destination: &Destination<ResourceAttribute>, context: &AdmissionContext<'_>) {
///     let _ = admit(destination, value, context);
/// }
/// ```
///
/// An arbitrary candidate also cannot mint the evidence required by the
/// relaxed reference relation.
///
/// ```compile_fail,E0277
/// use carina_core::validation::admission::{admit, Candidate, ReferenceCandidate, Destination, AdmissionContext, kind::ResourceAttribute};
/// fn bypass(value: Candidate, destination: &Destination<ResourceAttribute>, context: &AdmissionContext<'_>) {
///     let _: ReferenceCandidate = value.clone().into();
///     let _ = admit(destination, value, context);
/// }
/// ```
pub fn admit<K: DestinationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    candidate: Candidate<P, B>,
    context: &AdmissionContext<'_, P, B>,
) -> Result<Admission<K, P, B>, AdmissionErrors> {
    if let AdmissionLookup::Configuration(lookup) = context.lookup {
        return configuration::admit_field(destination, candidate, context, lookup);
    }
    if context.is_apply() {
        return admit_apply(destination, candidate, context).map(Admission::Ready);
    }
    let assignment::AssignmentJudgement { verdict, findings } =
        assignment::judge_with_findings(destination, &candidate, context)?;
    let sites = context.sites();
    match verdict {
        AssignmentVerdict::Proven(_) if sites.is_empty() => Ok(Admission::Ready(
            PlacedValue::admitted(destination, candidate.value, findings),
        )),
        assignment @ (AssignmentVerdict::Proven(_)
        | AssignmentVerdict::NeedsLocal(_)
        | AssignmentVerdict::NeedsSchema(_)) => {
            // Every unproven verdict remains owned work, even with no pending
            // sites. Apply must prove assignment after checking the known value.
            Ok(Admission::Pending(PendingAdmission {
                destination: destination.clone(),
                inner: Box::new(PendingCandidate {
                    findings,
                    candidate,
                    assignment,
                    sites,
                }),
            }))
        }
    }
}

/// Apply can only produce placement or an error; pending work cannot escape.
fn admit_apply<K: DestinationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    candidate: Candidate<P, B>,
    context: &AdmissionContext<'_, P, B>,
) -> Result<PlacedValue<K, P, B>, AdmissionErrors> {
    if !context.is_apply() {
        return Err(KnowledgeError::<B>::InvalidStage {
            operation: "apply admission",
        }
        .into());
    }
    let assignment::AssignmentJudgement { verdict, findings } =
        assignment::judge_with_findings(destination, &candidate, context)?;
    match verdict {
        AssignmentVerdict::Proven(_) => Ok(PlacedValue::admitted(
            destination,
            candidate.value,
            findings,
        )),
        AssignmentVerdict::NeedsLocal(_) | AssignmentVerdict::NeedsSchema(_) => {
            Err(KnowledgeError::<B>::UnresolvedAtBoundary {
                sites: context.sites(),
            }
            .into())
        }
    }
}

impl<K: DestinationKind, P: PlaceholderKind, B: BindingAddress> PendingAdmission<K, P, B> {
    /// Resolve and prove pending work at apply. Success is ready for placement.
    ///
    /// ```
    /// use carina_core::{binding_index::ResolvedBindings, validation::{
    ///     admission::{AdmissionErrors, DestinationKind, PendingAdmission, PlacedValue},
    ///     deferred_types::ProviderTypeCheckContext,
    /// }};
    /// fn discharge<K: DestinationKind>(
    ///     pending: PendingAdmission<K>,
    ///     resolved: &ResolvedBindings,
    ///     context: &dyn ProviderTypeCheckContext,
    /// ) -> Result<PlacedValue<K>, AdmissionErrors> {
    ///     pending.discharge(resolved, context)
    /// }
    /// ```
    pub fn discharge(
        self,
        resolved: &ResolvedBindings<P, B>,
        context: &dyn ProviderTypeCheckContext,
    ) -> Result<PlacedValue<K, P, B>, AdmissionErrors> {
        // Ordinary resolution changes the projection, so it cannot retain a
        // previous placement receipt. The sole apply proof re-enters admission.
        let value =
            crate::resolver::resolve_ref_value(&self.inner.candidate.value, resolved, context)?;
        let mut candidate = self.inner.candidate;
        match self.inner.assignment {
            AssignmentVerdict::Proven(_) => {}
            AssignmentVerdict::NeedsLocal(local) => candidate
                .obligations
                .schema
                .extend(K::schema(&local).clone()),
            AssignmentVerdict::NeedsSchema(schema) => candidate.obligations.schema.extend(schema),
        }
        let (candidate, boundary) = candidate.at_apply(
            value.knowledge(&KnowledgeContext::apply(resolved))?,
            context,
        )?;
        admit_apply(&self.destination, candidate, &boundary)
    }
    pub fn sites(&self) -> &[PendingSite<B>] {
        &self.inner.sites
    }
    pub fn local_obligation(&self) -> Option<&K::LocalObligation> {
        match &self.inner.assignment {
            AssignmentVerdict::NeedsLocal(local) => Some(local),
            AssignmentVerdict::Proven(_) | AssignmentVerdict::NeedsSchema(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SlotMismatch {
    #[error("placement belongs to another declaration")]
    Declaration,
    #[error("placement belongs to another instance")]
    Instance,
    #[error("placement belongs to another schema path")]
    Path,
    #[error("placement belongs to another catalog revision")]
    CatalogRevision,
}

/// A placement consumer: neither raw nor pending values can be placed here.
#[derive(Debug)]
pub struct TypedSlot<
    K: DestinationKind,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    destination: Destination<K>,
    value: Option<PlacedValue<K, P, B>>,
}
impl<K: DestinationKind, P: PlaceholderKind, B: BindingAddress> TypedSlot<K, P, B> {
    pub fn new(destination: Destination<K>) -> Self {
        Self {
            destination,
            value: None,
        }
    }
    pub fn place(&mut self, value: PlacedValue<K, P, B>) -> Result<(), SlotMismatch> {
        let expected = &self.destination.identity;
        let actual = &value.inner.destination.identity;
        let mismatch = if expected.revision != actual.revision {
            Some(SlotMismatch::CatalogRevision)
        } else if expected.instance != actual.instance {
            Some(SlotMismatch::Instance)
        } else if expected.declaration != actual.declaration {
            Some(SlotMismatch::Declaration)
        } else if expected.path != actual.path {
            Some(SlotMismatch::Path)
        } else {
            None
        };
        if let Some(error) = mismatch {
            return Err(error);
        }
        self.value = Some(value);
        Ok(())
    }
    pub fn value(&self) -> Option<&Value<P, B>> {
        self.value.as_ref().map(PlacedValue::value)
    }
}
impl<K: DestinationKind, P: PlaceholderKind, B: BindingAddress> Admission<K, P, B> {
    pub(crate) fn take_findings(&mut self) -> crate::schema::ValueValidationFindings {
        match self {
            Self::Ready(placed) => placed.take_findings(),
            Self::Pending(pending) => std::mem::take(&mut pending.inner.findings),
        }
    }

    pub fn ready(self) -> Result<PlacedValue<K, P, B>, PendingAdmission<K, P, B>> {
        match self {
            Self::Ready(value) => Ok(value),
            Self::Pending(pending) => Err(pending),
        }
    }
}
