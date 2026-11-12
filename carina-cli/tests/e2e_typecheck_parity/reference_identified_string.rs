use super::*;

#[path = "../../../carina-core/tests/support/reference_identified_string.rs"]
mod fixture;

fn assert_shape(wanted: &str) {
    let (shape, ty, value) = fixture::shapes()
        .into_iter()
        .find(|(shape, _, _)| *shape == wanted)
        .expect("registered nesting position");
    let schemas = fixture::schemas(ty);
    let engine = engine_with_schemas(schemas.clone());
    let main = fixture::consumer(value);
    let directory = fixture::directory(&main, fixture::UNKNOWN_SOURCE);
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for (known, source) in [
        (false, fixture::UNKNOWN_SOURCE),
        (true, fixture::KNOWN_SOURCE),
    ] {
        std::fs::write(directory.path().join("sources.crn"), source).unwrap();
        let expected_cli = fixture::cli_messages(shape, known);
        let cli = cli_diagnostics(factories_for(&schemas), &directory);
        let expected_lsp = vec![tower_lsp::lsp_types::Diagnostic {
            range: Range::new(Position::new(1, 2), Position::new(1, 7)),
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("carina".into()),
            message: fixture::lsp_message(shape).into(),
            ..Default::default()
        }];
        actual.push((cli, lsp_diagnostics(&engine, &directory, "main.crn")));
        expected.push((expected_cli, expected_lsp));
        for sibling in ["sources.crn", "providers.crn", "backend.crn"] {
            assert_eq!(
                lsp_diagnostics(&engine, &directory, sibling),
                [],
                "{shape}: {sibling}"
            );
        }
    }
    assert_eq!(actual, expected, "{shape}: unknown then known\n{main}");
}

macro_rules! shape_test {
    ($name:ident, $shape:literal) => {
        #[test]
        fn $name() {
            assert_shape($shape);
        }
    };
}
shape_test!(issue_4384_scalar, "scalar");
shape_test!(issue_4384_list_element, "list");
shape_test!(issue_4384_map_value, "map");
shape_test!(issue_4384_struct_field, "struct");
shape_test!(issue_4384_inside_secret, "secret");
shape_test!(issue_4384_secret_container, "secret_list");
shape_test!(issue_4384_behind_ref, "ref");
shape_test!(issue_4384_recursive_definition, "recursive");
shape_test!(issue_4384_recursive_child, "recursive_union");
shape_test!(issue_4384_list_in_union, "union_list");
shape_test!(issue_4384_map_in_union, "union_map");
shape_test!(issue_4384_struct_in_union, "union_struct");

#[test]
fn issue_4384_same_identity_identityless_and_reference_sinks_stay_accepted() {
    for (control, target, reference) in fixture::accepted_targets() {
        for (shape, ty, expression) in fixture::shapes_for(target.clone()) {
            let schemas = fixture::schemas_with_target(ty, target.clone());
            let engine = engine_with_schemas(schemas.clone());
            let main = fixture::consumer(&expression.replace("r.key", reference));
            let directory = fixture::directory(&main, fixture::UNKNOWN_SOURCE);
            for source in [fixture::UNKNOWN_SOURCE, fixture::KNOWN_SOURCE] {
                std::fs::write(directory.path().join("sources.crn"), source).unwrap();
                assert_eq!(
                    cli_diagnostics(factories_for(&schemas), &directory),
                    Vec::<String>::new(),
                    "{control}: {shape}: {source}{main}"
                );
                for file in ["main.crn", "sources.crn", "providers.crn", "backend.crn"] {
                    assert_eq!(
                        lsp_diagnostics(&engine, &directory, file),
                        [],
                        "{control}: {shape}: {file}: {source}{main}"
                    );
                }
            }
        }
    }
}

#[test]
fn issue_4384_reference_identity_admission_at_annotated_boundaries() {
    let schemas = fixture::schemas(fixture::identified_string(
        carina_core::schema::TypeIdentity::from_dotted("mock.RecoveryProbe"),
    ));
    let engine = engine_with_schemas(schemas.clone());
    for (boundary, main) in [
        (
            "attribute",
            "attributes { value: mock.RecoveryProbe = r.key }\n",
        ),
        ("export", "exports { value: mock.RecoveryProbe = r.key }\n"),
        (
            "parameter",
            "fn forward(value: mock.RecoveryProbe): String { 'ok' }\nmock.PlainStringSink { value = forward(r.key) }\n",
        ),
        (
            "return",
            "fn forward(): mock.RecoveryProbe { r.key }\nmock.PlainStringSink { value = forward() }\n",
        ),
        (
            "module",
            "let child = use { source = './child' }\nlet out = child { value = r.key }\nexports { value = out.output }\n",
        ),
    ] {
        let directory = fixture::directory(main, fixture::UNKNOWN_SOURCE);
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(directory.path().join("child/main.crn"), "arguments { value: mock.RecoveryProbe }\nattributes { output: mock.RecoveryProbe = value }\n").unwrap();
        let (source_file, document) = if boundary == "attribute" {
            std::fs::write(directory.path().join("main.crn"), "let child = use { source = './child' }\nlet out = child {}\nmock.PlainStringSink { value = out.value }\n").unwrap();
            std::fs::write(directory.path().join("sources.crn"), "").unwrap();
            std::fs::write(directory.path().join("child/main.crn"), main).unwrap();
            ("child/sources.crn", "child/main.crn")
        } else {
            ("sources.crn", "main.crn")
        };
        for accepted in [false, true] {
            if accepted {
                for file in ["main.crn", "child/main.crn"] {
                    let path = directory.path().join(file);
                    let text = std::fs::read_to_string(&path).unwrap();
                    std::fs::write(
                        path,
                        text.replace("mock.RecoveryProbe", "mock.keys.Key.Reference"),
                    )
                    .unwrap();
                }
            }
            for source in [fixture::UNKNOWN_SOURCE, fixture::KNOWN_SOURCE] {
                std::fs::write(directory.path().join(source_file), source).unwrap();
                let cli = cli_diagnostics(factories_for(&schemas), &directory);
                let lsp = lsp_diagnostics(&engine, &directory, document);
                if accepted {
                    assert_eq!(cli, Vec::<String>::new(), "{boundary}: {source}{main}");
                    assert_eq!(lsp, [], "{boundary}: {source}{main}");
                    continue;
                }
                let (message, line, start, end, severity) = match boundary {
                    "attribute" => (
                        "attribute 'value': type mismatch: expected mock.RecoveryProbe, got mock.keys.Key.Reference (from r.key)",
                        0,
                        13,
                        18,
                        DiagnosticSeverity::WARNING,
                    ),
                    "export" => (
                        "export 'value': type mismatch for 'r.key': expected mock.RecoveryProbe, got mock.keys.Key.Reference",
                        0,
                        10,
                        15,
                        DiagnosticSeverity::ERROR,
                    ),
                    "parameter" => (
                        "function 'forward': parameter 'value': expected mock.RecoveryProbe, got mock.keys.Key.Reference",
                        1,
                        39,
                        44,
                        DiagnosticSeverity::ERROR,
                    ),
                    "return" => (
                        "function 'forward': return value: expected mock.RecoveryProbe, got mock.keys.Key.Reference",
                        0,
                        35,
                        40,
                        DiagnosticSeverity::ERROR,
                    ),
                    "module" => (
                        "module call 'out': argument 'value': cannot assign mock.keys.Key.Reference to 'value': expected mock.RecoveryProbe, got mock.keys.Key.Reference (from r.key)",
                        1,
                        26,
                        31,
                        DiagnosticSeverity::ERROR,
                    ),
                    _ => unreachable!("registered boundary"),
                };
                let expected_cli = match boundary {
                    "attribute" => format!("child: {message}"),
                    "export" => format!(
                        "Module resolution error: Reference type validation failed: {message}"
                    ),
                    _ => format!(
                        "{} at line {}\n{message}",
                        directory.path().join("main.crn").display(),
                        line + 1
                    ),
                };
                assert_eq!(cli, [expected_cli], "{boundary}: {source}{main}");
                assert_eq!(
                    lsp,
                    [tower_lsp::lsp_types::Diagnostic {
                        range: Range::new(Position::new(line, start), Position::new(line, end)),
                        severity: Some(severity),
                        source: Some("carina".into()),
                        message: message.into(),
                        ..Default::default()
                    }],
                    "{boundary}: {source}{main}"
                );
            }
        }
    }
}
