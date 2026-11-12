//! Declaration-owned admission for configuration payloads.
//!
//! The payload and its field placements stay together. There is no constructor
//! accepting a checked marker and a replacement map.

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
    type Payload: std::fmt::Debug + Clone + PartialEq;
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

/// A provider slot cannot consume backend admission.
///
/// ```
/// use carina_core::validation::admission::{TypedSlot, PlacedValue, kind::ProviderConfigAttribute};
/// fn place(slot: &mut TypedSlot<ProviderConfigAttribute>, value: PlacedValue<ProviderConfigAttribute>) { slot.place(value).unwrap(); }
/// ```
/// ```compile_fail,E0308
/// use carina_core::validation::admission::{TypedSlot, PlacedValue, kind::{ProviderConfigAttribute, BackendAttribute}};
/// fn place(slot: &mut TypedSlot<ProviderConfigAttribute>, value: PlacedValue<BackendAttribute>) { slot.place(value).unwrap(); }
/// ```
/// An immutable, exact configuration payload admitted to its declaration.
///
/// ```compile_fail,E0451
/// use carina_core::validation::admission::{configuration::AdmittedConfiguration, kind::ProviderConfigAttribute};
/// fn forge() -> AdmittedConfiguration<ProviderConfigAttribute> {
///     AdmittedConfiguration { payload: unreachable!() }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct AdmittedConfiguration<K: ConfigurationKind> {
    payload: K::Payload,
    fields: Vec<PlacedValue<K>>,
    declaration: Arc<ConfigurationDeclaration>,
    instance: K::Instance,
}

impl<K: ConfigurationKind> PartialEq for AdmittedConfiguration<K> {
    fn eq(&self, other: &Self) -> bool {
        self.payload == other.payload
            && self.instance == other.instance
            && self.declaration.identity == other.declaration.identity
            && self.fields == other.fields
    }
}

impl<K: ConfigurationKind> AdmittedConfiguration<K> {
    pub fn attributes(&self) -> &ConfigurationAttributes<Value> {
        K::attributes(&self.payload)
    }
}

impl AdmittedConfiguration<kind::ProviderConfigAttribute> {
    pub fn wire_attributes(&self) -> &WireAttributes {
        self.payload.wire()
    }

    pub fn instance(&self) -> &ProviderInstanceKey {
        &self.instance
    }
}

impl AdmittedConfiguration<kind::BackendAttribute> {
    pub fn backend_kind(&self) -> backend::BackendKind {
        self.instance
    }
}

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

    fn destination<K: DestinationKind>(
        &self,
        key: &crate::parser::AttributeKey,
        instance: &ProviderInstanceKey,
        catalog: &ProviderContext,
    ) -> Option<Destination<K>> {
        self.destination_for(
            key,
            InstanceIdentity::Provider(instance.clone()),
            CatalogRevision(CatalogSnapshot::Provider(catalog.custom_types.clone())),
        )
    }

    fn destination_for<K: DestinationKind>(
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
) -> Result<Admission<K, P, B>, AdmissionErrors> {
    if context.identity != candidate.obligations.identity {
        return Err(constraints(vec![AdmissionFinding {
            path: LeafPath::from_segments([]),
            error: TypeError::ValidationFailed {
                message: "configuration candidate belongs to another proof".into(),
            },
        }]));
    }
    let AdmissionLookup::Configuration(lookup) = context.lookup else {
        return Err(constraints(vec![AdmissionFinding {
            path: LeafPath::from_segments([]),
            error: TypeError::ValidationFailed {
                message: "configuration destination requires its declaration owner".into(),
            },
        }]));
    };
    let contract = destination
        .contract
        .as_ref()
        .expect("configuration destination contract");
    let checked = match &context.proof {
        AdmissionProof::Validation(proof) => {
            crate::schema::value_validation::validate_final_provider_value(
                contract.attr(),
                proof,
                contract.defs(),
                contract.ancestry,
                lookup,
            )
        }
        AdmissionProof::Plan(proof) => {
            crate::schema::value_validation::validate_final_provider_value(
                contract.attr(),
                proof.knowledge(),
                contract.defs(),
                contract.ancestry,
                lookup,
            )
        }
        AdmissionProof::Apply(proof) => {
            crate::schema::value_validation::validate_final_provider_value(
                contract.attr(),
                &crate::resource::knowledge::Classification::<P, B>::Known(proof.clone()),
                contract.defs(),
                contract.ancestry,
                lookup,
            )
        }
    };
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

fn field<K: DestinationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    value: &Value<P, B>,
    session: &dyn ProviderTypeCheckContext,
    lookup: &dyn ValidatedCustomTypeLookup,
    operational: bool,
) -> Result<Admission<K, P, B>, AdmissionErrors> {
    let bindings = ResolvedBindings::default();
    let (candidate, mut context) = if operational {
        Candidate::apply(
            value.knowledge(&KnowledgeContext::apply(&bindings))?,
            session,
        )?
    } else {
        Candidate::validation_classified(
            value
                .knowledge(&KnowledgeContext::validation())?
                .into_classification(),
            None,
            session,
        )
    };
    context.lookup = AdmissionLookup::Configuration(lookup);
    admit(destination, candidate, &context)
}
