use super::*;

const FIXTURE_FILES: &[(&str, &str)] = &[
    (
        "caller/main.crn",
        include_str!("../fixtures/import_root_boundary/caller/main.crn"),
    ),
    (
        "caller/providers.crn",
        include_str!("../fixtures/import_root_boundary/caller/providers.crn"),
    ),
    (
        "rooted/main.crn",
        include_str!("../fixtures/import_root_boundary/rooted/main.crn"),
    ),
    (
        "rooted/providers.crn",
        include_str!("../fixtures/import_root_boundary/rooted/providers.crn"),
    ),
    (
        "rooted/backend.crn",
        include_str!("../fixtures/import_root_boundary/rooted/backend.crn"),
    ),
    (
        "rooted/exports.crn",
        include_str!("../fixtures/import_root_boundary/rooted/exports.crn"),
    ),
];

fn fixture() -> Directory {
    let temp = tempfile::tempdir().unwrap();
    for (relative, source) in FIXTURE_FILES {
        let path = temp.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let directory = Directory {
        project: temp.path().join("caller"),
        home: temp.path().join("home"),
        _temp: temp,
    };
    fs::create_dir(&directory.home).unwrap();
    directory
}

#[test]
fn lint_keeps_main_boundary_gate_for_resolution_only_body_failures() {
    for nested in [false, true] {
        for called in [false, true] {
            let directory = fixture();
            fs::remove_dir_all(directory._temp.path().join("rooted")).unwrap();
            fs::create_dir(directory._temp.path().join("rooted")).unwrap();
            fs::write(directory._temp.path().join("rooted/main.crn"),
                "arguments { n: Int = 1 }\nexports { value = 1 }\nlet a = 'present'\nlet broken = a ?? 1\n").unwrap();
            let mut caller = "let service = use { source = '../rooted' }\n".to_owned();
            if called {
                caller.push_str("let result = service {}\n");
            }
            if nested {
                fs::create_dir(directory._temp.path().join("bridge")).unwrap();
                fs::write(
                    directory._temp.path().join("bridge/main.crn"),
                    format!("arguments {{ n: Int = 1 }}\n{caller}"),
                )
                .unwrap();
                caller = "let outer = use { source = '../bridge' }\n".into();
                if called {
                    caller.push_str("let result = outer {}\n");
                }
            }
            fs::write(directory.project.join("main.crn"), caller).unwrap();
            let lint = directory.run(&["lint", "."]);
            let text = String::from_utf8(lint.stderr).unwrap();
            assert!(text.contains("exports blocks are not allowed"), "{text}");
            assert!(
                text.ends_with("Error: Found 1 lint warning(s).\n"),
                "{text}"
            );
            assert!(!text.contains("Module resolution error"), "{text}");
        }
    }
}

#[test]
fn failing_clean_bridge_keeps_mains_ordinary_io_scope_and_location() {
    let directory = fixture();
    fs::remove_dir_all(directory._temp.path().join("rooted")).unwrap();
    fs::create_dir_all(directory._temp.path().join("rooted/child")).unwrap();
    fs::write(
        directory._temp.path().join("rooted/main.crn"),
        "arguments { n: Int = 1 }\nlet child = use { source = './child' }\n",
    )
    .unwrap();
    fs::write(
        directory._temp.path().join("rooted/child/main.crn"),
        "arguments {}\nexports {}\n",
    )
    .unwrap();
    fs::write(directory._temp.path().join("rooted/unreadable.crn"), [0xff]).unwrap();
    fs::write(
        directory.project.join("main.crn"),
        "let service = use { source = '../rooted' }\n",
    )
    .unwrap();
    let output = directory.run(&["validate", "--json", "."]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2, "{report}");
    let io = errors
        .iter()
        .find(|error| error["message"] == "IO error: stream did not contain valid UTF-8")
        .unwrap();
    assert!(
        io["location"].is_null(),
        "ordinary uncalled IO keeps main's null location: {report}"
    );
    for command in ["validate", "lint"] {
        let output = directory.run(&[command, "."]);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr.matches("IO error:").count(), 1, "{stderr}");
    }
    fs::write(
        directory.project.join("main.crn"),
        "let service = use { source = '../rooted' }\nlet result = service { n = 'wrong' unexpected = 1 }\n",
    )
    .unwrap();
    let output = directory.run(&["validate", "--json", "."]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 4, "{report}");
    assert!(
        errors[0]["message"]
            .as_str()
            .unwrap()
            .contains("exports blocks"),
        "{report}"
    );
    assert!(
        errors[1]["message"]
            .as_str()
            .unwrap()
            .contains("expected Int"),
        "{report}"
    );
    assert!(
        errors[2]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown argument"),
        "{report}"
    );
    assert!(
        errors[3]["message"]
            .as_str()
            .unwrap()
            .starts_with("IO error:"),
        "{report}"
    );
    let output = directory.run(&["validate", "."]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.find("Unknown argument").unwrap() < stderr.find("IO error:").unwrap(),
        "{stderr}"
    );
}

#[test]
fn shared_reads_keep_main_ordinary_scope_on_each_route() {
    for child in [false, true] {
        for nested in [false, true] {
            for parent_first in [false, true] {
                for called in [false, true] {
                    let directory = fixture();
                    let root = directory._temp.path();
                    for name in ["x", "parent", "bridge"] {
                        fs::create_dir(root.join(name)).unwrap();
                    }
                    fs::write(root.join("x/main.crn"), "arguments { n: Int = 1 }\n").unwrap();
                    fs::write(root.join("x/unreadable.crn"), [0xff]).unwrap();
                    fs::write(
                        root.join("parent/main.crn"),
                        format!(
                            "arguments {{ n: Int = 1 }}\nexports {{}}\n{}",
                            if child {
                                "let child = use { source = '../x' }\n"
                            } else {
                                ""
                            },
                        ),
                    )
                    .unwrap();
                    let mut uses = [
                        "let plain = use { source = '../x' }\n",
                        "let parent = use { source = '../parent' }\n",
                    ];
                    if parent_first {
                        uses.reverse();
                    }
                    let body = format!(
                        "{}{}",
                        uses.concat(),
                        if called {
                            "let left = plain {}\nlet right = parent {}\n"
                        } else {
                            ""
                        }
                    );
                    let caller = if nested {
                        fs::write(
                            root.join("bridge/main.crn"),
                            format!("arguments {{ bridge_id: Int = 1 }}\n{body}"),
                        )
                        .unwrap();
                        format!(
                            "let bridge = use {{ source = '../bridge' }}\n{}",
                            if called {
                                "let outer = bridge {}\n"
                            } else {
                                ""
                            }
                        )
                    } else {
                        body
                    };
                    fs::write(directory.project.join("main.crn"), caller).unwrap();
                    let label = format!(
                        "child={child}, nested={nested}, parent_first={parent_first}, called={called}"
                    );
                    let expected_validate = if child && (!nested || !parent_first) {
                        2
                    } else {
                        1
                    };
                    let output = directory.run(&["validate", "--json", "."]);
                    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
                    let errors = report["errors"].as_array().unwrap();
                    let io = errors
                        .iter()
                        .filter(|error| error["message"].as_str().unwrap().starts_with("IO error:"))
                        .collect::<Vec<_>>();
                    assert_eq!(io.len(), expected_validate, "{label}: {report}");
                    assert_eq!(errors.len(), expected_validate + 1, "{label}: {report}");
                    let nulls = io
                        .iter()
                        .filter(|error| error["location"].is_null())
                        .count();
                    assert_eq!(
                        nulls,
                        if called {
                            0
                        } else {
                            expected_validate - usize::from(child)
                        },
                        "{label}: {report}"
                    );
                    let human = directory.run(&["validate", "."]);
                    let text = String::from_utf8(human.stderr).unwrap();
                    assert_eq!(
                        text.matches("IO error:").count(),
                        expected_validate,
                        "{label}: {text}"
                    );
                    let lint = directory.run(&["lint", "."]);
                    let text = String::from_utf8(lint.stderr).unwrap();
                    let expected_lint = if child && !parent_first { 2 } else { 1 };
                    assert_eq!(
                        text.matches("IO error:").count(),
                        expected_lint,
                        "{label}: {text}"
                    );
                    assert_eq!(
                        text.matches("exports blocks are not allowed").count(),
                        1,
                        "{label}: {text}"
                    );
                }
            }
        }
    }
}

fn assert_transports(directory: &Directory, stderr: &str, json: &str, lsp: &str) {
    let human = directory.run(&["validate", "."]);
    assert_eq!(human.status.code(), Some(1));
    assert_eq!(String::from_utf8(human.stdout).unwrap(), "Validating...\n");
    assert_eq!(String::from_utf8(human.stderr).unwrap(), stderr);

    let output = directory.run(&["validate", "--json", "."]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        serde_json::from_str::<Value>(json).unwrap()
    );

    let snapshot = Arc::new(loaded_provider_snapshot_from_factories(
        Arc::new(vec![Box::new(ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::memory(),
        ))]),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    ));
    let engine = DiagnosticEngine::new(snapshot.clone(), vec!["mock".into()]);
    let path = directory.project.join("main.crn");
    let document = Document::new_with_path(
        fs::read_to_string(&path).unwrap(),
        path.clone(),
        Arc::new(snapshot.provider_context().as_provider_context().clone()),
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    let root = tower_lsp::lsp_types::Url::from_directory_path(directory._temp.path()).unwrap();
    let expected = lsp.replace("$ROOT", root.as_str().trim_end_matches('/'));
    assert_eq!(
        serde_json::to_value(diagnostics).unwrap(),
        serde_json::from_str::<Value>(&expected).unwrap()
    );
}

#[test]
fn imported_root_pins_all_five_errors_and_call_site_on_every_transport() {
    assert_transports(
        &fixture(),
        include_str!("../fixtures/import_root_boundary/five.stderr"),
        include_str!("../fixtures/import_root_boundary/five.json"),
        include_str!("../fixtures/import_root_boundary/five.lsp.json"),
    );
}

#[test]
fn nonroot_prohibition_keeps_body_and_call_findings() {
    let directory = fixture();
    fs::remove_dir_all(directory._temp.path().join("rooted")).unwrap();
    fs::create_dir(directory._temp.path().join("rooted")).unwrap();
    fs::write(directory._temp.path().join("rooted/main.crn"),
        "arguments { n: Int = 1 }\nattributes { bad: TotallyMadeUpType = 'one' }\nexports { value = 'one' }\n").unwrap();
    fs::write(directory.project.join("main.crn"),
        "let service = use { source = '../rooted' }\nlet result = service { n = 'wrong' unexpected = 1 }\n").unwrap();
    let output = directory.run(&["validate", "."]);
    let text = output_text(&output);
    assert!(text.contains("exports blocks are not allowed"), "{text}");
    assert!(
        text.contains("attribute 'bad': unknown custom type 'TotallyMadeUpType'"),
        "{text}"
    );
    assert!(
        text.contains("module call 'result': argument 'n': expected Int, got string 'wrong'"),
        "{text}"
    );
    assert!(text.contains("Unknown argument 'unexpected'"), "{text}");
}

#[test]
fn readable_exports_precede_unreadable_sibling() {
    let directory = fixture();
    fs::remove_dir_all(directory._temp.path().join("rooted")).unwrap();
    fs::create_dir(directory._temp.path().join("rooted")).unwrap();
    fs::write(
        directory._temp.path().join("rooted/main.crn"),
        "arguments { n: Int = 1 }\nattributes { value: String = 'one' }\n",
    )
    .unwrap();
    fs::write(
        directory._temp.path().join("rooted/exports.crn"),
        "exports { value: String = 'one' }\n",
    )
    .unwrap();
    fs::write(
        directory._temp.path().join("rooted/z-unreadable.crn"),
        [0xff],
    )
    .unwrap();
    let output = directory.run(&["validate", "."]);
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Error: ../rooted: exports blocks are not allowed inside modules. Define exports at the root configuration level.\nError: Module resolution error: IO error: stream did not contain valid UTF-8\n"
    );
}

#[test]
fn full_reach_keeps_each_use_children_and_failing_bridges_on_every_transport() {
    for shape in [
        "two_uses",
        "two_called_uses",
        "two_calls",
        "two_children",
        "body_error",
        "unreadable",
        "prohibited_parent",
    ] {
        let directory = fixture();
        let bridge = directory._temp.path().join("bridge");
        fs::create_dir_all(bridge.join("child")).unwrap();
        fs::write(bridge.join("child/main.crn"), "arguments {}\nexports {}\n").unwrap();
        let mut body =
            "arguments { n: Int = 1 }\nlet child = use { source = './child' }\n".to_string();
        let expected = match shape {
            "two_uses" | "two_called_uses" => 2,
            "two_calls" => 1,
            "two_children" => {
                fs::create_dir(bridge.join("other")).unwrap();
                fs::write(bridge.join("other/main.crn"), "arguments {}\nexports {}\n").unwrap();
                body.push_str("let other = use { source = './other' }\n");
                2
            }
            "body_error" => {
                body.push_str("let a = 'present'\nlet broken = a ?? 1\n");
                1
            }
            "unreadable" => {
                fs::write(bridge.join("unreadable.crn"), [0xff]).unwrap();
                1
            }
            "prohibited_parent" => {
                body.push_str("exports {}\n");
                2
            }
            _ => unreachable!(),
        };
        fs::write(bridge.join("main.crn"), body).unwrap();
        let caller = if shape == "two_calls" {
            "let one = use { source = '../bridge/child' }\none {}\none {}\n"
        } else if shape == "two_called_uses" {
            "let one = use { source = '../bridge/child' }\none {}\nlet two = use { source = '../bridge/child' }\ntwo {}\n"
        } else if shape == "two_uses" {
            "let one = use { source = '../bridge/child' }\nlet two = use { source = '../bridge/child' }\n"
        } else {
            "let one = use { source = '../bridge' }\n"
        };
        fs::write(directory.project.join("main.crn"), caller).unwrap();
        let output = directory.run(&["validate", "--json", "."]);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let errors = report["errors"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|error| {
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("not allowed inside modules")
            })
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), expected, "{shape}: {report}");
        for command in ["validate", "lint"] {
            let output = directory.run(&[command, "."]);
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(
                stderr.matches("not allowed inside modules").count(),
                expected,
                "{shape}, {command}: {stderr}"
            );
            if shape == "body_error" && command == "lint" {
                assert!(
                    stderr.contains("Error: Module resolution error: Parse error: ../bridge/main.crn:4:19: Invalid expression at line 4: coalesce default type `Int` does not match left type `String`"),
                    "{stderr}"
                );
            }
        }
        let snapshot = Arc::new(loaded_provider_snapshot_from_factories(
            Arc::new(vec![Box::new(ConfiguredBuiltinMock::new(
                MockBehavior::default(),
                MockScope::memory(),
            ))]),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        ));
        let engine = DiagnosticEngine::new(snapshot.clone(), vec!["mock".into()]);
        let path = directory.project.join("main.crn");
        let document = Document::new_with_path(
            caller.into(),
            path.clone(),
            Arc::new(snapshot.provider_context().as_provider_context().clone()),
        );
        let diagnostics = engine.analyze(&document, Some(&path));
        let prohibitions = diagnostics
            .iter()
            .filter(|d| d.message.contains("not allowed inside modules"))
            .collect::<Vec<_>>();
        assert_eq!(prohibitions.len(), expected, "{shape}: {diagnostics:?}");
        for (index, (error, diagnostic)) in errors.iter().zip(prohibitions).enumerate() {
            let line = if shape == "two_called_uses" {
                index * 2 + 1
            } else if shape == "two_uses" {
                index + 1
            } else if shape == "two_calls" {
                2
            } else {
                1
            };
            assert_eq!(error["location"]["file"], "main.crn", "{shape}");
            assert_eq!(error["location"]["range"]["start"]["line"], line, "{shape}");
            assert_eq!(
                diagnostic.range.start,
                Position::new((line - 1) as u32, 0),
                "{shape}"
            );
        }
    }
}

#[test]
fn empty_import_human_matches_genuine_main_without_repeated_path() {
    let directory = fixture();
    fs::create_dir(directory._temp.path().join("empty")).unwrap();
    fs::write(
        directory.project.join("main.crn"),
        "let m = use { source = '../empty' }\nm {}\n",
    )
    .unwrap();
    let output = directory.run(&["validate", "."]);
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Error: Module resolution error: Module not found: ../empty\n"
    );
}

#[test]
fn nested_uncalled_prohibitions_belong_to_root_use() {
    let directory = fixture();
    fs::create_dir(directory.project.join("bridge")).unwrap();
    fs::write(
        directory.project.join("bridge/main.crn"),
        "arguments { n: Int = 1 }\nlet inner = use { source = '../../rooted' }\n",
    )
    .unwrap();
    fs::write(
        directory.project.join("main.crn"),
        "let outer = use { source = './bridge' }\n",
    )
    .unwrap();
    let output = directory.run(&["validate", "--json", "."]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["errors"].as_array().unwrap().len(), 5, "{report}");
    let snapshot = Arc::new(loaded_provider_snapshot_from_factories(
        Arc::new(vec![Box::new(ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::memory(),
        ))]),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    ));
    let engine = DiagnosticEngine::new(snapshot.clone(), vec!["mock".into()]);
    let path = directory.project.join("main.crn");
    let document = Document::new_with_path(
        fs::read_to_string(&path).unwrap(),
        path.clone(),
        Arc::new(snapshot.provider_context().as_provider_context().clone()),
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    assert_eq!(diagnostics.len(), 5, "{diagnostics:?}");
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.range.start == Position::new(0, 0)),
        "{diagnostics:?}"
    );
}

#[test]
fn removing_root_evidence_keeps_three_errors_on_every_transport() {
    let directory = fixture();
    for name in ["providers.crn", "backend.crn"] {
        fs::remove_file(directory._temp.path().join("rooted").join(name)).unwrap();
    }
    assert_transports(
        &directory,
        include_str!("../fixtures/import_root_boundary/three.stderr"),
        include_str!("../fixtures/import_root_boundary/three.json"),
        include_str!("../fixtures/import_root_boundary/three.lsp.json"),
    );
}

#[test]
fn rejected_imports_retain_the_json_call_site() {
    let location = json!({"file": "main.crn", "range": {
        "start": {"line": 2, "column": 1},
        "end": {"line": 2, "column": 2}
    }});
    let mut missing_locations = Vec::new();
    for contents in [
        Some("provider mock {}\n"),
        Some("backend local { path = 'state.json' }\n"),
        Some("moved { from = mock.test.resource 'old' to = mock.test.resource 'new' }\n"),
        Some("let prior = upstream_state { source = '../prior' }\n"),
        Some("exports { value = 'published' }\n"),
        None,
    ] {
        let directory = fixture();
        let rejected = directory.project.join("rejected");
        fs::create_dir(&rejected).unwrap();
        if let Some(contents) = contents {
            fs::write(rejected.join("main.crn"), contents).unwrap();
        }
        fs::write(
            directory.project.join("main.crn"),
            "let m = use { source = './rejected' }\nm {}\n",
        )
        .unwrap();
        let output = directory.run(&["validate", "--json", "."]);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stderr.is_empty());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{report}");
        if errors[0]["location"] != location {
            missing_locations.push((contents, report));
        }
    }
    assert!(missing_locations.is_empty(), "{missing_locations:#?}");
}

#[test]
fn uncalled_prohibited_imports_pin_use_locations_on_every_transport() {
    for root_evidence in [true, false] {
        let directory = fixture();
        if !root_evidence {
            for name in ["providers.crn", "backend.crn"] {
                fs::remove_file(directory._temp.path().join("rooted").join(name)).unwrap();
            }
        }
        let use_source = "let m = use { source = \"../rooted\" }\n";
        fs::write(directory.project.join("main.crn"), use_source).unwrap();
        let prefix = if root_evidence { "five" } else { "three" };
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/import_root_boundary");
        let mut expected: Value = serde_json::from_str(
            &fs::read_to_string(fixture_path.join(format!("{prefix}.json"))).unwrap(),
        )
        .unwrap();
        let source_span =
            json!({"start": {"line": 1, "column": 1}, "end": {"line": 1, "column": 37}});
        for error in expected["errors"].as_array_mut().unwrap() {
            error["location"]["range"] = source_span.clone();
        }
        let mut lsp: Value = serde_json::from_str(
            &fs::read_to_string(fixture_path.join(format!("{prefix}.lsp.json"))).unwrap(),
        )
        .unwrap();
        for diagnostic in lsp.as_array_mut().unwrap() {
            diagnostic["range"] =
                json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 36}});
        }
        assert_transports(
            &directory,
            &fs::read_to_string(fixture_path.join(format!("{prefix}.stderr"))).unwrap(),
            &serde_json::to_string(&expected).unwrap(),
            &serde_json::to_string(&lsp).unwrap(),
        );
    }
}

#[test]
fn root_evidence_suppresses_body_checks_and_nonroot_keeps_main_findings() {
    for root_evidence in [true, false] {
        for (body_error, nonroot_prefix) in [
            (
                "let a = 'present'\nlet broken = a ?? { inner = { leaf = 1 } }\n",
                "three-body-expression",
            ),
            (
                "attributes { bad: TotallyMadeUpType = 'one' }\n",
                "three-body-type",
            ),
        ] {
            let directory = fixture();
            if !root_evidence {
                for name in ["providers.crn", "backend.crn"] {
                    fs::remove_file(directory._temp.path().join("rooted").join(name)).unwrap();
                }
            }
            fs::write(directory._temp.path().join("rooted/errors.crn"), body_error).unwrap();
            let prefix = if root_evidence {
                "five"
            } else {
                nonroot_prefix
            };
            let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/import_root_boundary");
            assert_transports(
                &directory,
                &fs::read_to_string(fixture_path.join(format!("{prefix}.stderr"))).unwrap(),
                &fs::read_to_string(fixture_path.join(format!("{prefix}.json"))).unwrap(),
                &fs::read_to_string(fixture_path.join(format!("{prefix}.lsp.json"))).unwrap(),
            );
        }
    }
}

// Ordinary values remain main. The empty-directory case uses decision C's
// typed error while preserving main's null location and absent use anchor.
#[test]
fn ordinary_uncalled_imports_match_main_on_every_transport() {
    type MainImportCase<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str, &'a str);
    let cases: &[MainImportCase<'_>] = &[
        (
            "uncalled-missing",
            &[],
            r###"Error: Module resolution error: IO error: No such file or directory (os error 2)
"###,
            r###"{
  "status": "error",
  "resource_count": 0,
  "resources": [],
  "errors": [
    {
      "message": "IO error: No such file or directory (os error 2)",
      "severity": "error",
      "code": null,
      "location": null
    }
  ]
}
"###,
        ),
        (
            "uncalled-syntax",
            &[(
                r###"module/broken.crn"###,
                r###"let =
"###,
            )],
            r###"Error: Module resolution error: Parse error in ../module/broken.crn: Syntax error:  --> 1:5
Error:   |
Error: 1 | let =
Error:   |     ^---
Error:   |
Error:   = expected discard_pattern or identifier
"###,
            r###"{
  "status": "error",
  "resource_count": 0,
  "resources": [],
  "errors": [
    {
      "message": "Syntax error: expected discard_pattern or identifier",
      "severity": "error",
      "code": null,
      "location": {
        "file": "../module/broken.crn",
        "range": {
          "start": {
            "line": 1,
            "column": 5
          },
          "end": {
            "line": 1,
            "column": 5
          }
        }
      }
    }
  ]
}
"###,
        ),
        (
            "uncalled-empty",
            &[],
            r###"Error: Module resolution error: Module not found: ../module
"###,
            r###"{
  "status": "error",
  "resource_count": 0,
  "resources": [],
  "errors": [
    {
      "message": "No .crn files found in ../module",
      "severity": "error",
      "code": null,
      "location": null
    }
  ]
}
"###,
        ),
        (
            "uncalled-file",
            &[(
                r###"module"###,
                r###"arguments {}
"###,
            )],
            r###"Error: Module resolution error: Module path '../module' must be a directory. Single-file modules are not supported; put the module's .crn files in a directory and import the directory.
"###,
            r###"{
  "status": "error",
  "resource_count": 0,
  "resources": [],
  "errors": [
    {
      "message": "Module path '../module' must be a directory. Single-file modules are not supported; put the module's .crn files in a directory and import the directory.",
      "severity": "error",
      "code": null,
      "location": null
    }
  ]
}
"###,
        ),
        (
            "uncalled-cycle",
            &[
                (
                    r###"module/main.crn"###,
                    r###"let again = use { source = './nested' }
"###,
                ),
                (
                    r###"module/nested/main.crn"###,
                    r###"let again = use { source = '..' }
"###,
                ),
                (
                    r###"module/arguments.crn"###,
                    r###"arguments { n: Int = 1 }
"###,
                ),
                (
                    r###"module/nested/arguments.crn"###,
                    r###"arguments { n: Int = 1 }
"###,
                ),
            ],
            r###"Error: Module resolution error: Circular import detected: ..
"###,
            r###"{
  "status": "error",
  "resource_count": 0,
  "resources": [],
  "errors": [
    {
      "message": "Circular import detected: ..",
      "severity": "error",
      "code": null,
      "location": null
    }
  ]
}
"###,
        ),
    ];
    for (shape, files, human, report) in cases {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("caller");
        fs::create_dir(&project).unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        for (name, source) in *files {
            let path = temp.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        if *shape == "uncalled-empty" {
            fs::create_dir(temp.path().join("module")).unwrap();
        }
        fs::write(
            project.join("main.crn"),
            "let m = use { source = '../module' }\n",
        )
        .unwrap();
        fs::write(project.join("providers.crn"), "provider mock {}\n").unwrap();
        let directory = Directory {
            project,
            home,
            _temp: temp,
        };
        assert_transports(&directory, human, report, "[]");
    }
}

#[test]
fn imported_roots_keep_main_category_order_across_directories() {
    let directory = fixture();
    let root = directory._temp.path();
    fs::create_dir(root.join("second")).unwrap();
    for name in ["main.crn", "providers.crn", "backend.crn", "exports.crn"] {
        fs::copy(
            root.join("rooted").join(name),
            root.join("second").join(name),
        )
        .unwrap();
    }
    for child in ["rooted", "second"] {
        fs::write(
            root.join(child).join("arguments.crn"),
            "arguments { n: Int = 1 }\n",
        )
        .unwrap();
    }
    fs::write(
        directory.project.join("main.crn"),
        "let m = use { source = '../rooted' }\nlet n = use { source = '../second' }\nm {}\nn {}\n",
    )
    .unwrap();
    let single: Value =
        serde_json::from_str(include_str!("../fixtures/import_root_boundary/five.json")).unwrap();
    let expected = single["errors"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|error| {
            let message = error["message"].as_str().unwrap();
            [
                message.to_string(),
                message.replace("../rooted", "../second"),
            ]
        })
        .collect::<Vec<_>>();
    let output = directory.run(&["validate", "--json", "."]);
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| error["message"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    for (args, prefix, suffix) in [
        (vec!["validate", "."], "Error: ", ""),
        (
            vec!["lint", "."],
            "warning: .:1  ",
            "Error: Found 10 lint warning(s).\n",
        ),
    ] {
        let output = directory.run(&args);
        assert_eq!(output.status.code(), Some(1));
        let expected = expected
            .iter()
            .map(|message| {
                let (message, note) = message
                    .split_once('\n')
                    .map_or((message.as_str(), None), |(message, note)| {
                        (message, Some(note))
                    });
                format!(
                    "{prefix}{message}\n{}",
                    note.map_or_else(String::new, |note| format!("  {note}\n"))
                )
            })
            .collect::<String>()
            + suffix;
        assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
    }
}

#[test]
fn readable_prohibitions_beside_unreadable_siblings_keep_call_locations() {
    let directory = fixture();
    fs::write(
        directory._temp.path().join("rooted/z-unreadable.crn"),
        [0xff],
    )
    .unwrap();
    let output = directory.run(&["validate", "--json", "."]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 6, "{report}");
    assert_eq!(
        errors.last().unwrap()["message"],
        "IO error: stream did not contain valid UTF-8"
    );
    for error in errors {
        assert_eq!(error["location"]["range"]["start"]["line"], 2, "{report}");
    }
}

// Expected body findings were measured on genuine main with an otherwise
// equivalent module declaring `arguments { unused: Int = 1 }`.
#[test]
fn empty_interfaces_keep_duplicate_type_and_nested_call_checks() {
    let mut failures = Vec::new();
    for interface in ["", "arguments {}\n", "arguments { unused: Int = 1 }\n"] {
        for (kind, body, expected) in [
            (
                "duplicate",
                "let x = 1\n",
                "duplicate binding 'x': declared in main.crn and sibling.crn",
            ),
            (
                "expression",
                "let a = 'present'\nlet broken = a ?? 1\n",
                "coalesce default type `Int` does not match left type `String`",
            ),
            (
                "nested",
                "let inner = use { source = './child' }\nlet child_call = inner { n = 'wrong' }\n",
                "expected Int, got string 'wrong'",
            ),
        ] {
            let directory = fixture();
            let module = directory._temp.path().join("rooted");
            fs::remove_dir_all(&module).unwrap();
            fs::create_dir_all(module.join("child")).unwrap();
            fs::write(module.join("arguments.crn"), interface).unwrap();
            fs::write(module.join("main.crn"), body).unwrap();
            fs::write(
                module.join("sibling.crn"),
                if kind == "duplicate" {
                    "let x = 2\n"
                } else {
                    "// sibling\n"
                },
            )
            .unwrap();
            fs::write(module.join("child/main.crn"), "arguments { n: Int }\n").unwrap();
            fs::write(
                directory.project.join("main.crn"),
                "let service = use { source = '../rooted' }\nlet result = service {}\n",
            )
            .unwrap();
            for command in ["validate", "lint"] {
                let output = directory.run(&[command, "."]);
                let text = output_text(&output);
                if output.status.code() != Some(1) || !text.contains(expected) {
                    failures.push(format!("{interface:?}/{kind}/{command}: {text}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn rejected_parents_keep_child_prohibitions_and_suppress_child_body_findings() {
    for (root_evidence, child, expected_prohibitions) in [
        (true, "arguments { n: Int = 1 }\nexports { value = 1 }\n", 2),
        (
            true,
            "arguments { n: Int = 1 }\nattributes { bad: ChildType = 1 }\n",
            1,
        ),
        (
            false,
            "arguments { n: Int = 1 }\nexports { value = 1 }\n",
            2,
        ),
    ] {
        let directory = fixture();
        let module = directory._temp.path().join("rooted");
        fs::remove_dir_all(&module).unwrap();
        fs::create_dir_all(module.join("child")).unwrap();
        fs::write(module.join("arguments.crn"), "arguments { n: Int = 1 }\n").unwrap();
        fs::write(
            module.join("main.crn"),
            "let inner = use { source = './child' }\nattributes { bad: ParentType = 1 }\n",
        )
        .unwrap();
        fs::write(
            module.join("boundary.crn"),
            if root_evidence {
                "provider mock {}\n"
            } else {
                "exports { value = 1 }\n"
            },
        )
        .unwrap();
        fs::write(module.join("child/main.crn"), child).unwrap();
        let output = directory.run(&["validate", "--json", "."]);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(
            errors.len(),
            expected_prohibitions + usize::from(!root_evidence),
            "{report}"
        );
        assert!(!report.to_string().contains("ChildType"), "{report}");
        assert_eq!(
            report
                .to_string()
                .matches("not allowed inside modules")
                .count(),
            expected_prohibitions,
            "{report}"
        );
        assert_eq!(
            report.to_string().contains("ParentType"),
            !root_evidence,
            "{report}"
        );
    }
}

#[test]
fn ordinary_import_failures_share_resolver_order_beside_prohibitions() {
    // Main's category pass publishes the prohibition and stops before ordinary
    // resolution, for both missing paths and single files. Empty follows suit.
    for ordinary in ["empty", "file", "missing"] {
        let directory = fixture();
        let root = directory._temp.path();
        fs::write(root.join("rooted/main.crn"), "arguments { n: Int = 1 }\n").unwrap();
        for name in ["providers.crn", "backend.crn"] {
            fs::remove_file(root.join("rooted").join(name)).unwrap();
        }
        // Keep just exports as the parent's prohibition.
        fs::write(root.join("rooted/exports.crn"), "exports { value = 1 }\n").unwrap();
        if ordinary == "empty" {
            fs::create_dir(root.join("ordinary")).unwrap();
        }
        if ordinary == "file" {
            fs::write(root.join("ordinary"), "arguments {}\n").unwrap();
        }
        fs::write(directory.project.join("main.crn"), "let ordinary = use { source = '../ordinary' }\nlet service = use { source = '../rooted' }\nordinary {}\nservice {}\n").unwrap();
        let output = directory.run(&["validate", "--json", "."]);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{ordinary}: {report}");
        assert!(
            errors[0]["message"]
                .as_str()
                .unwrap()
                .contains("exports blocks are not allowed"),
            "{report}"
        );
    }
}
