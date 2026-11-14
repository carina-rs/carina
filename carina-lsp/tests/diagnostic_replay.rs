#[path = "support/replay.rs"]
mod replay;

use carina_core::{EnvironmentSnapshot, UserDirs, parser::ProviderContext};
use carina_lsp::admission_coverage::{AdmissionCoverage, with_admission_coverage};
use carina_lsp::backend::{
    FactoryBuilder, ProviderStates, analyze_document, reload_provider_states,
};
use carina_lsp::document::Document;
use carina_lsp::factories::{build_factories, install_prober};
use std::path::Path;
use std::sync::Arc;

fn control() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/diagnostic_replay/control");
    std::fs::create_dir(temp.path().join("module")).unwrap();
    for name in [
        "main.crn",
        "providers.crn",
        "exports.crn",
        "backend.crn",
        "module/main.crn",
        "module/attributes.crn",
    ] {
        std::fs::copy(source.join(name), temp.path().join(name)).unwrap();
    }
    temp
}

async fn states(root: &Path) -> ProviderStates {
    let dirs = UserDirs::under(&root.join("replay-home"));
    let builder: FactoryBuilder =
        Arc::new(move |configs| build_factories(Ok(&dirs), configs, &Default::default()));
    let states = tokio::sync::RwLock::new(ProviderStates::new(EnvironmentSnapshot::empty()));
    reload_provider_states(root, &builder, Some(&install_prober()), &states).await;
    states.into_inner()
}

fn analyze(
    states: &ProviderStates,
    path: &Path,
) -> (Vec<tower_lsp::lsp_types::Diagnostic>, AdmissionCoverage) {
    let document = Document::new_with_path(
        std::fs::read_to_string(path).unwrap(),
        path.to_path_buf(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    with_admission_coverage(|| analyze_document(states, &document, Some(path)))
}

#[test]
fn replay_rejects_zero_admission() {
    assert!(replay::validate_coverage(&AdmissionCoverage::default(), false).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_observes_both_admission_paths() {
    let fixture = control();
    let states = states(fixture.path()).await;
    let (diagnostics, coverage) = analyze(&states, &fixture.path().join("main.crn"));
    assert!(
        coverage.relative.reached_admission > 0,
        "{coverage:?}; {diagnostics:?}"
    );
    assert!(
        coverage.expanded.reached_admission > 0,
        "{coverage:?}; {diagnostics:?}"
    );
    replay::validate_coverage(&coverage, true).unwrap();
}

#[test]
fn replay_tracked_corpus_is_process_deterministic() {
    replay::gate("replay_tracked_corpus_is_process_deterministic", 0);
}

#[test]
fn replay_requires_each_repository_path_and_each_infra_provider() {
    let mut coverage = AdmissionCoverage::default();
    coverage.relative.reached_admission = 1;
    assert!(replay::validate_coverage(&coverage, true).is_err());
    coverage.expanded.reached_admission = 1;
    assert!(replay::validate_coverage(&coverage, true).is_ok());
    coverage
        .expanded
        .skipped_missing_schema
        .insert("unavailable".into(), 1);
    assert!(replay::validate_coverage(&coverage, false).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_uses_disk_siblings_and_only_the_opened_buffer_override() {
    let fixture = control();
    let root = fixture.path();
    std::fs::write(root.join("bindings.crn"), "let sibling = 123\n").unwrap();
    std::fs::write(
        root.join("main.crn"),
        "mock.test.resource { name = sibling }\n",
    )
    .unwrap();
    let states = states(root).await;
    let (disk, coverage) = analyze(&states, &root.join("main.crn"));
    assert!(coverage.relative.reached_admission > 0);
    assert!(
        disk.iter()
            .any(|diagnostic| diagnostic.message.contains("String")),
        "{disk:?}"
    );
    let path = root.join("bindings.crn");
    let unsaved = Document::new_with_path(
        "let sibling = 'unsaved'\n".into(),
        path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let _ = with_admission_coverage(|| analyze_document(&states, &unsaved, Some(&path)));
    let (disk_again, _) = analyze(&states, &root.join("main.crn"));
    assert_eq!(
        disk, disk_again,
        "previously opened buffers cannot become sibling overrides"
    );
    let path = root.join("main.crn");
    let unsaved = Document::new_with_path(
        "mock.test.resource { name = 'valid' }\n".into(),
        path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let (diagnostics, _) =
        with_admission_coverage(|| analyze_document(&states, &unsaved, Some(&path)));
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("String")),
        "{diagnostics:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_records_expanded_schema_declines_separately_from_module_skips() {
    use carina_lsp::admission_coverage::ExpandedDeclineReason;
    let fixture = control();
    std::fs::write(
        fixture.path().join("module/main.crn"),
        "let missing = absent.test.resource { name = 'missing' }\n",
    )
    .unwrap();
    let states = states(fixture.path()).await;
    let (_, coverage) = analyze(&states, &fixture.path().join("main.crn"));
    assert!(
        coverage
            .expanded
            .skipped_missing_schema
            .get("absent")
            .is_some_and(|count| *count > 0),
        "{coverage:?}"
    );
    assert!(
        coverage
            .expanded_declined_skipped_module_resource
            .get(&ExpandedDeclineReason::MissingSchema)
            .is_some_and(|count| *count > 0),
        "{coverage:?}"
    );
}

#[test]
fn replay_preserves_diagnostic_fields_order_and_duplicates_and_normalizes_nested_paths() {
    use serde_json::json;
    use tower_lsp::lsp_types::*;
    let root = Path::new("/corpus with spaces");
    let uri = Url::from_file_path(root.join("related.crn")).unwrap();
    let diagnostic = Diagnostic {
        range: Range::new(Position::new(2, 3), Position::new(4, 5)),
        severity: Some(DiagnosticSeverity::WARNING),
        code: Some(NumberOrString::String("code".into())),
        code_description: Some(CodeDescription { href: uri.clone() }),
        source: Some("carina".into()),
        message: format!("{}/main.crn", root.display()),
        related_information: Some(vec![DiagnosticRelatedInformation {
            location: Location {
                uri: uri.clone(),
                range: Range::default(),
            },
            message: uri.to_string(),
        }]),
        tags: Some(vec![DiagnosticTag::UNNECESSARY]),
        data: Some(
            json!({"nested": [uri.to_string(), "/corpus with spaces-more/keep.crn", "/corpus with spaces/buffer.crn"]}),
        ),
    };
    let mut second = diagnostic.clone();
    second.message = "second diagnostic".into();
    let diagnostics = vec![diagnostic.clone(), second, diagnostic];
    let bytes = replay::document_json(
        "repo/main.crn",
        &diagnostics,
        &AdmissionCoverage::default(),
        &[(root.to_path_buf(), "$CORPUS")],
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for (index, expected) in diagnostics.iter().enumerate() {
        let mut actual = value["diagnostics"][index].clone();
        assert_eq!(
            actual.as_object_mut().unwrap().remove("ordinal"),
            Some(json!(index))
        );
        let mut expected = serde_json::to_value(expected).unwrap();
        replay::normalize(&mut expected, &[(root.to_path_buf(), "$CORPUS")]);
        assert_eq!(actual, expected);
    }
    let diagnostic = &value["diagnostics"][0];
    assert_eq!(diagnostic["message"], "$CORPUS/main.crn");
    assert_eq!(
        diagnostic["data"]["nested"][0],
        "file:///$CORPUS/related.crn"
    );
    assert_eq!(
        diagnostic["data"]["nested"][1],
        "/corpus with spaces-more/keep.crn"
    );
    assert_eq!(diagnostic["data"]["nested"][2], "$CORPUS/buffer.crn");
}

#[test]
fn replay_artifact_selection_rejects_ambiguity_and_preserves_relative_source_authority() {
    use carina_core::parser::ProviderConfig;
    let root = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let config = ProviderConfig {
        name: "mock".into(),
        attributes: Default::default(),
        default_tags: Default::default(),
        source: Some("file://./original.wasm".parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: Default::default(),
        binding: None,
    };
    let configs = vec![(root.path().to_path_buf(), config)];
    let selected =
        replay::artifacts::project(&configs, root.path(), Some(artifacts.path())).unwrap();
    assert_eq!(
        selected[0].1.source.as_ref().unwrap().to_string(),
        format!("file://{}/./original.wasm", root.path().display())
    );
    for name in ["mock.wasm", "carina_provider_mock.wasm"] {
        std::fs::write(artifacts.path().join(name), b"fixture").unwrap();
    }
    assert!(replay::artifacts::project(&configs, root.path(), Some(artifacts.path())).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_discovery_scope_is_relative_to_workspace_and_shared_by_both_scanners() {
    use carina_lsp::workspace::DiscoveryScope;
    let fixture = control();
    let root = fixture.path().join(".worktrees/workspace");
    std::fs::create_dir_all(&root).unwrap();
    for name in [
        "visible",
        "target/hidden",
        ".worktrees/hidden",
        "evidence/hidden",
    ] {
        std::fs::create_dir_all(root.join(name)).unwrap();
        std::fs::write(
            root.join(name).join("providers.crn"),
            "provider mock {}\nlet child = use { source = './module' }\n",
        )
        .unwrap();
    }
    let document = root.join("visible/providers.crn");
    let scope = DiscoveryScope::replay(&root, vec![root.join("evidence")], &[document]).unwrap();
    assert!(!scope.prunes(&root));
    assert!(
        DiscoveryScope::replay(&root, vec![], &[root.join("target/hidden/providers.crn")]).is_err()
    );
    let builder: FactoryBuilder = Arc::new(|_| (vec![], Default::default(), vec![]));
    let providers = tokio::sync::RwLock::new(
        ProviderStates::new(EnvironmentSnapshot::empty()).with_replay_discovery(scope),
    );
    let first = reload_provider_states(&root, &builder, None, &providers).await;
    assert_eq!(first.directories, 1);
    std::fs::write(
        root.join("target/hidden/providers.crn"),
        "provider absent {}\nlet child = use { source = './changed' }\n",
    )
    .unwrap();
    let again = reload_provider_states(&root, &builder, None, &providers).await;
    assert!(!again.reloaded);
    assert!(!again.import_map_changed);
    let unrestricted = tokio::sync::RwLock::new(ProviderStates::new(EnvironmentSnapshot::empty()));
    assert_eq!(
        reload_provider_states(&root, &builder, None, &unrestricted)
            .await
            .directories,
        4
    );
}

#[test]
fn replay_manifest_rejects_changed_missing_and_new_sibling_inputs() {
    use replay::manifest::{Manifest, sha256};
    use std::collections::BTreeMap;
    let fixture = control();
    let root = replay::manifest::existing_locator(fixture.path());
    let mut inputs = BTreeMap::new();
    for name in [
        "main.crn",
        "providers.crn",
        "exports.crn",
        "backend.crn",
        "module/main.crn",
        "module/attributes.crn",
    ] {
        let path = root.join(name);
        inputs.insert(path.clone(), sha256(&path));
    }
    let mut manifest = Manifest {
        format: "carina-lsp-replay-input-v1".into(),
        root: root.clone(),
        corpus: "repo".into(),
        documents: vec!["main.crn".into()],
        inputs,
        exclusions: vec![],
        environment: BTreeMap::new(),
        replay_home: root.join("home"),
        artifacts: None,
        revision: "fixture".into(),
        shard: None,
        inventory: None,
        dependency_roots: Vec::new(),
    };
    manifest.freeze_inputs();
    manifest.verify();
    assert!(manifest.environment().get("AWS_REGION").is_none());
    let original = std::fs::read(root.join("main.crn")).unwrap();
    std::fs::write(root.join("main.crn"), "# changed\n").unwrap();
    assert!(std::panic::catch_unwind(|| manifest.verify()).is_err());
    std::fs::write(root.join("main.crn"), original).unwrap();
    std::fs::write(root.join("new-sibling.crn"), "exports {}\n").unwrap();
    assert!(std::panic::catch_unwind(|| manifest.verify()).is_err());
    std::fs::remove_file(root.join("new-sibling.crn")).unwrap();
    std::fs::write(root.join("new-provider.wasm"), b"new artifact").unwrap();
    assert!(std::panic::catch_unwind(|| manifest.verify()).is_err());
    std::fs::remove_file(root.join("new-provider.wasm")).unwrap();
    std::fs::remove_file(root.join("exports.crn")).unwrap();
    assert!(std::panic::catch_unwind(|| manifest.verify()).is_err());
}

// Six consecutive named tests distribute the complete directory corpus across
// the six CI partitions. Each directory still runs in eight fresh processes.
#[test]
fn replay_tracked_corpus_is_process_deterministic_shard_1() {
    replay::gate("replay_tracked_corpus_is_process_deterministic_shard_1", 1);
}
#[test]
fn replay_tracked_corpus_is_process_deterministic_shard_2() {
    replay::gate("replay_tracked_corpus_is_process_deterministic_shard_2", 2);
}
#[test]
fn replay_tracked_corpus_is_process_deterministic_shard_3() {
    replay::gate("replay_tracked_corpus_is_process_deterministic_shard_3", 3);
}
#[test]
fn replay_tracked_corpus_is_process_deterministic_shard_4() {
    replay::gate("replay_tracked_corpus_is_process_deterministic_shard_4", 4);
}
#[test]
fn replay_tracked_corpus_is_process_deterministic_shard_5() {
    replay::gate("replay_tracked_corpus_is_process_deterministic_shard_5", 5);
}

#[test]
fn replay_shards_partition_every_tracked_directory_once() {
    use std::collections::{BTreeMap, BTreeSet};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let documents = replay::manifest::tracked_documents(root);
    let mut manifest = replay::manifest::Manifest {
        format: "carina-lsp-replay-input-v1".into(),
        root: root.to_path_buf(),
        corpus: "repo".into(),
        documents: documents.clone(),
        inputs: BTreeMap::new(),
        exclusions: vec![],
        environment: BTreeMap::new(),
        replay_home: root.join("unused"),
        artifacts: None,
        revision: "fixture".into(),
        shard: None,
        inventory: None,
        dependency_roots: vec![],
    };
    let mut seen = BTreeSet::new();
    let mut directory_owners = BTreeMap::new();
    for index in 0..replay::SHARDS {
        manifest.shard = Some((index, replay::SHARDS));
        let selected = manifest.selected_documents();
        assert!(!selected.is_empty());
        for document in selected {
            assert!(seen.insert(document.clone()), "document assigned twice");
            let owner = directory_owners
                .entry(document.parent().unwrap().to_path_buf())
                .or_insert(index);
            assert_eq!(*owner, index, "one directory cannot span shards");
        }
    }
    assert_eq!(seen.into_iter().collect::<Vec<_>>(), documents);
}

#[test]
fn replay_tracked_corpus_reaches_both_admission_paths() {
    replay::coverage_gate("replay_tracked_corpus_reaches_both_admission_paths");
}

#[test]
fn replay_manifest_leaves_provider_rejection_to_production_construction() {
    use std::collections::BTreeMap;
    let fixture = control();
    let root = replay::manifest::existing_locator(fixture.path());
    std::fs::write(
        root.join("providers.crn"),
        "provider mock {}\nprovider absent { source = 'example/absent' }\n",
    )
    .unwrap();
    let mut manifest = replay::manifest::Manifest {
        format: "carina-lsp-replay-input-v1".into(),
        root: root.clone(),
        corpus: "infra".into(),
        documents: vec!["main.crn".into()],
        inputs: BTreeMap::new(),
        exclusions: vec![],
        environment: BTreeMap::new(),
        replay_home: root.join("home"),
        artifacts: Some(root.clone()),
        revision: "fixture".into(),
        shard: None,
        inventory: None,
        dependency_roots: vec![],
    };
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| manifest.freeze_inputs())).is_ok(),
        "the shared builder must own rejection policy and render its load error into replay evidence"
    );
}

#[test]
fn replay_supports_each_flat_name_and_both_installed_layouts_without_rewriting_configs() {
    use carina_core::parser::ProviderConfig;
    let fixture = control();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".carina/providers")).unwrap();
    let config = ProviderConfig {
        name: "mock".into(),
        source: Some("example/mock".parse().unwrap()),
        version: Some(carina_core::version_constraint::VersionConstraint::parse("^1.0").unwrap()),
        revision: None,
        attributes: Default::default(),
        default_tags: Default::default(),
        unresolved_attributes: Default::default(),
        binding: None,
    };
    let configs = vec![(fixture.path().to_path_buf(), config.clone())];
    for directory in [
        project.path().to_path_buf(),
        project.path().join(".carina/providers"),
    ] {
        let projected =
            replay::artifacts::project(&configs, fixture.path(), Some(&directory)).unwrap();
        assert_eq!(projected[0].0, project.path());
        assert_eq!(projected[0].1.source, config.source);
        assert_eq!(projected[0].1.version, config.version);
    }
    let flat = tempfile::tempdir().unwrap();
    for name in [
        "mock.wasm",
        "carina_provider_mock.wasm",
        "carina-provider-mock.wasm",
    ] {
        let artifact = flat.path().join(name);
        std::fs::write(&artifact, b"fixture").unwrap();
        let projected =
            replay::artifacts::project(&configs, fixture.path(), Some(flat.path())).unwrap();
        assert_eq!(
            projected[0].1.source.as_ref().unwrap().to_string(),
            format!("file://{}", artifact.display())
        );
        assert!(projected[0].1.version.is_none());
        std::fs::remove_file(artifact).unwrap();
    }
    assert_eq!(configs[0].1.source, config.source);
    assert_eq!(configs[0].1.version, config.version);
    let mut named = config;
    named.binding = Some("alias".into());
    named.source = None;
    named.version = None;
    std::fs::write(flat.path().join("mock.wasm"), b"fixture").unwrap();
    let projected = replay::artifacts::project(
        &[(fixture.path().to_path_buf(), named)],
        fixture.path(),
        Some(flat.path()),
    )
    .unwrap();
    assert!(
        projected[0].1.source.is_none(),
        "named instances inherit rather than acquire a kind source"
    );
}

#[cfg(unix)]
#[test]
fn replay_prunes_aliases_of_build_output_and_rejects_aliased_targets() {
    use carina_lsp::workspace::DiscoveryScope;
    let root = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(output.path().join("main.crn"), "provider mock {}\n").unwrap();
    let alias = root.path().join("build-output-alias");
    std::os::unix::fs::symlink(output.path(), &alias).unwrap();
    let scope =
        DiscoveryScope::replay(root.path(), vec![output.path().to_path_buf()], &[]).unwrap();
    assert!(
        scope.prunes(&alias),
        "build/output exclusion must follow directory identity"
    );
    assert!(
        DiscoveryScope::replay(
            root.path(),
            vec![output.path().to_path_buf()],
            &[alias.join("main.crn")]
        )
        .is_err()
    );
}

#[test]
fn replay_fingerprint_observes_the_same_projected_artifact_as_factory_loading() {
    use std::collections::BTreeMap;
    let fixture = control();
    let artifacts = tempfile::tempdir().unwrap();
    let artifact = artifacts.path().join("aws.wasm");
    std::fs::write(&artifact, b"first").unwrap();
    let config = carina_core::parser::ProviderConfig {
        name: "aws".into(),
        source: Some("example/aws".parse().unwrap()),
        version: None,
        revision: None,
        attributes: Default::default(),
        default_tags: Default::default(),
        unresolved_attributes: Default::default(),
        binding: None,
    };
    let manifest = replay::manifest::Manifest {
        format: "carina-lsp-replay-input-v1".into(),
        root: fixture.path().to_path_buf(),
        corpus: "infra".into(),
        documents: vec!["main.crn".into()],
        inputs: BTreeMap::new(),
        exclusions: vec![],
        environment: BTreeMap::new(),
        replay_home: fixture.path().join("home"),
        artifacts: Some(artifacts.path().to_path_buf()),
        revision: "fixture".into(),
        shard: None,
        inventory: None,
        dependency_roots: vec![],
    };
    let prober = replay::provider_prober(&manifest);
    let before = prober(fixture.path(), &config).unwrap();
    assert!(
        before.artifact.is_some(),
        "replay fingerprints must describe the selected artifact"
    );
    std::fs::write(artifact, b"replacement with different metadata").unwrap();
    assert_ne!(before, prober(fixture.path(), &config).unwrap());
}
