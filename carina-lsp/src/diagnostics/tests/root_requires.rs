use super::*;
use tower_lsp::lsp_types::{Position, Range};

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
