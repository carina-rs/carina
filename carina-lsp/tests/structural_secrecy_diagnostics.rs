//! #4470 directory diagnostics share authored sites with CLI text/JSON tests.

mod support;

#[path = "../../carina-cli/tests/support/structural_secrecy.rs"]
mod fixtures;

use std::collections::HashMap;
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
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .map(|file| (file, std::fs::read_to_string(root.join(file)).unwrap()))
        .collect::<HashMap<_, _>>();
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
    let diagnostics = analyze(&engine(), fixture, "main.crn");
    assert_no_payload(&diagnostics);
    let findings = diagnostics
        .iter()
        .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), 1, "{diagnostics:#?}");
    let finding = findings[0];
    assert_eq!(finding.message, expected.message);
    assert_eq!(finding.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(finding.source.as_deref(), Some("carina"));
    // A root buffer's primary range identifies its instantiated call. Its
    // related location carries the exact module operator used by CLI JSON,
    // without pretending a module coordinate belongs to the root document.
    assert_eq!(
        finding.range,
        lsp_range(fixture.path(), fixtures::SECRET_CALL)
    );
    let authored = expected.site;
    let related = finding.related_information.as_deref().unwrap_or_default();
    let matching = related
        .iter()
        .filter(|information| {
            information
                .location
                .uri
                .to_file_path()
                .ok()
                .is_some_and(|file| {
                    CanonicalPath::new(file).unwrap()
                        == CanonicalPath::new(fixture.path().join(authored.file)).unwrap()
                })
                && information.location.range == lsp_range(fixture.path(), authored)
        })
        .count();
    assert_eq!(
        matching, 1,
        "missing authored module location: {finding:#?}"
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
            .count(),
        1,
        "the public instance must remain valid: {diagnostics:#?}"
    );
}

#[test]
fn module_instances_report_authored_operator_with_secret_call_context() {
    let fixture = fixtures::fixture("module_instances");
    assert_module_findings(&fixture, fixtures::MODULE_FINDING);
}

#[test]
fn unsaved_module_lines_supply_the_related_range() {
    let fixture = fixtures::fixture("module_instances");
    let root_path = fixture.path().join("main.crn");
    let module_path = fixture.path().join("module/main.crn");
    let disk = std::fs::read_to_string(&module_path).unwrap();
    // Shift the authored operator and put a non-BMP character before it.
    let edited = format!("// unsaved header\n\n{}", disk.replace("??", "/* 😀 */ ??"));
    let document = |path: &Path, text: String| {
        Document::new_with_path(
            text,
            path.to_path_buf(),
            Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
        )
    };
    let root = document(&root_path, std::fs::read_to_string(&root_path).unwrap());
    let module = document(&module_path, edited.clone());
    let diagnostics = engine().analyze_with_open_documents(&root, Some(&root_path), &[&module]);
    let findings = diagnostics
        .iter()
        .filter(|diagnostic| fixtures::is_secrecy_message(&diagnostic.message))
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        findings[0].range,
        lsp_range(fixture.path(), fixtures::SECRET_CALL)
    );
    let operator = edited.find("??").unwrap();
    let prefix = &edited[..operator];
    let start = Position::new(
        prefix.bytes().filter(|byte| *byte == b'\n').count() as u32,
        prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32,
    );
    let related = findings[0].related_information.as_ref().unwrap();
    assert_eq!(related.len(), 1);
    assert_eq!(
        related[0].location.range,
        Range::new(start, Position::new(start.line, start.character + 2))
    );
    assert_eq!(
        CanonicalPath::new(related[0].location.uri.to_file_path().unwrap()).unwrap(),
        CanonicalPath::new(&module_path).unwrap()
    );
    assert_eq!(std::fs::read_to_string(&module_path).unwrap(), disk);
    assert_no_payload(&diagnostics);
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
        let diagnostics = analyze(&engine, &fixture, file);
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
    assert_nested_module_diagnostics(500);
}

#[test]
fn nested_module_chain_of_2000_validates_without_stack_growth() {
    assert_nested_module_diagnostics(2000);
}
