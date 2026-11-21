//! Directory and unsaved-buffer controls for the core value-key diagnostics.
use std::sync::Arc;

use carina_core::provider::{ProviderFactory, loaded_provider_snapshot_from_factories};
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

fn snapshot() -> carina_core::provider::LoadedProviderSnapshot {
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            carina_provider_mock::MockBehavior::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        Default::default(),
        carina_core::EnvironmentSnapshot::empty(),
    )
}

fn checked_configuration(
    directory: &std::path::Path,
    snapshot: &carina_core::provider::LoadedProviderSnapshot,
) -> Result<carina_core::parser::ModuleResolvedInferredFile, String> {
    let base = carina_core::paths::ResolvedPath::new(directory).unwrap();
    let context = snapshot.provider_context().as_provider_context();
    let loaded = carina_core::config_loader::load_configuration_with_config(
        &base,
        context,
        snapshot.schema_registry(),
    )?;
    carina_core::validation::deferred_types::TypeCheckSession::run(context, |session| {
        carina_core::module_resolver::resolve_modules_with_config(
            loaded.parsed,
            &base,
            session,
            snapshot.schema_registry(),
        )
    })
    .publish(&mut Vec::new())
    .map_err(|error| error.display_relative_to(&base.source_label()))
}

#[test]
fn keyed_loop_group_directory_diagnostic_lists() {
    let snapshot = Arc::new(snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    for (file, source) in [
        ("providers.crn", "provider mock {}\n"),
        (
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        ),
        (
            "main.crn",
            "let items = { a = 'x' }\nlet children = for k, v in items { mock.test.resource { name = v } }\n",
        ),
    ] {
        std::fs::write(directory.path().join(file), source).unwrap();
    }
    let bare_group = "'children' is a for binding, not a value; reference an instance attribute, e.g. 'children['key'].<attribute>'";
    let coalesce = "coalesce default type `Map` does not match left type `keyed loop group`";
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for (position, other, exports, file, errors) in [
        (
            "iterable",
            "let copies = for c in children { mock.test.resource { name = c.name } }\n",
            "exports {}\n",
            "other.crn",
            vec![bare_group],
        ),
        (
            "sink",
            "let other = mock.test.resource { name = 'other' comment = children }\n",
            "exports { out: String = other.name }\n",
            "other.crn",
            vec![bare_group],
        ),
        (
            "coalesce sink",
            "let other = mock.test.resource { name = 'other' comment = children ?? {} }\n",
            "exports { out: String = other.name }\n",
            "other.crn",
            vec![coalesce, bare_group],
        ),
        (
            "export",
            "",
            "exports { out = children }\n",
            "exports.crn",
            vec![bare_group],
        ),
        (
            "coalesce export",
            "",
            "exports { out = children ?? {} }\n",
            "exports.crn",
            vec![bare_group],
        ),
    ] {
        std::fs::write(directory.path().join("other.crn"), other).unwrap();
        std::fs::write(directory.path().join("exports.crn"), exports).unwrap();
        let path = directory.path().join(file);
        let document = Document::new_with_path(
            std::fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::new(snapshot.provider_context().as_provider_context().clone()),
        );
        let diagnostics = engine.analyze(&document, Some(&path));
        actual.push((
            position,
            diagnostics
                .into_iter()
                .map(|diagnostic| (diagnostic.severity, diagnostic.message))
                .collect::<Vec<_>>(),
        ));
        expected.push((
            position,
            errors
                .into_iter()
                .map(|message| (Some(DiagnosticSeverity::ERROR), message.to_owned()))
                .collect::<Vec<_>>(),
        ));
    }
    assert_eq!(actual, expected, "complete LSP diagnostic lists");
}

#[test]
fn keyed_loop_compound_reader_directory_diagnostics() {
    let snapshot = Arc::new(snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    for (file, source) in [
        ("providers.crn", "provider mock {}\n"),
        (
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        ),
        (
            "main.crn",
            "let items = { a = 'x' }\nlet children = for k, v in items { mock.test.resource { name = v } }\n",
        ),
        ("exports.crn", "exports { out: String = other.name }\n"),
    ] {
        std::fs::write(directory.path().join(file), source).unwrap();
    }
    let path = directory.path().join("other.crn");
    for (reader, accepted) in [("children['a'].name", true), ("children.a.name", false)] {
        let source = format!(
            "let other = mock.test.resource {{ name = 'other' comment = {reader} ?? 'p' }}\n"
        );
        std::fs::write(&path, &source).unwrap();
        let document = Document::new_with_path(
            source,
            path.clone(),
            Arc::new(snapshot.provider_context().as_provider_context().clone()),
        );
        let diagnostics = engine.analyze(&document, Some(&path));
        if accepted {
            assert!(diagnostics.is_empty(), "{reader}: {diagnostics:#?}");
        } else {
            assert!(
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.severity == Some(DiagnosticSeverity::ERROR)
                        && diagnostic.message.contains("cannot apply access .a")
                }),
                "{reader}: {diagnostics:#?}"
            );
        }
        assert_eq!(
            checked_configuration(directory.path(), &snapshot).is_ok(),
            accepted,
            "{reader}"
        );
    }
}

#[test]
fn for_value_keys_upstream_authored_diagnostic_parity() {
    let snapshot = Arc::new(snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("up")).unwrap();
    for (file, source) in [
        ("providers.crn", "provider mock {}\n"),
        (
            "iterable.crn",
            "let up = upstream_state { source = './up' }\n",
        ),
        ("up/providers.crn", "provider mock {}\n"),
        ("up/exports.crn", "exports { items: list(Int) = [] }\n"),
    ] {
        std::fs::write(directory.path().join(file), source).unwrap();
    }
    let path = directory.path().join("resources.crn");
    let message =
        "Element type Int cannot be a value key. Rewrite as strings or use `for (i, x) in ...`.";
    for (iterable, iterable_end) in [("up.items", 26), ("(up.items ?? [1])", 34)] {
        let source = format!(
            "let rs = for x in {iterable} {{ mock.test.resource {{ name = 'constant' }} }}\n"
        );
        std::fs::write(&path, &source).unwrap();
        let document = Document::new_with_path(
            source,
            path.clone(),
            Arc::new(snapshot.provider_context().as_provider_context().clone()),
        );
        let findings = engine.analyze(&document, Some(&path));
        let matching = findings
            .iter()
            .filter(|d| d.message.contains("cannot be a value key"))
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "{findings:#?}");
        assert_eq!(matching[0].message, message);
        assert_eq!(
            matching[0].range,
            Range::new(Position::new(0, 18), Position::new(0, iterable_end))
        );
    }
}

#[test]
fn for_value_keys_diagnostic_parity() {
    let snapshot = Arc::new(snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["mock".into()]);
    for (iterable, message) in [
        (
            "['DUPLICATE_CANARY', 'DUPLICATE_CANARY']",
            "Elements 1 and 2 produce the same key. Use `for (i, x) in ...`.",
        ),
        (
            "secret(['PROTECTED_CANARY'])",
            "Protected values cannot be value keys. Use `for (i, x) in ...`.",
        ),
        (
            "[producer.tags]",
            "Element type Map cannot be a value key. Rewrite as strings or use `for (i, x) in ...`.",
        ),
        (
            "[producer.tags, 'TYPE_CANARY']",
            "Element type Map cannot be a value key. Rewrite as strings or use `for (i, x) in ...`.",
        ),
        (
            "['TYPE_CANARY', producer.tags]",
            "Element type Map cannot be a value key. Rewrite as strings or use `for (i, x) in ...`.",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(directory.path().join("producer.crn"), "let producer = mock.test.resource { name = 'producer' tags = { purpose = 'example' } }\n").unwrap();
        std::fs::write(directory.path().join("exports.crn"), "exports {}\n").unwrap();
        let path = directory.path().join("resources.crn");
        let source = format!(
            "let rs = for x in {iterable} {{ mock.test.resource {{ name = 'constant' }} }}\n"
        );
        // The same error must survive an editor override of a valid disk file.
        for unsaved in [false, true] {
            std::fs::write(
                &path,
                if unsaved {
                    "let rs = for x in ['valid'] { mock.test.resource { name = x } }\n"
                } else {
                    &source
                },
            )
            .unwrap();
            let document = Document::new_with_path(
                source.clone(),
                path.clone(),
                Arc::new(snapshot.provider_context().as_provider_context().clone()),
            );
            let diagnostics = engine.analyze(&document, Some(&path));
            let matching: Vec<_> = diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.message == message
                        && diagnostic.severity == Some(DiagnosticSeverity::ERROR)
                })
                .collect();
            assert!(
                !matching.is_empty(),
                "unsaved={unsaved}, {iterable}: {diagnostics:#?}"
            );
            let expected = Range::new(
                Position::new(0, 18),
                Position::new(0, 18 + iterable.len() as u32),
            );
            assert!(
                matching
                    .iter()
                    .all(|diagnostic| diagnostic.range == expected),
                "{diagnostics:#?}"
            );
            assert!(
                !format!("{diagnostics:?}").contains("CANARY"),
                "{diagnostics:#?}"
            );
        }
        std::fs::write(&path, &source).unwrap();
        let checked = checked_configuration(directory.path(), &snapshot);
        let messages = match checked {
            Err(error) => error,
            Ok(loaded) => {
                let tree = carina_core::parser::AnalysisTree::try_from(&loaded).unwrap();
                carina_core::validation::validate_for_iterable_types(
                    &tree,
                    snapshot.provider_context().as_provider_context(),
                )
                .iter()
                .map(|finding| finding.detail().to_string())
                .collect::<Vec<_>>()
                .join("\n")
            }
        };
        assert!(messages.contains(message), "{messages}");
    }
}

#[test]
fn for_value_keys_diagnostic_parity_sibling_values() {
    let snapshot = Arc::new(snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["mock".into()]);
    for (iterable, message) in [
        (
            "['SIBLING_CANARY', 'SIBLING_CANARY']",
            "Elements 1 and 2 produce the same key. Use `for (i, x) in ...`.",
        ),
        (
            "secret(['SIBLING_CANARY'])",
            "Protected values cannot be value keys. Use `for (i, x) in ...`.",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            directory.path().join("iterable.crn"),
            format!("let items = {iterable}\n"),
        )
        .unwrap();
        let path = directory.path().join("resources.crn");
        let source = "let rs = for x in items { mock.test.resource { name = x } }\n";
        std::fs::write(&path, source).unwrap();
        let document = Document::new_with_path(
            source.into(),
            path.clone(),
            Arc::new(snapshot.provider_context().as_provider_context().clone()),
        );
        let diagnostics = engine.analyze(&document, Some(&path));
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message == message)
            .unwrap_or_else(|| panic!("missing sibling-value finding: {diagnostics:#?}"));
        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(0, 18), Position::new(0, 23)),
            "the authored iterable owns the failure: {diagnostics:#?}",
        );
        assert!(!format!("{diagnostics:?}").contains("SIBLING_CANARY"));
        let error = checked_configuration(directory.path(), &snapshot)
            .expect_err("sibling values are admitted during validate");
        assert!(error.contains(message), "{error}");
        assert!(!error.contains("SIBLING_CANARY"), "{error}");
    }
}

#[test]
fn for_value_keys_diagnostic_parity_module_arguments() {
    let snapshot = Arc::new(snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    for (file, text) in [
        ("providers.crn", "provider mock {}\n"),
        (
            "main.crn",
            "let child = use { source = './child' }\nlet call = child { items = ['MODULE_CANARY', 'MODULE_CANARY'] }\n",
        ),
        ("child/arguments.crn", "arguments { items: list(String) }\n"),
        (
            "child/resources.crn",
            "let rs = for x in items { mock.test.resource { name = x } }\n",
        ),
    ] {
        std::fs::write(directory.path().join(file), text).unwrap();
    }
    let message = "Elements 1 and 2 produce the same key. Use `for (i, x) in ...`.";
    let path = directory.path().join("main.crn");
    let document = Document::new_with_path(
        std::fs::read_to_string(&path).unwrap(),
        path.clone(),
        Arc::new(snapshot.provider_context().as_provider_context().clone()),
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains(message))
        .unwrap_or_else(|| panic!("missing substituted-value diagnostic: {diagnostics:#?}"));
    assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(1, 11), Position::new(1, 16)),
        "the authored module call owns its imported failure"
    );
    assert!(
        !format!("{diagnostics:?}").contains("MODULE_CANARY"),
        "{diagnostics:#?}"
    );
    let error = checked_configuration(directory.path(), &snapshot)
        .expect_err("validate must admit substituted values");
    assert!(error.contains(message), "{error}");
    assert!(!error.contains("MODULE_CANARY"), "{error}");
}

#[test]
fn for_value_keys_diagnostic_parity_postfix_and_body() {
    let snapshot = Arc::new(snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(directory.path().join("iterable.crn"), "let items = ['a']\n").unwrap();
    std::fs::write(
        directory.path().join("resources.crn"),
        "let rs = for x in items { mock.test.resource { name = x } }\n",
    )
    .unwrap();
    let path = directory.path().join("references.crn");
    let source = "let sink = mock.test.resource { name = rs[0].name }\n";
    std::fs::write(&path, source).unwrap();
    let document = Document::new_with_path(
        source.into(),
        path.clone(),
        Arc::new(snapshot.provider_context().as_provider_context().clone()),
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    let message = "This loop is value-keyed; use a string subscript, or `for (i, x) in ...` for index addressing.";
    let error = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .expect("numeric access has the core-owned error");
    let start = source.find("[0]").unwrap() as u32;
    assert_eq!(
        error.range,
        Range::new(Position::new(0, start), Position::new(0, start + 3))
    );
    let checked = checked_configuration(directory.path(), &snapshot)
        .expect_err("numeric access must fail validation");
    assert!(checked.contains(message), "{checked}");

    std::fs::remove_file(&path).unwrap();
    std::fs::write(
        directory.path().join("producer.crn"),
        "let producer = mock.test.resource { name = 'producer' }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("iterable.crn"),
        "let items = [producer.identifier, 'BODY_CANARY']\n",
    )
    .unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        "arguments { item: String }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("calls.crn"),
        "let child = use { source = './child' }\n",
    )
    .unwrap();
    let path = directory.path().join("resources.crn");
    for iterable in ["items", "[producer.identifier, 'BODY_CANARY']"] {
        for body in ["read mock.test.resource { name = x }", "child { item = x }"] {
            let source = format!("let rs = for x in {iterable} {{ {body} }}\n");
            std::fs::write(&path, &source).unwrap();
            let document = Document::new_with_path(
                source.clone(),
                path.clone(),
                Arc::new(snapshot.provider_context().as_provider_context().clone()),
            );
            let diagnostics = engine.analyze(&document, Some(&path));
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains("cannot use")
                        && diagnostic.message.contains("`for (i, x) in ...`")
                        && diagnostic
                            .message
                            .contains("`for x in [producer.identifier, 'BODY_CANARY']`")),
                "{diagnostics:#?}"
            );
            let checked = checked_configuration(directory.path(), &snapshot)
                .expect_err("a newly deferred body cannot cross module expansion");
            for diagnostic in diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.message.contains("cannot use"))
            {
                assert!(
                    checked.contains(&diagnostic.message),
                    "{checked}\n{diagnostic:?}"
                );
                assert_eq!(diagnostic.range.start, Position::new(0, 9));
                assert_eq!(
                    diagnostic.range.end,
                    Position::new(0, source.trim_end().len() as u32)
                );
                assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
            }
        }
    }
}

#[test]
fn for_value_keys_diagnostic_parity_deferred_bodies() {
    let snapshot = Arc::new(snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    for (file, source) in [
        ("providers.crn", "provider mock {}\n"),
        (
            "producer.crn",
            "let producer = mock.test.resource { name = 'producer' }\n",
        ),
        (
            "iterable.crn",
            "let items = [producer.identifier, 'BODY_CANARY']\n",
        ),
        ("child/main.crn", "arguments { item: String }\n"),
        ("calls.crn", "let child = use { source = './child' }\n"),
    ] {
        std::fs::write(directory.path().join(file), source).unwrap();
    }
    let path = directory.path().join("resources.crn");
    for iterable in ["items", "[producer.identifier, 'BODY_CANARY']"] {
        for body in ["read mock.test.resource { name = x }", "child { item = x }"] {
            let source = format!("let rs = for x in {iterable} {{ {body} }}\n");
            std::fs::write(&path, &source).unwrap();
            let document = Document::new_with_path(
                source.clone(),
                path.clone(),
                Arc::new(snapshot.provider_context().as_provider_context().clone()),
            );
            let diagnostics = engine.analyze(&document, Some(&path));
            let matching: Vec<_> = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.message.contains("cannot use"))
                .collect();
            assert!(!matching.is_empty(), "{diagnostics:#?}");
            let checked = checked_configuration(directory.path(), &snapshot)
                .expect_err("a newly deferred body cannot cross module expansion");
            for diagnostic in matching {
                assert!(
                    checked.contains(&diagnostic.message),
                    "{checked}\nLSP finding: {diagnostic:#?}",
                );
                assert!(
                    diagnostic
                        .message
                        .contains("`for x in [producer.identifier, 'BODY_CANARY']`")
                );
                assert!(diagnostic.message.contains("`for (i, x) in ...`"));
                assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
                assert_eq!(
                    diagnostic.range,
                    Range::new(
                        Position::new(0, 9),
                        Position::new(0, source.trim_end().len() as u32),
                    ),
                );
            }
            assert!(checked.contains("BODY_CANARY"), "{checked}");
        }
    }
}

#[test]
fn for_value_keys_diagnostic_parity_declared_types() {
    let snapshot = Arc::new(snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["mock".into()]);
    for (element_type, values, label) in [
        ("Bool", "[]", "Bool"),
        ("String | Int", "['FUNCTION_VALUE_CANARY']", "Int"),
    ] {
        for deferred in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
            let declaration = if deferred {
                format!("arguments {{ items: list({element_type}) }}\n")
            } else {
                format!(
                    "fn typed_items(): list({element_type}) {{ {values} }}\nlet items = typed_items()\n"
                )
            };
            std::fs::write(directory.path().join("iterable.crn"), declaration).unwrap();
            let path = directory.path().join("resources.crn");
            let source = "let rs = for x in items { mock.test.resource { name = 'constant' } }\n";
            std::fs::write(&path, source).unwrap();
            let document = Document::new_with_path(
                source.into(),
                path.clone(),
                Arc::new(snapshot.provider_context().as_provider_context().clone()),
            );
            let diagnostics = engine.analyze(&document, Some(&path));
            let message = format!(
                "Element type {label} cannot be a value key. Rewrite as strings or use `for (i, x) in ...`."
            );
            let matching: Vec<_> = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.message == message)
                .collect();
            assert_eq!(matching.len(), 1, "deferred={deferred}: {diagnostics:#?}");
            assert_eq!(
                matching[0].range,
                Range::new(Position::new(0, 18), Position::new(0, 23))
            );
            assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
            assert!(!format!("{diagnostics:?}").contains("FUNCTION_VALUE_CANARY"));
        }
    }
}
