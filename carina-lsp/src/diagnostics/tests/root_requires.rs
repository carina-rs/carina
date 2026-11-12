use super::*;
use tower_lsp::lsp_types::{Position, Range};

fn assert_round1_buffer_guard(condition: &str, broken_sibling: bool) {
    let engine = test_engine();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("guards.crn");
    let source = format!("// valid buffer\nrequire {condition}, 'guard'\n");
    std::fs::write(&path, &source).unwrap();
    if broken_sibling {
        std::fs::write(directory.path().join("broken.crn"), "let broken = [\n").unwrap();
    }
    let diagnostics = engine.analyze(
        &create_document_at(&source, path.clone()),
        broken_sibling.then_some(path.as_path()),
    );
    assert!(
        diagnostics
            .iter()
            .all(|d| !d.message.contains("require failed:")),
        "incomplete declarations cannot establish directory ownership: {diagnostics:?}"
    );
    if condition == "false" {
        return;
    }
    let matching: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message == "operator '+': left operand must be Int, got Float")
        .collect();
    assert_eq!(matching.len(), 1, "{diagnostics:?}");
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(1, 12), Position::new(1, 13))
    );
}

#[test]
fn round1_broken_sibling_fallback_leaves_require_scope_unknown() {
    assert_round1_buffer_guard("false", true);
}

#[test]
fn round1_broken_sibling_fallback_reports_require_arithmetic_error() {
    assert_round1_buffer_guard("1.5 + 1 == 2", true);
}

#[test]
fn round1_no_directory_buffer_leaves_require_scope_unknown() {
    assert_round1_buffer_guard("false", false);
}

#[test]
fn round1_no_directory_buffer_reports_require_arithmetic_error() {
    assert_round1_buffer_guard("1.5 + 1 == 2", false);
}

fn assert_round2_module_fallback(condition: &str, message: &str) {
    let engine = test_engine();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("guards.crn");
    let source = format!("require {condition}, '{message}'\n");
    std::fs::write(&path, &source).unwrap();
    std::fs::write(
        directory.path().join("arguments.crn"),
        "arguments { n: Int }\n",
    )
    .unwrap();
    let broken = directory.path().join("broken.crn");
    std::fs::write(&broken, "let broken = [\n").unwrap();
    let document = create_document_at(&source, path.clone());
    let fallback = engine.analyze(&document, Some(&path));
    assert!(fallback.is_empty(), "{fallback:?}");
    std::fs::remove_file(broken).unwrap();
    let complete = engine.analyze(&document, Some(&path));
    assert!(complete.is_empty(), "{complete:?}");
}

#[test]
fn round2_fallback_with_sibling_arguments_does_not_evaluate_false_require() {
    assert_round2_module_fallback("false", "template guard");
}

#[test]
fn round2_fallback_with_sibling_arguments_does_not_reject_argument_name() {
    assert_round2_module_fallback("n > 0", "positive");
}

#[test]
fn root_require_directory_diagnostics_follow_authored_sibling() {
    let providers = loaded_providers_from_factories(vec![Box::new(
        carina_provider_mock::ConfiguredBuiltinMock::new(
            carina_provider_mock::MockBehavior::default(),
            carina_provider_mock::MockScope::memory(),
        ),
    )]);
    let engine = DiagnosticEngine::new(providers, vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("checked")).unwrap();
    std::fs::write(
        directory.path().join("providers.crn"),
        "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("checked/arguments.crn"),
        "arguments { n: Int }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("checked/guards.crn"),
        "require n == 2, 'module guard'\n",
    )
    .unwrap();
    let main = "let n = 2\nlet r = mock.test.resource { name = 'root' }\nlet checked = use { source = './checked' }\nlet control = checked { n = 2 }\n";
    std::fs::write(directory.path().join("main.crn"), main).unwrap();
    for (condition, message) in [
        ("1 == 2", "require failed: root guard"),
        (
            "n == 2",
            "require failed: root guard; error evaluating constraint: unknown variable 'n' in constraint expression",
        ),
        (
            "r.identifier != null",
            "require failed: root guard; error evaluating constraint: unknown variable 'r' in constraint expression",
        ),
        (
            "missing == 2",
            "require failed: root guard; error evaluating constraint: unknown variable 'missing' in constraint expression",
        ),
    ] {
        let guard = format!("// sibling guard\nrequire {condition}, 'root guard'\n");
        std::fs::write(directory.path().join("guards.crn"), &guard).unwrap();
        let main_path = directory.path().join("main.crn");
        let diagnostics = engine.analyze(
            &create_document_at(main, main_path.clone()),
            Some(&main_path),
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.message != message),
            "{diagnostics:?}"
        );
        let path = directory.path().join("guards.crn");
        let diagnostics = engine.analyze(&create_document_at(&guard, path.clone()), Some(&path));
        let matching: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message == message)
            .collect();
        assert_eq!(matching.len(), 1, "{diagnostics:?}");
        assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            matching[0].range,
            Range::new(
                Position::new(1, 8),
                Position::new(1, 8 + condition.len() as u32)
            )
        );
    }
}
