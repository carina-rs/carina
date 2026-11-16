//! #4470 directory diagnostics share authored sites with CLI text/JSON tests.

mod support;

#[path = "../../carina-cli/tests/support/structural_secrecy.rs"]
mod fixtures;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use carina_core::{
    EnvironmentSnapshot, parser::ProviderContext, paths::CanonicalPath, schema::SchemaRegistry,
};
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use fixtures::{Finding, Site};
use support::fixture::{analyze, engine_with_schemas};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

fn engine() -> DiagnosticEngine {
    let mut schemas = SchemaRegistry::new();
    for schema in carina_provider_mock::builtin_schemas() {
        schemas.insert("mock", schema);
    }
    engine_with_schemas(schemas)
}

fn assert_no_payload(diagnostics: &[Diagnostic]) {
    let rendered = serde_json::to_string(diagnostics).unwrap();
    assert!(
        !rendered.contains(fixtures::CANARY),
        "LSP diagnostics disclosed a secret payload"
    );
}

fn lsp_range(root: &Path, site: Site) -> Range {
    let source = std::fs::read_to_string(root.join(site.file)).unwrap();
    range_in_lines(&source.lines().collect::<Vec<_>>(), site)
}

fn range_in_lines(lines: &[&str], site: Site) -> Range {
    let position = |line: u32, column: u32| {
        let text = lines[(line - 1) as usize];
        let utf16_column = text
            .chars()
            .take((column - 1) as usize)
            .map(|character| character.len_utf16() as u32)
            .sum();
        Position::new(line - 1, utf16_column)
    };
    let (end_line, end_column) = site.end.expect("an exact authored source span");
    Range::new(
        position(site.line, site.column),
        position(end_line, end_column),
    )
}

fn assert_findings(root: &Path, diagnostics: &[Diagnostic], expected: &[Finding]) {
    assert_no_payload(diagnostics);
    let findings = diagnostics
        .iter()
        .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), expected.len(), "{diagnostics:#?}");
    let sources = expected
        .iter()
        .map(|finding| finding.site.file)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|file| (file, std::fs::read_to_string(root.join(file)).unwrap()))
        .collect::<BTreeMap<_, _>>();
    let lines = sources
        .iter()
        .map(|(file, source)| (*file, source.lines().collect::<Vec<_>>()))
        .collect::<HashMap<_, _>>();
    let key = |message: &str, range: Range| {
        (
            message.to_string(),
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
        )
    };
    let mut by_site = HashMap::<_, Vec<_>>::new();
    for finding in findings {
        by_site
            .entry(key(&finding.message, finding.range))
            .or_default()
            .push(finding);
    }
    for finding in expected {
        let range = range_in_lines(&lines[finding.site.file], finding.site);
        let matching = by_site
            .get(&key(finding.message, range))
            .map(Vec::as_slice)
            .unwrap_or_default();
        assert_eq!(
            matching.len(),
            1,
            "missing or duplicated {finding:?}: {diagnostics:#?}"
        );
        assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(matching[0].source.as_deref(), Some("carina"));
    }
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
            .count(),
        expected.len(),
        "unexpected validation failure: {diagnostics:#?}"
    );
}

#[test]
fn mixed_coalesce_matches_cli_message_authored_file_and_utf16_span() {
    let fixture = fixtures::fixture("mixed_coalesce");
    let engine = engine();
    let diagnostics = analyze(&engine, &fixture, "main.crn");
    assert_findings(fixture.path(), &diagnostics, fixtures::COALESCE_FINDINGS);
    for file in ["resources.crn", "providers.crn", "backend.crn"] {
        let diagnostics = analyze(&engine, &fixture, file);
        assert_findings(fixture.path(), &diagnostics, &[]);
    }
}

#[test]
fn mixed_alternatives_and_default_tag_diagnostics_are_both_reported() {
    let fixture = fixtures::mixed_and_default_tag_fixture();
    let engine = engine();
    let diagnostics = ["main.crn", "providers.crn"]
        .into_iter()
        .flat_map(|file| analyze(&engine, &fixture, file))
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message == fixtures::COALESCE_MESSAGE)
            .count(),
        1,
        "{diagnostics:#?}"
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("default_tags.Owner"))
            .count(),
        1,
        "{diagnostics:#?}"
    );
    assert_no_payload(&diagnostics);
}

#[test]
fn mixed_lookup_matches_cli_message_authored_file_and_call_span() {
    let fixture = fixtures::fixture("mixed_lookup");
    let diagnostics = analyze(&engine(), &fixture, "main.crn");
    assert_findings(fixture.path(), &diagnostics, fixtures::LOOKUP_FINDINGS);
}

#[test]
fn homogeneous_alternatives_and_if_remain_valid_in_directory_diagnostics() {
    let fixture = fixtures::fixture("homogeneous");
    let engine = engine();
    for file in ["main.crn", "resources.crn", "providers.crn", "backend.crn"] {
        let diagnostics = analyze(&engine, &fixture, file);
        assert_findings(fixture.path(), &diagnostics, &[]);
    }
}

fn assert_module_findings(fixture: &tempfile::TempDir, expected: Finding) {
    let root_path = fixture.path().join("main.crn");
    let root = Document::new_with_path(
        std::fs::read_to_string(&root_path).unwrap(),
        root_path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let report = engine().analyze_document_set(&root, Some(&root_path), &[]);
    assert_findings(fixture.path(), &report[&root_path], &[]);
    let module_path = fixture.path().join(expected.site.file);
    let diagnostics = &report[&module_path];
    assert_no_payload(diagnostics);
    assert_eq!(diagnostics.len(), 1, "{report:#?}");
    let finding = &diagnostics[0];
    assert!(finding.message.starts_with(expected.message));
    assert!(finding.message.contains("secret_instance"));
    assert_eq!(finding.range, lsp_range(fixture.path(), expected.site));
    let related = finding.related_information.as_ref().unwrap();
    assert_eq!(related.len(), 1);
    assert_eq!(
        CanonicalPath::new(related[0].location.uri.to_file_path().unwrap()).unwrap(),
        CanonicalPath::new(&root_path).unwrap()
    );
    assert_eq!(
        related[0].location.range,
        lsp_range(fixture.path(), fixtures::SECRET_CALL)
    );
}

#[test]
fn root_and_module_findings_match_cli_at_both_authored_sites() {
    let fixture = fixtures::root_and_module_fixture();
    let root_path = fixture.path().join("main.crn");
    let root = Document::new_with_path(
        std::fs::read_to_string(&root_path).unwrap(),
        root_path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let report = engine().analyze_document_set(&root, Some(&root_path), &[]);
    assert_eq!(
        report
            .values()
            .flat_map(|diagnostics| diagnostics.iter())
            .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
            .count(),
        2,
        "{report:#?}"
    );
    for expected in fixtures::ROOT_AND_MODULE_FINDINGS {
        let path = fixture.path().join(expected.site.file);
        let findings = report[&path]
            .iter()
            .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
            .collect::<Vec<_>>();
        assert_eq!(findings.len(), 1, "{report:#?}");
        assert!(findings[0].message.starts_with(expected.message));
        assert_eq!(findings[0].range, lsp_range(fixture.path(), expected.site));
    }
}

#[test]
fn impossible_expansion_keeps_the_same_root_secrecy_subset_as_cli() {
    let fixture = fixtures::unexpandable_root_and_module_fixture();
    let root_path = fixture.path().join("main.crn");
    let root = Document::new_with_path(
        std::fs::read_to_string(&root_path).unwrap(),
        root_path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let report = engine().analyze_document_set(&root, Some(&root_path), &[]);
    let findings = report
        .values()
        .flat_map(|diagnostics| diagnostics.iter())
        .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), 1, "{report:#?}");
    assert_eq!(
        findings[0].range,
        lsp_range(fixture.path(), fixtures::ROOT_AND_MODULE_FINDINGS[1].site)
    );
    assert!(!report.contains_key(&fixture.path().join("module/main.crn")));
}

#[test]
fn ordinary_root_errors_still_collect_the_same_expanded_secrecy_findings() {
    let fixture = fixtures::root_and_module_with_backend_error_fixture();
    let engine = engine();
    let root_path = fixture.path().join("main.crn");
    let root = Document::new_with_path(
        std::fs::read_to_string(&root_path).unwrap(),
        root_path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let report = engine.analyze_document_set(&root, Some(&root_path), &[]);
    let findings = report
        .values()
        .flat_map(|diagnostics| diagnostics.iter())
        .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), 2, "{report:#?}");
    // Ordinary backend diagnostics belong to backend.crn. Analyzing main.crn
    // still has to collect both secrecy findings despite that sibling error.
    let backend_path = fixture.path().join("backend.crn");
    let backend = Document::new_with_path(
        std::fs::read_to_string(&backend_path).unwrap(),
        backend_path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let backend_report = engine.analyze_document_set(&backend, Some(&backend_path), &[]);
    let backend_errors = backend_report[&backend_path]
        .iter()
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .collect::<Vec<_>>();
    assert_eq!(backend_errors.len(), 1, "{backend_report:#?}");
    assert_eq!(
        backend_errors[0].message, "Unknown attribute 'typo' for backend type 'local'",
        "{backend_report:#?}"
    );
    assert_eq!(backend_errors[0].source.as_deref(), Some("carina"));
    let typo_column = backend.text().find("typo").unwrap() as u32;
    assert_eq!(
        backend_errors[0].range,
        Range::new(
            Position::new(0, typo_column),
            Position::new(0, typo_column + 4),
        )
    );
    assert!(
        report[&root_path]
            .iter()
            .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
            .all(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message)),
        "a sibling backend error must not be attached to main.crn: {report:#?}"
    );
    for expected in fixtures::ROOT_AND_MODULE_FINDINGS {
        assert!(
            report[&fixture.path().join(expected.site.file)]
                .iter()
                .any(
                    |diagnostic| diagnostic.message.starts_with(expected.message)
                        && diagnostic.range == lsp_range(fixture.path(), expected.site)
                ),
            "{report:#?}"
        );
    }
}

#[test]
fn non_attribute_declaration_messages_match_cli() {
    let (fixture, findings) = fixtures::declaration_kinds_fixture();
    let diagnostics = analyze(&engine(), &fixture, "main.crn");
    assert_findings(fixture.path(), &diagnostics, &findings);
}

#[tokio::test]
async fn publication_routes_module_findings_without_duplicating_the_root() {
    use tower_lsp::lsp_types::{PublishDiagnosticsParams, Url};
    let fixture = fixtures::root_and_module_fixture();
    let root_path = fixture.path().join("main.crn");
    let module_path = fixture.path().join("module/main.crn");
    let root_uri = Url::from_file_path(&root_path).unwrap();
    let module_uri = Url::from_file_path(&module_path).unwrap();
    let mut schemas = SchemaRegistry::new();
    for schema in carina_provider_mock::builtin_schemas() {
        schemas.insert("mock", schema);
    }
    let mut client = support::TestClient::with_schemas(fixture.path(), schemas).await;
    client
        .open_document(
            root_uri.as_str(),
            &std::fs::read_to_string(&root_path).unwrap(),
        )
        .await;
    let mut publications = HashMap::new();
    let mut published_uris = Vec::new();
    for _ in 0..2 {
        let notification = client
            ._read_notification(
                "textDocument/publishDiagnostics",
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        let params: PublishDiagnosticsParams =
            serde_json::from_value(notification["params"].clone()).unwrap();
        published_uris.push(params.uri.clone());
        publications.insert(params.uri, params.diagnostics);
    }
    assert_eq!(published_uris, [root_uri.clone(), module_uri.clone()]);
    for (uri, expected) in [
        (root_uri.clone(), fixtures::ROOT_AND_MODULE_FINDINGS[1]),
        (module_uri.clone(), fixtures::MODULE_FINDING),
    ] {
        let findings = publications[&uri]
            .iter()
            .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
            .collect::<Vec<_>>();
        assert_eq!(findings.len(), 1, "{publications:#?}");
        assert_eq!(findings[0].range, lsp_range(fixture.path(), expected.site));
    }
    // Opening the module must preserve the caller's instance diagnostic.
    client
        .open_document(
            module_uri.as_str(),
            &std::fs::read_to_string(&module_path).unwrap(),
        )
        .await;
    let notification = client.wait_for_diagnostics(module_uri.as_str()).await;
    let params: PublishDiagnosticsParams =
        serde_json::from_value(notification["params"].clone()).unwrap();
    assert_eq!(
        params
            .diagnostics
            .iter()
            .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
            .count(),
        1,
        "{params:#?}"
    );
    // Removing the last caller must clear the externally published finding.
    client.close_document(root_uri.as_str()).await;
    let notification = client.wait_for_diagnostics(module_uri.as_str()).await;
    let params: PublishDiagnosticsParams =
        serde_json::from_value(notification["params"].clone()).unwrap();
    assert!(
        params
            .diagnostics
            .iter()
            .all(|diagnostic| !fixtures::is_secrecy_message(&diagnostic.message)),
        "{params:#?}"
    );
    client.shutdown().await;
}

#[test]
fn module_instances_report_authored_operator_with_secret_call_context() {
    let fixture = fixtures::fixture("module_instances");
    assert_module_findings(&fixture, fixtures::MODULE_FINDING);
}

#[test]
fn module_finding_uses_the_analyzed_text_for_utf16_range() {
    let fixture = fixtures::fixture("module_instances");
    let root_path = fixture.path().join("main.crn");
    let module_path = fixture.path().join("module/main.crn");
    let disk = std::fs::read_to_string(&module_path)
        .unwrap()
        .replace("??", "/* 😀 */ ??");
    std::fs::write(&module_path, &disk).unwrap();
    let edited = disk.clone();
    let document = |path: &Path, text: String| {
        Document::new_with_path(
            text,
            path.to_path_buf(),
            Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
        )
    };
    let root = document(&root_path, std::fs::read_to_string(&root_path).unwrap());
    let module = document(&module_path, edited.clone());
    let report = engine().analyze_document_set(&root, Some(&root_path), &[&module]);
    assert_findings(fixture.path(), &report[&root_path], &[]);
    let findings = &report[&module_path];
    assert_eq!(findings.len(), 1, "{report:#?}");
    let prefix = &edited[..edited.find("??").unwrap()];
    let start = Position::new(
        prefix.bytes().filter(|byte| *byte == b'\n').count() as u32,
        prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32,
    );
    assert_eq!(
        findings[0].range,
        Range::new(start, Position::new(start.line, start.character + 2))
    );
    assert_eq!(std::fs::read_to_string(&module_path).unwrap(), disk);
    assert_no_payload(findings);
}

#[test]
fn dirty_sibling_findings_are_owned_only_by_the_siblings_buffer_analysis() {
    let fixture = tempfile::tempdir().unwrap();
    let root_path = fixture.path().join("providers.crn");
    let sibling_path = fixture.path().join("sibling.crn");
    std::fs::write(&root_path, "provider mock {}\n").unwrap();
    std::fs::write(
        &sibling_path,
        "mock.test.resource { name = 'sink' comment = 'public' ?? secret('canary') }\n",
    )
    .unwrap();
    let document = |path: &Path, text: String| {
        Document::new_with_path(
            text,
            path.to_path_buf(),
            Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
        )
    };
    let root = document(&root_path, std::fs::read_to_string(&root_path).unwrap());
    for (edited, expected_count) in [
        (
            "mock.test.resource { name = 'sink' comment = 'fixed' }\n",
            0,
        ),
        (
            "\n\nmock.test.resource { name = 'sink' comment = 'public' /* 😀 */ ?? secret('canary') }\n",
            1,
        ),
    ] {
        let sibling = document(&sibling_path, edited.into());
        let engine = engine();
        let report = engine.analyze_document_set(&root, Some(&root_path), &[&sibling]);
        assert_eq!(
            report.keys().collect::<Vec<_>>(),
            vec![&root_path],
            "a sibling must never be routed: {report:#?}"
        );
        let sibling_report = engine.analyze_document_set(&sibling, Some(&sibling_path), &[&root]);
        let findings = sibling_report[&sibling_path]
            .iter()
            .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
            .collect::<Vec<_>>();
        assert_eq!(findings.len(), expected_count, "{sibling_report:#?}");
        if expected_count == 1 {
            let prefix = &edited[..edited.find("??").unwrap()];
            let start = Position::new(
                2,
                prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32,
            );
            assert_eq!(
                findings[0].range,
                Range::new(start, Position::new(2, start.character + 2))
            );
        }
    }
}

#[test]
fn dirty_module_buffer_is_not_routed_until_the_analyzed_text_matches() {
    let fixture = fixtures::fixture("module_instances");
    let root_path = fixture.path().join("main.crn");
    let module_path = fixture.path().join("module/main.crn");
    let document = |path: &Path, text: String| {
        Document::new_with_path(
            text,
            path.to_path_buf(),
            Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
        )
    };
    let root = document(&root_path, std::fs::read_to_string(&root_path).unwrap());
    let edited = std::fs::read_to_string(&module_path)
        .unwrap()
        .replace("??", "/* 😀 */ ??");
    let module = document(&module_path, edited.clone());
    let engine = engine();
    let dirty = engine.analyze_document_set(&root, Some(&root_path), &[&module]);
    assert!(!dirty.contains_key(&module_path), "{dirty:#?}");
    std::fs::write(&module_path, &edited).unwrap();
    let saved = engine.analyze_document_set(&root, Some(&root_path), &[&module]);
    let findings = &saved[&module_path];
    assert_eq!(findings.len(), 1, "{saved:#?}");
    let prefix = &edited[..edited.find("??").unwrap()];
    let start = Position::new(
        prefix.bytes().filter(|byte| *byte == b'\n').count() as u32,
        prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32,
    );
    assert_eq!(
        findings[0].range,
        Range::new(start, Position::new(start.line, start.character + 2))
    );
    assert_no_payload(findings);
}

#[test]
fn dirty_sibling_call_context_is_owned_by_the_callers_buffer_analysis() {
    let fixture = fixtures::fixture("module_instances");
    let root_path = fixture.path().join("main.crn");
    let sibling_path = fixture.path().join("providers.crn");
    let module_path = fixture.path().join("module/main.crn");
    let document = |path: &Path, text: String| {
        Document::new_with_path(
            text,
            path.to_path_buf(),
            Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
        )
    };
    let edited = std::fs::read_to_string(&root_path)
        .unwrap()
        .replace("secret(", "(");
    let caller = document(&root_path, edited);
    let sibling = document(
        &sibling_path,
        std::fs::read_to_string(&sibling_path).unwrap(),
    );
    let engine = engine();
    let current = engine.analyze_document_set(&caller, Some(&root_path), &[&sibling]);
    assert!(
        !current.contains_key(&module_path),
        "the caller buffer made both module instances public: {current:#?}"
    );
    let other = engine.analyze_document_set(&sibling, Some(&sibling_path), &[&caller]);
    assert!(
        !other.contains_key(&module_path),
        "a sibling must not restore the on-disk caller's stale context: {other:#?}"
    );
}

#[test]
fn dirty_sibling_module_inputs_use_the_current_directory_snapshot_from_either_root() {
    for (saved, edited, expected) in [
        ("secret('canary')", "'public'", 0),
        ("'public'", "secret('canary')", 1),
    ] {
        let fixture = fixtures::fixture("module_instances");
        let root_path = fixture.path().join("main.crn");
        let sibling_path = fixture.path().join("providers.crn");
        let module_path = fixture.path().join("module/main.crn");
        let text = std::fs::read_to_string(&root_path)
            .unwrap()
            .replace(&format!("secret(\"{}\")", fixtures::CANARY), "input");
        std::fs::write(&root_path, &text).unwrap();
        std::fs::write(
            &sibling_path,
            format!("provider mock {{}}\nlet input = {saved}\n"),
        )
        .unwrap();
        let document = |path: &Path, text: String| {
            Document::new_with_path(
                text,
                path.to_path_buf(),
                Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
            )
        };
        let root = document(&root_path, text);
        let sibling = document(
            &sibling_path,
            format!("provider mock {{}}\nlet input = {edited}\n"),
        );
        let engine = engine();
        let root_report = engine.analyze_document_set(&root, Some(&root_path), &[&sibling]);
        assert_eq!(
            root_report.get(&module_path).map_or(0, Vec::len),
            expected,
            "the root snapshot must contain the edited sibling input: {root_report:#?}"
        );
        let current = engine.analyze_document_set(&sibling, Some(&sibling_path), &[&root]);
        assert_eq!(
            current.get(&module_path).map_or(0, Vec::len),
            expected,
            "the edited sibling supplies the current module input: {current:#?}"
        );
        assert_eq!(root_report.get(&module_path), current.get(&module_path));
    }
}

#[tokio::test]
async fn two_dirty_root_buffers_publish_one_module_finding_through_save_close_and_call_removal() {
    use tower_lsp::lsp_types::{PublishDiagnosticsParams, Url};

    // Exercise both possible save/close orders. Both root buffers differ from
    // disk when the second document opens, including the caller's source range.
    for close_caller in [false, true] {
        let fixture = fixtures::fixture("module_instances");
        let main_path = fixture.path().join("main.crn");
        let calls_path = fixture.path().join("modules.crn");
        let module_path = fixture.path().join("module/main.crn");
        let calls = std::fs::read_to_string(&main_path).unwrap();
        std::fs::write(&calls_path, &calls).unwrap();
        std::fs::write(&main_path, "// root\n").unwrap();
        let main_text = "// unsaved root\n";
        let calls_text = format!("\n\n{calls}");
        let main_uri = Url::from_file_path(&main_path).unwrap();
        let calls_uri = Url::from_file_path(&calls_path).unwrap();
        let module_uri = Url::from_file_path(&module_path).unwrap();
        let mut schemas = SchemaRegistry::new();
        for schema in carina_provider_mock::builtin_schemas() {
            schemas.insert("mock", schema);
        }
        let mut client = support::TestClient::with_schemas(fixture.path(), schemas).await;
        client.open_document(main_uri.as_str(), main_text).await;
        client.wait_for_diagnostics(module_uri.as_str()).await;
        client.open_document(calls_uri.as_str(), &calls_text).await;

        let assert_current = |notification: serde_json::Value| {
            let params: PublishDiagnosticsParams =
                serde_json::from_value(notification["params"].clone()).unwrap();
            let findings = params
                .diagnostics
                .iter()
                .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
                .collect::<Vec<_>>();
            assert_eq!(findings.len(), 1, "{params:#?}");
            let finding = findings[0];
            assert_eq!(
                finding.range,
                lsp_range(fixture.path(), fixtures::MODULE_FINDING.site)
            );
            assert!(
                finding
                    .message
                    .starts_with(fixtures::MODULE_FINDING.message)
            );
            assert!(finding.message.contains("secret_instance"));
            let related = finding.related_information.as_ref().unwrap();
            assert_eq!(related.len(), 1);
            assert_eq!(related[0].location.uri, calls_uri);
            let mut call_site = fixtures::SECRET_CALL;
            call_site.line += 2;
            call_site.end = call_site.end.map(|(line, column)| (line + 2, column));
            assert_eq!(
                related[0].location.range,
                range_in_lines(&calls_text.lines().collect::<Vec<_>>(), call_site)
            );
            assert_no_payload(&params.diagnostics);
        };
        assert_current(client.wait_for_diagnostics(module_uri.as_str()).await);
        let (save_path, save_uri, save_text) = if close_caller {
            (&calls_path, &calls_uri, calls_text.as_str())
        } else {
            (&main_path, &main_uri, main_text)
        };
        std::fs::write(save_path, save_text).unwrap();
        client.save_document(save_uri.as_str()).await;
        assert_current(client.wait_for_diagnostics(module_uri.as_str()).await);
        client.close_document(save_uri.as_str()).await;
        assert_current(client.wait_for_diagnostics(module_uri.as_str()).await);

        // Remove the call from the remaining dirty buffer. In the other close
        // order, reopen the saved caller first, then edit that buffer.
        if close_caller {
            client.open_document(calls_uri.as_str(), &calls_text).await;
            assert_current(client.wait_for_diagnostics(module_uri.as_str()).await);
        }
        client
            .change_document(calls_uri.as_str(), 2, "// call removed\n")
            .await;
        let notification = client.wait_for_diagnostics(module_uri.as_str()).await;
        let params: PublishDiagnosticsParams =
            serde_json::from_value(notification["params"].clone()).unwrap();
        assert!(
            params
                .diagnostics
                .iter()
                .all(|diagnostic| !fixtures::is_secrecy_message(&diagnostic.message)),
            "{params:#?}"
        );
        client.shutdown().await;
    }
}

#[test]
fn unrelated_open_buffers_do_not_override_directory_checks() {
    let fixture = fixtures::fixture("homogeneous");
    let unrelated = tempfile::tempdir().unwrap();
    let root_path = fixture.path().join("main.crn");
    let root = Document::new_with_path(
        std::fs::read_to_string(&root_path).unwrap(),
        root_path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let sibling = Document::new_with_path(
        "this is invalid syntax {".to_string(),
        unrelated.path().join("resources.crn"),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let engine = engine();
    let baseline = engine.analyze(&root, Some(&root_path));
    let with_open = engine.analyze_with_open_documents(&root, Some(&root_path), &[&sibling]);
    assert_eq!(with_open, baseline);
}

#[test]
fn module_dependent_if_keeps_cli_authored_operator_and_secret_call_context() {
    let fixture = fixtures::module_if_fixture();
    assert_module_findings(&fixture, fixtures::MODULE_IF_FINDING);
}

#[test]
fn public_module_instance_has_no_d2_diagnostic() {
    let fixture = fixtures::public_module_fixture();
    let diagnostics = analyze(&engine(), &fixture, "main.crn");
    assert_findings(fixture.path(), &diagnostics, &[]);
}

#[test]
fn uninstantiated_module_directory_leaves_d2_pending() {
    // Copy only the template directory: there are no root calls to substitute.
    // Keep schemas loaded, so a missing provider cannot mask a false D2 error.
    let fixture = fixtures::fixture("module_instances/module");
    let engine = engine();
    for file in ["main.crn", "arguments.crn"] {
        let diagnostics = analyze(&engine, &fixture, file);
        assert_findings(fixture.path(), &diagnostics, &[]);
    }
}

#[test]
fn mixed_alternatives_use_current_buffer_instead_of_disk() {
    let fixture = fixtures::fixture("homogeneous");
    let path = fixture.path().join("main.crn");
    let disk = std::fs::read_to_string(&path).unwrap();
    let edited = disk.replace(
        "comment = public.comment ?? \"default\"",
        "comment = public.comment ?? other.comment",
    );
    let document = Document::new_with_path(
        edited,
        path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let engine = engine();
    let diagnostics = engine.analyze(&document, Some(&path));
    assert_findings(
        fixture.path(),
        &diagnostics,
        &[Finding {
            site: Site::token("main.crn", 11, 28, 2),
            message: fixtures::COALESCE_MESSAGE,
        }],
    );
    // The same engine must still accept the unchanged on-disk directory.
    let diagnostics = analyze(&engine, &fixture, "main.crn");
    assert_findings(fixture.path(), &diagnostics, &[]);
}

fn assert_depth_findings(last: usize, shallow_first: bool, shape: fixtures::ChainShape) {
    let (fixture, findings) = fixtures::depth_fixture(last, shallow_first, shape);
    assert_chain_findings(&fixture, &findings);
}

fn assert_chain_findings(fixture: &tempfile::TempDir, findings: &[Finding]) {
    let engine = engine();
    for file in ["main.crn", "resources.crn"] {
        if !fixture.path().join(file).exists() {
            continue;
        }
        let expected = findings
            .iter()
            .filter(|finding| finding.site.file == file)
            .copied()
            .collect::<Vec<_>>();
        let diagnostics = analyze(&engine, fixture, file);
        assert_findings(fixture.path(), &diagnostics, &expected);
    }
}

#[test]
fn reference_depth_accepts_127_and_rejects_128_in_both_file_shapes_and_orders() {
    for shape in fixtures::CHAIN_SHAPES {
        for shallow_first in [true, false] {
            for last in [127, 128] {
                assert_depth_findings(last, shallow_first, shape);
            }
        }
    }
}

#[test]
fn expression_wrapped_chains_accept_127_and_reject_128_in_both_shapes_and_orders() {
    for shape in fixtures::CHAIN_SHAPES {
        for wrapper in fixtures::EXPRESSION_WRAPPERS {
            for shallow_first in [true, false] {
                for last in [127, 128] {
                    let (fixture, findings) =
                        fixtures::expression_depth_fixture(last, shallow_first, shape, wrapper);
                    assert_chain_findings(&fixture, &findings);
                }
            }
        }
    }
}

#[test]
fn module_output_chains_accept_63_and_reject_64_in_both_shapes() {
    for shape in fixtures::CHAIN_SHAPES {
        for instances in [63, 64] {
            let (fixture, findings) = fixtures::module_output_depth_fixture(instances, shape);
            let root_path = fixture.path().join("main.crn");
            let root = Document::new_with_path(
                std::fs::read_to_string(&root_path).unwrap(),
                root_path.clone(),
                Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
            );
            let report = engine().analyze_document_set(&root, Some(&root_path), &[]);
            assert!(report.contains_key(&root_path), "{report:#?}");
            for (path, diagnostics) in &report {
                let expected = findings
                    .iter()
                    .filter(|finding| fixture.path().join(finding.site.file) == *path)
                    .copied()
                    .collect::<Vec<_>>();
                assert_findings(fixture.path(), diagnostics, &expected);
            }
        }
    }
}

#[test]
fn one_reference_in_deep_expressions_is_valid_in_both_shapes() {
    // The large stack is only for the parser's syntax recursion tracked in #4481.
    // The secrecy owner uses heap worklists, which the 256/5000-reference chain
    // tests pin on the default stack.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            for shape in fixtures::CHAIN_SHAPES {
                for (wrapper, levels) in [
                    (fixtures::ExpressionWrapper::Builtin, 130),
                    (fixtures::ExpressionWrapper::List, 70),
                ] {
                    let fixture = fixtures::deep_expression_fixture(shape, wrapper, levels);
                    assert_chain_findings(&fixture, &[]);
                }
            }
        })
        .expect("failed to spawn deep-expression test thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn issue_4474_deep_chains_report_the_same_limit_without_crashing() {
    for shape in fixtures::CHAIN_SHAPES {
        for last in [256, 5000] {
            assert_depth_findings(last, false, shape);
        }
    }
}

#[test]
fn reference_visits_accept_4096_and_reject_4097_with_fresh_attribute_budgets() {
    for shape in fixtures::CHAIN_SHAPES {
        for visits in [4096, 4097] {
            let (fixture, finding) = fixtures::visits_fixture(visits, shape);
            let diagnostics = analyze(&engine(), &fixture, "main.crn");
            let expected = if visits == 4097 {
                vec![finding]
            } else {
                vec![]
            };
            assert_findings(fixture.path(), &diagnostics, &expected);
        }
    }
}

fn assert_nested_module_diagnostics(depth: usize) {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    std::fs::create_dir(root.join("modules")).unwrap();
    std::fs::write(root.join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        root.join("backend.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.crn"),
        "let child = use { source = './modules/m1' }\nlet instance = child {}\n",
    )
    .unwrap();
    for index in 1..=depth {
        let module = root.join(format!("modules/m{index}"));
        std::fs::create_dir(&module).unwrap();
        let body = if index == depth {
            "mock.test.resource { name = 'leaf' comment = x }\n".to_string()
        } else {
            format!(
                "let child = use {{ source = '../m{}' }}\nlet instance = child {{}}\n",
                index + 1
            )
        };
        std::fs::write(
            module.join("main.crn"),
            "arguments { x: String = 'a' }\n".to_string() + &body,
        )
        .unwrap();
    }
    let diagnostics = analyze(&engine(), &fixture, "main.crn");
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "{depth} nested modules: {errors:#?}");
}

#[test]
fn nested_module_chain_of_500_validates_without_stack_growth() {
    // The default 2 MiB test-thread stack makes 500 levels sufficient to prove no per-level recursion; larger chains are omitted because validate time is quadratic (#4482).
    assert_nested_module_diagnostics(500);
}
