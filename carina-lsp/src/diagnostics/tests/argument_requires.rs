use super::*;

#[test]
fn issue4398_require_buffer_recovery() {
    for broken_sibling in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("arguments.crn"),
            "arguments { other: Int }\n",
        )
        .unwrap();
        if broken_sibling {
            std::fs::write(dir.path().join("broken.crn"), "let broken = [\n").unwrap();
        }
        let path = dir.path().join("guards.crn");
        for condition in ["false", "n == other"] {
            let source = format!("arguments {{ require {condition}, 'guard' n: Int }}\n");
            std::fs::write(&path, &source).unwrap();
            let findings =
                test_engine().analyze(&create_document_at(&source, path.clone()), Some(&path));
            assert!(findings.is_empty(), "{source}: {findings:?}");
        }
        let source = "arguments {\nrequire 1.5 + 1 == 2, 'guard'\nn: Int\n}\n";
        std::fs::write(&path, source).unwrap();
        let findings =
            test_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
        let arithmetic: Vec<_> = findings
            .iter()
            .filter(|d| d.message == "operator '+': left operand must be Int, got Float")
            .collect();
        assert_eq!(arithmetic.len(), 1, "{findings:?}");
        assert_eq!(
            arithmetic[0].range,
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(1, 12),
                tower_lsp::lsp_types::Position::new(1, 13)
            )
        );
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_editor_argument_keeps_its_authored_path() {
    use std::os::unix::ffi::OsStringExt;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("guards.crn"),
        "arguments { require false, 'template guard' }\n",
    )
    .unwrap();
    let path = directory
        .path()
        .join(std::ffi::OsString::from_vec(b"\xff.crn".to_vec()));
    if std::fs::write(&path, "arguments { n: Int }\n").is_ok() {
        let guard = directory.path().join("guards.crn");
        let diagnostics = test_engine().analyze(
            &create_document_at(
                "arguments { require false, 'template guard' }\n",
                guard.clone(),
            ),
            Some(&guard),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
    let document = create_document_at("arguments { n: Int }\n", path.clone());
    let pinned = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
    let merged = test_engine()
        .with_run_for_test(|run| {
            run.parse_and_resolve_merged_with_buffer(&document.for_diagnostics().unwrap(), &pinned)
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
