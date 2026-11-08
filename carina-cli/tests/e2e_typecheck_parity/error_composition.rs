use super::*;
use carina_core::provider::{ProviderError, ProviderErrorKind};
use carina_core::provider_text::ProviderDiagnostic;
use carina_core::schema::{ProviderValidation, TypeError, TypeIdentity, ValidatedConfigSchema};
use carina_core::test_utils::provider_text::{BindProviderText, context};

const EXPECTED: &str = "No union alternative accepts this value: Validation failed: provider arm: (secret); or Validation failed: plain arm";

fn union_failure(diagnostic: bool) -> TypeError {
    let first: ProviderValidation = if diagnostic {
        let error: ProviderError = ProviderError::api_error_caused_by(
            "provider arm",
            std::io::Error::other("union-private"),
        )
        .with_secrets_for_test(&context(&["union-private"]))
        .into();
        ProviderDiagnostic::new(error).into()
    } else {
        // Main's textual validation carrier renders this exact wording.
        "provider arm: (secret)".into()
    };
    let arm = |name, message: ProviderValidation| {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare(name),
            None,
            None,
            legacy_validator(move |_| Err(message.clone())),
            None,
        )
    };
    let schema = ValidatedConfigSchema::validate(BTreeMap::from([(
        "value".into(),
        AttributeType::union(vec![
            arm("FirstArm", first),
            arm("SecondArm", "plain arm".into()),
        ]),
    )]))
    .unwrap();
    let value: Value = Value::Concrete(ConcreteValue::String("invalid".into()));
    schema
        .rootless_type("value")
        .unwrap()
        .validate(&value)
        .expect_err("both alternatives reject this value")
}

struct UnionDiagnosticFactory {
    diagnostic: bool,
}

impl ProviderFactory for UnionDiagnosticFactory {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "union validation fixture"
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::default()
    }
    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Err(ProviderError::from_diagnostic(
            ProviderErrorKind::InvalidInput,
            union_failure(self.diagnostic),
        ))
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![ResourceSchema::new("probe").validate().unwrap()]
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        unreachable!("config validation does not initialize providers")
    }
}

#[test]
fn union_provider_diagnostic_cli_lsp_preserve_main_text_and_restore_provider_reason() {
    let fixture = write_fixture(&[("providers.crn", "provider test {}\n")]);
    let run = |diagnostic| {
        let cli = cli_diagnostics(
            vec![Box::new(UnionDiagnosticFactory { diagnostic })],
            &fixture,
        );
        let factories: Vec<Box<dyn ProviderFactory>> =
            vec![Box::new(UnionDiagnosticFactory { diagnostic })];
        let providers = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(factories),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let engine = DiagnosticEngine::new(Arc::new(providers), vec!["test".into()]);
        let lsp = lsp_diagnostics(&engine, &fixture, "providers.crn");
        (cli, lsp)
    };
    let (reference_cli, reference_lsp) = run(false);
    assert!(
        reference_cli.iter().any(|text| text.contains(EXPECTED)),
        "{reference_cli:#?}"
    );
    assert!(
        reference_lsp
            .iter()
            .any(|item| item.message.contains(EXPECTED)),
        "{reference_lsp:#?}"
    );
    let (cli, lsp) = run(true);
    assert_eq!(cli, reference_cli);
    assert_eq!(lsp, reference_lsp);
    for text in cli
        .iter()
        .map(String::as_str)
        .chain(lsp.iter().map(|item| item.message.as_str()))
    {
        assert!(!text.contains("union-private"), "{text}");
        assert!(!text.contains("<provider diagnostic>"), "{text}");
    }
}
