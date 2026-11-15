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
        for body_error in [
            "let a = 'present'\nlet broken = a ?? { inner = { leaf = 1 } }\n",
            "attributes { bad: TotallyMadeUpType = 'one' }\n",
        ] {
            let directory = fixture();
            if !root_evidence {
                for name in ["providers.crn", "backend.crn"] {
                    fs::remove_file(directory._temp.path().join("rooted").join(name)).unwrap();
                }
            }
            fs::write(directory._temp.path().join("rooted/errors.crn"), body_error).unwrap();
            let prefix = if root_evidence { "five" } else { "three" };
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
