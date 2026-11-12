use super::*;
use carina_core::provider::{
    BoxFuture, CanonicalCustomTypeValue, CanonicalProviderConfigAttributes, InitializeOutcomeMint,
    ProviderError, ProviderInitialization, ProviderResult, ProviderValidationInputs,
};
use carina_core::schema::{
    AttributeType, TypeIdentity, ValidatedConfigSchema, ValidatedProviderSchemaWitness,
    ValidatedResourceSchema,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

struct ConfigFactory {
    semantic: Arc<AtomicUsize>,
}
impl ProviderFactory for ConfigFactory {
    fn name(&self) -> &str {
        "owner"
    }
    fn display_name(&self) -> &str {
        "owner"
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![]
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::validate(BTreeMap::from([(
            "endpoints".into(),
            AttributeType::union(vec![
                AttributeType::string(),
                AttributeType::list(AttributeType::refined_string(
                    Some(TypeIdentity::new(
                        Some("owner"),
                        Vec::<String>::new(),
                        "Endpoint",
                    )),
                    None,
                    None,
                    None,
                )),
            ]),
        )]))
        .unwrap()
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            vec![],
            self.provider_config_attribute_types(),
            Default::default(),
            vec![],
        )
        .unwrap()
    }
    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.semantic.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn validate_custom_type(
        &self,
        _: &TypeIdentity,
        value: &CanonicalCustomTypeValue,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        if value.as_str() == "bad" {
            Err(ProviderError::invalid_input(
                "Endpoint callback rejects bad",
            ))
        } else {
            Ok(())
        }
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        panic!("LSP must not initialize providers")
    }
}

#[test]
fn provider_config_directory_compact_callback_parity() {
    for (value, rejected) in [("bad", true), ("good", false)] {
        let directory = tempfile::tempdir().unwrap();
        let source = "provider owner { endpoints = [Endpoint] }\n";
        let path = directory.path().join("providers.crn");
        std::fs::write(&path, source).unwrap();
        std::fs::write(
            directory.path().join("values.crn"),
            format!("let Endpoint = '{value}'\n"),
        )
        .unwrap();
        let semantic = Arc::new(AtomicUsize::new(0));
        let engine = DiagnosticEngine::new(
            loaded_providers_from_factories(vec![Box::new(ConfigFactory {
                semantic: Arc::clone(&semantic),
            })]),
            vec!["owner".into()],
        );
        let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
        let found = diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("Endpoint callback rejects bad"));
        assert_eq!(found, rejected, "{value}: {diagnostics:?}");
        if rejected {
            assert_eq!(semantic.load(Ordering::SeqCst), 0);
        } else {
            assert!(semantic.load(Ordering::SeqCst) > 0);
        }
    }
}
