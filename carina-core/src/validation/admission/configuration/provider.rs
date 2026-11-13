//! Provider configuration's sole declaration and semantic admission owner.

use std::ops::Deref;

use super::*;
use crate::executor::provider_ready::{
    ProviderConfigPreparationError, ProviderConfigValidationError,
};
use crate::parser::ProviderConfiguration;
use crate::provider::{ProviderFactory, ProviderValidationInputs};

#[derive(Debug, Clone)]
pub struct CanonicalProviderConfigAttributes {
    admitted: AdmittedConfiguration<kind::ProviderConfigAttribute>,
}

impl CanonicalProviderConfigAttributes {
    pub fn attributes(&self) -> &ConfigurationAttributes<Value> {
        self.admitted.attributes()
    }
    pub fn wire_attributes(&self) -> &WireAttributes {
        self.admitted.wire_attributes()
    }
}

impl Deref for CanonicalProviderConfigAttributes {
    type Target = ConfigurationAttributes<Value>;
    fn deref(&self) -> &Self::Target {
        self.attributes()
    }
}

/// Configuration ready for a declaration-bound runtime session.
///
/// ```
/// use carina_core::provider::{ProviderPlan, ProviderReadyConfig};
/// fn session(config: &ProviderReadyConfig) {
///     let session = ProviderPlan::state_only().session(config);
///     assert_eq!(session.configuration().attributes(), config.attributes());
/// }
/// ```
/// ```compile_fail,E0308
/// use carina_core::provider::ProviderPlan;
/// use carina_core::validation::admission::{PendingAdmission, kind::ProviderConfigAttribute};
/// fn pending(plan: &ProviderPlan, value: PendingAdmission<ProviderConfigAttribute>) {
///     let _ = plan.session(&value);
/// }
/// ```
/// ```compile_fail,E0451
/// use carina_core::provider::ProviderReadyConfig;
/// let _ = ProviderReadyConfig { inner: todo!() };
/// ```
/// ```compile_fail,E0596
/// use carina_core::provider::ProviderReadyConfig;
/// fn replace(config: &mut ProviderReadyConfig) { config.attributes().clear(); }
/// ```
#[derive(Debug, Clone)]
pub struct ProviderReadyConfig {
    inner: Arc<ReadyConfiguration>,
}
#[derive(Debug)]
struct ReadyConfiguration {
    canonical: CanonicalProviderConfigAttributes,
    region: Option<String>,
}

impl ProviderReadyConfig {
    fn new(canonical: CanonicalProviderConfigAttributes, region: Option<String>) -> Self {
        Self {
            inner: Arc::new(ReadyConfiguration { canonical, region }),
        }
    }
    pub fn attributes(&self) -> &ConfigurationAttributes<Value> {
        self.inner.canonical.attributes()
    }
    pub fn wire_attributes(&self) -> &WireAttributes {
        self.inner.canonical.wire_attributes()
    }
    pub fn region(&self) -> Option<&str> {
        self.inner.region.as_deref()
    }
    pub fn instance(&self) -> &ProviderInstanceKey {
        self.inner.canonical.admitted.instance()
    }
}

fn attribute_error(
    provider: &str,
    attribute: &crate::parser::AttributeKey,
    error: AdmissionErrors,
) -> ProviderConfigPreparationError {
    let errors = match error.into_resolution_error() {
        ResolutionError::AdmissionConstraints(findings) => findings
            .findings()
            .iter()
            .map(|finding| match finding.error.clone() {
                TypeError::ResourceValidationFailed { message, .. } => {
                    TypeError::ValidationFailed { message }
                }
                error => error,
            })
            .collect(),
        ResolutionError::Knowledge(error) => vec![TypeError::Knowledge {
            error,
            attribute: None,
        }],
        error => vec![TypeError::Reference {
            error: Box::new(crate::resource::SecretDiagnosticError::Public(
                super::super::AdmissionReferenceError::Resolution(Box::new(error)),
            )),
            attribute: None,
        }],
    };
    ProviderConfigPreparationError::InvalidAttribute {
        provider: provider.to_owned(),
        attribute: attribute.clone(),
        error: errors.into(),
    }
}

fn admitted<P: PlaceholderKind, B: BindingAddress>(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    binding: Option<&str>,
    attributes: &ConfigurationAttributes<Value<P, B>>,
    schema: &ValidatedConfigSchema,
    operational: bool,
) -> Result<CanonicalProviderConfigAttributes, ProviderConfigPreparationError> {
    let declaration = ConfigurationDeclaration::provider(factory.name(), schema.clone());
    let canonical = crate::value::canonicalize_provider_config_attributes_with_types(
        provider_name,
        attributes,
        schema.parts(),
    )
    .map_err(|error| {
        let (provider, attribute, error) = error.into_parts();
        ProviderConfigPreparationError::InvalidAttribute {
            provider,
            attribute,
            error: vec![error].into(),
        }
    })?;
    let inputs = ProviderValidationInputs::collect(provider_name, canonical.values());
    let lookup = crate::parser::custom_type_lookup_for_factory(factory, &inputs);
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::new([]));
    let instance = ProviderInstanceKey::new(provider_name, binding.map(str::to_owned));
    let checked = crate::validation::deferred_types::TypeCheckSession::run(&catalog, |session| {
        let mut aggregate = ConfigurationBuilder::new(Arc::clone(&declaration), instance.clone());
        for (key, value) in &canonical {
            let destination = declaration.destination::<kind::ProviderConfigAttribute>(
                key,
                InstanceIdentity::Provider(instance.clone()),
                CatalogRevision(CatalogSnapshot::Provider(
                    session.catalog().custom_types.clone(),
                )),
            );
            if let Some(known) = value
                .to_expanded()
                .and_then(Value::try_into_address_free_absolute)
            {
                if let Some(destination) = destination {
                    match field(&destination, &known, session, &lookup, operational)
                        .map_err(|error| attribute_error(provider_name, key, error))?
                    {
                        Admission::Ready(placed) => aggregate.place(key.clone(), placed),
                        Admission::Pending(pending) if operational => {
                            return Err(attribute_error(
                                provider_name,
                                key,
                                KnowledgeError::<AbsoluteBindingAddress>::UnresolvedAtBoundary {
                                    sites: pending.inner.sites,
                                }
                                .into(),
                            ));
                        }
                        Admission::Pending(_) => {}
                    }
                } else {
                    aggregate.passthrough(key.clone(), known);
                }
            } else {
                if let Some(destination) = destination {
                    field(&destination, value, session, &lookup, false)
                        .map_err(|error| attribute_error(provider_name, key, error))?;
                }
                if operational {
                    let proof = value
                        .knowledge(&KnowledgeContext::validation())
                        .map_err(|error| attribute_error(provider_name, key, error.into()))?;
                    return Err(attribute_error(
                        provider_name,
                        key,
                        KnowledgeError::<B>::UnresolvedAtBoundary {
                            sites: proof.sites().collect(),
                        }
                        .into(),
                    ));
                }
            }
        }
        Ok::<_, ProviderConfigPreparationError>(aggregate)
    });
    let mut deferred = Vec::new();
    let aggregate = checked.publish(&mut deferred)?;
    if !deferred.is_empty() {
        return Err(ProviderConfigPreparationError::ProviderValidation {
            provider: provider_name.to_owned(),
            error: ProviderConfigValidationError::deferred(deferred),
        });
    }
    let canonical = CanonicalProviderConfigAttributes {
        admitted: aggregate.admit_payload()?,
    };
    let session = ProviderValidationInputs::collect(provider_name, canonical.values());
    session
        .diagnostics()
        .dispatch_sync(|| factory.validate_config(&canonical, &session))
        .map_err(|error| {
            let error = if canonical.values().any(crate::value::contains_secret) {
                error.with_redacted_message("configuration containing a secret failed validation")
            } else {
                error
            };
            ProviderConfigPreparationError::ProviderValidation {
                provider: provider_name.to_owned(),
                error: ProviderConfigValidationError::new(
                    error.with_secrets(session.redaction_set()),
                ),
            }
        })?;
    Ok(canonical)
}

fn analyze<P: PlaceholderKind, B: BindingAddress>(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &ConfigurationAttributes<Value<P, B>>,
) -> Result<CanonicalProviderConfigAttributes, ProviderConfigPreparationError> {
    let witness = factory.schema_witness();
    admitted(
        factory,
        provider_name,
        None,
        attributes,
        witness.config(),
        false,
    )
}

pub fn analysis(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &ConfigurationAttributes<Value>,
) -> Result<CanonicalProviderConfigAttributes, ProviderConfigPreparationError> {
    analyze(factory, provider_name, attributes)
}
pub fn validate_parser_provider_config(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &ConfigurationAttributes<
        Value<crate::resource::ParserPlaceholder, crate::resource::RelativeBindingName>,
    >,
) -> Result<CanonicalProviderConfigAttributes, ProviderConfigPreparationError> {
    analyze(factory, provider_name, attributes)
}

fn ready(
    factory: &dyn ProviderFactory,
    instance: &ProviderInstanceKey,
    attributes: &ConfigurationAttributes<Value>,
    schema: &ValidatedConfigSchema,
) -> Result<ProviderReadyConfig, ProviderConfigPreparationError> {
    let canonical = admitted(
        factory,
        instance.provider(),
        instance.provider_instance(),
        attributes,
        schema,
        true,
    )?;
    let region = schema
        .rootless_type("region")
        .and_then(|_| crate::utils::region_from_attrs(canonical.attributes()));
    Ok(ProviderReadyConfig::new(canonical, region))
}

#[cfg(any(test, feature = "test-utils"))]
pub fn prepare(
    factory: &dyn ProviderFactory,
    config: &dyn ProviderConfiguration,
) -> Result<ProviderReadyConfig, ProviderConfigPreparationError> {
    ready(
        factory,
        &ProviderInstanceKey::new(config.name(), config.binding().map(str::to_owned)),
        config.attributes(),
        factory.schema_witness().config(),
    )
}

/// The selected factory and the exact configuration admitted by it stay together.
/// Neither a ready map nor an independently selected session can initialize a provider.
///
/// ```compile_fail,E0308
/// use carina_core::provider::{ProviderBoundary, ProviderFactory, ProviderSession};
/// async fn bypass(factory: &dyn ProviderFactory, session: &ProviderSession) {
///     let _ = ProviderBoundary::initialize(factory, session).await;
/// }
/// ```
/// ```
/// use carina_core::provider::ProviderBoundary;
/// use carina_core::validation::admission::configuration::provider::ProviderConstruction;
/// async fn initialize(construction: &ProviderConstruction<'_>) {
///     let _ = ProviderBoundary::initialize(construction).await.unwrap();
/// }
/// ```
/// A normalizer hook also requires the boundary's initialization capability.
///
/// ```compile_fail,E0061
/// use carina_core::provider::{ProviderFactory, ProviderSession};
/// fn bypass(factory: &dyn ProviderFactory, session: &ProviderSession) {
///     let _ = factory.create_normalizer(session);
/// }
/// ```
pub struct ProviderConstruction<'a> {
    factory: &'a dyn ProviderFactory,
    witness: crate::schema::ValidatedProviderSchemaWitness,
    session: crate::provider::ProviderSession,
}
impl ProviderConstruction<'_> {
    pub fn session(&self) -> &crate::provider::ProviderSession {
        &self.session
    }
    pub(crate) fn factory(&self) -> &dyn ProviderFactory {
        self.factory
    }
    pub(crate) fn witness(&self) -> &crate::schema::ValidatedProviderSchemaWitness {
        &self.witness
    }
}

crate::provider_text::host_api! {
/// Admit the configuration for the route it declares.
fn construction<'a>(factory: &'a dyn ProviderFactory, config: &dyn ProviderConfiguration, plan: &crate::provider::ProviderPlan) -> Result<ProviderConstruction<'a>, ProviderConfigPreparationError> {
    construct(factory, &ProviderInstanceKey::new(config.name(), config.binding().map(str::to_owned)), config.attributes(), plan)
}
}
crate::provider_text::host_api! {
/// Admit the selected source for an implicit operation's default route.
fn default_construction<'a>(factory: &'a dyn ProviderFactory, config: &dyn ProviderConfiguration, plan: &crate::provider::ProviderPlan) -> Result<ProviderConstruction<'a>, ProviderConfigPreparationError> {
    construct(factory, &ProviderInstanceKey::new(config.name(), None), config.attributes(), plan)
}
}
fn construct<'a>(
    factory: &'a dyn ProviderFactory,
    instance: &ProviderInstanceKey,
    attributes: &ConfigurationAttributes<Value>,
    plan: &crate::provider::ProviderPlan,
) -> Result<ProviderConstruction<'a>, ProviderConfigPreparationError> {
    let witness = factory.schema_witness();
    let ready = ready(factory, instance, attributes, witness.config())?;
    Ok(ProviderConstruction {
        factory,
        witness,
        session: plan.session(&ready),
    })
}

#[cfg(any(test, feature = "test-utils"))]
pub fn construction_for_test<'a>(
    factory: &'a dyn ProviderFactory,
    session: &crate::provider::ProviderSession,
) -> Result<ProviderConstruction<'a>, ProviderConfigPreparationError> {
    let witness = factory.schema_witness();
    let ready = ready(
        factory,
        session.configuration().instance(),
        session.configuration().attributes(),
        witness.config(),
    )?;
    let mut session = session.clone();
    session.replace_test_configuration(ready);
    Ok(ProviderConstruction {
        factory,
        witness,
        session,
    })
}

#[cfg(any(test, feature = "test-utils"))]
pub(crate) fn test_configuration(
    provider: &str,
    binding: Option<&str>,
    config: Option<&ProviderReadyConfig>,
) -> ProviderReadyConfig {
    let empty = ConfigurationAttributes::new();
    let factory =
        crate::test_utils::configuration::ConfigFactory::new(ValidatedConfigSchema::default())
            .named(provider);
    let canonical = admitted(
        &factory,
        provider,
        binding,
        config.map_or(&empty, ProviderReadyConfig::attributes),
        &factory.provider_config_attribute_types(),
        true,
    )
    .expect("admit fixture configuration");
    ProviderReadyConfig::new(
        canonical,
        config.and_then(|config| config.inner.region.clone()),
    )
}

crate::provider_text::host_api! {
/// Construct an implicit provider only after its empty configuration is admitted.
fn empty_construction<'a>(
    factory: &'a dyn ProviderFactory,
    route: &ProviderInstanceKey,
    plan: &crate::provider::ProviderPlan,
) -> Result<ProviderConstruction<'a>, ProviderConfigPreparationError> {
    construct(factory, route, &ConfigurationAttributes::new(), plan)
}
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
