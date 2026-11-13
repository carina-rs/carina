use carina_core::provider::*;
use carina_core::schema::{
    ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

pub struct NativeConfigErrorFactory(pub ProviderError);

impl ProviderFactory for NativeConfigErrorFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "native configuration error fixture"
    }

    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Err(self.0.clone())
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        Vec::new()
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            ValidatedConfigSchema::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn create_provider(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        unreachable!("failed configuration must not initialize a provider")
    }
}
