//! Directory-scoped CLI/LSP parity for whole-secret collection validation (#4275).

use std::sync::Arc;

use carina_cli::wiring::LoadedProviderFactory;
use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderFactory,
    ProviderInitialization, ProviderResult, loaded_provider_snapshot_from_factories,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity,
    ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use carina_lsp::diagnostics::DiagnosticEngine;
use carina_lsp::document::Document;

struct CollectionFactory;

impl ProviderFactory for CollectionFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Secret collection diagnostics fixture"
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
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
        let password = AttributeType::refined_string(None, None, Some((Some(2), Some(4))), None);
        vec![
            ResourceSchema::new("test.Collection")
                .attribute(
                    AttributeSchema::optional("passwords", AttributeType::list(password.clone()))
                        .build(),
                )
                .attribute(
                    AttributeSchema::optional(
                        "modes",
                        AttributeType::list(AttributeType::enum_(
                            TypeIdentity::bare("Mode"),
                            Some(vec!["allowed".into()]),
                            Vec::new(),
                            None,
                            None,
                        )),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::optional("password_map", AttributeType::map(password)).build(),
                )
                .validate()
                .unwrap(),
        ]
    }
}

fn assert_secret_collection_parity(
    attribute: &str,
    assignment: &str,
    expected: &str,
    private: &[&str],
) {
    let directory = tempfile::tempdir().unwrap();
    let source = format!("mock.test.Collection {{\n  {assignment}\n}}\n");
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(directory.path().join("main.crn"), &source).unwrap();

    let cli = carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
        LoadedProviderFactory::for_test(vec![Box::new(CollectionFactory)]),
    );
    assert_eq!(cli.len(), 1, "CLI diagnostics: {cli:#?}");
    // CLI prefixes the resource and spells list locations as a wrapper. LSP
    // puts the same index in the attribute name, so normalize those labels.
    let cli_messages = cli
        .iter()
        .map(|error| {
            let (_, message) = error.split_once(": ").expect("CLI resource label");
            message
                .strip_prefix("List item at index 0: ")
                .unwrap_or(message)
        })
        .collect::<Vec<_>>();
    assert_eq!(cli_messages, [expected], "CLI diagnostics: {cli:#?}");

    let providers = loaded_provider_snapshot_from_factories(
        Arc::new(vec![Box::new(CollectionFactory) as Box<dyn ProviderFactory>]),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(providers), vec!["mock".into()]);
    let mut lsp = Vec::new();
    for name in ["providers.crn", "main.crn"] {
        let path = directory.path().join(name);
        let doc = Document::new_with_path(
            std::fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        let diagnostics = engine.analyze(&doc, Some(&path));
        if name == "providers.crn" {
            assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        }
        lsp.extend(diagnostics);
    }
    let lsp_messages = lsp
        .iter()
        .map(|diagnostic| {
            assert_eq!(
                diagnostic.range,
                tower_lsp::lsp_types::Range::new(
                    tower_lsp::lsp_types::Position::new(1, 2),
                    tower_lsp::lsp_types::Position::new(1, 2 + attribute.len() as u32),
                )
            );
            diagnostic
                .message
                .replace(&format!("'{attribute}[0]'"), &format!("'{attribute}'"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lsp_messages, cli_messages,
        "LSP and CLI must report the same masked item error: {lsp:#?}"
    );
    let rendered = format!("{cli:#?}\n{lsp:#?}");
    for value in private {
        assert!(!rendered.contains(value), "leaked {value}: {rendered}");
    }
}

#[test]
fn whole_secret_refined_list_directory_lsp_matches_cli() {
    assert_secret_collection_parity(
        "passwords",
        "passwords = secret(['way-too-long'])",
        "Invalid value (secret) for 'passwords': length 12 is outside allowed range [2, 4]",
        &["way-too-long"],
    );
}

#[test]
fn whole_secret_enum_list_directory_lsp_matches_cli() {
    assert_secret_collection_parity(
        "modes",
        "modes = secret([\"bogus\"])",
        "'modes' (Mode) expects an enum identifier, got a string literal (secret). Use one of: allowed",
        &["bogus"],
    );
}

#[test]
fn whole_secret_refined_map_directory_lsp_matches_cli() {
    assert_secret_collection_parity(
        "password_map",
        "password_map = secret({ private_key = 'way-too-long' })",
        "Map value for key '(secret)': Invalid value (secret) for 'password_map': length 12 is outside allowed range [2, 4]",
        &["private_key", "way-too-long"],
    );
}
