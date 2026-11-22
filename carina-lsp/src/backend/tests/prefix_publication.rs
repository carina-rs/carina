//! Full publication maps from the R9 standalone/caller measurement corpus.
use super::{FactoryBuilder, ProviderStates, reload_provider_states};
use crate::{
    document::Document,
    factories::{build_factories, install_prober},
};
use carina_core::{EnvironmentSnapshot, UserDirs, parser::ProviderContext};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

const INPUTS: &str =
    include_str!("../../../../carina-cli/tests/fixtures/deferred_prefix/r10/inputs.json");
const DIRECTORIES: &str = include_str!(
    "../../../../carina-cli/tests/fixtures/deferred_prefix/r10/module_directories.json"
);
const EXPECTED: &str =
    include_str!("../../../../carina-cli/tests/fixtures/deferred_prefix/r10/publications.json");

async fn states(root: &Path) -> (ProviderStates, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let dirs = UserDirs::under(home.path());
    let builder: FactoryBuilder =
        Arc::new(move |configs| build_factories(Ok(&dirs), configs, &Default::default()));
    let states = tokio::sync::RwLock::new(ProviderStates::new(EnvironmentSnapshot::empty()));
    let loaded = reload_provider_states(root, &builder, Some(&install_prober()), &states).await;
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert!(loaded.schemas > 0);
    (states.into_inner(), home)
}

async fn assert_publications(module_documents: bool) {
    let inputs: Value = serde_json::from_str(INPUTS).unwrap();
    let directories: Value = serde_json::from_str(DIRECTORIES).unwrap();
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    assert_eq!(
        inputs.as_object().unwrap().keys().collect::<Vec<_>>(),
        expected.as_object().unwrap().keys().collect::<Vec<_>>()
    );
    let mut opened = 0;
    for (case, files) in inputs.as_object().unwrap() {
        let root = tempfile::tempdir().unwrap();
        for (file, source) in files.as_object().unwrap() {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source.as_str().unwrap()).unwrap();
        }
        let (states, _home) = states(root.path()).await;
        for (file, source) in files.as_object().unwrap() {
            if !file.ends_with(".crn") {
                continue;
            }
            let parent = Path::new(file).parent().unwrap().to_str().unwrap();
            let is_module = directories[case]
                .as_array()
                .unwrap()
                .iter()
                .any(|dir| dir == parent);
            if is_module != module_documents {
                continue;
            }
            let path = root.path().join(file);
            let document = Document::new_with_path(
                source.as_str().unwrap().into(),
                path.clone(),
                Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
            );
            let report = states
                .state_for_path(path.parent().unwrap())
                .diagnostic_engine
                .analyze_document_snapshot(&document, Some(&path), &[]);
            let inventory = report.documents.into_iter().map(|(path, findings)| {
                let file = path.strip_prefix(root.path()).unwrap().to_string_lossy().into_owned();
                let diagnostics = findings.into_iter().map(|finding| {
                    let diagnostic = finding.diagnostic;
                    json!({"range": diagnostic.range, "severity": diagnostic.severity, "message": diagnostic.message})
                }).collect::<Vec<_>>();
                (file, json!(diagnostics))
            }).collect::<serde_json::Map<_, _>>();
            assert_eq!(
                Value::Object(inventory),
                expected[case][file],
                "{case}: only {file} open; entire map including unopened modules"
            );
            opened += 1;
        }
    }
    assert_eq!(opened, if module_documents { 48 } else { 112 });
}

#[tokio::test]
async fn r10_prefix_root_publication_maps_preserve_head() {
    // The original 23-case corpus contributes 108 maps; the literal-only
    // control contributes four more. Every routed file remains part of the pin.
    assert_publications(false).await;
}

#[tokio::test]
async fn r10_prefix_module_publication_maps_use_standalone_authority() {
    assert_publications(true).await;
}

#[tokio::test]
async fn r10_prefix_standalone_module_includes_its_own_nested_expansion() {
    let inputs: Value = serde_json::from_str(include_str!(
        "../../../../carina-cli/tests/fixtures/deferred_prefix/c/inputs.json"
    ))
    .unwrap();
    let expected: Value = serde_json::from_str(include_str!(
        "../../../../carina-cli/tests/fixtures/deferred_prefix/r10/nested_standalone.json"
    ))
    .unwrap();
    for (case, scopes) in expected.as_object().unwrap() {
        let root = tempfile::tempdir().unwrap();
        let files = inputs[case].as_object().unwrap();
        for (file, source) in files {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source.as_str().unwrap()).unwrap();
        }
        let (states, _home) = states(root.path()).await;
        for (scope, expected) in scopes.as_object().unwrap() {
            for (file, source) in files {
                if !file.ends_with(".crn") || Path::new(file).parent() != Some(Path::new(scope)) {
                    continue;
                }
                let path = root.path().join(file);
                let document = Document::new_with_path(
                    source.as_str().unwrap().into(),
                    path.clone(),
                    Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
                );
                let report = states
                    .state_for_path(path.parent().unwrap())
                    .diagnostic_engine
                    .analyze_document_snapshot(&document, Some(&path), &[]);
                let prefixes = report.documents.into_iter().filter_map(|(path, findings)| {
                    let file = path.strip_prefix(root.path()).unwrap().to_string_lossy().into_owned();
                    let findings = findings.into_iter().map(|finding| finding.diagnostic)
                        .filter(|diagnostic| diagnostic.message.starts_with("'name_prefix' must be known"))
                        .map(|diagnostic| json!({"range":diagnostic.range,"severity":diagnostic.severity,"message":diagnostic.message})).collect::<Vec<_>>();
                    (!findings.is_empty()).then(|| (file,json!(findings)))
                }).collect::<serde_json::Map<_,_>>();
                assert_eq!(
                    Value::Object(prefixes),
                    *expected,
                    "{case}: only {file} open; its own standalone imports, not an outer caller"
                );
            }
        }
    }
}
