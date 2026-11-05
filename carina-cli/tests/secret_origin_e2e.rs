use carina_cli::wiring::LoadedProviderFactory;
use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderFactory,
    ProviderInitialization, ProviderReadyConfig, ProviderResult,
};
use carina_core::schema::{
    ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

#[path = "../../carina-core/tests/support/secret_origin.rs"]
mod fixture;

struct SecretFactory(bool);
impl ProviderFactory for SecretFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Secret diagnostic fixture"
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![if self.0 {
            fixture::reference_schema()
        } else {
            fixture::schema()
        }]
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
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
    fn validate_config(&self, _: &CanonicalProviderConfigAttributes) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: Option<&str>,
        _: &ProviderReadyConfig,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { panic!("validation never instantiates providers") })
    }
}

#[test]
fn secret_origin_4275_cli_directory_diagnostics_are_redacted() {
    let dir = tempfile::tempdir().unwrap();
    fixture::write_directory(dir.path());
    let diagnostics = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(dir.path()).unwrap(),
        LoadedProviderFactory::for_test(vec![Box::new(SecretFactory(false))]),
        Default::default(),
    );
    let messages = diagnostics
        .errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        messages.contains("expected Int") && messages.contains("String"),
        "{messages}"
    );
    assert!(messages.contains("main.crn"), "{messages}");
    assert!(
        !format!(
            "{messages}\n{:?}\n{:?}",
            diagnostics.errors, diagnostics.warnings
        )
        .contains("private-")
    );
}

#[test]
fn secret_origin_4275_cli_nested_failure_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    fixture::write_directory(dir.path());
    std::fs::write(
        dir.path().join("main.crn"),
        "mock.test.Secret { params = secret(cidr_subnet(\"private-invalid-cidr\", 1, 1)) }\n",
    )
    .unwrap();
    let diagnostics = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(dir.path()).unwrap(),
        LoadedProviderFactory::for_test(vec![Box::new(SecretFactory(false))]),
        Default::default(),
    );
    assert!(!diagnostics.errors.is_empty());
    for error in &diagnostics.errors {
        assert!(!format!("{error}\n{error:?}").contains("private-"));
    }
}

#[test]
fn secret_origin_4275_cli_reference_errors_are_redacted() {
    let dir = tempfile::tempdir().unwrap();
    fixture::write_directory(dir.path());
    std::fs::write(
        dir.path().join("main.crn"),
        "mock.test.Secret { params = secret(producer.config.private_field) }\n",
    )
    .unwrap();
    let diagnostics = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(dir.path()).unwrap(),
        LoadedProviderFactory::for_test(vec![Box::new(SecretFactory(true))]),
        Default::default(),
    );
    assert!(!diagnostics.errors.is_empty());
    for error in &diagnostics.errors {
        assert!(!format!("{error}\n{error:?}").contains("private_field"));
    }
}
