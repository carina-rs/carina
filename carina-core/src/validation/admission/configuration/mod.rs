//! Declaration-owned admission for configuration payloads.
//!
//! The payload retains its declared field placements and explicitly named
//! opaque factory-owned passthrough. There is no constructor accepting a
//! checked marker and a replacement map.

use std::marker::PhantomData;
use std::sync::Arc;

use super::*;
use crate::parser::ConfigurationAttributes;
use crate::provider::{ProviderInstanceKey, ProviderWireTarget, WireAttributes, WireReady};
use crate::schema::{ValidatedConfigSchema, ValidatedCustomTypeLookup};

pub mod backend;
pub mod provider;

/// The two sealed configuration destinations and their exact payloads.
/// Backend settings do not acquire a provider transport or provider identity.
pub trait ConfigurationKind: DestinationKind {
    type Payload: std::fmt::Debug + Clone;
    type Instance: std::fmt::Debug + Clone + PartialEq;
    #[doc(hidden)]
    fn attributes(payload: &Self::Payload) -> &ConfigurationAttributes<Value>;
}

impl ConfigurationKind for kind::ProviderConfigAttribute {
    type Payload = WireReady<ConfigurationAttributes<Value>>;
    type Instance = ProviderInstanceKey;
    fn attributes(payload: &Self::Payload) -> &ConfigurationAttributes<Value> {
        payload.as_inner()
    }
}

impl ConfigurationKind for kind::BackendAttribute {
    type Payload = ConfigurationAttributes<Value>;
    type Instance = backend::BackendKind;
    fn attributes(payload: &Self::Payload) -> &ConfigurationAttributes<Value> {
        payload
    }
}

#[derive(Debug)]
struct ConfigurationDeclaration {
    identity: DeclarationId,
    owner: DeclarationOwner,
    schema: Arc<ValidatedConfigSchema>,
}

mod aggregate;
pub use aggregate::AdmittedConfiguration;
use aggregate::ConfigurationBuilder;

impl ConfigurationDeclaration {
    fn provider(owner: &str, schema: ValidatedConfigSchema) -> Arc<Self> {
        Arc::new(Self {
            identity: DeclarationId::default(),
            owner: DeclarationOwner::Provider(crate::parser::SchemaTypeOwner::Provider(
                owner.to_owned(),
            )),
            schema: Arc::new(schema),
        })
    }

    fn destination<K: ConfigurationKind>(
        &self,
        key: &crate::parser::AttributeKey,
        instance: InstanceIdentity,
        revision: CatalogRevision,
    ) -> Option<Destination<K>> {
        let (ordinal, (name, _)) = self
            .schema
            .parts()
            .iter()
            .enumerate()
            .find(|(_, (name, _))| key.matches(name))?;
        Some(Destination {
            identity: Box::new(DestinationIdentity {
                declaration: DeclarationIdentity {
                    owner: self.owner.clone(),
                    schema: self.identity,
                },
                instance,
                path: vec![ordinal],
                revision,
            }),
            label: key.to_string(),
            contract: Some(Box::new(DestinationContract {
                schema: ContractSchema::Configuration(Arc::clone(&self.schema)),
                attribute: name.clone(),
                owner: self.owner.clone(),
                ancestry: FieldAncestry::WRITABLE,
                path: LeafPath::from_segments([]),
            })),
            kind: PhantomData,
        })
    }
}

/// Configuration is a concrete-value boundary, not a resource assignment.
/// It uses the same declaration walk with final-input callback policy, including
/// compact lists. Resource authored/reference policy remains with its owner.
pub(super) fn admit_field<K: DestinationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    candidate: Candidate<P, B>,
    context: &AdmissionContext<'_, P, B>,
    lookup: &dyn ValidatedCustomTypeLookup,
) -> Result<Admission<K, P, B>, AdmissionErrors> {
    let contract = destination
        .contract
        .as_ref()
        .expect("configuration destination contract");
    let AdmissionProof::Validation(proof) = &context.proof else {
        unreachable!("configuration context is constructed from one classification")
    };
    let checked = crate::schema::value_validation::validate_final_provider_value(
        contract.attr(),
        proof,
        contract.defs(),
        contract.ancestry,
        lookup,
    );
    // Keep rootless configuration's established primary-local-error policy.
    let findings = match checked.local_error() {
        Some(error) => vec![AdmissionFinding {
            path: contract.path.clone(),
            error: error.clone(),
        }],
        None => checked
            .clone()
            .into_located()
            .into_iter()
            .map(|(path, error)| AdmissionFinding { path, error })
            .collect(),
    };
    if !findings.is_empty() {
        let mut error = constraints(findings);
        error.findings = Some(Box::new(checked));
        return Err(error);
    }
    let sites = context.sites();
    if sites.is_empty() {
        Ok(Admission::Ready(PlacedValue::admitted(
            destination,
            candidate.value,
            checked,
        )))
    } else {
        // Knownness is the outstanding work; discharge always re-enters this
        // destination's full declaration with the newly known value.
        Ok(Admission::Pending(PendingAdmission {
            destination: destination.clone(),
            inner: Box::new(PendingCandidate {
                candidate,
                findings: checked,
                assignment: AssignmentVerdict::NeedsSchema(SchemaObligation::default()),
                sites,
            }),
        }))
    }
}

fn field<K: ConfigurationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    value: &Value<P, B>,
    session: &dyn ProviderTypeCheckContext,
    lookup: &dyn ValidatedCustomTypeLookup,
) -> Result<Admission<K, P, B>, AdmissionErrors> {
    let proof = value
        .knowledge(&KnowledgeContext::validation())?
        .into_classification();
    let (candidate, context) = Candidate::configuration(value, session, lookup, proof);
    admit(destination, candidate, &context)
}

fn complete_field<K: ConfigurationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    value: &Value<P, B>,
    session: &dyn ProviderTypeCheckContext,
    lookup: &dyn ValidatedCustomTypeLookup,
) -> Result<PlacedValue<K, P, B>, AdmissionErrors> {
    let known = Candidate::known_at_apply(
        value.knowledge(&KnowledgeContext::apply(&ResolvedBindings::default()))?,
    )?;
    let (candidate, context) = Candidate::configuration(
        value,
        session,
        lookup,
        crate::resource::knowledge::Classification::Known(known),
    );
    match admit(destination, candidate, &context)? {
        Admission::Ready(placed) => Ok(placed),
        Admission::Pending(pending) => Err(KnowledgeError::<B>::UnresolvedAtBoundary {
            sites: pending.inner.sites,
        }
        .into()),
    }
}
