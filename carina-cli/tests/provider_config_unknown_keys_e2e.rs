use std::process::Command;

#[test]
fn provider_config_unknown_keys_are_errors_in_directory_validate_text_and_json() {
    let directory = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/provider_config_unknown_keys");
    for name in ["main.crn", "providers.crn", "backend.crn"] {
        std::fs::copy(source.join(name), directory.path().join(name)).unwrap();
    }
    let text = Command::new(env!("CARGO_BIN_EXE_carina"))
        .arg("validate")
        .arg(directory.path())
        .output()
        .unwrap();
    let json = Command::new(env!("CARGO_BIN_EXE_carina"))
        .arg("validate")
        .arg(directory.path())
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(text.status.code(), Some(1), "{text:?}");
    assert_eq!(json.status.code(), Some(1), "{json:?}");
    let errors: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let errors = errors["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 3, "{errors:?}");
    let stderr = String::from_utf8(text.stderr).unwrap();
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(source.join("expected.json")).unwrap()).unwrap();
    let messages: Vec<_> = errors
        .iter()
        .map(|error| error["message"].as_str().unwrap())
        .collect();
    assert_eq!(serde_json::json!(messages), expected["errors"]);
    for message in messages {
        assert!(stderr.contains(message), "{stderr}");
    }
    assert!(stderr.contains("did you mean 'validate_delay_millis'?"));
}
