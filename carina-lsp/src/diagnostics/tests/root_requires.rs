use super::*;
use tower_lsp::lsp_types::{Position, Range};

fn assert_empty_template(blocks: &str) {
    let engine = test_engine();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("arguments.crn"), blocks).unwrap();
    let path = directory.path().join("guards.crn");
    let source = "require false, 'guard'\n";
    std::fs::write(&path, source).unwrap();
    let document = create_document_at(source, path.clone());
    for broken in [false, true] {
        if broken {
            std::fs::write(directory.path().join("broken.crn"), "let broken = [\n").unwrap();
        }
        let findings = engine.analyze(&document, Some(&path));
        assert!(findings.is_empty(), "{findings:?}");
    }
}

#[test]
fn empty_arguments_keep_lsp_template_scope() {
    assert_empty_template("arguments {}\n");
}
#[test]
fn empty_attributes_keep_lsp_template_scope() {
    assert_empty_template("attributes {}\n");
}
#[test]
fn both_empty_blocks_keep_lsp_template_scope() {
    assert_empty_template("arguments {}\nattributes {}\n");
}

#[cfg(unix)]
#[test]
fn non_utf8_editor_file_keeps_its_authored_path() {
    use std::os::unix::ffi::OsStringExt;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("guards.crn"),
        "require false, 'template guard'\n",
    )
    .unwrap();
    let path = directory
        .path()
        .join(std::ffi::OsString::from_vec(b"\xff.crn".to_vec()));
    if std::fs::write(&path, "arguments { n: Int }\n").is_ok() {
        let guard = directory.path().join("guards.crn");
        let diagnostics = test_engine().analyze(
            &create_document_at("require false, 'template guard'\n", guard.clone()),
            Some(&guard),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
    let document = create_document_at("arguments { n: Int }\n", path.clone());
    let pinned = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
    let name = path.file_name().unwrap().to_string_lossy();
    let merged = test_engine()
        .with_run_for_test(|run| {
            run.parse_and_resolve_merged_with_buffer(
                &document.for_diagnostics().unwrap(),
                Some(&name),
                &pinned,
            )
        })
        .unwrap();
    let syntax =
        carina_core::parser::AnalysisTree::try_from_file(merged.directory.parsed.parsed()).unwrap();
    let argument = syntax
        .syntax()
        .declarations()
        .iter()
        .find(|declaration| {
            declaration.detail().kind() == carina_core::config_loader::DeclarationKind::Argument
        })
        .unwrap();
    assert_eq!(argument.location().file().path(), path);
}

fn assert_buffer_require(condition: &str, broken_sibling: bool) {
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
            .all(|d| !d.message.contains("require failed:")
                && d.message != "`require` is only allowed inside a module"),
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
fn broken_sibling_fallback_leaves_require_scope_unknown() {
    assert_buffer_require("false", true);
}

#[test]
fn broken_sibling_fallback_reports_require_arithmetic_error() {
    assert_buffer_require("1.5 + 1 == 2", true);
}

#[test]
fn no_directory_buffer_leaves_require_scope_unknown() {
    assert_buffer_require("false", false);
}

#[test]
fn no_directory_buffer_reports_require_arithmetic_error() {
    assert_buffer_require("1.5 + 1 == 2", false);
}

fn assert_module_fallback(condition: &str, message: &str) {
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
fn fallback_with_sibling_arguments_does_not_evaluate_false_require() {
    assert_module_fallback("false", "template guard");
}

#[test]
fn fallback_with_sibling_arguments_does_not_reject_argument_name() {
    assert_module_fallback("n > 0", "positive");
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
    let message = "`require` is only allowed inside a module";
    for condition in [
        "true",
        "false",
        "n == 2",
        "r.identifier != null",
        "missing == 2",
        "1.5 + 1 == 2",
        "1 / 0 == 0",
        "true && 1",
        "custom('PRIVATE_CONDITION') == 1",
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
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            matching[0].range,
            Range::new(
                Position::new(1, 0),
                Position::new(1, 22 + condition.len() as u32)
            )
        );
    }
}

#[test]
fn root_require_unreadable_sibling_keeps_unknown_scope() {
    let engine = test_engine();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("guards.crn");
    let source = "require true, 'PRIVATE_MESSAGE'\n";
    std::fs::write(&path, source).unwrap();
    let broken = directory.path().join("broken.crn");
    std::fs::write(&broken, [0xff]).unwrap();
    let document = create_document_at(source, path.clone());
    assert!(engine.analyze(&document, Some(&path)).is_empty());
    std::fs::remove_file(broken).unwrap();
    let diagnostics = engine.analyze(&document, Some(&path));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "`require` is only allowed inside a module"
    );
    assert_eq!(
        diagnostics[0].range,
        Range::new(
            Position::new(0, 0),
            Position::new(0, source.trim_end().encode_utf16().count() as u32)
        )
    );
}
