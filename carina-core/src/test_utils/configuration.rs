//! Configuration-only factories and directory fixtures using production loading.

use crate::parser::ConfigurationAttributes;
#[cfg(not(target_arch = "wasm32"))]
use crate::parser::{ProviderConfig, ProviderContext, ResolvedProviderConfigs};
use crate::provider::*;
#[cfg(not(target_arch = "wasm32"))]
use crate::resource::NoPlaceholder;
use crate::resource::Value;
use crate::schema::{
    TypeIdentity, ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

type CustomCheck = dyn Fn(&TypeIdentity, &CanonicalCustomTypeValue, &ProviderValidationInputs) -> ProviderResult<()>
    + Send
    + Sync;
pub struct ConfigFactory {
    name: String,
    pub schema: ValidatedConfigSchema,
    pub seen: Mutex<Vec<ConfigurationAttributes<Value>>>,
    pub witnesses: AtomicUsize,
    pub initialized: AtomicUsize,
    pub custom_calls: AtomicUsize,
    pub semantic_calls: Arc<AtomicUsize>,
    custom: Option<Box<CustomCheck>>,
}
impl ConfigFactory {
    pub fn new(schema: ValidatedConfigSchema) -> Self {
        Self {
            name: "owner".into(),
            schema,
            seen: Default::default(),
            witnesses: AtomicUsize::new(0),
            initialized: AtomicUsize::new(0),
            custom_calls: AtomicUsize::new(0),
            semantic_calls: Arc::default(),
            custom: None,
        }
    }
    pub fn named(mut self, name: &str) -> Self {
        self.name = name.into();
        self
    }
    pub fn with_custom_validation(
        mut self,
        custom: impl Fn(
            &TypeIdentity,
            &CanonicalCustomTypeValue,
            &ProviderValidationInputs,
        ) -> ProviderResult<()>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.custom = Some(Box::new(custom));
        self
    }
}
impl ProviderFactory for ConfigFactory {
    fn name(&self) -> &str {
        &self.name
    }
    fn display_name(&self) -> &str {
        &self.name
    }

    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.semantic_calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap()
            .push(attributes.attributes().clone());
        Ok(())
    }
    fn validate_custom_type(
        &self,
        identity: &TypeIdentity,
        value: &CanonicalCustomTypeValue,
        inputs: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.custom_calls.fetch_add(1, Ordering::SeqCst);
        self.custom
            .as_ref()
            .map_or(Ok(()), |check| check(identity, value, inputs))
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![]
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        self.witnesses.fetch_add(1, Ordering::SeqCst);
        ValidatedProviderSchemaWitness::new(vec![], self.schema.clone(), Default::default(), vec![])
            .unwrap()
    }
    fn create_provider(
        &self,
        session: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        assert_eq!(
            self.seen.lock().unwrap().last().unwrap(),
            session.configuration().attributes()
        );
        self.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            #[cfg(not(target_arch = "wasm32"))]
            {
                Ok(InitializeOutcome::unavailable()
                    .with_provider(super::provider_text::idle_provider()))
            }
            #[cfg(target_arch = "wasm32")]
            {
                Err(ProviderError::invalid_input(
                    "configuration fixture requires a native host",
                ))
            }
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct ConfigurationDirectory {
    root: tempfile::TempDir,
}
#[cfg(not(target_arch = "wasm32"))]
impl ConfigurationDirectory {
    pub fn new(files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        for (name, source) in files {
            std::fs::write(root.path().join(name), source).unwrap();
        }
        Self { root }
    }
    pub fn path(&self) -> &std::path::Path {
        self.root.path()
    }
    pub fn load(&self, context: &ProviderContext) -> crate::config_loader::LoadedConfig {
        crate::config_loader::load_configuration_with_config(
            &crate::paths::ResolvedPath::new(self.path()).unwrap(),
            context,
            &crate::schema::SchemaRegistry::new(),
        )
        .unwrap()
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub fn provider_directory(providers: &str, values: &str) -> ResolvedProviderConfigs {
    let directory =
        ConfigurationDirectory::new(&[("providers.crn", providers), ("values.crn", values)]);
    let loaded = directory.load(&ProviderContext::new(crate::EnvironmentSnapshot::new([])));
    ResolvedProviderConfigs::from_test_configs(
        loaded
            .parsed
            .providers
            .iter()
            .map(
                |config| ProviderConfig::<NoPlaceholder, crate::resource::AbsoluteBindingAddress> {
                    name: config.name.clone(),
                    binding: config.binding.clone(),
                    attributes: config
                        .attributes
                        .iter()
                        .map(|(key, value)| {
                            (
                                key.clone(),
                                value
                                    .to_expanded()
                                    .and_then(Value::try_into_address_free_absolute)
                                    .unwrap(),
                            )
                        })
                        .collect(),
                    default_tags: Default::default(),
                    source: None,
                    version: None,
                    revision: None,
                    unresolved_attributes: Default::default(),
                },
            )
            .collect(),
    )
    .unwrap()
}
