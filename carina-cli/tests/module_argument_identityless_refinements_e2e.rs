//! Declared module arguments preserve deferred checks at identity-less sinks.

use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderError,
    ProviderFactory, ProviderInitialization, ProviderResult,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

struct RefinedValueFactory {
    wrap: fn(AttributeType) -> AttributeType,
}

impl ProviderFactory for RefinedValueFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "Module argument refinement fixture"
    }

    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async {
            Err(ProviderError::internal(
                "validation must not initialize a provider",
            ))
        })
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let original = AttributeType::refined_string(
            None,
            Some(carina_core::schema::PatternSpec::new(
                "^[a-z]+$",
                carina_core::schema::PatternMatch::Full,
            )),
            Some((Some(1), Some(100))),
            None,
        );
        let sink = AttributeType::refined_string(None, None, Some((Some(1), Some(64))), None);
        vec![
            ResourceSchema::new("Source").attribute(
                AttributeSchema::provider_populated("label", (self.wrap)(original)).build(),
            ),
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::required("name", (self.wrap)(sink)).build()),
        ]
        .into_iter()
        .map(|schema| schema.validate().expect("valid refinement fixture"))
        .collect()
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            ValidatedConfigSchema::default(),
            Default::default(),
            Vec::new(),
        )
        .expect("valid provider fixture without edges")
    }
}

fn validate_identityless_sink(argument_type: &str, wrap: fn(AttributeType) -> AttributeType) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let consumer = temp.path().join("consumer");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&consumer).unwrap();
    std::fs::write(
        root.join("main.crn"),
        "provider test {}\n\
         let source = test.Source {}\n\
         let consumer = use { source = '../consumer' }\n\
         let instance = consumer { name = source.label }\n",
    )
    .unwrap();
    std::fs::write(
        consumer.join("arguments.crn"),
        format!("arguments {{ name: {argument_type} }}\n"),
    )
    .unwrap();
    std::fs::write(
        consumer.join("resources.crn"),
        "let sink = test.Sink { name = name }\n",
    )
    .unwrap();

    let diagnostics = carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(&root).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(vec![Box::new(RefinedValueFactory {
            wrap,
        })]),
    );
    assert!(
        diagnostics.is_empty(),
        "{argument_type} must defer the identity-less sink's length check until the value is known: {diagnostics:#?}"
    );
}

#[test]
fn module_argument_string_defers_identityless_sink_refinements_without_edges() {
    validate_identityless_sink("String", |ty| ty);
}

#[test]
fn module_argument_list_defers_identityless_sink_refinements_without_edges() {
    validate_identityless_sink("list(String)", AttributeType::list);
}

#[test]
fn module_argument_map_defers_identityless_sink_refinements_without_edges() {
    validate_identityless_sink("map(String)", AttributeType::map);
}
