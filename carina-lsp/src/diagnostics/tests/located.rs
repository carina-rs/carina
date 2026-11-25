//! Directory acceptance tests for parser-owned diagnostic locations (#4220).
use super::*;

#[path = "../../../../carina-cli/tests/common/loop_dot_cases.rs"]
mod loop_dot_cases;

#[path = "../../../../carina-cli/tests/common/reported_loop_selector_cases.rs"]
mod reported_selector_cases;

#[test]
fn loop_dot_reported_selector_preserves_sibling_diagnostics() {
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(
            carina_provider_mock::ConfiguredBuiltinMock::new(
                Default::default(),
                carina_provider_mock::MockScope::memory(),
            ),
        )]),
        vec!["mock".into()],
    );
    let mut failures = Vec::new();
    for case in reported_selector_cases::cases() {
        for module in [false, true] {
            let directory = Directory::new(&[]);
            let root = directory.root.path();
            let owner = reported_selector_cases::fixture(root, &case, module);
            let path = root.join(owner);
            let source = std::fs::read_to_string(&path).unwrap();
            let diagnostics =
                engine.analyze(&create_document_at(&source, path.clone()), Some(&path));
            let mut errors = diagnostics
                .iter()
                .filter(|finding| finding.severity == Some(DiagnosticSeverity::ERROR))
                .map(|finding| finding.message.clone())
                .collect::<Vec<_>>();
            let mut expected = case.errors[..1].to_vec();
            errors.sort();
            expected.sort();
            // The ordinary LSP typo pass owns this warning and its attribute-token range.
            let siblings = diagnostics
                .iter()
                .filter(|finding| finding.message.contains("'nope'"))
                .map(|finding| (finding.severity, finding.message.as_str(), finding.range))
                .collect::<Vec<_>>();
            let expected_siblings = if case.errors.len() > 1 {
                let column = source.find("r.nope").unwrap() as u32 + 2;
                vec![(
                    Some(DiagnosticSeverity::WARNING),
                    "Unknown attribute 'nope' on 'r' (type 'test.resource') Did you mean 'name'?",
                    Range::new(Position::new(0, column), Position::new(0, column + 4)),
                )]
            } else {
                Vec::new()
            };
            if errors != expected || siblings != expected_siblings {
                failures.push(format!(
                    "{}, module={module}: {errors:?}, expected exactly {expected:?}; all diagnostics: {diagnostics:?}",
                    case.id,
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn loop_dot_case_table_diagnostics_match_validate() {
    run_loop_dot_case_table(|_| true);
}

#[test]
fn loop_dot_case_table_module_internal_rejections_are_reported_once() {
    let directory = Directory::new(&[]);
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(
            carina_provider_mock::ConfiguredBuiltinMock::new(
                Default::default(),
                carina_provider_mock::MockScope::memory(),
            ),
        )]),
        vec!["mock".into()],
    );
    let root = directory.root.path();
    let mut failures = Vec::new();
    for row in loop_dot_cases::rows()
        .into_iter()
        .filter(|row| matches!(row.id, "2-3" | "2-11" | "2-13"))
    {
        loop_dot_cases::module_fixture(root, &row);
        let loop_dot_cases::Outcome::Error(expected) = row.expected else {
            unreachable!("rejected selector row");
        };
        for &position in &loop_dot_cases::POSITIONS[..7] {
            let owner = loop_dot_cases::module_reader(root, position, row.reader);
            let path = root.join(&owner);
            let source = std::fs::read_to_string(&path).unwrap();
            let direct = engine
                .analyze(&create_document_at(&source, path.clone()), Some(&path))
                .into_iter()
                .filter(|finding| finding.severity == Some(DiagnosticSeverity::ERROR))
                .map(|finding| finding.message)
                .collect::<Vec<_>>();
            if direct != expected {
                failures.push(format!("{}, {:?}, module-internal {position}, {}: {direct:?}, expected exactly {expected:?}", row.id, row.kind, row.reader));
            }
            let path = root.join("main.crn");
            let source = std::fs::read_to_string(&path).unwrap();
            let diagnostics = engine.analyze_document_set(
                &create_document_at(&source, path.clone()),
                Some(&path),
                &[],
            );
            let errors = diagnostics
                .iter()
                .flat_map(|(path, findings)| {
                    findings
                        .iter()
                        .filter(|finding| finding.severity == Some(DiagnosticSeverity::ERROR))
                        .map(move |finding| (path.clone(), finding.message.clone()))
                })
                .collect::<Vec<_>>();
            // Import resolution already reports one wrapped parse error at
            // the caller. Preserve that path as well as the direct module view.
            if errors.len() != 1 || !errors[0].1.ends_with(expected[0]) {
                failures.push(format!("{}, {:?}, imported module-internal {position}, {}: {errors:?}, expected exactly one {expected:?}", row.id, row.kind, row.reader));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn loop_dot_case_table_export_rejection_diagnostics() {
    let directory = Directory::new(&[]);
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(
            carina_provider_mock::ConfiguredBuiltinMock::new(
                Default::default(),
                carina_provider_mock::MockScope::memory(),
            ),
        )]),
        vec!["mock".into()],
    );
    let root = directory.root.path();
    let mut rows = loop_dot_cases::rows()
        .into_iter()
        .filter(|row| matches!(row.id, "2-3" | "2-10" | "2-11" | "2-12" | "2-13"))
        .collect::<Vec<_>>();
    rows.push(loop_dot_cases::Row {
        id: "subscript",
        kind: loop_dot_cases::Kind::Map,
        key: "0",
        reader: "children[0].name",
        positions: &["export plain"],
        expected: loop_dot_cases::Outcome::Error(&["cannot apply access [0] to keyed loop group"]),
    });
    for row in rows {
        loop_dot_cases::fixture(root, &row);
        let owner = loop_dot_cases::reader(root, "export plain", row.reader);
        let loop_dot_cases::Outcome::Error([error]) = row.expected else {
            unreachable!("one rejected selector");
        };
        let mut expected = vec![serde_json::json!({
            "range": { "start": { "line": 0, "character": 10 }, "end": { "line": 0, "character": 13 } },
            "severity": 2,
            "source": "carina",
            "message": format!("export 'out': type annotation required: {error}"),
        })];
        if row.id == "subscript" {
            // Main's invalid map subscript also has an independent scope
            // error. Keep that existing behavior while pinning its export record.
            expected.insert(0, serde_json::json!({
                "range": { "start": { "line": 0, "character": 16 }, "end": { "line": 0, "character": 32 } },
                "severity": 1,
                "source": "carina",
                "message": "Undefined resource: 'children[0]'. Define it with 'let children[0] = aws...'",
            }));
        }
        assert_eq!(
            observe_complete_diagnostics(&engine, root, owner),
            serde_json::Value::Array(expected),
            "{}, {:?}, {}",
            row.id,
            row.kind,
            row.reader,
        );
    }
}

#[test]
fn loop_dot_review_round3_function_diagnostics() {
    run_loop_dot_case_table(|row| matches!(row.id, "2-24" | "2-25"));
}

#[test]
fn loop_dot_review_round3_whole_group_function_location() {
    run_loop_dot_case_table(|row| row.id == "2-24");
}

#[test]
fn loop_dot_review_round1_function_contexts() {
    run_loop_dot_case_table(|row| row.id == "2-19");
}

#[test]
fn loop_dot_function_body_and_default_keep_main_call_site_diagnostics() {
    let directory = Directory::new(&[
        ("providers.crn", "provider mock {}\n"),
        (
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        ),
    ]);
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(
            carina_provider_mock::ConfiguredBuiltinMock::new(
                Default::default(),
                carina_provider_mock::MockScope::memory(),
            ),
        )]),
        vec!["mock".into()],
    );
    let main: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../carina-cli/tests/fixtures/loop_dot_function_main.json"
    ))
    .unwrap();
    let root = directory.root.path();
    for (label, case) in main["direct_calls"].as_object().unwrap() {
        loop_dot_cases::write(root, "main.crn", case["source"].as_str().unwrap());
        assert_eq!(
            observe_complete_diagnostics(&engine, root, "main.crn"),
            case["lsp"],
            "{label}"
        );
    }
}

fn observe_complete_diagnostics(
    engine: &DiagnosticEngine,
    root: &std::path::Path,
    owner: &str,
) -> serde_json::Value {
    let path = root.join(owner);
    let source = std::fs::read_to_string(&path).unwrap();
    let diagnostics = engine.analyze(&create_document_at(&source, path.clone()), Some(&path));
    let normalized = serde_json::to_string(&diagnostics)
        .unwrap()
        .replace(root.to_str().unwrap(), "<fixture>");
    serde_json::from_str(&normalized).unwrap()
}

#[test]
fn loop_dot_review_round2_deferred_function_contexts() {
    run_loop_dot_case_table(|row| {
        row.id == "2-22" || row.positions.contains(&"deferred function body")
    });
}

#[test]
fn loop_dot_review_round2_unrelated_export_failure() {
    run_loop_dot_case_table(|row| row.id == "2-23");
}

#[test]
fn loop_dot_review_round2_module_arguments() {
    run_loop_dot_case_table(|row| matches!(row.id, "2-2" | "2-17"));
}

#[test]
fn loop_dot_review_round2_bare_child_diagnostic_locations_match_subscript() {
    let directory = Directory::new(&[]);
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(
            carina_provider_mock::ConfiguredBuiltinMock::new(
                Default::default(),
                carina_provider_mock::MockScope::memory(),
            ),
        )]),
        vec!["mock".into()],
    );
    let root = directory.root.path();
    for row in loop_dot_cases::rows()
        .into_iter()
        .filter(|row| row.id == "2-4")
    {
        loop_dot_cases::fixture(root, &row);
        for &position in row.positions {
            let mut observations = Vec::new();
            for reader in ["children.a", "children['a']"] {
                let owner = loop_dot_cases::reader(root, position, reader);
                let path = root.join(owner);
                let source = std::fs::read_to_string(&path).unwrap();
                let reader_start = source.find(reader).unwrap();
                let reader_end = reader_start + reader.len();
                let offset = |point: tower_lsp::lsp_types::Position| {
                    let line_start: usize = source
                        .split_inclusive('\n')
                        .take(point.line as usize)
                        .map(str::len)
                        .sum();
                    let byte = line_start + point.character as usize;
                    // Remove only the subscript punctuation difference. Keep
                    // root, key, and whole-expression anchors distinct.
                    let suffix = reader_start + "children".len();
                    if reader == "children.a" || byte <= suffix {
                        byte
                    } else if byte >= reader_end {
                        byte - 3
                    } else {
                        suffix + if byte - suffix <= 2 { 1 } else { 2 }
                    }
                };
                observations.push(
                    engine
                        .analyze(&create_document_at(&source, path.clone()), Some(&path))
                        .into_iter()
                        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
                        .map(|diagnostic| {
                            (
                                diagnostic.message,
                                offset(diagnostic.range.start),
                                offset(diagnostic.range.end),
                            )
                        })
                        .collect::<Vec<_>>(),
                );
            }
            assert_eq!(
                observations[0], observations[1],
                "{:?}, {position}",
                row.kind
            );
        }
    }
}

#[test]
fn loop_dot_review_round1_unrelated_lsp_wording_matches_main() {
    run_loop_dot_case_table(|row| matches!(row.id, "2-20" | "2-21"));
}

#[test]
fn loop_dot_review_round1_export_preserves_distinct_boundary_failure() {
    use carina_core::binding_index::BindingIndex;
    use carina_core::parser::ExportParamLike;

    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(
            carina_provider_mock::ConfiguredBuiltinMock::new(
                Default::default(),
                carina_provider_mock::MockScope::memory(),
            ),
        )]),
        vec!["mock".into()],
    );
    let source = "provider mock {}\nlet src = mock.test.resource { name = 'source' }\nexports { out: Bool = src.comment ?? 1 }\n";
    let document = create_document(source);
    // Keep the authored expression available after its failure, as the LSP
    // does, then compare the CLI's shared export validation with the LSP pass.
    let parsed = carina_core::parser::parse_file(
        source,
        Path::new("test.crn"),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let configuration = carina_core::test_utils::load_canonical_buffer_configuration(
        &parsed,
        &engine.schemas,
        &engine.provider_context,
    );
    let inferred = configuration.inferred_export_params();
    let expression = inferred.parameters()[0].judgement().unwrap();
    assert_eq!(
        expression.check_owner(),
        carina_core::parser::retained::ExpressionCheckOwner::Reported,
    );
    let expression_error = expression.static_type().unwrap_err().to_string();
    let bindings = BindingIndex::from_parsed(&parsed, &engine.schemas, &engine.provider_context);
    let mut cli_errors = carina_core::validation::validate_export_param_ref_types_with_bindings(
        inferred.parameters(),
        &bindings,
    )
    .unwrap_err()
    .into_iter()
    .map(|error| error.to_string())
    .collect::<Vec<_>>();
    assert_eq!(cli_errors.len(), 1, "{cli_errors:?}");
    assert!(cli_errors[0].contains("Bool"), "{cli_errors:?}");
    assert_ne!(cli_errors[0], expression_error);
    let tree = carina_core::parser::AnalysisTree::try_from_file(&parsed).unwrap();
    let mut lsp_errors = engine.with_run_for_test(|run| {
        run.check_inferred_export_types(
            &document.for_diagnostics().unwrap(),
            configuration.judged(),
            &tree,
        )
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect::<Vec<_>>()
    });
    cli_errors.push(expression_error.clone());
    lsp_errors.push(expression_error);
    cli_errors.sort();
    lsp_errors.sort();
    assert_eq!(lsp_errors, cli_errors);
}

#[test]
fn loop_dot_module_call_keeps_independent_type_errors() {
    let directory = Directory::new(&[]);
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(
            carina_provider_mock::ConfiguredBuiltinMock::new(
                Default::default(),
                carina_provider_mock::MockScope::memory(),
            ),
        )]),
        vec!["mock".into()],
    );
    let row = loop_dot_cases::rows()
        .into_iter()
        .find(|row| row.id == "2-2" && row.kind == loop_dot_cases::Kind::Map)
        .unwrap();
    let root = directory.root.path();
    loop_dot_cases::fixture(root, &row);
    loop_dot_cases::write(
        root,
        "consumer/main.crn",
        "mock.test.resource { name = 1 comment = value }\n",
    );
    let mut observations = Vec::new();
    for reader in ["children.a_missing.name", "children['a_missing'].name"] {
        let owner = loop_dot_cases::reader(root, "module argument", reader);
        let path = root.join(owner);
        let source = std::fs::read_to_string(&path).unwrap();
        let document = create_document_at(&source, path.clone());
        let merged = engine
            .with_run_for_test(|run| {
                run.parse_and_resolve_merged_with_buffer(
                    &document.for_diagnostics().unwrap(),
                    &carina_core::paths::ResolvedPath::new(root).unwrap(),
                )
            })
            .unwrap();
        assert!(
            matches!(
                merged.module_resolution,
                checks::MergedModuleResolution::Expanded { .. }
            ),
            "the independent error must be checked after module expansion"
        );
        let errors = engine
            .analyze(&document, Some(&path))
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("Undefined identifier `children.a_missing`")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("expected String") && error.contains("got Int")),
            "{errors:?}"
        );
        observations.push(errors);
    }
    assert_eq!(observations[0], observations[1]);
}

fn run_loop_dot_case_table(include: impl Fn(&loop_dot_cases::Row) -> bool) {
    let directory = Directory::new(&[]);
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(
            carina_provider_mock::ConfiguredBuiltinMock::new(
                Default::default(),
                carina_provider_mock::MockScope::memory(),
            ),
        )]),
        vec!["mock".into()],
    );
    let root = directory.root.path();
    let observe = |owner: &str, include_inference_warning: bool| {
        let path = root.join(owner);
        let source = std::fs::read_to_string(&path).unwrap();
        engine
            .analyze(&create_document_at(&source, path.clone()), Some(&path))
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.severity == Some(DiagnosticSeverity::ERROR)
                    || (include_inference_warning
                        && diagnostic
                            .message
                            .starts_with("export 'out': type annotation required:"))
            })
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>()
    };
    let mut failures = Vec::new();
    for row in loop_dot_cases::rows().into_iter().filter(include) {
        loop_dot_cases::fixture(root, &row);
        for &position in row.positions {
            let owner = loop_dot_cases::reader(root, position, row.reader);
            if matches!(row.expected, loop_dot_cases::Outcome::MainFunction) {
                let actual = observe_complete_diagnostics(&engine, root, owner);
                assert_eq!(
                    actual,
                    loop_dot_cases::main_function_record(&row, position)["lsp"],
                    "{}, {:?}, {position}, {}",
                    row.id,
                    row.kind,
                    row.reader
                );
                continue;
            }
            let dot = observe(
                owner,
                row.id == "2-23" || loop_dot_cases::inferred_selector_export(&row, position),
            );
            let expected = match row.expected {
                loop_dot_cases::Outcome::MainFunction => unreachable!("main records checked above"),
                loop_dot_cases::Outcome::Validates => Vec::new(),
                loop_dot_cases::Outcome::Error(errors) => errors
                    .iter()
                    .map(|error| {
                        if loop_dot_cases::inferred_selector_export(&row, position) {
                            format!("export 'out': type annotation required: {error}")
                        } else {
                            (*error).to_owned()
                        }
                    })
                    .collect(),
                loop_dot_cases::Outcome::SurfaceErrors { cli, lsp } => {
                    assert_ne!(cli, lsp, "each surface preserves main's wording");
                    lsp.iter().map(|error| (*error).to_owned()).collect()
                }
                loop_dot_cases::Outcome::SameAsSubscript(subscript) => {
                    loop_dot_cases::reader(root, position, subscript);
                    let bracket = observe(owner, false);
                    // Bare-child exports retain independent annotation errors
                    // from the shared core boundary pass, alongside expression errors.
                    let expected: Vec<String> = if row.id == "2-4" {
                        match position {
                            "plain" | "if" => vec![
                                "expected String, got mock.test.resource",
                                "expected String, got mock.test.resource",
                            ],
                            "single interpolation" | "multipart interpolation" => vec![
                                "mock.test.resource.other: 'children.a' is a resource handle, not a value",
                            ],
                            "coalesce" => vec![
                                "coalesce default type `String` does not match left type `mock.test.resource`",
                            ],
                            "export" => vec![
                                "coalesce default type `String` does not match left type `mock.test.resource`",
                                "export 'out': type mismatch for 'children.a': expected String, got mock.test.resource",
                            ],
                            "export plain" => vec![
                                "export 'out': inferred resource handle mock.test.resource cannot cross a state boundary; export an identity value such as mock.test.resource.Id instead",
                            ],
                            "module argument" => vec![
                                "module call 'consume (anonymous call)': argument 'value': cannot assign mock.test.resource to 'value': expected String, got mock.test.resource (from children.a)",
                            ],
                            "function argument" | "arithmetic" => Vec::new(),
                            _ => unreachable!("bare-child position {position}"),
                        }
                        .into_iter()
                        .map(str::to_owned)
                        .collect()
                    } else if row.id == "2-6a" {
                        vec![format!(
                            "deferred-for iterable `{subscript}`: expected list iterable, got map"
                        )]
                    } else {
                        Vec::new()
                    };
                    if bracket != expected {
                        failures.push(format!(
                            "{}, {:?}, {position}, {subscript}: LSP {bracket:?}, validate {expected:?}",
                            row.id, row.kind
                        ));
                    }
                    if row.id == "2-6a" {
                        expected
                            .into_iter()
                            .map(|error| error.replace(subscript, row.reader))
                            .collect()
                    } else {
                        expected
                    }
                }
            };
            if dot != expected {
                failures.push(format!(
                    "{}, {:?}, {position}, {}: {dot:?}, expected {expected:?}",
                    row.id, row.kind, row.reader
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

struct Directory {
    root: tempfile::TempDir,
}

impl Directory {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        for (name, source) in files {
            let path = root.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        Self { root }
    }

    fn assert_ranges(
        &self,
        engine: &DiagnosticEngine,
        name: &str,
        message: &str,
        tokens: &[(&str, usize)],
    ) {
        let source = std::fs::read_to_string(self.root.path().join(name)).unwrap();
        self.assert_buffer_ranges(engine, name, &source, message, tokens);
    }

    fn assert_buffer_ranges(
        &self,
        engine: &DiagnosticEngine,
        name: &str,
        source: &str,
        message: &str,
        tokens: &[(&str, usize)],
    ) {
        let path = self.root.path().join(name);
        let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
        let mut actual: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains(message))
            .map(|diagnostic| diagnostic.range)
            .collect();
        let mut expected: Vec<_> = tokens
            .iter()
            .map(|(token, occurrence)| {
                let start = source.match_indices(token).nth(*occurrence).unwrap().0;
                let position = |byte| {
                    let prefix = &source[..byte];
                    Position::new(
                        prefix.bytes().filter(|byte| *byte == b'\n').count() as u32,
                        prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32,
                    )
                };
                Range::new(position(start), position(start + token.len()))
            })
            .collect();
        let order = |range: &Range| {
            (
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character,
            )
        };
        actual.sort_by_key(order);
        expected.sort_by_key(order);
        assert_eq!(actual, expected, "{name}: {diagnostics:#?}");
    }

    fn assert_sibling_owner(
        &self,
        engine: &DiagnosticEngine,
        message: &str,
        tokens: &[(&str, usize)],
    ) {
        self.assert_ranges(engine, "main.crn", message, &[]);
        self.assert_ranges(engine, "sibling.crn", message, tokens);
    }
}

const MAIN: &str = "provider aws {}\nlet cert = aws.acm.Certificate { domain_name = 'main' }\n";

#[test]
fn deferred_provider_annotations_use_exact_parser_tokens_in_the_owner_file() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider broken {}\nlet main_decoy = 'SharedId'\n",
        ),
        (
            "sibling.crn",
            "let decoy = '😀 SharedId'\nattributes { value: SharedId = 'id' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine()
            .with_provider_errors(HashMap::from([("broken".into(), "unavailable".into())])),
        "type check for SharedId was deferred",
        &[("SharedId", 1)],
    );
}

#[test]
fn deferred_function_checks_preserve_annotation_and_complete_call_locations() {
    let directory = Directory::new(&[
        ("main.crn", "provider broken {}\nlet unrelated = 'main'\n"),
        (
            "sibling.crn",
            "let decoy = '😀 Missing echo'\nfn echo(value: Missing) { value }\nlet actual = echo('id')\n",
        ),
    ]);
    let engine = test_engine()
        .with_provider_errors(HashMap::from([("broken".into(), "unavailable".into())]));
    directory.assert_sibling_owner(
        &engine,
        "type check for Missing was deferred",
        &[("Missing", 1), ("echo('id')", 0)],
    );
}

#[test]
fn unused_function_arithmetic_uses_the_operator_in_its_owner_file() {
    let directory = Directory::new(&[
        ("main.crn", "let decoy = '😀 +'\n"),
        (
            "sibling.crn",
            "let decoy = '😀 +'\nfn bad(value: String): Int { value + 1 }\n",
        ),
    ]);
    directory.assert_sibling_owner(&test_engine(), "left operand must be Int", &[("+", 1)]);
}

#[test]
fn resource_arithmetic_preserves_each_authored_operator() {
    let directory = Directory::new(&[
        ("main.crn", "provider mock {}\nlet main_decoy = '+'\n"),
        (
            "sibling.crn",
            "arguments { text: String }\nlet decoy = '😀 +'\nmock.test.Thing { enabled = text + 1 }\nmock.test.Thing { enabled = text + 2 }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &resource_location_schema_engine(),
        "left operand must be Int",
        &[("+", 1), ("+", 2)],
    );
}

#[test]
fn substituted_integer_operator_keeps_its_file_when_coordinates_coincide() {
    use carina_core::parser::AnalysisTree;
    let source = "arguments { ratio: Float }\nmock.test.Thing { enabled = ratio + 1 }\n";
    let directory = Directory::new(&[("main.crn", source), ("sibling.crn", source)]);
    let main = directory.root.path().join("main.crn");
    let sibling = directory.root.path().join("sibling.crn");
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut consumer = carina_core::parser::parse_file(source, &main, &catalog).unwrap();
    let authored = carina_core::parser::parse_file(source, &sibling, &catalog).unwrap();
    // Expansion substitutes a value, retaining its own parser provenance.
    consumer.resources[0].attributes["enabled"] =
        authored.resources[0].attributes["enabled"].clone();
    let tree = AnalysisTree::try_from_file(&consumer).unwrap();
    let resource = tree.resources().next().unwrap();
    let engine = resource_location_schema_engine();
    let schema = engine
        .schemas
        .get(
            "mock",
            "test.Thing",
            carina_core::schema::SchemaKind::Resource,
        )
        .unwrap();
    let bindings =
        carina_core::binding_index::BindingIndex::from_parsed(&consumer, &engine.schemas, &catalog);
    let findings = tree.integer_arithmetic_findings(resource, "enabled", schema, &bindings);
    assert_eq!(findings.len(), 1, "{findings:?}");
    let finding = &findings[0];
    assert_eq!(finding.location().file().path(), sibling);
    let span = finding.location().span();
    assert_eq!(&source[span.start_byte()..span.end_byte()], "+");
    assert!(
        render_merged(
            &create_document_at(source, main)
                .for_diagnostics()
                .expect("public fixture"),
            finding,
            DiagnosticSeverity::ERROR,
            finding.detail().to_string()
        )
        .is_none()
    );
    let rendered = render_merged(
        &create_document_at(source, sibling)
            .for_diagnostics()
            .expect("public fixture"),
        finding,
        DiagnosticSeverity::ERROR,
        finding.detail().to_string(),
    )
    .unwrap();
    let column = source.lines().nth(1).unwrap().find('+').unwrap() as u32;
    assert_eq!(
        rendered.range,
        Range::new(Position::new(1, column), Position::new(1, column + 1))
    );
}

#[test]
fn undefined_references_use_each_parser_occurrence_and_ignore_decoys() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider mock {}\nlet main_decoy = 'missing.value'\n",
        ),
        (
            "sibling.crn",
            "let decoy = '😀 missing.value'\nmock.test.Thing { enabled = missing.value }\nmock.test.Thing { enabled = missing.value }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &resource_location_schema_engine(),
        "Undefined resource: 'missing'",
        &[("missing.value", 1), ("missing.value", 2)],
    );
}

#[test]
fn authored_config_label_does_not_render_in_unrelated_buffers() {
    use carina_core::validation::deferred_types::{ProviderTypeCheckSite, TypeCheckSession};
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let source = "attributes { value: Missing = 'id' }";
    let parsed = carina_core::parser::parse_file(source, Path::new("<config>"), &catalog).unwrap();
    let terminal = TypeCheckSession::run(&catalog, |session| {
        session.record(
            &carina_core::parser::TypeExpr::String,
            ProviderTypeCheckSite::new(
                parsed.attribute_params[0].type_check_location().clone(),
                "authored check",
            ),
        );
        Vec::new()
    });
    let doc = create_document_at(source, "/project/sibling.crn".into());
    assert!(
        DiagnosticEngine::publish_type_diagnostics(
            terminal,
            &doc.for_diagnostics().expect("public fixture")
        )
        .is_empty()
    );
}

#[test]
fn unloaded_provider_notice_ignores_comment_and_string_decoys() {
    let source = "// provider aws {}\nlet decoy = 'provider aws {}'\nprovider aws {}\n";
    let directory = Directory::new(&[
        ("main.crn", "let main = 'provider aws {}'\n"),
        ("sibling.crn", source),
    ]);
    let engine =
        test_engine().with_provider_errors(HashMap::from([("aws".into(), "unavailable".into())]));
    directory.assert_sibling_owner(&engine, "Provider 'aws' is not loaded", &[("aws", 2)]);
}

fn resource_location_schema_engine() -> DiagnosticEngine {
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
    };
    let entry = || {
        AttributeType::struct_(
            "Entry",
            vec![
                StructField::optional("enabled", AttributeType::bool()).build(),
                StructField::optional(
                    "children",
                    AttributeType::list(AttributeType::struct_("Child", vec![])),
                )
                .with_block_name("child")
                .build(),
            ],
        )
    };
    let schema = ResourceSchema::new("test.Thing")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("enabled", AttributeType::bool()).build())
        .attribute(
            AttributeSchema::optional(
                "mode",
                AttributeType::enum_(
                    TypeIdentity::bare("Mode"),
                    Some(vec!["fast".into(), "slow".into()]),
                    vec![],
                    None,
                    None,
                ),
            )
            .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "config",
                AttributeType::struct_(
                    "Config",
                    vec![StructField::optional("enabled", AttributeType::bool()).build()],
                ),
            )
            .build(),
        )
        .attribute(
            AttributeSchema::optional("items", AttributeType::list(entry()))
                .with_block_name("item")
                .build(),
        )
        .attribute(AttributeSchema::optional("mapping", AttributeType::map(entry())).build())
        .attribute(
            AttributeSchema::optional(
                "variant",
                AttributeType::union(vec![
                    AttributeType::struct_(
                        "Enabled",
                        vec![StructField::required("enabled", AttributeType::bool()).build()],
                    ),
                    AttributeType::struct_(
                        "Other",
                        vec![StructField::required("other", AttributeType::bool()).build()],
                    ),
                ]),
            )
            .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema.validate().unwrap());
    custom_engine(schemas)
}

fn assert_resource_location(body: &str, message: &str, tokens: &[(&str, usize)]) {
    // Both the buffer parse and the sibling-function semantic fallback carry
    // the same authored spans. Test them with inline syntax and decoy text.
    for iterable in ["['one']", "names()"] {
        let source = format!(
            "let decoy = '😀 mock.test.Missing enabled = mode = config typo'\nlet items = for item in {iterable} {{ {body} }}\n"
        );
        let directory = Directory::new(&[
            (
                "main.crn",
                "provider mock {}\nfn names(): list(String) { ['one'] }\n",
            ),
            ("sibling.crn", &source),
        ]);
        let doc = create_document_at(&source, directory.root.path().join("sibling.crn"));
        assert_eq!(
            doc.parsed().is_none(),
            iterable == "names()",
            "{iterable}: {:?}",
            doc.parse_error()
        );
        directory.assert_sibling_owner(&resource_location_schema_engine(), message, tokens);
    }
}

#[test]
fn resource_type_uses_parser_header() {
    assert_resource_location(
        "mock.test.Missing {}",
        "No schema for mock.test.Missing",
        &[("mock.test.Missing", 1)],
    );
}

#[test]
fn attribute_type_uses_parser_key() {
    assert_resource_location(
        "mock.test.Thing { enabled = 'bad' }",
        "expected Bool",
        &[("enabled", 1)],
    );
}

#[test]
fn enum_uses_parser_value() {
    assert_resource_location(
        "mock.test.Thing { mode = 'bad' }",
        "string literal",
        &[("'bad'", 0)],
    );
}

#[test]
fn unknown_attribute_uses_parser_assignment() {
    assert_resource_location(
        "mock.test.Thing { typo = true }",
        "Unknown attribute",
        &[("typo = true", 0)],
    );
}

#[test]
fn multiline_unknown_attribute_keeps_the_precise_parser_key() {
    let directory = Directory::new(&[
        ("main.crn", "provider mock {}\n"),
        (
            "sibling.crn",
            "let decoy = '😀 typo'\nmock.test.Thing {\n  typo = [\n    'value'\n  ]\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &resource_location_schema_engine(),
        "Unknown attribute 'typo'",
        &[("typo", 1)],
    );
}

#[test]
fn block_syntax_uses_each_parser_key() {
    assert_resource_location(
        "mock.test.Thing { config { enabled = true } }",
        "cannot use block syntax",
        &[("config", 1)],
    );
}

#[test]
fn nested_schema_uses_parser_field() {
    assert_resource_location(
        "mock.test.Thing { config = { typo = true } }",
        "Unknown field",
        &[("typo", 1)],
    );
}

#[test]
fn nested_unknown_keys_keep_their_parser_key() {
    for body in [
        "mock.test.Thing { items = [{ enabled = true }, { typo = true }] }",
        "mock.test.Thing { mapping = { 'a.b' = { typo = true } } }",
        "mock.test.Thing { item { enabled = true } item { typo = true } }",
        "mock.test.Thing { item { child {} child { typo = true } } }",
    ] {
        assert_resource_location(body, "Unknown field", &[("typo", 1)]);
    }
}

#[test]
fn unknown_key_keeps_its_selected_union_arm_location_in_the_lsp() {
    assert_resource_location(
        "mock.test.Thing { variant = { enabled = true typo = true } }",
        "Unknown field",
        &[("typo", 1)],
    );
}

#[test]
fn missing_unknown_child_keeps_the_nearest_authored_parent_in_the_lsp() {
    use carina_core::schema::{AttributeType, ValidatedConfigSchema};
    let source = create_document_at(
        "mock.test.Thing { config = {} }",
        "/project/sibling.crn".into(),
    );
    let value = create_document_at(
        "mock.test.Thing { config = { typo = true } }",
        "/project/sibling.crn".into(),
    );
    let schema = ValidatedConfigSchema::validate(std::collections::BTreeMap::from([(
        "config".to_string(),
        AttributeType::struct_("Config", vec![]),
    )]))
    .unwrap();
    let resource = value.parsed().unwrap().iter_all_resources().next().unwrap();
    let findings = schema
        .effective_type("config")
        .unwrap()
        .validate_findings(&resource.attributes()["config"])
        .into_diagnostic_findings("config");
    let tree = carina_core::parser::AnalysisTree::try_from_file(source.parsed().unwrap()).unwrap();
    let (diagnostics, _) = resource_location_schema_engine().render_container_findings(
        &source.for_diagnostics().expect("public fixture"),
        tree.project_resources().next().unwrap(),
        "config",
        findings,
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::WARNING));
    assert!(
        diagnostics[0].message.contains("Unknown field"),
        "{diagnostics:#?}"
    );
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 18), Position::new(0, 24))
    );
}

#[test]
fn degraded_duplicates_keep_only_buffer_findings() {
    let directory = Directory::new(&[
        ("main.crn", "let same = 'first'\n"),
        (
            "sibling.crn",
            "let same = provider mock {}\nlet same = provider mock {}\n",
        ),
    ]);
    let engine = test_engine();
    directory.assert_ranges(&engine, "main.crn", "duplicate binding", &[]);
    directory.assert_ranges(&engine, "sibling.crn", "duplicate binding", &[("same", 1)]);
}

#[test]
fn module_call_findings_are_reported_once() {
    let directory = Directory::new(&[
        ("main.crn", "let service = use { source = './module' }\n"),
        ("sibling.crn", "service {}\n"),
        ("module/main.crn", "arguments { required_name: String }\n"),
    ]);
    let path = directory.root.path().join("sibling.crn");
    let doc = create_document_at("service {}\n", path.clone());
    let engine = test_engine();
    let findings = engine.analyze(&doc, Some(&path));
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.message.contains("required_name"))
            .count(),
        1,
        "{findings:#?}"
    );
    directory.assert_sibling_owner(&engine, "required_name", &[("service {}", 0)]);
}

#[test]
fn cached_module_findings_keep_upstream_type_authority() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let up = upstream_state { source = './upstream' }\nlet service = use { source = './module' }\nlet broken = use { source = './missing' }\nbroken {}\n",
        ),
        ("sibling.crn", "service { count = up.value }\n"),
        ("module/main.crn", "arguments { count: Int }\n"),
        ("upstream/main.crn", "exports { value: String = 'wrong' }\n"),
    ]);
    directory.assert_sibling_owner(&test_engine(), "expected Int", &[("up.value", 0)]);
}

#[test]
fn cycle_reports_once_per_member_file_in_ring_order() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let a = aws.acm.Certificate { domain_name = 'a' directives { depends_on = [c] } }\n",
        ),
        (
            "values.crn",
            "let b = aws.acm.Certificate { domain_name = a.domain_name }\nlet c = aws.acm.Certificate { domain_name = b.domain_name }\n",
        ),
    ]);
    let engine = test_engine_with_wait_target();
    let message = "Circular dependency detected: a -> c -> b -> a";
    directory.assert_sibling_owner(&engine, message, &[("c", 3)]);
    directory.assert_ranges(&engine, "values.crn", message, &[("b.domain_name", 0)]);
}

#[test]
fn wait_does_not_resolve_deferred_if_target() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\narguments { enabled: Bool }\nlet selected = if enabled { aws.acm.Certificate { domain_name = 'conditional' } }\n",
        ),
        (
            "sibling.crn",
            "let ready = wait selected { until = selected.status == 'ISSUED' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "target binding `selected` is not a known resource",
        &[("selected", 0)],
    );
}

#[test]
fn type_resolution_invariant_renders_in_each_directory_buffer() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let other = aws.acm.Certificate { domain_name = 'sibling' }\n",
        ),
    ]);
    let config =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut parsed = carina_core::config_loader::parse_directory(
        &carina_core::paths::ResolvedPath::new(directory.root.path()).unwrap(),
        &config,
    )
    .unwrap()
    .into_parsed();
    parsed.resources[0].declaration = Default::default();
    let error =
        carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
            carina_core::validation::resolve_file_type_exprs_report(&mut parsed, session, None)
        })
        .expect_err("return the typed boundary failure");
    for name in ["main.crn", "sibling.crn"] {
        let path = directory.root.path().join(name);
        let source = std::fs::read_to_string(&path).unwrap();
        let doc = create_document_at(&source, path);
        let diagnostic =
            render_analysis_invariant(&doc.for_diagnostics().expect("public fixture"), &error)
                .unwrap();
        assert_eq!(diagnostic.range, Range::default());
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(
            diagnostic
                .message
                .contains("missing declaration provenance")
        );
    }
}

#[test]
fn failed_function_annotation_keeps_main_finding_set() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nprovider broken { source = 'file:///definitely-missing/broken.wasm' }\n",
        ),
        (
            "sibling.crn",
            "fn bad(x: aws.Missing) { x }\nfn echo(x: Arn): String { 'ok' }\nexports { failed: String = bad('id') x = echo('id') }\n",
        ),
    ]);
    let engine =
        test_engine_with_wait_target().with_provider_errors(std::collections::HashMap::from([(
            "broken".to_string(),
            "fixture provider unavailable".to_string(),
        )]));
    directory.assert_sibling_owner(
        &engine,
        "unknown custom type 'aws.Missing'",
        &[("aws.Missing", 0)],
    );
    directory.assert_sibling_owner(&engine, "export 'failed'", &[]);
}

#[test]
fn merged_function_error_uses_parser_owned_body_location() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet cert = aws.acm.Certificate { domain_name = 'main' }\nlet invalid = identity(cert)\n",
        ),
        (
            "sibling.crn",
            "# identity is called from main.crn\nfn identity(r: aws.acm.Certificate): String { r }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "'r' is a resource handle, not a value",
        &[("r", 5)],
    );
}

#[test]
fn module_function_error_keeps_its_element_location() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet child = use { source = './module' }\nlet instance = child {}\n",
        ),
        ("sibling.crn", "let unrelated = 'sibling'\n"),
        (
            "module/arguments.crn",
            "arguments { unused: Bool = true }\n",
        ),
        (
            "module/functions.crn",
            "fn identity(resource_argument: aws.acm.Certificate): String { resource_argument }\n",
        ),
        (
            "module/resources.crn",
            "let cert = aws.acm.Certificate { domain_name = 'module' }\nlet invalid = identity(cert)\n",
        ),
    ]);
    let engine = test_engine_with_wait_target();
    let message = "'resource_argument' is a resource handle, not a value";
    directory.assert_ranges(&engine, "main.crn", message, &[("child", 1)]);
    directory.assert_ranges(&engine, "sibling.crn", message, &[]);
    directory.assert_ranges(
        &engine,
        "module/functions.crn",
        message,
        &[("resource_argument", 1)],
    );
}

#[test]
fn tp4_module_function_promise_is_routed_to_child() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        ResourceSchema::new("acm.Certificate")
            .attribute(AttributeSchema::optional("domain_name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("status", AttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["aws".into()]);
    for (declared, rejected) in [("'dev' | 'prod'", false), ("String", true)] {
        let child = format!(
            "arguments {{ env: {declared} }}\nfn pick(x: 'dev' | 'prod'): String {{ 'accepted' }}\nlet sink = aws.acm.Certificate {{ domain_name = pick(env) }}\n"
        );
        let root = "provider aws {}\nlet src = aws.acm.Certificate { domain_name = 'source' }\nlet child = use { source = './child' }\nchild { env = src.status }\n";
        let directory = Directory::new(&[("main.crn", root), ("child/main.crn", &child)]);
        let path = directory.root.path().join("main.crn");
        let diagnostics =
            engine.analyze_document_set(&create_document_at(root, path.clone()), Some(&path), &[]);
        let errors: Vec<_> = diagnostics
            .iter()
            .flat_map(|(path, diagnostics)| diagnostics.iter().map(move |finding| (path, finding)))
            .filter(|(_, finding)| finding.severity == Some(DiagnosticSeverity::ERROR))
            .collect();
        if rejected {
            assert_eq!(errors.len(), 1, "{diagnostics:#?}");
            assert_eq!(errors[0].0, &directory.root.path().join("child/main.crn"));
            assert_eq!(
                errors[0].1.message,
                "function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value."
            );
            let column = child.lines().nth(2).unwrap().find("pick(env)").unwrap() as u32;
            assert_eq!(
                errors[0].1.range,
                Range::new(Position::new(2, column), Position::new(2, column + 9))
            );
        } else {
            assert!(errors.is_empty(), "{diagnostics:#?}");
        }
    }
}

#[test]
fn provider_error_uses_sibling_attribute_location() {
    let directory = Directory::new(&[
        ("main.crn", "let regional = provider aws {}\n"),
        ("sibling.crn", "provider aws { default_tags = 'wrong' }\n"),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "default_tags must resolve to a map",
        &[("'wrong'", 0)],
    );
}

#[test]
fn expanded_provider_invariant_is_rendered() {
    use carina_core::parser::{AnalysisResolutionError, ProviderContext};
    let directory = Directory::new(&[
        ("main.crn", "provider aws {}\n"),
        ("sibling.crn", "let unrelated = 'sibling'\n"),
    ]);
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut expanded = carina_core::config_loader::parse_directory(
        &carina_core::paths::ResolvedPath::new(directory.root.path()).unwrap(),
        &config,
    )
    .unwrap()
    .into_parsed()
    .resolve_root_for_test();
    expanded.providers.push(expanded.providers[0].clone());
    let error = carina_core::parser::resolve_expanded_provider_unresolved_attributes_for_analysis(
        &mut expanded,
        &config,
    )
    .unwrap_err();
    assert!(matches!(&error, AnalysisResolutionError::Invariant(_)));
    for name in ["main.crn", "sibling.crn"] {
        let source = std::fs::read_to_string(directory.root.path().join(name)).unwrap();
        let doc = create_document_at(&source, directory.root.path().join(name));
        let diagnostic = render_analysis_resolution_error(
            &doc.for_diagnostics().expect("public fixture"),
            &error,
        )
        .expect("surface the directory invariant");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostic.message.contains("provider aws declaration"));
    }
}

#[test]
fn routing_waits_for_the_directory_parse() {
    let directory = Directory::new(&[
        ("main.crn", "let regional = provider aws {}\n"),
        (
            "sibling.crn",
            "aws.acm.Certificate { domain_name = 'sibling' directives { provider = regional } }\n",
        ),
        ("broken.crn", "invalid {{{"),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "not a known provider instance",
        &[],
    );
}

#[test]
fn unmappable_span_drops_only_that_finding() -> Result<(), carina_core::parser::AnalysisTreeError> {
    let directory = Directory::new(&[("main.crn", MAIN), ("sibling.crn", "let live = 'live'\n")]);
    let path = directory.root.path().join("sibling.crn");
    let document = create_document_at("let live = 'live'\n", path.clone());
    let stale_source = format!("{}let stale = 'disk'\n", "\n".repeat(80));
    let stale = create_document_at(&stale_source, path);
    let live_tree = carina_core::parser::AnalysisTree::try_from_file(document.parsed().unwrap())?;
    let stale_tree = carina_core::parser::AnalysisTree::try_from_file(stale.parsed().unwrap())?;
    let findings = [
        &stale_tree.syntax().declarations()[0],
        &live_tree.syntax().declarations()[0],
    ];
    let diagnostics: Vec<_> = findings
        .into_iter()
        .filter_map(|finding| {
            render_merged(
                &document.for_diagnostics().expect("public fixture"),
                finding,
                DiagnosticSeverity::ERROR,
                "test finding".to_string(),
            )
        })
        .collect();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 4), Position::new(0, 8))
    );
    Ok(())
}

#[test]
fn module_reload_uses_the_unsaved_buffer() {
    let directory = Directory::new(&[
        ("main.crn", "invalid {{{"),
        (
            "sibling.crn",
            "arguments { label: String = 'default' }\nattributes { name = label }\n",
        ),
    ]);
    let path = directory.root.path().join("main.crn");
    // Loading this directory as a module must see the live self-import and
    // report its cycle, rather than parsing the obsolete on-disk syntax.
    let document = create_document_at(
        "let local = use { source = '.' }\nlet call = local {}\n",
        path.clone(),
    );
    let merged = test_engine()
        .with_run_for_test(|run| {
            run.parse_and_resolve_merged_with_buffer(
                &document.for_diagnostics().expect("public fixture"),
                &carina_core::paths::ResolvedPath::new(directory.root.path()).unwrap(),
            )
        })
        .unwrap();
    let errors = match &merged.module_resolution {
        checks::MergedModuleResolution::Unanalyzed => panic!("module analysis must run"),
        checks::MergedModuleResolution::Unowned { errors }
        | checks::MergedModuleResolution::ConstraintFailed { errors, .. } => errors,
        checks::MergedModuleResolution::Expanded { .. } => panic!("self-import must fail"),
    };
    let messages: Vec<_> = errors
        .iter()
        .map(|error| error.display_relative_to(directory.root.path()))
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("ircular import")),
        "{messages:#?}"
    );
    assert!(
        !messages.iter().any(|message| message.contains("invalid")),
        "{messages:#?}"
    );
}

#[test]
fn upstream_reload_uses_the_unsaved_buffer() {
    let directory = Directory::new(&[
        ("main.crn", "invalid {{{"),
        ("sibling.crn", "let label = 'sibling'\n"),
    ]);
    directory.assert_buffer_ranges(
        &test_engine(),
        "main.crn",
        "let prior = upstream_state { source = '.' }\nexports { value: String = 'live' }\n",
        "failed to parse",
        &[],
    );
}

#[test]
fn unreadable_sibling_keeps_buffer_duplicates() {
    let source = "exports { repeated: String = 'one' repeated: String = 'two' }\n";
    let directory = Directory::new(&[
        ("main.crn", "let unrelated = 'main'\n"),
        ("sibling.crn", "let on_disk = 'old'\n"),
    ]);
    std::fs::write(directory.root.path().join("broken.crn"), [0xff]).unwrap();
    directory.assert_ranges(&test_engine(), "main.crn", "duplicate export", &[]);
    directory.assert_buffer_ranges(
        &test_engine(),
        "sibling.crn",
        source,
        "duplicate export",
        &[("repeated", 0)],
    );
}

#[test]
fn function_refs_use_each_authored_occurrence() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let decoy = 'cert.domain_name'\n# cert.domain_name\nfn returns_int(): Int { cert.domain_name }\nfn another_int(): Int { cert.domain_name }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "expected Int, got String",
        &[("cert.domain_name", 2), ("cert.domain_name", 3)],
    );
}

#[test]
fn resource_refs_ignore_decoys_and_keep_each_occurrence() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "# cert.statu\naws.acm.Certificate { domain_name = cert.statu }\naws.acm.Certificate { domain_name = cert.statu }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "Unknown attribute 'statu'",
        &[("statu", 1), ("statu", 2)],
    );
}

#[test]
fn non_bmp_positions_agree_across_renderers() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let note = '😀' let values = map('.name', [{ name = 'one' }])\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "Consider using pipe form",
        &[("map", 0)],
    );
    assert_eq!(position::byte_offset_to_char_offset("😀x", 4), 2);
    assert_eq!(position::char_len("😀x"), 3);
    let source = std::fs::read_to_string(directory.root.path().join("sibling.crn")).unwrap();
    let tokens = crate::semantic_tokens::SemanticTokensProvider::new(&[]).tokenize(&source);
    let mut column = 0;
    let operators: Vec<_> = tokens
        .iter()
        .filter_map(|token| {
            column = if token.delta_line == 0 {
                column + token.delta_start
            } else {
                token.delta_start
            };
            (token.token_type == 6).then_some(column)
        })
        .collect();
    let expected: Vec<_> = source
        .match_indices('=')
        .map(|(byte, _)| position::byte_offset_to_char_offset(&source, byte))
        .collect();
    assert_eq!(
        operators, expected,
        "semantic tokens must agree with diagnostic UTF-16 columns"
    );

    let path = directory.root.path().join("sibling.crn");
    let mut document = create_document_at(&source, path);
    let start = source.find("values").unwrap();
    document.apply_change(tower_lsp::lsp_types::TextDocumentContentChangeEvent {
        range: Some(Range::new(
            Position::new(0, position::byte_offset_to_char_offset(&source, start)),
            Position::new(
                0,
                position::byte_offset_to_char_offset(&source, start + "values".len()),
            ),
        )),
        range_length: None,
        text: "updated".into(),
    });
    assert_eq!(document.text(), source.replace("values", "updated"));
}

#[test]
fn parser_ranges_use_utf16() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let note = '😀' aws.acm.Certificate { domain_name = 'first' domain_name = 'second' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "Duplicate attribute 'domain_name'",
        &[("domain_name", 1)],
    );
}

#[test]
fn depends_on_belongs_to_sibling_unnamed_resource() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = '😀 missing'\n  directives { depends_on = [\n    missing\n  ] }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "binding 'missing' is not declared",
        &[("missing", 1)],
    );
}

#[test]
fn depends_on_duplicate_anchors_second_occurrence() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let consumer = aws.acm.Certificate {\n  domain_name = 'consumer'\n  directives { depends_on = [cert,\n    cert, cert] }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "listed multiple times",
        &[("cert", 1)],
    );
}

#[test]
fn depends_on_belongs_to_sibling_for_body() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let items = for (_, item) in [1] {\n  aws.acm.Certificate {\n    domain_name = 'loop'\n    directives { depends_on = [missing] }\n  }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "binding 'missing' is not declared",
        &[("missing", 0)],
    );
}

#[test]
fn wait_target_belongs_to_sibling() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let ready = wait absent {\n  until = absent.status == 'ready'\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "not a known resource",
        &[("absent", 0)],
    );
}

#[test]
fn wait_operands_belong_to_sibling() {
    let engine = test_engine_with_wait_target();
    for (source, message, token) in [
        (
            "let ready = wait cert {\n  until =\n    cert.statu == 'ready'\n}\n",
            "unknown attribute",
            "cert.statu",
        ),
        (
            "let ready = wait cert {\n  until = cert.status ==\n    ready\n}\n",
            "cannot resolve `ready`",
            "ready",
        ),
        (
            "let ready = wait cert {\n  until = cert.status == 'ready'\n  depends_on = [\n    missing\n  ]\n}\n",
            "binding 'missing' is not declared",
            "missing",
        ),
    ] {
        let directory = Directory::new(&[("main.crn", MAIN), ("sibling.crn", source)]);
        let occurrence = usize::from(message == "cannot resolve `ready`");
        directory.assert_sibling_owner(&engine, message, &[(token, occurrence)]);
    }
}

#[test]
fn provider_routing_belongs_to_sibling() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = 'consumer'\n  directives { provider = typo }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "`typo` is not a known provider instance",
        &[("typo", 0)],
    );
}

#[test]
fn repeated_provider_directives_anchor_the_effective_operand() {
    let directory = Directory::new(&[
        ("main.crn", "provider aws {}\nlet valid = provider aws {}\n"),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = 'consumer'\n  directives { provider = valid provider = typo }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "`typo` is not a known provider instance",
        &[("typo", 0)],
    );
}

#[test]
fn cycle_belongs_only_to_members_and_preserves_value_edge() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet independent = aws.acm.Certificate { domain_name = 'unrelated' }\n",
        ),
        (
            "sibling.crn",
            "let a = aws.acm.Certificate {\n  domain_name = 'a'\n  directives { depends_on = [b] }\n}\n",
        ),
        (
            "values.crn",
            "let b = aws.acm.Certificate {\n  domain_name = a.domain_name\n}\n",
        ),
    ]);
    let engine = test_engine_with_wait_target();
    directory.assert_sibling_owner(&engine, "Circular dependency", &[("b", 0)]);
    directory.assert_ranges(
        &engine,
        "values.crn",
        "Circular dependency",
        &[("a.domain_name", 0)],
    );
}

fn deferred_engine() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        ResourceSchema::new("acm.Certificate")
            .attribute(AttributeSchema::optional("domain_name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("status", AttributeType::string())
                    .deferred_populate()
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    DiagnosticEngine::new(loaded_test_providers(schemas), vec!["aws".into()])
}

#[test]
fn deferred_populate_belongs_to_sibling_attribute() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = cert.status\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &deferred_engine(),
        "populated asynchronously",
        &[("domain_name", 0)],
    );
}

#[test]
fn repeated_attribute_keys_anchor_the_effective_value() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = 'overwritten'\n  domain_name = cert.status\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &deferred_engine(),
        "populated asynchronously",
        &[("domain_name", 1)],
    );
}

#[test]
fn deferred_if_body_diagnostics_belong_to_sibling() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        ("arguments.crn", "arguments { enabled: Bool }\n"),
        (
            "sibling.crn",
            "let selected = if enabled {\n  aws.acm.Certificate {\n    domain_name = cert.status\n    directives { provider = typo depends_on = [missing] }\n  }\n}\n",
        ),
    ]);
    let engine = deferred_engine();
    directory.assert_sibling_owner(&engine, "populated asynchronously", &[("domain_name", 0)]);
    directory.assert_sibling_owner(
        &engine,
        "binding 'missing' is not declared",
        &[("missing", 0)],
    );
    directory.assert_sibling_owner(
        &engine,
        "`typo` is not a known provider instance",
        &[("typo", 0)],
    );
}

#[test]
fn expanded_module_diagnostic_keeps_its_authored_file()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet module = use { source = './module' }\nlet instance = module {}\n",
        ),
        (
            "module/arguments.crn",
            "arguments { label: String = 'module' }\n",
        ),
        (
            "module/sibling.crn",
            "let cert = aws.acm.Certificate { domain_name = 'module' }\nlet job = aws.acm.Certificate { domain_name = 'job' }\nlet ready = wait job { until = job.statu == 'ready' }\naws.acm.Certificate {\n  domain_name = cert.status\n  directives { depends_on = [cert, cert] }\n}\n",
        ),
    ]);
    let engine = deferred_engine();
    let root_path = directory.root.path().join("main.crn");
    let root_source = std::fs::read_to_string(&root_path).unwrap();
    let document = create_document_at(&root_source, root_path);
    let merged = engine
        .with_run_for_test(|run| {
            run.parse_and_resolve_merged_with_buffer(
                &document.for_diagnostics().expect("public fixture"),
                &carina_core::paths::ResolvedPath::new(directory.root.path()).unwrap(),
            )
        })
        .unwrap();
    let crate::diagnostics::checks::MergedModuleResolution::Expanded { graph } =
        &merged.module_resolution
    else {
        panic!("fixture must exercise a genuinely expanded module")
    };
    let tree = carina_core::parser::AnalysisTree::try_from_file(graph.as_ref())?;
    let waits = engine.with_run_for_test(|run| {
        carina_core::validation::wait::validate_wait_bindings(&tree, &run.schemas)
    });
    assert_eq!(waits.len(), 1);
    assert_eq!(
        waits[0].location().file().path(),
        directory.root.path().join("module/sibling.crn")
    );
    directory.assert_ranges(&engine, "main.crn", "populated asynchronously", &[]);
    directory.assert_ranges(
        &engine,
        "module/sibling.crn",
        "populated asynchronously",
        &[("domain_name", 2)],
    );
    directory.assert_ranges(&engine, "main.crn", "unknown attribute", &[]);
    directory.assert_ranges(
        &engine,
        "module/sibling.crn",
        "unknown attribute",
        &[("job.statu", 0)],
    );
    directory.assert_ranges(&engine, "main.crn", "listed multiple times", &[]);
    directory.assert_ranges(
        &engine,
        "module/sibling.crn",
        "listed multiple times",
        &[("cert", 3)],
    );
    Ok(())
}

#[test]
fn deferred_populate_and_routing_in_static_and_deferred_for_bodies_keep_the_owner() {
    for iterable in ["['item']", "names"] {
        let source = format!(
            "let items = for item in {iterable} {{\n  aws.acm.Certificate {{\n    domain_name = cert.status\n    directives {{ provider = typo }}\n  }}\n}}\n"
        );
        let directory = Directory::new(&[
            ("main.crn", MAIN),
            ("arguments.crn", "arguments { names: list(String) }\n"),
            ("sibling.crn", &source),
        ]);
        let engine = deferred_engine();
        directory.assert_sibling_owner(&engine, "populated asynchronously", &[("domain_name", 0)]);
        directory.assert_sibling_owner(
            &engine,
            "`typo` is not a known provider instance",
            &[("typo", 0)],
        );
    }
}

#[test]
fn source_override_never_written_buffer_is_visible_from_sibling() {
    let directory = Directory::new(&[("sibling.crn", "let observed = fresh\n")]);
    let path = directory.root.path().join("unsaved.crn");
    assert!(!path.exists());
    let source = "let fresh = 'from buffer'\n";
    let document = create_document_at(source, path.clone());
    let base = carina_core::paths::ResolvedPath::new(directory.root.path()).unwrap();
    let merged = test_engine()
        .with_run_for_test(|run| {
            run.parse_and_resolve_merged_with_buffer(&document.for_diagnostics().unwrap(), &base)
        })
        .unwrap();
    let sibling = merged
        .directory
        .semantic_file(&directory.root.path().join("sibling.crn"))
        .unwrap();
    assert_eq!(
        sibling.variables.get("observed"),
        Some(&Value::Concrete(ConcreteValue::String(
            "from buffer".into()
        )))
    );
    assert!(
        merged
            .directory
            .source_files
            .iter()
            .any(|(file, text)| { file.source_label() == path && text == source })
    );
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn source_override_merged_buffer_reuses_pinned_base() {
    let directory = Directory::new(&[
        ("before/sibling.crn", "let observed = fresh\n"),
        ("after/sibling.crn", "let observed = 'other tree'\n"),
    ]);
    let alias = directory.root.path().join("project");
    std::os::unix::fs::symlink("before", &alias).unwrap();
    let base = carina_core::paths::ResolvedPath::new(&alias).unwrap();
    let path = alias.join("unsaved.crn");
    let document = create_document_at("let fresh = 'pinned buffer'\n", path);
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink("after", &alias).unwrap();

    let merged = test_engine()
        .with_run_for_test(|run| {
            run.parse_and_resolve_merged_with_buffer(&document.for_diagnostics().unwrap(), &base)
        })
        .unwrap();
    let sibling = merged
        .directory
        .semantic_file(&alias.join("sibling.crn"))
        .unwrap();
    assert_eq!(
        sibling.variables.get("observed"),
        Some(&Value::Concrete(ConcreteValue::String(
            "pinned buffer".into()
        )))
    );
}

#[test]
fn unsaved_sibling_override_uses_its_current_parser_span() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate { domain_name = 'on disk' }\n",
        ),
    ]);
    let source = "# unsaved 😀\n\naws.acm.Certificate {\n  domain_name = 'current'\n  directives { depends_on = [missing] }\n}\n";
    let path = directory.root.path().join("sibling.crn");
    let engine = test_engine_with_wait_target();
    let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    let matching: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("binding 'missing' is not declared")
        })
        .collect();
    assert_eq!(matching.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(4, 29), Position::new(4, 36))
    );
}

#[test]
fn cycle_from_exports_uses_the_sibling_lets_reference_location() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet a = aws.acm.Certificate { domain_name = 'a' directives { depends_on = [b] } }\nlet value = a.domain_name\n",
        ),
        (
            "exports.crn",
            "let b = aws.acm.Certificate { domain_name = value }\nexports { result = b.domain_name }\n",
        ),
        ("unrelated.crn", "let unrelated = 'unrelated'\n"),
    ]);
    let engine = test_engine_with_wait_target();
    directory.assert_ranges(
        &engine,
        "main.crn",
        "Circular dependency",
        &[("a.domain_name", 0)],
    );
    directory.assert_ranges(&engine, "exports.crn", "Circular dependency", &[]);
    directory.assert_ranges(&engine, "unrelated.crn", "Circular dependency", &[]);
}

#[test]
fn depends_on_precise_anchors_ignore_strings_comments_and_other_resources() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let first = aws.acm.Certificate {\n  domain_name = 'depends_on = [missing]'\n  # depends_on = [missing]\n  directives { depends_on = [missing] }\n}\nlet second = aws.acm.Certificate {\n  domain_name = 'other'\n  directives { depends_on = [another] }\n}\n",
        ),
    ]);
    let engine = test_engine_with_wait_target();
    directory.assert_sibling_owner(
        &engine,
        "binding 'missing' is not declared",
        &[("missing", 2)],
    );
    directory.assert_sibling_owner(
        &engine,
        "binding 'another' is not declared",
        &[("another", 0)],
    );
}

#[test]
fn upstream_resolve_owns_each_recorded_source() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let other = upstream_state { source = './broken' }\n",
        ),
        (
            "sibling.crn",
            "let orgs = upstream_state {\n  source =\n    './broken'\n}\n",
        ),
        ("broken/main.crn", "invalid {{{"),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "upstream_state `orgs`: failed to parse",
        &[("'./broken'", 0)],
    );
}

#[test]
fn round4_upstream_unknown_type_uses_loaded_discovery_diagnostic() {
    let directory = Directory::new(&[
        (
            "upstream/exports.crn",
            "exports { value: Foo = 'upstream-only' }\n",
        ),
        (
            "downstream/main.crn",
            "provider aws {}\nlet ups = upstream_state { source = '../upstream' }\n",
        ),
    ]);
    directory.assert_ranges(
        &test_engine_with_wait_target(),
        "downstream/main.crn",
        "upstream_state `ups`: failed to parse source `../upstream`:",
        &[("'../upstream'", 0)],
    );
    let path = directory.root.path().join("downstream/main.crn");
    let source = std::fs::read_to_string(&path).unwrap();
    let diagnostics = test_engine_with_wait_target()
        .analyze(&create_document_at(&source, path.clone()), Some(&path));
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.ends_with("unknown custom type 'Foo'") }),
        "{diagnostics:#?}"
    );
}

#[test]
fn upstream_field_owns_repeated_occurrences_after_unicode_heredoc() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\nlet decoy = 'orgs.missing'\n",
        ),
        (
            "sibling.crn",
            "let note = <<TEXT\n😀 orgs.missing\nTEXT\n# orgs.missing\nlet first = orgs.missing\nlet second = orgs.missing\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "does not export `missing`",
        &[("orgs.missing", 2), ("orgs.missing", 3)],
    );
}

#[test]
fn upstream_for_shape_owns_multiline_iterable() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet orgs = upstream_state { source = './upstream' }\nlet decoy = 'orgs.accounts'\n",
        ),
        (
            "sibling.crn",
            "let note = '😀 orgs.accounts'\nfor item in\n  orgs.accounts\n{ aws.acm.Certificate { domain_name = 'loop' } }\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: map(String) = { a = 'a' } }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "is declared as",
        &[("orgs.accounts", 1)],
    );
}

#[test]
fn upstream_attribute_shape_owns_reference_prefix() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\nlet decoy = 'orgs.accounts'\n",
        ),
        (
            "sibling.crn",
            "exports {\n  first = orgs.accounts.name\n  second = orgs.accounts.name\n}\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "reads it as a struct",
        &[("orgs.accounts", 0), ("orgs.accounts", 1)],
    );
}

#[test]
fn upstream_subscript_shape_owns_reference_prefix() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\nlet decoy = 'orgs.accounts'\n",
        ),
        (
            "sibling.crn",
            "exports {\n  first = orgs.accounts['bad']\n  second = orgs.accounts['bad']\n}\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "subscript",
        &[("orgs.accounts", 0), ("orgs.accounts", 1)],
    );
}

#[test]
fn upstream_field_in_main_is_not_reanchored_at_export_alias() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\nlet bad = orgs.missing\n",
        ),
        ("exports.crn", "exports { result = bad }\n# orgs.missing\n"),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    let engine = test_engine();
    directory.assert_ranges(&engine, "exports.crn", "does not export `missing`", &[]);
    directory.assert_ranges(
        &engine,
        "main.crn",
        "does not export `missing`",
        &[("orgs.missing", 0)],
    );
}

#[test]
fn upstream_field_unsaved_override_owns_new_occurrences() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\n",
        ),
        ("sibling.crn", "let old = orgs.missing\n"),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    let engine = test_engine();
    directory.assert_buffer_ranges(
        &engine,
        "sibling.crn",
        "# orgs.missing\nlet note = '😀'\nlet edited = orgs.missing\n",
        "does not export `missing`",
        &[("orgs.missing", 1)],
    );
    directory.assert_buffer_ranges(
        &engine,
        "sibling.crn",
        "let fixed = orgs.accounts\n",
        "does not export `missing`",
        &[],
    );
}

#[test]
fn remaining_type_resolution_owns_annotation_tokens() {
    for source in [
        "arguments { value:\n  TotallyMadeUpType }\n",
        "attributes { value:\n  TotallyMadeUpType = 'x' }\n",
        "exports { value:\n  TotallyMadeUpType = 'x' }\n",
        "fn example(value:\n  TotallyMadeUpType) { value }\n",
        "fn example(value: String):\n  TotallyMadeUpType { value }\n",
    ] {
        let directory = Directory::new(&[("main.crn", MAIN), ("sibling.crn", source)]);
        directory.assert_sibling_owner(
            &test_engine_with_wait_target(),
            "unknown custom type",
            &[("TotallyMadeUpType", 0)],
        );
    }
}

#[test]
fn remaining_deferred_iterable_type_owns_multiline_expression() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "arguments { count: Int }\nfor item in\n  count\n{ aws.acm.Certificate { domain_name = 'loop' } }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "expected list iterable, got Int",
        &[("count", 1)],
    );
}

#[test]
fn remaining_inference_failure_owns_inline_export_name() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let note = '😀'\nexports { result = [1, 'a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "type annotation required",
        &[("result", 0)],
    );
}

#[test]
fn remaining_duplicate_bindings_keep_last_occurrence_per_file() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let same = '😀'\nlet same = 'second'\nlet same = 'third'\n",
        ),
    ]);
    directory.assert_sibling_owner(&test_engine(), "same'", &[("same", 2)]);
}

#[test]
fn remaining_duplicate_declarations_keep_occurrences_across_files() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "exports { same = 'first' same = 'second' }\n",
        ),
        ("exports.crn", "exports { same = 'third' }\n"),
    ]);
    let engine = test_engine();
    directory.assert_sibling_owner(&engine, "same'", &[("same", 0)]);
    directory.assert_ranges(&engine, "exports.crn", "same'", &[("same", 0)]);
}

#[test]
fn remaining_unknown_iterable_root_owns_recorded_reference() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "for item in\n  missing.items\n{ aws.acm.Certificate { domain_name = 'loop' } }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "Undefined identifier `missing`",
        &[("missing.items", 0)],
    );
}

#[test]
fn remaining_missing_upstream_directory_owns_source_value() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let other = upstream_state { source = './missing' }\n",
        ),
        (
            "sibling.crn",
            "let orgs = upstream_state {\n  source =\n    './missing'\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "upstream_state 'orgs': source",
        &[("'./missing'", 0)],
    );
}

#[test]
fn upstream_field_utf16_columns_after_heredoc_and_astral_literal() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\n",
        ),
        (
            "sibling.crn",
            "let note = <<TEXT\n😀 orgs.missing\nTEXT\nlet values = ['😀', orgs.missing, orgs.missing]\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "does not export `missing`",
        &[("orgs.missing", 1), ("orgs.missing", 2)],
    );
}

#[test]
fn remaining_duplicate_inventory_covers_all_declaration_kinds() {
    for (source, name, message, occurrence) in [
        (
            "let repeated = 'a'\nlet repeated = 'b'\n",
            "repeated",
            "duplicate binding",
            1,
        ),
        (
            "arguments { repeated: String repeated: String }\n",
            "repeated",
            "duplicate argument",
            0,
        ),
        (
            "attributes { repeated = 'a' repeated = 'b' }\n",
            "repeated",
            "duplicate module attribute",
            0,
        ),
        (
            "exports { repeated = 'a' repeated = 'b' }\n",
            "repeated",
            "duplicate export",
            0,
        ),
        (
            "fn repeated() { 'a' }\nfn repeated() { 'b' }\n",
            "repeated",
            "duplicate function",
            0,
        ),
        (
            "provider mock {}\nprovider mock {}\n",
            "mock",
            "duplicate provider kind",
            0,
        ),
    ] {
        let directory = Directory::new(&[("main.crn", MAIN), ("sibling.crn", source)]);
        directory.assert_sibling_owner(
            &test_engine_with_wait_target(),
            message,
            &[(name, occurrence)],
        );
    }
}

#[test]
fn remaining_duplicate_inventory_honors_unsaved_override() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        ("sibling.crn", "let repeated = 'a'\nlet repeated = 'b'\n"),
    ]);
    let engine = test_engine();
    directory.assert_buffer_ranges(
        &engine,
        "sibling.crn",
        "let fixed = '😀'\n",
        "duplicate binding",
        &[],
    );
    directory.assert_buffer_ranges(
        &engine,
        "sibling.crn",
        "let note = '😀'\nlet repeated = 'a'\nlet repeated = 'b'\n",
        "duplicate binding",
        &[("repeated", 1)],
    );
}

#[test]
fn remaining_provider_finalization_owns_recorded_attribute_value() {
    let directory = Directory::new(&[
        ("main.crn", "let tag_values = 'bad'\n"),
        (
            "sibling.crn",
            "provider aws {\n  default_tags =\n    tag_values\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "default_tags must resolve to a map",
        &[("tag_values", 0)],
    );
}

#[test]
fn closure_module_placement_owns_multiline_provider_header() {
    let directory = Directory::new(&[
        ("main.crn", "arguments { label: String = 'module' }\n"),
        (
            "sibling.crn",
            "let note = <<TEXT\nprovider aws {}\nTEXT\nprovider\n  aws {}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "provider blocks are not allowed inside modules",
        &[("provider\n  aws", 0)],
    );
}

#[test]
fn closure_arguments_in_root_owns_keyword_after_heredoc() {
    let directory = Directory::new(&[
        ("main.crn", "provider aws {}\n"),
        (
            "sibling.crn",
            "let note = <<TEXT\narguments {\nTEXT\narguments\n{ label: String = 'module' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "arguments blocks are only valid inside module definitions",
        &[("arguments", 1)],
    );
}

#[test]
fn closure_backend_attribute_owns_utf16_key() {
    let directory = Directory::new(&[
        ("main.crn", "let label = 'main'\n"),
        (
            "sibling.crn",
            "let note = '😀' backend local { path = 'state.json' typo = 'x' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "Unknown attribute 'typo' for backend type 'local'",
        &[("typo", 0)],
    );
}

#[test]
fn closure_anonymous_identity_owns_utf16_resource_headers() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Thing")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let directory = Directory::new(&[
        ("main.crn", "provider mock {}\n"),
        (
            "sibling.crn",
            "let note = '😀' mock.test.Thing { name = 'one' }\nmock.test.Thing { name = 'two' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &custom_engine(schemas),
        "identity is derived from mutable attributes",
        &[("mock.test.Thing", 0), ("mock.test.Thing", 1)],
    );
}

#[test]
fn closure_module_attribute_owns_inline_name_and_value() {
    for (source, message, token) in [
        (
            "let note = '😀'\nattributes { result: String = 1 }\n",
            "expected String",
            "result",
        ),
        (
            "let note = '😀'\nattributes { result = missing }\n",
            "Undefined resource 'missing'",
            "missing",
        ),
    ] {
        let directory = Directory::new(&[
            ("main.crn", "let label = 'main'\n"),
            ("sibling.crn", source),
        ]);
        directory.assert_sibling_owner(&test_engine_with_wait_target(), message, &[(token, 0)]);
    }
}

#[test]
fn r5_provider_finalization_survives_unrelated_failed_secret_let() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let tag_values = 'bad'\nlet unused = secret(env('R5_FINALIZE_UNSET'))\n",
        ),
        (
            "sibling.crn",
            "provider aws {\n default_tags =\n tag_values\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "default_tags must resolve to a map",
        &[("tag_values", 0)],
    );
}
