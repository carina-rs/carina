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
            format!(
                "arguments blocks are only valid inside module definitions, not in root configurations.\nNote: directory is a root configuration because of the backend block at {}:1.",
                carina_core::paths::DisplayPath::for_diagnostics(
                    fixture.path().join("backend.crn")
                )
            ),
            "Unknown attribute 'typo' for backend type 's3'".to_string(),
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
        vec![
            "backend blocks are not allowed inside modules. Define the backend at the root configuration level."
                .to_string()
        ],
        "CLI validate must match the LSP module-directory diagnostic"
    );
}
