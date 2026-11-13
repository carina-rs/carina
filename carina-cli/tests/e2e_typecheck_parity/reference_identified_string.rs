use super::*;

#[path = "../../../carina-core/tests/support/reference_identified_string.rs"]
mod fixture;

fn reference_warning(message: &str) -> tower_lsp::lsp_types::Diagnostic {
    tower_lsp::lsp_types::Diagnostic {
        range: Range::new(Position::new(1, 2), Position::new(1, 7)),
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("carina".into()),
        message: message.into(),
        ..Default::default()
    }
}

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
fn issue_4384_differently_identified_string_and_reference_inputs_stay_accepted() {
    for (kind, source_type) in fixture::differently_identified_reference_input_sources() {
        let schemas = fixture::differently_identified_reference_input_schemas(source_type);
        let engine = engine_with_schemas(schemas.clone());
        let main = fixture::consumer("source.value");
        let directory = fixture::directory(&main, "");
        for source in [
            "let source = mock.InputSource {}\n",
            "let source = mock.InputSource { value = 'k' }\n",
        ] {
            std::fs::write(directory.path().join("sources.crn"), source).unwrap();
            assert_eq!(
                cli_diagnostics(factories_for(&schemas), &directory),
                Vec::<String>::new(),
                "{kind}: {source}{main}"
            );
            for file in ["main.crn", "sources.crn", "providers.crn", "backend.crn"] {
                assert_eq!(
                    lsp_diagnostics(&engine, &directory, file),
                    [],
                    "{kind}: {file}: {source}{main}"
                );
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
fn assert_upstream_reference_export(annotated: bool) {
    for (target, accepted) in [
        (
            fixture::identified_string(carina_core::schema::TypeIdentity::from_dotted(
                "mock.RecoveryProbe",
            )),
            false,
        ),
        (fixture::identified_string(fixture::key_identity()), true),
        (AttributeType::string(), true),
    ] {
        let schemas = fixture::schemas_with_target(target.clone(), target);
        let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(factories_for(&schemas)),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let engine = engine_with_schemas(schemas.clone());
        let main = fixture::consumer("up.key");
        let directory =
            fixture::directory(&main, "let up = upstream_state { source = './upstream' }\n");
        std::fs::create_dir(directory.path().join("upstream")).unwrap();
        for (file, text) in [
            ("main.crn", ""),
            ("providers.crn", "provider mock {}\n"),
            ("backend.crn", "backend local { path = 'state.json' }\n"),
            (
                "exports.crn",
                if annotated {
                    "exports { key: mock.keys.Key.Reference = r.key }\n"
                } else {
                    "exports { key = r.key }\n"
                },
            ),
        ] {
            std::fs::write(directory.path().join("upstream").join(file), text).unwrap();
        }
        for source in [fixture::UNKNOWN_SOURCE, fixture::KNOWN_SOURCE] {
            std::fs::write(directory.path().join("upstream/sources.crn"), source).unwrap();
            if !annotated {
                let base = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
                let exports = carina_core::validation::deferred_types::with_session_for_test(
                    loaded.provider_context(),
                    |session| {
                        carina_core::upstream_exports::discover_upstream_exports(
                            &base,
                            &[carina_core::parser::UpstreamState {
                                binding: carina_core::resource::RelativeBindingName::new("up"),
                                source: "./upstream".into(),
                            }],
                            session,
                            Some(&schemas),
                        )
                    },
                );
                let carina_core::upstream_exports::UpstreamExportType::Static(export_type) =
                    &exports[&carina_core::resource::RelativeBindingName::new("up")]["key"]
                        .static_type
                else {
                    panic!("the upstream export must have an inferred type");
                };
                let export_type = export_type.known().unwrap();
                let carina_core::parser::TypeExpr::SchemaType { owned } = export_type else {
                    panic!("the reference export must retain its schema projection");
                };
                use carina_core::parser::CustomTypeResolver;
                let projection = loaded
                    .provider_context()
                    .custom_types
                    .projection_declaration(owned)
                    .unwrap();
                assert!(matches!(
                    projection.shape_ref_free(),
                    Ok(carina_core::schema::Shape::Reference { .. })
                ));
                // origin/main also lifts the Reference read projection to an identified String here.
                let export_type = carina_core::validation::lift_type_expr(export_type).unwrap();
                assert!(matches!(
                    export_type.shape_ref_free(),
                    Ok(carina_core::schema::Shape::String { .. })
                ));
            }
            let cli = cli_diagnostics(factories_for(&schemas), &directory);
            let lsp = lsp_diagnostics(&engine, &directory, "main.crn");
            // Unannotated upstream exports are not source-typed at validate yet: carina#4377.
            if accepted || !annotated {
                assert_eq!(cli, Vec::<String>::new());
                assert_eq!(lsp, []);
            } else {
                assert_eq!(
                    cli,
                    [
                        "mock.test.recovery_probe.<pending>: cannot assign mock.keys.Key.Reference to 'value': expected mock.RecoveryProbe, got mock.keys.Key.Reference (from up.key)"
                    ]
                );
                assert_eq!(
                    lsp,
                    [reference_warning(
                        "Type mismatch: expected mock.RecoveryProbe, got mock.keys.Key.Reference (from up.key)"
                    )]
                );
            }
            for file in [
                "sources.crn",
                "providers.crn",
                "backend.crn",
                "upstream/main.crn",
                "upstream/sources.crn",
                "upstream/exports.crn",
                "upstream/providers.crn",
                "upstream/backend.crn",
            ] {
                assert_eq!(
                    lsp_diagnostics(&engine, &directory, file),
                    [],
                    "{file}: {source}"
                );
            }
        }
    }
}

#[test]
fn issue_4384_annotated_upstream_reference_export_checks_string_sink_identity() {
    assert_upstream_reference_export(true);
}

#[test]
fn issue_4384_inferred_upstream_reference_export_keeps_4377_behavior() {
    assert_upstream_reference_export(false);
}

#[test]
fn issue_4384_plain_string_union_keeps_main_known_unknown_findings() {
    // The unknown-only duplicate finding predates #4384 and is tracked as carina#4397.
    for (shape, ty, expression) in fixture::shapes() {
        if !matches!(
            shape,
            "union_list" | "union_map" | "union_struct" | "recursive_union"
        ) {
            continue;
        }
        let mut schemas = fixture::schemas(ty);
        schemas.insert(
            "mock",
            ResourceSchema::new("Source")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .validate()
                .unwrap(),
        );
        let engine = engine_with_schemas(schemas.clone());
        let main = fixture::consumer(&expression.replace("r.key", "a.name"));
        let directory = fixture::directory(&main, "");
        for (known, source) in [
            (false, "let a = mock.Source {}\n"),
            (true, "let a = mock.Source { name = 'abc' }\n"),
        ] {
            std::fs::write(directory.path().join("sources.crn"), source).unwrap();
            let cli = cli_diagnostics(factories_for(&schemas), &directory);
            let lsp = lsp_diagnostics(&engine, &directory, "main.crn");
            assert_eq!(cli.len(), if known { 1 } else { 2 }, "{shape}: {cli:#?}");
            assert_eq!(lsp.len(), 1, "{shape}: {lsp:#?}");
            let expected_cli: Vec<_> = fixture::cli_messages(shape, known)
                .iter()
                .map(|message| {
                    message
                        .replace("mock.keys.Key.Reference", "String")
                        .replace("r.key", "a.name")
                })
                .collect();
            assert_eq!(cli, expected_cli, "{shape}: {source}{main}");
            assert_eq!(
                lsp,
                [reference_warning(
                    &fixture::lsp_message(shape)
                        .replace("mock.keys.Key.Reference", "String")
                        .replace("r.key", "a.name")
                )],
                "{shape}: {source}{main}"
            );
        }
    }
}

#[derive(Clone, Copy)]
enum ReferenceFlow {
    Conditional,
    Coalesce,
    For,
}

fn assert_reference_result(flow: ReferenceFlow) {
    let schemas = fixture::schemas(fixture::identified_string(
        carina_core::schema::TypeIdentity::from_dotted("mock.RecoveryProbe"),
    ));
    let engine = engine_with_schemas(schemas.clone());
    let directory = fixture::directory("", "");
    for (known, source) in [
        (false, fixture::UNKNOWN_SOURCE),
        (true, fixture::KNOWN_SOURCE),
    ] {
        std::fs::write(directory.path().join("sources.crn"), source).unwrap();
        std::fs::write(
            directory.path().join("main.crn"),
            fixture::consumer("r.key"),
        )
        .unwrap();
        let direct_cli = cli_diagnostics(factories_for(&schemas), &directory);
        let direct_lsp = lsp_diagnostics(&engine, &directory, "main.crn");
        assert_eq!(direct_cli.len(), 1);
        assert_eq!(direct_lsp.len(), 1);
        let (main, expected_cli, expected_lsp) = match flow {
            ReferenceFlow::Conditional => (
                fixture::consumer("if true { r.key } else { 'fallback' }"),
                direct_cli,
                direct_lsp,
            ),
            ReferenceFlow::Coalesce => (
                fixture::consumer("r.key ?? r.key"),
                vec![direct_cli[0].clone(); if known { 1 } else { 2 }],
                vec![reference_warning(
                    "Type mismatch: expected mock.RecoveryProbe, got mock.keys.Key.Reference",
                )],
            ),
            ReferenceFlow::For => {
                let message = "Type mismatch: expected mock.RecoveryProbe, got mock.keys.Key.Reference (from r.key)";
                (
                    format!("for key in [r.key] {{\n{}\n}}\n", fixture::consumer("key")),
                    vec![format!(
                        "{} at line 3\n{message}",
                        directory.path().join("main.crn").display()
                    )],
                    vec![tower_lsp::lsp_types::Diagnostic {
                        range: Range::new(Position::new(2, 10), Position::new(2, 13)),
                        severity: Some(DiagnosticSeverity::ERROR),
                        source: Some("carina".into()),
                        message: message.into(),
                        ..Default::default()
                    }],
                )
            }
        };
        std::fs::write(directory.path().join("main.crn"), &main).unwrap();
        assert_eq!(
            cli_diagnostics(factories_for(&schemas), &directory),
            expected_cli,
            "{source}{main}"
        );
        assert_eq!(
            lsp_diagnostics(&engine, &directory, "main.crn"),
            expected_lsp,
            "{source}{main}"
        );
        for file in ["sources.crn", "providers.crn", "backend.crn"] {
            assert_eq!(
                lsp_diagnostics(&engine, &directory, file),
                [],
                "{file}: {source}{main}"
            );
        }
    }
}

#[test]
fn issue_4384_conditional_reference_result_matches_direct_verdict() {
    assert_reference_result(ReferenceFlow::Conditional);
}

#[test]
fn issue_4384_coalesced_reference_result_matches_direct_verdict() {
    assert_reference_result(ReferenceFlow::Coalesce);
}

#[test]
fn issue_4384_for_reference_binding_matches_direct_verdict() {
    assert_reference_result(ReferenceFlow::For);
}

fn assert_reference_builtin(target: AttributeType, expression: &str, builtin: &str) {
    let schemas = fixture::schemas(target);
    let engine = engine_with_schemas(schemas.clone());
    let main = fixture::consumer(expression);
    let directory = fixture::directory(&main, "");
    for source in [fixture::UNKNOWN_SOURCE, fixture::KNOWN_SOURCE] {
        std::fs::write(directory.path().join("sources.crn"), source).unwrap();
        assert_eq!(
            cli_diagnostics(factories_for(&schemas), &directory),
            Vec::<String>::new(),
            "{source}{main}"
        );
        assert_eq!(
            lsp_diagnostics(&engine, &directory, "main.crn"),
            [tower_lsp::lsp_types::Diagnostic {
                range: Range::new(
                    Position::new(1, 10),
                    Position::new(1, 10 + builtin.len() as u32)
                ),
                severity: Some(DiagnosticSeverity::INFORMATION),
                source: Some("carina".into()),
                message: format!(
                    "Consider using pipe form for '{builtin}': data |> {builtin}(...)"
                ),
                ..Default::default()
            }],
            "{source}{main}"
        );
        for file in ["sources.crn", "providers.crn", "backend.crn"] {
            assert_eq!(
                lsp_diagnostics(&engine, &directory, file),
                [],
                "{file}: {source}{main}"
            );
        }
    }
}

#[test]
fn issue_4384_join_accepts_reference_inputs() {
    assert_reference_builtin(
        fixture::identified_string(carina_core::schema::TypeIdentity::from_dotted(
            "mock.RecoveryProbe",
        )),
        "join('', [r.key])",
        "join",
    );
}

#[test]
fn issue_4384_concat_accepts_reference_inputs() {
    assert_reference_builtin(
        AttributeType::list(AttributeType::string()),
        "concat([r.key], [])",
        "concat",
    );
}

#[test]
fn issue_4384_literal_alias_keeps_value_admission() {
    let schemas = fixture::schemas(fixture::identified_string(
        carina_core::schema::TypeIdentity::from_dotted("mock.RecoveryProbe"),
    ));
    let engine = engine_with_schemas(schemas.clone());
    let expression = "alias";
    let directory = fixture::directory(
        &fixture::consumer(expression),
        "let literal = 'ab'\nlet alias = literal\n",
    );
    assert_eq!(
        cli_diagnostics(factories_for(&schemas), &directory),
        Vec::<String>::new()
    );
    assert_eq!(lsp_diagnostics(&engine, &directory, "main.crn"), []);
    for file in ["sources.crn", "providers.crn", "backend.crn"] {
        assert_eq!(lsp_diagnostics(&engine, &directory, file), []);
    }
}

#[test]
fn issue_4384_module_output_keeps_non_schema_value_admission() {
    let schemas = fixture::schemas(fixture::identified_string(
        carina_core::schema::TypeIdentity::from_dotted("mock.RecoveryProbe"),
    ));
    let engine = engine_with_schemas(schemas.clone());
    let directory = fixture::directory(
        "",
        "let child = use { source = './child' }\nlet module = child {}\n",
    );
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        "attributes { output: String = 'ab' }\n",
    )
    .unwrap();
    let expression = "module.output";
    let main = fixture::consumer(expression);
    std::fs::write(directory.path().join("main.crn"), &main).unwrap();
    let cli = cli_diagnostics(factories_for(&schemas), &directory);
    let lsp = lsp_diagnostics(&engine, &directory, "main.crn");
    // Non-schema sources keep their existing value admission after substitution.
    let expected = (
            vec!["mock.test.recovery_probe.<pending>: cannot assign String to 'value': expected mock.RecoveryProbe, got String (from module.output)".to_owned()],
            vec![reference_warning("Type mismatch: expected mock.RecoveryProbe, got String (from module.output)")],
    );
    assert_eq!((cli, lsp), expected);
    for file in ["sources.crn", "providers.crn", "backend.crn"] {
        assert_eq!(lsp_diagnostics(&engine, &directory, file), []);
    }
}
