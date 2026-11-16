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
fn round31_nonroot_prohibition_keeps_body_and_call_findings() {
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
fn round31_readable_exports_precede_unreadable_sibling() {
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
        "Error: ../rooted: exports blocks are not allowed inside modules. Define exports at the root configuration level.\n"
    );
}

#[test]
fn round31_nested_uncalled_prohibitions_belong_to_root_use() {
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
fn prohibited_imports_precede_body_expression_and_type_errors_on_every_transport() {
    for root_evidence in [true, false] {
        for (body_error, nonroot_prefix) in [
            (
                "let a = 'present'\nlet broken = a ?? { inner = { leaf = 1 } }\n",
                "three",
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
                r###"target/broken.crn"###,
                r###"let =
"###,
            )],
            r###"Error: Module resolution error: Parse error in ../target/broken.crn: Syntax error:  --> 1:5
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
        "file": "../target/broken.crn",
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
            r###"Error: ../target: No .crn files found in ../target
"###,
            r###"{
  "status": "error",
  "resource_count": 0,
  "resources": [],
  "errors": [
    {
      "message": "../target: No .crn files found in ../target",
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
                r###"target"###,
                r###"arguments {}
"###,
            )],
            r###"Error: Module resolution error: Module path '../target' must be a directory. Single-file modules are not supported; put the module's .crn files in a directory and import the directory.
"###,
            r###"{
  "status": "error",
  "resource_count": 0,
  "resources": [],
  "errors": [
    {
      "message": "Module path '../target' must be a directory. Single-file modules are not supported; put the module's .crn files in a directory and import the directory.",
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
                    r###"target/main.crn"###,
                    r###"let again = use { source = './nested' }
"###,
                ),
                (
                    r###"target/nested/main.crn"###,
                    r###"let again = use { source = '..' }
"###,
                ),
                (
                    r###"target/arguments.crn"###,
                    r###"arguments { n: Int = 1 }
"###,
                ),
                (
                    r###"target/nested/arguments.crn"###,
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
            fs::create_dir(temp.path().join("target")).unwrap();
        }
        fs::write(
            project.join("main.crn"),
            "let m = use { source = '../target' }\n",
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
