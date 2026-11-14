use super::*;
use tower_lsp::lsp_types::{Position, Range};

#[test]
fn root_placement_recovery_reuses_complete_declaration_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let providers = directory.path().join("providers.crn");
    std::fs::write(&providers, "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("arguments.crn"),
        "arguments { value: MissingType }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("guards.crn"),
        "require false, 'PRIVATE_GUARD'\n",
    )
    .unwrap();
    for (file, source, message, end) in [
        (
            "arguments.crn",
            "arguments { value: MissingType }\n",
            "arguments blocks are only valid inside module definitions, not in root configurations.",
            9,
        ),
        (
            "guards.crn",
            "require false, 'PRIVATE_GUARD'\n",
            "`require` is only allowed inside a module",
            30,
        ),
    ] {
        let path = directory.path().join(file);
        let diagnostics =
            test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
        let placement: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message == message)
            .collect();
        assert_eq!(placement.len(), 1, "{diagnostics:#?}");
        assert_eq!(
            placement[0].range,
            Range::new(Position::new(0, 0), Position::new(0, end))
        );
        let related = placement[0]
            .related_information
            .as_ref()
            .expect("recovery retains the issued evidence");
        assert_eq!(related.len(), 1);
        assert_eq!(
            related[0].location.uri,
            tower_lsp::lsp_types::Url::from_file_path(&providers).unwrap()
        );
        assert_eq!(
            related[0].location.range,
            Range::new(Position::new(0, 0), Position::new(0, 13))
        );
    }
}

#[test]
fn upstream_root_require_keeps_backend_evidence_in_related_information() {
    let directory = tempfile::tempdir().unwrap();
    let caller = directory.path().join("caller");
    let upstream = directory.path().join("upstream");
    std::fs::create_dir(&caller).unwrap();
    std::fs::create_dir(&upstream).unwrap();
    let backend = upstream.join("backend.crn");
    std::fs::write(&backend, "backend local { path = 'state.json' }\n").unwrap();
    std::fs::write(
        upstream.join("guards.crn"),
        "require true, 'PRIVATE_GUARD'\n",
    )
    .unwrap();
    std::fs::write(upstream.join("exports.crn"), "exports { value: Int = 1 }\n").unwrap();
    let path = caller.join("main.crn");
    let source = "let previous = upstream_state { source = '../upstream' }\n";
    std::fs::write(&path, source).unwrap();
    let diagnostics = test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    assert_eq!(errors.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        errors[0].message,
        format!(
            "upstream_state `previous`: failed to parse source `../upstream`: {}:1:1: `require` is only allowed inside a module",
            caller.join("../upstream/guards.crn").display()
        )
    );
    assert!(!errors[0].message.contains("PRIVATE_GUARD"));
    let related = errors[0]
        .related_information
        .as_ref()
        .expect("upstream root evidence must remain typed");
    assert_eq!(related.len(), 1);
    assert_eq!(
        related[0].message,
        "This backend block makes the directory a root configuration."
    );
    assert_eq!(
        related[0].location.uri,
        tower_lsp::lsp_types::Url::from_file_path(&backend).unwrap()
    );
    assert_eq!(
        related[0].location.range,
        Range::new(Position::new(0, 0), Position::new(0, 7))
    );
}

#[test]
fn aliased_provider_supplies_root_evidence_for_module_declarations_and_require() {
    let directory = tempfile::tempdir().unwrap();
    let providers = directory.path().join("providers.crn");
    std::fs::write(&providers, "let alias = provider mock {}\n").unwrap();
    for (file, source) in [
        ("arguments.crn", "arguments {}\n"),
        ("guards.crn", "require false, 'PRIVATE_GUARD'\n"),
    ] {
        std::fs::write(directory.path().join(file), source).unwrap();
    }
    for (file, source, message) in [
        (
            "arguments.crn",
            "arguments {}\n",
            "arguments blocks are only valid inside module definitions, not in root configurations.",
        ),
        (
            "guards.crn",
            "require false, 'PRIVATE_GUARD'\n",
            "`require` is only allowed inside a module",
        ),
    ] {
        let path = directory.path().join(file);
        let diagnostics =
            test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect();
        assert_eq!(errors.len(), 1, "{diagnostics:#?}");
        assert_eq!(errors[0].message, message);
        let related = errors[0]
            .related_information
            .as_ref()
            .expect("aliased provider evidence");
        assert_eq!(related.len(), 1);
        assert_eq!(
            related[0].location.uri,
            tower_lsp::lsp_types::Url::from_file_path(&providers).unwrap()
        );
        assert_eq!(
            related[0].location.range,
            Range::new(Position::new(0, 12), Position::new(0, 25))
        );
    }
}

#[test]
fn require_only_directory_is_a_template_without_related_information() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("guards.crn");
    let source = "require false, 'PRIVATE_GUARD'\n";
    std::fs::write(&path, source).unwrap();
    std::fs::write(directory.path().join("main.crn"), "// sibling\n").unwrap();
    let diagnostics = test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

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

fn assert_root_declaration_diagnostics(block: &str, declaration: &str, same_file: bool) {
    let providers = loaded_providers_from_factories(vec![Box::new(
        carina_provider_mock::ConfiguredBuiltinMock::new(
            carina_provider_mock::MockBehavior::default(),
            carina_provider_mock::MockScope::memory(),
        ),
    )]);
    let engine = DiagnosticEngine::new(providers, vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "let r = mock.test.resource { name = 'root' }\n",
    )
    .unwrap();
    let require = "require false, 'PRIVATE_GUARD'\n";
    let declarations = format!("{declaration}\n");
    let source = if same_file {
        format!("{declarations}{require}")
    } else {
        std::fs::write(directory.path().join("declarations.crn"), &declarations).unwrap();
        require.to_string()
    };
    std::fs::write(directory.path().join("guards.crn"), source).unwrap();
    for file in [
        "providers.crn",
        "main.crn",
        "guards.crn",
        "declarations.crn",
    ] {
        let path = directory.path().join(file);
        if !path.exists() {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let diagnostics = engine.analyze(&create_document_at(&source, path.clone()), Some(&path));
        let actual: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
            .map(|diagnostic| (diagnostic.message.clone(), diagnostic.range))
            .collect();
        let mut expected = Vec::new();
        if file
            == if same_file {
                "guards.crn"
            } else {
                "declarations.crn"
            }
        {
            expected.push((
                format!("{block} blocks are only valid inside module definitions, not in root configurations."),
                Range::new(Position::new(0, 0), Position::new(0, block.len() as u32)),
            ));
        }
        if file == "guards.crn" {
            let line = u32::from(same_file);
            expected.push((
                "`require` is only allowed inside a module".to_string(),
                Range::new(
                    Position::new(line, 0),
                    Position::new(line, require.trim_end().len() as u32),
                ),
            ));
        }
        let mut actual = actual;
        actual.sort_by_key(|(_, range)| range.start);
        expected.sort_by_key(|(_, range)| range.start);
        assert_eq!(actual, expected, "{file}: {diagnostics:?}");
        for diagnostic in diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        {
            let related = diagnostic
                .related_information
                .as_ref()
                .expect("root placement needs evidence");
            assert_eq!(related.len(), 1);
            assert_eq!(
                related[0].message,
                "This provider block makes the directory a root configuration."
            );
            assert_eq!(
                related[0].location.uri,
                tower_lsp::lsp_types::Url::from_file_path(directory.path().join("providers.crn"))
                    .unwrap()
            );
            assert_eq!(
                related[0].location.range,
                Range::new(Position::new(0, 0), Position::new(0, 13))
            );
        }
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("PRIVATE_GUARD"))
        );
    }
}

#[test]
fn root_empty_arguments_beside_require_have_lsp_placement_errors() {
    assert_root_declaration_diagnostics("arguments", "arguments {}", true);
}

#[test]
fn root_empty_arguments_in_sibling_have_lsp_placement_errors() {
    assert_root_declaration_diagnostics("arguments", "arguments {}", false);
}

#[test]
fn root_empty_attributes_beside_require_have_lsp_placement_errors() {
    assert_root_declaration_diagnostics("attributes", "attributes {}", true);
}

#[test]
fn root_empty_attributes_in_sibling_have_lsp_placement_errors() {
    assert_root_declaration_diagnostics("attributes", "attributes {}", false);
}

#[test]
fn root_populated_attributes_beside_require_have_lsp_placement_errors() {
    assert_root_declaration_diagnostics("attributes", "attributes { value: Int = 1 }", true);
}

#[test]
fn root_populated_attributes_in_sibling_have_lsp_placement_errors() {
    assert_root_declaration_diagnostics("attributes", "attributes { value: Int = 1 }", false);
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
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

#[test]
fn broken_sibling_keeps_module_buffer_provider_and_backend_checks_provisional() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.crn");
    let source = "arguments { x: String }\nunknown.test.resource { name = x }\nbackend s3 { bucket = 'x' key = 'state' region = 'invalid' }\n";
    std::fs::write(&path, source).unwrap();
    std::fs::write(directory.path().join("broken.crn"), "let broken = {\n").unwrap();
    let diagnostics = test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
    assert!(
        diagnostics
            .iter()
            .all(|d| d.severity != Some(DiagnosticSeverity::ERROR)),
        "{diagnostics:#?}"
    );
}

#[test]
fn relative_document_path_keeps_root_evidence_in_its_directory() {
    let directory = tempfile::Builder::new()
        .prefix("root-evidence-")
        .tempdir_in(".")
        .unwrap();
    let providers = directory.path().join("providers.crn");
    std::fs::write(&providers, "provider mock {}\n").unwrap();
    let absolute = directory.path().join("main.crn");
    let path = absolute
        .strip_prefix(std::env::current_dir().unwrap())
        .unwrap()
        .to_path_buf();
    assert!(path.is_relative());
    let source = "require true, 'PRIVATE_GUARD'\n";
    std::fs::write(&path, source).unwrap();
    let diagnostics = test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
    let rejection = diagnostics
        .iter()
        .find(|d| d.message == "`require` is only allowed inside a module")
        .unwrap();
    assert_eq!(
        rejection
            .related_information
            .as_ref()
            .expect("relative document paths need located evidence")[0]
            .location
            .uri,
        tower_lsp::lsp_types::Url::from_file_path(
            carina_core::paths::ResolvedPath::new(providers)
                .unwrap()
                .persisted_path()
                .to_path_buf()
        )
        .unwrap()
    );
}

#[test]
fn require_only_template_changes_to_root_when_a_provider_is_added() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("guards.crn");
    let source = "require false, 'PRIVATE_GUARD'\n";
    std::fs::write(&path, source).unwrap();
    std::fs::write(directory.path().join("main.crn"), "// sibling\n").unwrap();
    let document = create_document_at(source, path.clone());
    let engine = test_engine();
    assert!(engine.analyze(&document, Some(&path)).is_empty());
    let provider = directory.path().join("providers.crn");
    std::fs::write(&provider, "provider mock {}\n").unwrap();
    let diagnostics = engine.analyze(&document, Some(&path));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "`require` is only allowed inside a module"
    );
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 0), Position::new(0, 30))
    );
    assert_eq!(
        diagnostics[0].related_information.as_ref().unwrap()[0]
            .location
            .uri,
        tower_lsp::lsp_types::Url::from_file_path(provider).unwrap()
    );
}

#[test]
fn blockless_directory_lsp_interprets_module_evidence_and_neutral_resources() {
    let loaded = loaded_providers_from_factories(vec![Box::new(
        carina_provider_mock::ConfiguredBuiltinMock::new(
            carina_provider_mock::MockBehavior::default(),
            carina_provider_mock::MockScope::memory(),
        ),
    )]);
    let engine = DiagnosticEngine::new(loaded, vec!["mock".into()]);
    for source in [
        "let value = 1\n",
        "require false, 'PRIVATE_GUARD'\n",
        "mock.test.resource { name = 'child' }\n",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.crn");
        std::fs::write(&path, source).unwrap();
        std::fs::write(directory.path().join("locals.crn"), "// sibling\n").unwrap();
        let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
        assert!(diagnostics.is_empty(), "{source}: {diagnostics:#?}");
    }
}

#[test]
fn imported_root_prohibition_keeps_evidence_and_excludes_placement_errors() {
    let directory = tempfile::tempdir().unwrap();
    let child = directory.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let evidence = child.join("providers.crn");
    std::fs::write(&evidence, "provider mock {}\n").unwrap();
    std::fs::write(child.join("arguments.crn"), "arguments {}\n").unwrap();
    std::fs::write(child.join("guards.crn"), "require false, 'PRIVATE_GUARD'\n").unwrap();
    let source = "let child = use { source = './child' }\nlet call = child {}\n";
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    let diagnostics = test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "provider blocks are not allowed inside modules. Define providers at the root configuration level."
    );
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(1, 11), Position::new(1, 16))
    );
    let related = diagnostics[0].related_information.as_ref().unwrap();
    assert_eq!(related.len(), 1);
    assert_eq!(
        related[0].message,
        "This provider block makes the directory a root configuration."
    );
    assert_eq!(
        related[0].location.uri,
        tower_lsp::lsp_types::Url::from_file_path(evidence).unwrap()
    );
    assert_eq!(
        related[0].location.range,
        Range::new(Position::new(0, 0), Position::new(0, 13))
    );
}

#[test]
fn no_evidence_upstream_directory_keeps_its_root_exports() {
    let directory = tempfile::tempdir().unwrap();
    let upstream = directory.path().join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(
        upstream.join("exports.crn"),
        "exports { value: String = 'published' }\n",
    )
    .unwrap();
    std::fs::write(upstream.join("locals.crn"), "// sibling\n").unwrap();
    let source = "let prior = upstream_state { source = './upstream' }\nexports { value: String = prior.value }\n";
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    let diagnostics = test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

#[test]
fn imported_provider_and_backend_each_keep_their_own_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("module");
    std::fs::create_dir(&module).unwrap();
    let provider = module.join("a-provider.crn");
    let backend = module.join("b-backend.crn");
    std::fs::write(&provider, "provider aws {}\n").unwrap();
    std::fs::write(&backend, "backend local { path = 'state.json' }\n").unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider aws {}\n").unwrap();
    let path = directory.path().join("main.crn");
    let source = "let rejected = use { source = './module' }\nrejected {}\n";
    std::fs::write(&path, source).unwrap();
    let diagnostics = test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    assert_eq!(errors.len(), 2, "{diagnostics:#?}");
    for (diagnostic, (message, file, kind, end)) in errors.iter().zip([
        ("provider blocks are not allowed inside modules. Define providers at the root configuration level.", provider, "provider", 12),
        ("backend blocks are not allowed inside modules. Define the backend at the root configuration level.", backend, "backend", 7),
    ]) {
        assert_eq!(diagnostic.message, message);
        assert_eq!(diagnostic.range, Range::new(Position::new(1, 0), Position::new(1, 8)));
        let related = diagnostic.related_information.as_ref().unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].message, format!("This {kind} block makes the directory a root configuration."));
        assert_eq!(related[0].location.uri, tower_lsp::lsp_types::Url::from_file_path(file).unwrap());
        assert_eq!(related[0].location.range, Range::new(Position::new(0, 0), Position::new(0, end)));
    }
}

#[test]
fn direct_template_root_only_blocks_are_rejected_in_the_authored_sibling() {
    for evidence in ["arguments {}\n", "require true, 'x'\n"] {
        for (file, source, message, start, end) in [
            (
                "exports.crn",
                "exports {}\n",
                "exports blocks are not allowed inside modules. Define exports at the root configuration level.",
                0,
                7,
            ),
            (
                "exports.crn",
                "exports { value: Int = 1 }\n",
                "exports blocks are not allowed inside modules. Define exports at the root configuration level.",
                0,
                7,
            ),
            (
                "state.crn",
                "removed { from = aws.s3.Bucket 'old' }\n",
                "state blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level.",
                0,
                7,
            ),
            (
                "upstream.crn",
                "let previous = upstream_state { source = './upstream' }\n",
                "upstream_state declarations are not allowed inside modules. Define upstream_state declarations at the root configuration level.",
                0,
                3,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("declarations.crn"), evidence).unwrap();
            std::fs::create_dir(directory.path().join("upstream")).unwrap();
            std::fs::write(
                directory.path().join("upstream/exports.crn"),
                "exports { value: Int = 1 }\n",
            )
            .unwrap();
            std::fs::write(directory.path().join("upstream/main.crn"), "// sibling\n").unwrap();
            let path = directory.path().join(file);
            std::fs::write(&path, source).unwrap();
            let diagnostics =
                test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
            assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
            assert_eq!(diagnostics[0].message, message);
            assert_eq!(
                diagnostics[0].range,
                Range::new(Position::new(0, start), Position::new(0, end))
            );
            assert!(diagnostics[0].related_information.is_none());
        }
    }
}
