//! Fixture generator uses main's public schema APIs so its no-`as` twins can
//! also be validated by the main branch's binary with the same native factory.
use carina_core::{provider::*, schema::*};

pub struct TotalityFactory;

impl ProviderFactory for TotalityFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Mock totality fixture"
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        let provider = carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        )
        .create_provider();
        Box::pin(
            async move { Ok(InitializeOutcome::unavailable().with_provider(Box::new(provider))) },
        )
    }
    fn create_normalizer(
        &self,
        _: &ProviderSession,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let name = || {
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("mock.Name")),
                None,
                Some((Some(1), Some(8))),
                None,
            )
        };
        let mut schemas = carina_provider_mock::builtin_schemas();
        schemas.extend([
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::optional("attr", name()).build())
                .attribute(AttributeSchema::provider_populated("read_only", name()).build())
                .attribute(
                    AttributeSchema::optional(
                        "entries",
                        AttributeType::list(AttributeType::struct_(
                            "Entry",
                            vec![StructField::optional("value", name()).build()],
                        )),
                    )
                    .with_block_name("entry")
                    .build(),
                )
                .attribute(AttributeSchema::optional("items", AttributeType::list(name())).build())
                .attribute(AttributeSchema::optional("values", AttributeType::map(name())).build())
                .attribute(
                    AttributeSchema::optional(
                        "config",
                        AttributeType::struct_(
                            "Config",
                            vec![StructField::optional("value", name()).build()],
                        ),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::optional(
                        "choice",
                        AttributeType::union(vec![name(), AttributeType::string()]),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("Source")
                .attribute(
                    AttributeSchema::optional(
                        "items",
                        AttributeType::list(AttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("Query")
                .as_data_source()
                .attribute(AttributeSchema::optional("attr", name()).build())
                .validate()
                .unwrap(),
            carina_provider_mock::unique_list_schema()
                .validate()
                .unwrap(),
        ]);
        schemas
    }
}

pub fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(TotalityFactory)]
}
