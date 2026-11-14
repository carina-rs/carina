//! Directory-scoped validation coverage for typed backend configuration.

fn location_fixture(case: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/backend_error_locations")
        .join(case)
}

fn validate_output(directory: &std::path::Path, json: bool) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .arg("validate")
        .arg(directory)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    if json {
        command.arg("--json");
    }
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    output
}

fn bucket_error() -> &'static str {
    "Invalid attribute 'bucket' for backend type 's3': Type mismatch: expected String, got Int"
}

fn key_error() -> &'static str {
    "Invalid attribute 'key' for backend type 's3': Type mismatch: expected String, got Int"
}

#[test]
fn backend_attribute_json_errors_are_individually_located() {
    for (case, messages) in [
        ("multiple", vec![bucket_error(), key_error()]),
        ("single", vec![bucket_error()]),
    ] {
        let directory = location_fixture(case);
        let output = validate_output(&directory, true);
        assert!(output.stderr.is_empty(), "{case}");
        let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let errors: Vec<_> = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                serde_json::json!({
                    "message": message,
                    "severity": "error",
                    "code": null,
                    "location": {
                        "file": directory.join("backend.crn"),
                        "range": {
                            "start": { "line": index + 2, "column": 3 },
                            "end": { "line": index + 2, "column": if index == 0 { 9 } else { 6 } }
                        }
                    }
                })
            })
            .collect();
        assert_eq!(
            actual,
            serde_json::json!({
                "status": "error", "resource_count": 0, "resources": [], "errors": errors
            }),
            "{case}"
        );
    }
}

#[test]
fn backend_attribute_text_errors_are_individually_located() {
    for (case, messages) in [
        ("multiple", vec![bucket_error(), key_error()]),
        ("single", vec![bucket_error()]),
    ] {
        let directory = location_fixture(case);
        let output = validate_output(&directory, false);
        let expected: String = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                format!(
                    "Error: {}:{}:3: {message}\n",
                    directory.join("backend.crn").display(),
                    index + 2
                )
            })
            .collect();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "Validating...\n",
            "{case}"
        );
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            expected,
            "{case}"
        );
    }
}

#[test]
fn unsupported_backend_error_is_located_at_the_block() {
    let directory = tempfile::tempdir().unwrap();
    let backend = directory.path().join("backend.crn");
    std::fs::write(&backend, "\nbackend gcs {}\n").unwrap();
    std::fs::write(directory.path().join("main.crn"), "\n").unwrap();
    let output = validate_output(directory.path(), true);
    let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual["errors"].as_array().unwrap().len(), 1);
    assert_eq!(
        actual["errors"][0]["message"],
        "Unsupported backend type: gcs"
    );
    assert_eq!(
        actual["errors"][0]["location"]["file"],
        backend.to_str().unwrap()
    );
    assert_eq!(
        actual["errors"][0]["location"]["range"]["start"],
        serde_json::json!({"line": 2, "column": 1})
    );
    let output = validate_output(directory.path(), false);
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "Error: {}:2:1: Unsupported backend type: gcs\n",
            backend.display()
        )
    );
}

fn validate_backend(source: &str) -> Vec<String> {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("main.crn"), "\n").unwrap();
    std::fs::write(fixture.path().join("backend.crn"), source).unwrap();
    carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        Vec::new(),
    )
    .into_iter()
    .map(|message| message.replace(fixture.path().to_str().unwrap(), "$DIR"))
    .collect()
}

#[test]
fn validate_reports_unknown_backend_attribute_from_sibling_file() {
    let diagnostics = validate_backend(
        r#"backend s3 {
  bucket = "state"
  key = "state.json"
  typo = true
}
"#,
    );
    assert!(
        diagnostics
            .iter()
            .any(|message| message.contains("Unknown attribute 'typo'") && message.contains("s3")),
        "diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn validate_reports_bad_backend_region_type_from_sibling_file() {
    let diagnostics = validate_backend(
        r#"backend s3 {
  bucket = "state"
  key = "state.json"
  region = true
}
"#,
    );
    assert!(
        diagnostics.iter().any(|message| {
            message.contains("Invalid attribute 'region'") && message.contains("Bool")
        }),
        "diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn validate_reports_all_backend_attribute_errors_in_sorted_order() {
    let diagnostics = validate_backend(
        r#"backend s3 {
  auto_create = "yes"
  encrypt = 1
  region = true
  typo = false
}
"#,
    );
    let backend_lines: Vec<_> = diagnostics
        .iter()
        .filter(|message| {
            message.contains("attribute") || message.contains("Missing required attribute")
        })
        .collect();
    assert_eq!(
        backend_lines,
        vec![
            "$DIR/backend.crn:2:3: Invalid attribute 'auto_create' for backend type 's3': Type mismatch: expected Bool, got String",
            "$DIR/backend.crn:1:1: Missing required attribute: bucket",
            "$DIR/backend.crn:3:3: Invalid attribute 'encrypt' for backend type 's3': Type mismatch: expected Bool, got Int",
            "$DIR/backend.crn:1:1: Missing required attribute: key",
            "$DIR/backend.crn:4:3: Invalid attribute 'region' for backend type 's3': Type mismatch: expected Region, got Bool",
            "$DIR/backend.crn:5:3: Unknown attribute 'typo' for backend type 's3'",
        ],
        "diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn validate_accepts_backend_enum_region_from_sibling_file() {
    let diagnostics = validate_backend(
        r#"backend s3 {
  bucket = "state"
  key = "state.json"
  region = aws.Region.us_west_2
}
"#,
    );
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:#?}");

    let positive_control = validate_backend(
        r#"backend s3 {
  bucket = "state"
  key = "state.json"
  region = aws.Region.us_west_2
  typo = true
}
"#,
    );
    assert!(
        positive_control
            .iter()
            .any(|message| message.contains("Unknown attribute 'typo'")),
        "positive-control diagnostics: {positive_control:#?}"
    );
}

#[test]
fn validate_module_backend_reports_only_the_structural_error() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(
        fixture.path().join("main.crn"),
        "arguments {\n  name: String\n}\n",
    )
    .unwrap();
    std::fs::write(
        fixture.path().join("backend.crn"),
        r#"backend s3 {
  bucket = "state"
  key = "state.json"
  typo = true
}
"#,
    )
    .unwrap();

    let diagnostics = carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        Vec::new(),
    );

    assert_eq!(
        diagnostics,
        vec![
            format!(
                "{}:1:1: arguments blocks are only valid inside module definitions, not in root configurations.\nNote: directory is a root configuration because of the backend block at {}:1.",
                carina_core::paths::DisplayPath::for_diagnostics(fixture.path().join("main.crn")),
                carina_core::paths::DisplayPath::for_diagnostics(
                    fixture.path().join("backend.crn")
                )
            ),
            format!(
                "{}:4:3: Unknown attribute 'typo' for backend type 's3'",
                carina_core::paths::DisplayPath::for_diagnostics(
                    fixture.path().join("backend.crn")
                )
            ),
        ],
        "CLI validate must match the LSP module-directory diagnostic"
    );
}

#[test]
fn validate_module_backend_reports_only_the_structural_error_in_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let module = fixture.path().join("module");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        fixture.path().join("main.crn"),
        "let component = use { source = './module' }\nlet call = component { name = 'test' }\n",
    )
    .unwrap();
    std::fs::write(module.join("main.crn"), "arguments {\n  name: String\n}\n").unwrap();
    std::fs::write(
        module.join("backend.crn"),
        r#"backend s3 {
  bucket = "state"
  key = "state.json"
  typo = true
}
"#,
    )
    .unwrap();

    let diagnostics = carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        Vec::new(),
    )
    .into_iter()
    .map(|message| {
        message
            .strip_prefix("module: ")
            .expect("module error retains its import label")
            .to_string()
    })
    .collect::<Vec<_>>();

    assert_eq!(
        diagnostics,
        vec![format!(
            "backend blocks are not allowed inside modules. Define the backend at the root configuration level.\nNote: directory is a root configuration because of the backend block at {}:1.",
            carina_core::paths::DisplayPath::for_diagnostics(module.join("backend.crn"))
        )],
        "CLI validate must match the LSP module-directory diagnostic"
    );
}

#[test]
fn deferred_backend_directory_text_and_status_match_snapshot() {
    for case in ["resource_reference", "sibling_reference", "sibling_literal"] {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/backend_deferred_config")
            .join(case);
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join("expected.json")).unwrap(),
        )
        .unwrap();
        let run = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
            .args(["validate"])
            .arg(&directory)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .output()
            .unwrap();
        assert_eq!(
            run.status.code(),
            expected["status"].as_i64().map(|n| n as i32),
            "{case}"
        );
        assert_eq!(
            String::from_utf8(run.stdout)
                .unwrap()
                .replace(directory.to_str().unwrap(), "$DIR"),
            expected["stdout"].as_str().unwrap(),
            "{case}"
        );
        assert_eq!(
            String::from_utf8(run.stderr)
                .unwrap()
                .replace(directory.to_str().unwrap(), "$DIR"),
            expected["stderr"].as_str().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn backend_builtin_failure_text_and_json_match_snapshot() {
    for case in ["direct", "sibling"] {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/backend_builtin_failure")
            .join(case);
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join("expected.json")).unwrap(),
        )
        .unwrap();
        for mode in ["text", "json"] {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_carina"));
            command
                .arg("validate")
                .arg(&directory)
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE");
            if mode == "json" {
                command.arg("--json");
            }
            let run = command.output().unwrap();
            assert_eq!(
                run.status.code(),
                expected[mode]["status"].as_i64().map(|n| n as i32),
                "{case}/{mode}"
            );
            for (name, bytes) in [("stdout", run.stdout), ("stderr", run.stderr)] {
                let output = String::from_utf8(bytes)
                    .unwrap()
                    .replace(directory.to_str().unwrap(), "$DIR");
                assert_eq!(
                    output,
                    expected[mode][name].as_str().unwrap(),
                    "{case}/{mode}/{name}"
                );
            }
        }
    }
}

#[test]
fn recursive_configuration_text_and_json_match_snapshot() {
    for case in ["backend_cycle", "provider_cycle"] {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/configuration_classification")
            .join(case);
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join("expected.json")).unwrap(),
        )
        .unwrap();
        for mode in ["text", "json"] {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_carina"));
            command
                .arg("validate")
                .arg(&directory)
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE");
            if mode == "json" {
                command.arg("--json");
            }
            let run = command.output().unwrap();
            assert_eq!(
                run.status.code(),
                expected[mode]["status"].as_i64().map(|n| n as i32),
                "{case}/{mode}"
            );
            for (name, bytes) in [("stdout", run.stdout), ("stderr", run.stderr)] {
                let output = String::from_utf8(bytes)
                    .unwrap()
                    .replace(directory.to_str().unwrap(), "$DIR");
                assert_eq!(
                    output,
                    expected[mode][name].as_str().unwrap(),
                    "{case}/{mode}/{name}"
                );
            }
        }
    }
}
