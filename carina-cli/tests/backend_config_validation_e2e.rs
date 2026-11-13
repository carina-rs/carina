//! Directory-scoped validation coverage for typed backend configuration.

fn validate_backend(source: &str) -> Vec<String> {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("main.crn"), "\n").unwrap();
    std::fs::write(fixture.path().join("backend.crn"), source).unwrap();
    carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        Vec::new(),
    )
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
        .flat_map(|message| message.lines())
        .filter(|message| {
            message.contains("attribute") || message.contains("Missing required attribute")
        })
        .collect();
    assert_eq!(
        backend_lines,
        vec![
            "Invalid attribute 'auto_create' for backend type 's3': Type mismatch: expected Bool, got String",
            "Missing required attribute: bucket",
            "Invalid attribute 'encrypt' for backend type 's3': Type mismatch: expected Bool, got Int",
            "Missing required attribute: key",
            "Invalid attribute 'region' for backend type 's3': Type mismatch: expected Region, got Bool",
            "Unknown attribute 'typo' for backend type 's3'",
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
            "backend blocks are not allowed inside modules. Define the backend at the root configuration level."
                .to_string()
        ],
        "CLI validate must match the LSP module-directory diagnostic"
    );
}

#[test]
fn review_r1_deferred_backend_directory_text_and_status_match_main() {
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
            String::from_utf8(run.stdout).unwrap(),
            expected["stdout"].as_str().unwrap(),
            "{case}"
        );
        assert_eq!(
            String::from_utf8(run.stderr).unwrap(),
            expected["stderr"].as_str().unwrap(),
            "{case}"
        );
    }
}
