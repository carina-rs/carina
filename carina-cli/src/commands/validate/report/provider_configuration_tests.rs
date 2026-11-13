use super::*;
use carina_core::provider::{ProviderFactory, loaded_provider_snapshot_from_factories};
use carina_core::schema::{
    AttributeType, DslTransform, SchemaRegistry, TypeIdentity, ValidatedConfigSchema,
};
use carina_core::test_utils::configuration::{ConfigFactory, ConfigurationDirectory};
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use std::sync::Arc;
use tower_lsp::lsp_types::DiagnosticSeverity;

fn region_factory() -> Box<dyn ProviderFactory> {
    Box::new(ConfigFactory::new(
        ValidatedConfigSchema::validate(std::collections::BTreeMap::from([(
            "region".into(),
            AttributeType::enum_(
                TypeIdentity::new(Some("owner"), Vec::<String>::new(), "Region"),
                None,
                vec![],
                None,
                Some(DslTransform::HyphenToUnderscore),
            ),
        )]))
        .unwrap(),
    ))
}

#[test]
fn undeclared_enum_configuration_has_one_owner_diagnostic_in_both_clients() {
    let providers = "provider owner {\n  regoin = owner.Region.ap_northeast_1\n}\n";
    let main = "exports { message: String = 'value' }\n";
    let backend = "backend local { path = 'carina.state.json' }\n";
    let directory = ConfigurationDirectory::new(&[
        ("providers.crn", providers),
        ("main.crn", main),
        ("backend.crn", backend),
    ]);
    let path = ResolvedPath::new(directory.path()).unwrap();
    let report = super::super::collect_with_factories(
        &path,
        crate::wiring::LoadedProviderFactory::for_test(vec![region_factory()]),
        Default::default(),
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    report
        .render_human(&mut stdout, &mut stderr, ColorMode::Plain)
        .unwrap();
    let mut bytes = Vec::new();
    report.render_json(&mut bytes).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let factories = Arc::new(vec![region_factory()]);
    let loaded = loaded_provider_snapshot_from_factories(
        factories,
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(loaded), vec!["owner".into()]);
    let file = directory.path().join("providers.crn");
    let document = Document::new_with_path(
        providers.to_owned(),
        file.clone(),
        Arc::new(ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    let diagnostics: Vec<_> = engine
        .analyze(&document, Some(&file))
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    eprintln!(
        "CONFIG_ENUM_MEASUREMENT {}",
        serde_json::json!({"sources":{"main.crn":main,"providers.crn":providers,"backend.crn":backend},"cli_text":{"exit":report.exit_code(),"stdout":String::from_utf8(stdout).unwrap(),"stderr":String::from_utf8(stderr.clone()).unwrap()},"cli_json":{"exit":report.exit_code(),"document":json},"lsp":diagnostics})
    );
    let expected = "provider owner: regoin: Unknown attribute 'regoin', did you mean 'region'?";
    assert_eq!(report.exit_code(), 1);
    assert_eq!(
        String::from_utf8(stderr).unwrap(),
        format!("Error: {expected}\n")
    );
    assert_eq!(json["errors"].as_array().unwrap().len(), 1, "{json}");
    assert_eq!(json["errors"][0]["message"], expected);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].message, expected);
    assert_eq!(
        diagnostics[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(1, 2),
            tower_lsp::lsp_types::Position::new(1, 8)
        )
    );
}
