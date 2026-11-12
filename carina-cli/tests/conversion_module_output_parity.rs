use carina_core::{provider::*, schema::*};
use std::{collections::HashMap, sync::Arc};
use tower_lsp::lsp_types::DiagnosticSeverity;

struct ConversionFactory;
impl ProviderFactory for ConversionFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Mock"
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
        unimplemented!("validation fixture never constructs a provider")
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
        vec![
            ResourceSchema::new("Sink")
                .attribute(
                    AttributeSchema::optional(
                        "attr",
                        AttributeType::refined_string(
                            Some(TypeIdentity::from_dotted("mock.Name")),
                            None,
                            Some((Some(1), Some(8))),
                            None,
                        ),
                    )
                    .build(),
                )
                .attribute(AttributeSchema::optional("plain", AttributeType::string()).build())
                .validate()
                .unwrap(),
        ]
    }
}
fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(ConversionFactory)]
}

#[test]
fn conversion_module_output_cli_and_lsp_use_the_same_admission_and_completion() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        "arguments { value: String }\nattributes { out: String = value }\n",
    )
    .unwrap();
    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine =
        carina_lsp::diagnostics::DiagnosticEngine::new(Arc::new(loaded), vec!["mock".into()]);
    let path = directory.path().join("main.crn");
    for (attributes, rejected) in [
        ("attr = net.out as mock.Name", false),
        ("plain = net.out attr = 'ok' as mock.Name", false),
        ("plain = net.out as mock.Name", true),
    ] {
        let source = format!(
            "let child = use {{ source = './child' }}\nlet net = child {{ value = 'ok' }}\nmock.Sink {{ {attributes} }}\n"
        );
        std::fs::write(&path, &source).unwrap();
        let cli = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
            HashMap::new(),
        );
        let document = carina_lsp::document::Document::new_with_path(
            source.clone(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        let lsp = engine.analyze(&document, Some(&path));
        let errors = lsp
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect::<Vec<_>>();
        assert_eq!(
            cli.errors.len(),
            usize::from(rejected),
            "{source}\n{:?}",
            cli.json
        );
        assert_eq!(errors.len(), usize::from(rejected), "{source}\n{lsp:#?}");
        if rejected {
            assert_eq!(cli.json["errors"][0]["code"], "conversion_destination");
            assert_eq!(cli.json["errors"][0]["message"], errors[0].message);
            assert_eq!(
                cli.json["errors"][0]["location"]["range"]["start"]["line"],
                errors[0].range.start.line + 1
            );
            assert_eq!(
                cli.json["errors"][0]["location"]["range"]["start"]["column"],
                errors[0].range.start.character + 1
            );
        }
    }
}
