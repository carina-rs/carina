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
    payload: WireReady<ConfigurationAttributes<Value>>,
}

impl CanonicalProviderConfigAttributes {
    pub fn attributes(&self) -> &ConfigurationAttributes<Value> {
        self.payload.as_inner()
    }
    pub fn wire_attributes(&self) -> &WireAttributes {
        self.payload.wire()
    }
}

impl Deref for CanonicalProviderConfigAttributes {
    type Target = ConfigurationAttributes<Value>;
    fn deref(&self) -> &Self::Target {
        self.attributes()
    }
}

/// A partial analysis projection, which grants no construction authority.
///
/// ```compile_fail
/// use carina_core::provider::ProviderReadyConfig;
/// use carina_core::validation::admission::configuration::provider::AnalyzedConfiguration;
/// fn bypass(analyzed: AnalyzedConfiguration) {
///     let _: ProviderReadyConfig = analyzed.into(); // forbidden: analysis grants no construction authority
/// }
/// ```
/// ```
/// # use carina_core::provider::ProviderReadyConfig;
/// # use carina_core::validation::admission::configuration::provider::AnalyzedConfiguration;
/// # fn bypass(analyzed: AnalyzedConfiguration) {
/// # }
/// ```
/// ```
/// use carina_core::parser::ProviderConfiguration;
/// use carina_core::provider::{ProviderFactory, ProviderPlan, ProviderReadyConfig};
/// use carina_core::validation::admission::configuration::provider::construction;
/// fn ready(factory: &dyn ProviderFactory, config: &dyn ProviderConfiguration) {
///     let plan = ProviderPlan::state_only();
///     let construction = construction(factory, config, &plan).unwrap();
///     let _: &ProviderReadyConfig = construction.session().configuration();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct AnalyzedConfiguration {
    canonical: CanonicalProviderConfigAttributes,
}
impl Deref for AnalyzedConfiguration {
    type Target = CanonicalProviderConfigAttributes;
    fn deref(&self) -> &Self::Target {
        &self.canonical
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
/// ```compile_fail
/// use carina_core::provider::ProviderPlan;
/// use carina_core::validation::admission::{PendingAdmission, kind::ProviderConfigAttribute};
/// fn pending(plan: &ProviderPlan, value: PendingAdmission<ProviderConfigAttribute>) {
///     let _ = plan.session(&value); // forbidden: pending admission cannot construct a session
/// }
/// ```
/// ```
/// # use carina_core::provider::ProviderPlan;
/// # use carina_core::validation::admission::{PendingAdmission, kind::ProviderConfigAttribute};
/// # fn pending(plan: &ProviderPlan, value: PendingAdmission<ProviderConfigAttribute>) {
/// # }
/// ```
/// ```compile_fail
/// use carina_core::provider::ProviderReadyConfig;
/// fn bypass(config: &ProviderReadyConfig) {
///     let _ = &config.inner; // forbidden: the existing ready configuration field is private
/// }
/// ```
/// ```
/// # use carina_core::provider::ProviderReadyConfig;
/// # fn bypass(config: &ProviderReadyConfig) {
/// # }
/// ```
/// ```compile_fail
/// use carina_core::provider::ProviderReadyConfig;
/// fn replace(config: &mut ProviderReadyConfig) {
///     config.attributes().clear(); // forbidden: ready attributes are immutable
/// }
/// ```
/// ```
/// # use carina_core::provider::ProviderReadyConfig;
/// # fn replace(config: &mut ProviderReadyConfig) {
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ProviderReadyConfig {
    inner: Arc<ReadyConfiguration>,
}
#[derive(Debug)]
struct ReadyConfiguration {
    admitted: AdmittedConfiguration<kind::ProviderConfigAttribute>,
    region: Option<String>,
}

impl ProviderReadyConfig {
    fn new(
        admitted: AdmittedConfiguration<kind::ProviderConfigAttribute>,
        region: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(ReadyConfiguration { admitted, region }),
        }
    }
    fn from_admitted(admitted: AdmittedConfiguration<kind::ProviderConfigAttribute>) -> Self {
        let region = admitted
            .field("region")
            .and_then(|_| crate::utils::region_from_attrs(admitted.attributes()));
        Self::new(admitted, region)
    }
    pub fn attributes(&self) -> &ConfigurationAttributes<Value> {
        self.inner.admitted.attributes()
    }
    pub fn wire_attributes(&self) -> &WireAttributes {
        self.inner.admitted.wire_attributes()
    }
    pub fn region(&self) -> Option<&str> {
        self.inner.region.as_deref()
    }
    pub fn instance(&self) -> &ProviderInstanceKey {
        self.inner.admitted.instance()
    }
}

trait PreparationPhase {
    type Output;
    fn declared(
        destination: &Destination<kind::ProviderConfigAttribute>,
        key: &crate::parser::AttributeKey,
        value: &Value,
        session: &dyn ProviderTypeCheckContext,
        lookup: &dyn ValidatedCustomTypeLookup,
        aggregate: &mut ConfigurationBuilder<kind::ProviderConfigAttribute>,
    ) -> Result<(), ConfigurationErrors>;
    fn unresolved<P: PlaceholderKind, B: BindingAddress>(
        value: &Value<P, B>,
    ) -> Result<(), ConfigurationErrors>;
    fn finish(
        aggregate: ConfigurationBuilder<kind::ProviderConfigAttribute>,
    ) -> Result<Self::Output, crate::provider::ProviderWireError>;
    fn projection(result: &Self::Output) -> CanonicalProviderConfigAttributes;
}
struct Analysis;
struct Complete;
impl PreparationPhase for Analysis {
    type Output = AnalyzedConfiguration;
    fn declared(
        destination: &Destination<kind::ProviderConfigAttribute>,
        key: &crate::parser::AttributeKey,
        value: &Value,
        session: &dyn ProviderTypeCheckContext,
        lookup: &dyn ValidatedCustomTypeLookup,
        aggregate: &mut ConfigurationBuilder<kind::ProviderConfigAttribute>,
    ) -> Result<(), ConfigurationErrors> {
        match field(destination, value, session, lookup)? {
            Admission::Ready(placed) => {
                aggregate.place(key.clone(), placed);
                Ok(())
            }
            Admission::Pending(pending) => Err(KnowledgeError::<
                crate::resource::AbsoluteBindingAddress,
            >::UnresolvedAtBoundary {
                sites: pending.inner.sites,
            }
            .into()),
        }
    }
    fn unresolved<P: PlaceholderKind, B: BindingAddress>(
        _: &Value<P, B>,
    ) -> Result<(), ConfigurationErrors> {
        Ok(())
    }
    fn finish(
        aggregate: ConfigurationBuilder<kind::ProviderConfigAttribute>,
    ) -> Result<Self::Output, crate::provider::ProviderWireError> {
        Ok(AnalyzedConfiguration {
            canonical: CanonicalProviderConfigAttributes {
                payload: aggregate.analyze_payload()?,
            },
        })
    }
    fn projection(result: &Self::Output) -> CanonicalProviderConfigAttributes {
        result.canonical.clone()
    }
}
impl PreparationPhase for Complete {
    type Output = AdmittedConfiguration<kind::ProviderConfigAttribute>;
    fn declared(
        destination: &Destination<kind::ProviderConfigAttribute>,
        key: &crate::parser::AttributeKey,
        value: &Value,
        session: &dyn ProviderTypeCheckContext,
        lookup: &dyn ValidatedCustomTypeLookup,
        aggregate: &mut ConfigurationBuilder<kind::ProviderConfigAttribute>,
    ) -> Result<(), ConfigurationErrors> {
        aggregate.place(
            key.clone(),
            complete_field(destination, value, session, lookup)?,
        );
        Ok(())
    }
    fn unresolved<P: PlaceholderKind, B: BindingAddress>(
        value: &Value<P, B>,
    ) -> Result<(), ConfigurationErrors> {
        let proof = value.knowledge(&KnowledgeContext::validation())?;
        Err(KnowledgeError::<B>::UnresolvedAtBoundary {
            sites: proof.sites().collect(),
        }
        .into())
    }
    fn finish(
        aggregate: ConfigurationBuilder<kind::ProviderConfigAttribute>,
    ) -> Result<Self::Output, crate::provider::ProviderWireError> {
        aggregate.admit_payload()
    }
    fn projection(result: &Self::Output) -> CanonicalProviderConfigAttributes {
        CanonicalProviderConfigAttributes {
            payload: result.canonical_payload(),
        }
    }
}

fn attribute_error(
    provider: &str,
    attribute: &crate::parser::AttributeKey,
    error: ConfigurationErrors,
) -> ProviderConfigPreparationError {
    ProviderConfigPreparationError::InvalidAttribute {
        provider: provider.to_owned(),
        attribute: attribute.clone(),
        error: error.errors.into(),
    }
}

/// All undeclared keys in one configuration, with their parser-owned identities.
#[derive(Debug)]
pub struct UnknownProviderConfigAttributes {
    provider: String,
    errors: Vec<(crate::parser::AttributeKey, TypeError)>,
}

impl UnknownProviderConfigAttributes {
    pub(crate) fn into_diagnostics(self) -> Vec<ProviderConfigPreparationError> {
        self.errors
            .into_iter()
            .map(
                |(attribute, error)| ProviderConfigPreparationError::InvalidAttribute {
                    provider: self.provider.clone(),
                    attribute,
                    error: vec![error].into(),
                },
            )
            .collect()
    }

    pub(crate) fn diagnostics(
        &self,
    ) -> impl Iterator<Item = crate::executor::provider_ready::ProviderConfigDiagnostic<'_>> {
        self.errors.iter().map(|(attribute, error)| {
            crate::executor::provider_ready::ProviderConfigDiagnostic::Attribute {
                provider: &self.provider,
                attribute,
                error,
            }
        })
    }
}

impl std::fmt::Display for UnknownProviderConfigAttributes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, diagnostic) in self.diagnostics().enumerate() {
            if index > 0 {
                f.write_str("\n")?;
            }
            std::fmt::Display::fmt(&diagnostic, f)?;
        }
        Ok(())
    }
}

fn unknown_attribute_report<'a>(
    provider: &str,
    keys: impl IntoIterator<Item = &'a crate::parser::AttributeKey>,
    schema: &ValidatedConfigSchema,
) -> Option<ProviderConfigPreparationError> {
    let known: Vec<_> = schema.parts().keys().map(String::as_str).collect();
    let mut errors: Vec<_> = keys
        .into_iter()
        .map(|key| {
            (
                key.clone(),
                TypeError::unknown_attribute(&key.to_string(), &known),
            )
        })
        .collect();
    errors.sort_by(|(left, _), (right, _)| left.cmp(right));
    (!errors.is_empty()).then(|| {
        ProviderConfigPreparationError::UnknownAttributes(UnknownProviderConfigAttributes {
            provider: provider.to_owned(),
            errors,
        })
    })
}

struct ResolvedProviderField<'a, P: crate::resource::ValueKind, B: BindingAddress> {
    key: crate::parser::AttributeKey,
    value: Value<P, B>,
    declared: DeclaredConfigurationField<'a, kind::ProviderConfigAttribute>,
}

fn resolve_fields<'a, P: crate::resource::ValueKind, B: BindingAddress>(
    provider: &str,
    declaration: &'a ConfigurationDeclaration,
    instance: &ProviderInstanceKey,
    revision: CatalogRevision,
    attributes: &ConfigurationAttributes<Value<P, B>>,
) -> Result<Vec<ResolvedProviderField<'a, P, B>>, ProviderConfigPreparationError> {
    let mut fields = Vec::with_capacity(attributes.len());
    let mut unknown = Vec::new();
    for (key, value) in attributes {
        match declaration.field(
            key,
            InstanceIdentity::Provider(instance.clone()),
            revision.clone(),
        ) {
            Some(declared) => fields.push(ResolvedProviderField {
                key: key.clone(),
                value: value.clone(),
                declared,
            }),
            None => unknown.push(key),
        }
    }
    if let Some(error) = unknown_attribute_report(provider, unknown, &declaration.schema) {
        return Err(error);
    }
    Ok(fields)
}

/// Check persisted declarations before secret rehydration. This grants no
/// construction authority; complete values still enter `construction()`.
pub fn validate_keys<P: crate::resource::ValueKind, B: BindingAddress>(
    configs: &[crate::parser::ProviderConfig<P, B>],
    factories: &[&dyn ProviderFactory],
) -> Result<(), ProviderConfigPreparationError> {
    let errors = configs
        .iter()
        .filter_map(|config| {
            let factory = factories
                .iter()
                .find(|factory| factory.name() == config.name)?;
            let declaration = ConfigurationDeclaration::provider(
                factory.name(),
                factory.schema_witness().config().clone(),
            );
            let catalog = ProviderContext::new(crate::EnvironmentSnapshot::new([]));
            resolve_fields(
                &config.name,
                &declaration,
                &ProviderInstanceKey::new(&config.name, config.binding.clone()),
                CatalogRevision(CatalogSnapshot::Provider(catalog.custom_types.clone())),
                &config.attributes,
            )
            .err()
        })
        .collect();
    ProviderConfigPreparationError::collect(errors)
}

/// Analyze each declared instance through the same configuration owner.
pub fn analyze_configs(
    configs: &[crate::parser::ProviderConfig<crate::resource::ParserPlaceholder>],
    factories: &[&dyn ProviderFactory],
) -> Result<(), ProviderConfigPreparationError> {
    let errors = configs
        .iter()
        .filter_map(|config| {
            let factory = factories
                .iter()
                .find(|factory| factory.name() == config.name)?;
            validate_parser_provider_config(*factory, &config.name, &config.attributes).err()
        })
        .collect();
    ProviderConfigPreparationError::collect(errors)
}

fn prepare_fields<M: PreparationPhase, P: PlaceholderKind, B: BindingAddress>(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    binding: Option<&str>,
    attributes: &ConfigurationAttributes<Value<P, B>>,
    schema: &ValidatedConfigSchema,
) -> Result<M::Output, ProviderConfigPreparationError> {
    let declaration = ConfigurationDeclaration::provider(factory.name(), schema.clone());
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::new([]));
    let instance = ProviderInstanceKey::new(provider_name, binding.map(str::to_owned));
    let mut fields = resolve_fields(
        provider_name,
        &declaration,
        &instance,
        CatalogRevision(CatalogSnapshot::Provider(catalog.custom_types.clone())),
        attributes,
    )?;
    for field in &mut fields {
        field.value = crate::value::canonicalize_provider_config_with_type_and_ancestry(
            provider_name,
            field.value.clone(),
            field.declared.attr_type,
            crate::schema::empty_defs_for_schema_walks(),
            FieldAncestry::WRITABLE,
        )
        .map_err(|error| ProviderConfigPreparationError::InvalidAttribute {
            provider: provider_name.to_owned(),
            attribute: field.key.clone(),
            error: vec![error].into(),
        })?;
    }
    let inputs =
        ProviderValidationInputs::collect(provider_name, fields.iter().map(|field| &field.value));
    let lookup = crate::parser::custom_type_lookup_for_factory(factory, &inputs);
    let checked = crate::validation::deferred_types::TypeCheckSession::run(&catalog, |session| {
        let mut aggregate = ConfigurationBuilder::new(Arc::clone(&declaration), instance.clone());
        for entry in &fields {
            let key = &entry.key;
            let value = &entry.value;
            let destination = &entry.declared.destination;
            if let Some(known) = value
                .to_expanded()
                .and_then(Value::try_into_address_free_absolute)
            {
                M::declared(destination, key, &known, session, &lookup, &mut aggregate)
                    .map_err(|error| attribute_error(provider_name, key, error))?;
            } else {
                field(destination, value, session, &lookup)
                    .map_err(|error| attribute_error(provider_name, key, error))?;
                M::unresolved(value).map_err(|error| attribute_error(provider_name, key, error))?;
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
    let result = M::finish(aggregate)?;
    let canonical = M::projection(&result);
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
    Ok(result)
}

fn analyze<P: PlaceholderKind, B: BindingAddress>(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &ConfigurationAttributes<Value<P, B>>,
) -> Result<AnalyzedConfiguration, ProviderConfigPreparationError> {
    prepare_fields::<Analysis, _, _>(
        factory,
        provider_name,
        None,
        attributes,
        factory.schema_witness().config(),
    )
}
fn admitted<P: PlaceholderKind, B: BindingAddress>(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    binding: Option<&str>,
    attributes: &ConfigurationAttributes<Value<P, B>>,
    schema: &ValidatedConfigSchema,
) -> Result<AdmittedConfiguration<kind::ProviderConfigAttribute>, ProviderConfigPreparationError> {
    prepare_fields::<Complete, _, _>(factory, provider_name, binding, attributes, schema)
}
pub fn analysis(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &ConfigurationAttributes<Value>,
) -> Result<AnalyzedConfiguration, ProviderConfigPreparationError> {
    analyze(factory, provider_name, attributes)
}
pub fn validate_parser_provider_config(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &ConfigurationAttributes<
        Value<crate::resource::ParserPlaceholder, crate::resource::RelativeBindingName>,
    >,
) -> Result<AnalyzedConfiguration, ProviderConfigPreparationError> {
    analyze(factory, provider_name, attributes)
}

fn ready(
    factory: &dyn ProviderFactory,
    instance: &ProviderInstanceKey,
    attributes: &ConfigurationAttributes<Value>,
    schema: &ValidatedConfigSchema,
) -> Result<ProviderReadyConfig, ProviderConfigPreparationError> {
    Ok(ProviderReadyConfig::from_admitted(admitted(
        factory,
        instance.provider(),
        instance.provider_instance(),
        attributes,
        schema,
    )?))
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
/// ```compile_fail
/// use carina_core::provider::{ProviderBoundary, ProviderSession};
/// async fn bypass(session: &ProviderSession) {
///     let _ = ProviderBoundary::initialize(session).await; // forbidden: initialization requires ProviderConstruction
/// }
/// ```
/// ```
/// # use carina_core::provider::{ProviderBoundary, ProviderSession};
/// # async fn bypass(session: &ProviderSession) {
/// # }
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
/// ```compile_fail
/// use carina_core::provider::{ProviderBoundary, ProviderSession};
/// async fn bypass(session: &ProviderSession) {
///     let _ = ProviderBoundary::initialize_normalizer(session).await; // forbidden: normalizers require ProviderConstruction
/// }
/// ```
/// ```
/// # use carina_core::provider::{ProviderBoundary, ProviderSession};
/// # async fn bypass(session: &ProviderSession) {
/// # }
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
    let schema = config.map_or_else(ValidatedConfigSchema::default, |config| {
        config.inner.admitted.declaration_schema().clone()
    });
    let factory = crate::test_utils::configuration::ConfigFactory::new(schema).named(provider);
    let canonical = admitted(
        &factory,
        provider,
        binding,
        config.map_or(&empty, ProviderReadyConfig::attributes),
        factory.schema_witness().config(),
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
