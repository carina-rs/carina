use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderFactory,
    ProviderInitialization, ProviderResult, ProviderSession, ProviderValidationInputs,
};
use carina_core::schema::{
    AttributeType, ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

/// A test declaration for values that cannot cross the configuration wire.
/// The wrapped factory's real hooks remain in place behind host admission.
pub struct DurationConfiguration<'a>(pub &'a dyn ProviderFactory);

impl ProviderFactory for DurationConfiguration<'_> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn display_name(&self) -> &str {
        self.0.display_name()
    }

    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        session: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.0.validate_config(attributes, session)
    }

    fn create_provider(
        &self,
        session: &ProviderSession,
        mint: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        self.0.create_provider(session, mint)
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        self.0.schemas()
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        let witness = self.0.schema_witness();
        ValidatedProviderSchemaWitness::new(
            witness.resources().to_vec(),
            ValidatedConfigSchema::validate(std::collections::BTreeMap::from([(
                "duration".into(),
                AttributeType::duration(),
            )]))
            .unwrap(),
            witness.reference_forms().clone(),
            witness.assignability_edges().to_vec(),
        )
        .unwrap()
    }
}
