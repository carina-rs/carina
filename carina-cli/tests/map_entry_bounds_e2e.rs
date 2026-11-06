//! carina#4256: validate map entry bounds before provider execution.

use carina_cli::wiring::LoadedProviderFactory;
use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderFactory,
    ProviderInitialization, ProviderReadyConfig, ProviderResult,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

struct MapBoundsFactory;

impl ProviderFactory for MapBoundsFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Map bounds validation mock"
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(&self, _: &CanonicalProviderConfigAttributes) -> ProviderResult<()> {
        Ok(())
    }

    fn create_provider(
        &self,
        _: Option<&str>,
        _: &ProviderReadyConfig,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { panic!("validation must not instantiate providers") })
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
        let params = AttributeType::refined_map(
            AttributeType::string(),
            AttributeType::string(),
            Some((Some(0), Some(10))),
        );
        vec![
            ResourceSchema::new("Request")
                .attribute(AttributeSchema::required("params", params).build())
                .validate()
                .unwrap(),
        ]
    }
}

fn validate_directory(entry_count: usize) -> Vec<String> {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let entries = (0..entry_count)
        .map(|i| format!("    key_{i} = 'value'\n"))
        .collect::<String>();
    std::fs::write(
        directory.path().join("main.crn"),
        format!("mock.Request {{\n  params = {{\n{entries}  }}\n}}\n"),
    )
    .unwrap();
    carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
        LoadedProviderFactory::for_test(vec![Box::new(MapBoundsFactory)]),
    )
}

#[test]
fn map_length_validate_accepts_ten_entries_in_directory() {
    let diagnostics = validate_directory(10);
    assert!(diagnostics.is_empty(), "{}", diagnostics.join("\n"));
}

#[test]
fn map_length_validate_rejects_eleven_entries_in_directory() {
    let diagnostics = validate_directory(11);
    assert_eq!(diagnostics.len(), 1, "{}", diagnostics.join("\n"));
    assert!(diagnostics[0].contains("params"), "{}", diagnostics[0]);
    assert!(
        diagnostics[0].contains("length 11 is outside allowed range [0, 10]"),
        "{}",
        diagnostics[0]
    );
}
