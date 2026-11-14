//! Declaration-owned admission for configuration payloads.
//!
//! Every payload entry is derived from its declared field placement. There is
//! no constructor accepting unchecked entries or a checked marker plus a
//! replacement map.

use std::marker::PhantomData;
use std::sync::Arc;

use super::*;
use crate::parser::ConfigurationAttributes;
use crate::provider::{ProviderInstanceKey, ProviderWireTarget, WireAttributes, WireReady};
use crate::schema::{ValidatedConfigSchema, ValidatedCustomTypeLookup};

mod contract {
    #[derive(Debug, Clone)]
    pub struct ConfigurationContract {
        pub(super) schema: std::sync::Arc<crate::schema::ValidatedConfigSchema>,
        pub(super) attribute: String,
    }
}
pub(super) use self::contract::ConfigurationContract;

impl ConfigurationContract {
    fn attr(&self) -> &CompiledAttributeType {
        &self.schema.parts()[&self.attribute]
    }
}

pub mod backend;
pub mod provider;

/// The two sealed configuration destinations and their exact payloads.
/// Backend settings do not acquire a provider transport or provider identity.
pub trait ConfigurationKind: DestinationKind<Contract = Box<ConfigurationContract>> {
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

struct DeclaredConfigurationField<'a, K: ConfigurationKind> {
    destination: Destination<K>,
    attr_type: &'a crate::schema::CompiledAttributeType,
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

    fn destination<K: ConfigurationKind>(
        &self,
        key: &crate::parser::AttributeKey,
        instance: InstanceIdentity,
        revision: CatalogRevision,
    ) -> Option<Destination<K>> {
        self.field(key, instance, revision)
            .map(|field| field.destination)
    }

    fn field<K: ConfigurationKind>(
        &self,
        key: &crate::parser::AttributeKey,
        instance: InstanceIdentity,
        revision: CatalogRevision,
    ) -> Option<DeclaredConfigurationField<'_, K>> {
        let (ordinal, (name, attr_type)) = self
            .schema
            .parts()
            .iter()
            .enumerate()
            .find(|(_, (name, _))| key.matches(name))?;
        Some(DeclaredConfigurationField {
            attr_type,
            destination: Destination {
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
                contract: Box::new(ConfigurationContract {
                    schema: Arc::clone(&self.schema),
                    attribute: name.clone(),
                }),
                kind: PhantomData,
            },
        })
    }
}

/// Configuration is a concrete-value boundary. Unresolved sites are returned
/// to the configuration caller; resource discharge is never available here.
#[derive(Debug)]
enum ConfigurationAdmission<K: ConfigurationKind, P: PlaceholderKind, B: BindingAddress> {
    Ready(PlacedValue<K, P, B>),
    Unresolved(Vec<PendingSite<B>>),
}

/// Validates one configuration field against its declaration; unresolved sites
/// are returned instead of being held as a pending obligation.
fn admit_field<K: ConfigurationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    value: &Value<P, B>,
    proof: crate::resource::knowledge::Classification<'_, P, B>,
    lookup: &dyn ValidatedCustomTypeLookup,
) -> Result<ConfigurationAdmission<K, P, B>, ConfigurationErrors> {
    let checked = crate::schema::value_validation::validate_final_provider_value(
        destination.contract.attr(),
        &proof,
        crate::schema::empty_defs_for_schema_walks(),
        FieldAncestry::WRITABLE,
        lookup,
    );
    // Keep rootless configuration's established primary-local-error policy.
    let errors: Vec<_> = match checked.local_error() {
        Some(error) => vec![error.clone()],
        None => checked
            .clone()
            .into_located()
            .into_iter()
            .map(|(_, error)| error)
            .collect(),
    };
    if !errors.is_empty() {
        return Err(ConfigurationErrors {
            errors: errors
                .into_iter()
                .map(|error| match error {
                    TypeError::ResourceValidationFailed { message, .. } => {
                        TypeError::ValidationFailed { message }
                    }
                    error => error,
                })
                .collect(),
        });
    }
    let sites: Vec<_> = proof.sites().collect();
    if sites.is_empty() {
        Ok(ConfigurationAdmission::Ready(PlacedValue::admitted(
            destination,
            value.clone(),
            checked,
        )))
    } else {
        Ok(ConfigurationAdmission::Unresolved(sites))
    }
}

/// Configuration classification and validation retain their schema categories.
#[derive(Debug)]
struct ConfigurationErrors {
    errors: Vec<TypeError>,
}
impl<B: BindingAddress> From<KnowledgeError<B>> for ConfigurationErrors {
    fn from(error: KnowledgeError<B>) -> Self {
        Self {
            errors: vec![TypeError::from(error)],
        }
    }
}

fn field<K: ConfigurationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    value: &Value<P, B>,
    lookup: &dyn ValidatedCustomTypeLookup,
) -> Result<ConfigurationAdmission<K, P, B>, ConfigurationErrors> {
    let proof = value
        .knowledge(&KnowledgeContext::validation())?
        .into_classification();
    admit_field(destination, value, proof, lookup)
}

fn complete_field<K: ConfigurationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    value: &Value<P, B>,
    lookup: &dyn ValidatedCustomTypeLookup,
) -> Result<PlacedValue<K, P, B>, ConfigurationErrors> {
    let proof = value.knowledge(&KnowledgeContext::apply(&ResolvedBindings::default()))?;
    let sites = proof.sites().collect();
    let known = KnownValue::from_apply(proof).map_err(|error| match error {
        KnowledgeError::InvalidStage { operation: _ } => {
            KnowledgeError::UnresolvedAtBoundary { sites }
        }
        error @ (KnowledgeError::Unavailable { .. }
        | KnowledgeError::Cycle { .. }
        | KnowledgeError::UnavailableHistoricalSecret(_)
        | KnowledgeError::UnresolvedAtBoundary { .. }) => error,
    })?;
    match admit_field(
        destination,
        value,
        crate::resource::knowledge::Classification::Known(known),
        lookup,
    )? {
        ConfigurationAdmission::Ready(placed) => Ok(placed),
        ConfigurationAdmission::Unresolved(sites) => {
            Err(KnowledgeError::<B>::UnresolvedAtBoundary { sites }.into())
        }
    }
}
