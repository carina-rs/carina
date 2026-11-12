//! Provider configuration's sole declaration and semantic admission owner.

use std::ops::Deref;

use super::*;
use crate::executor::provider_ready::{
    ProviderConfigPreparationError, ProviderConfigValidationError,
};
use crate::parser::ProviderConfiguration;
use crate::provider::{ProviderFactory, ProviderValidationInputs};
use crate::resource::{ParserPlaceholder, RelativeBindingName};

#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderReadyConfig {
    inner: Arc<ReadyConfiguration>,
}
#[derive(Debug, PartialEq)]
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
        let mut payload = ConfigurationAttributes::new();
        let mut fields = Vec::new();
        for (key, value) in &canonical {
            if let Some(known) = value
                .to_expanded()
                .and_then(Value::try_into_address_free_absolute)
            {
                if let Some(destination) =
                    declaration.destination(key, &instance, session.catalog())
                {
                    match field(&destination, &known, session, &lookup, operational)
                        .map_err(|error| attribute_error(provider_name, key, error))?
                    {
                        Admission::Ready(placed) => {
                            let mut slot = TypedSlot::new(destination);
                            slot.place(placed.clone())
                                .expect("declaration-owned config slot");
                            payload.insert(key.clone(), placed.value().clone());
                            fields.push(placed);
                        }
                        Admission::Pending(_) => {}
                    }
                } else {
                    payload.insert(key.clone(), known);
                }
            } else if let Some(destination) = declaration
                .destination::<kind::ProviderConfigAttribute>(key, &instance, session.catalog())
            {
                field(&destination, value, session, &lookup, false)
                    .map_err(|error| attribute_error(provider_name, key, error))?;
            }
        }
        Ok::<_, ProviderConfigPreparationError>((payload, fields))
    });
    let mut deferred = Vec::new();
    let (payload, fields) = checked.publish(&mut deferred)?;
    if !deferred.is_empty() {
        return Err(ProviderConfigPreparationError::ProviderValidation {
            provider: provider_name.to_owned(),
            error: ProviderConfigValidationError::deferred(deferred),
        });
    }
    let payload = WireReady::try_new(
        payload,
        ProviderWireTarget::Configuration {
            provider: provider_name.to_owned(),
        },
    )?;
    let canonical = CanonicalProviderConfigAttributes {
        admitted: AdmittedConfiguration {
            payload,
            fields,
            declaration,
            instance,
        },
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

pub(crate) fn analysis<P: PlaceholderKind, B: BindingAddress>(
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

pub(crate) fn parser_analysis(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &ConfigurationAttributes<Value<ParserPlaceholder, RelativeBindingName>>,
) -> Result<CanonicalProviderConfigAttributes, ProviderConfigPreparationError> {
    analysis(factory, provider_name, attributes)
}

pub(crate) fn prepare(
    factory: &dyn ProviderFactory,
    config: &dyn ProviderConfiguration,
) -> Result<ProviderReadyConfig, ProviderConfigPreparationError> {
    let witness = factory.schema_witness();
    let schema = witness.config();
    let canonical = admitted(
        factory,
        config.name(),
        config.binding(),
        config.attributes(),
        schema,
        true,
    )?;
    let region = schema
        .rootless_type("region")
        .and_then(|_| crate::utils::region_from_attrs(canonical.attributes()));
    Ok(ProviderReadyConfig::new(canonical, region))
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
/// Admit the selected source configuration for its effective operational route.
/// Source selection remains the responsibility of the resolved collection.
fn construction<'a>(
    factory: &'a dyn ProviderFactory,
    config: &dyn ProviderConfiguration,
    route: &ProviderInstanceKey,
    plan: &crate::provider::ProviderPlan,
) -> Result<ProviderConstruction<'a>, ProviderConfigPreparationError> {
    let witness = factory.schema_witness();
    let schema = witness.config();
    let canonical = admitted(
        factory,
        route.provider(),
        route.provider_instance(),
        config.attributes(),
        schema,
        true,
    )?;
    let region = schema
        .rootless_type("region")
        .and_then(|_| crate::utils::region_from_attrs(canonical.attributes()));
    let ready = ProviderReadyConfig::new(canonical, region);
    Ok(ProviderConstruction {
        factory,
        witness,
        session: plan.session(&ready),
    })
}
}

#[cfg(any(test, feature = "test-utils"))]
pub fn construction_for_test<'a>(
    factory: &'a dyn ProviderFactory,
    session: &crate::provider::ProviderSession,
) -> Result<ProviderConstruction<'a>, ProviderConfigPreparationError> {
    let route = session.configuration().instance();
    let witness = factory.schema_witness();
    let schema = witness.config();
    let canonical = admitted(
        factory,
        route.provider(),
        route.provider_instance(),
        session.configuration().attributes(),
        schema,
        true,
    )?;
    let region = schema
        .rootless_type("region")
        .and_then(|_| crate::utils::region_from_attrs(canonical.attributes()));
    let ready = ProviderReadyConfig::new(canonical, region);
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
    struct FixtureDeclaration<'a>(&'a str);
    impl ProviderFactory for FixtureDeclaration<'_> {
        fn name(&self) -> &str {
            self.0
        }
        fn display_name(&self) -> &str {
            self.0
        }
        fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
            ValidatedConfigSchema::validate(Default::default()).unwrap()
        }
        fn validate_config(
            &self,
            _: &CanonicalProviderConfigAttributes,
            _: &crate::provider::ProviderValidationInputs,
        ) -> crate::provider::ProviderResult<()> {
            Ok(())
        }
        fn create_provider(
            &self,
            _: &crate::provider::ProviderSession,
            _: crate::provider::InitializeOutcomeMint,
        ) -> crate::provider::BoxFuture<
            '_,
            crate::provider::ProviderResult<crate::provider::ProviderInitialization>,
        > {
            panic!("fixture declaration cannot initialize providers")
        }
        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            vec![]
        }
        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                vec![],
                self.provider_config_attribute_types(),
                Default::default(),
                vec![],
            )
            .unwrap()
        }
    }
    let empty = ConfigurationAttributes::new();
    let factory = FixtureDeclaration(provider);
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
    let witness = factory.schema_witness();
    let schema = witness.config();
    let canonical = admitted(
        factory,
        route.provider(),
        route.provider_instance(),
        &ConfigurationAttributes::<Value>::new(),
        schema,
        true,
    )?;
    let ready = ProviderReadyConfig::new(canonical, None);
    Ok(ProviderConstruction {
        factory,
        witness,
        session: plan.session(&ready),
    })
}
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
