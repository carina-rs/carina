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
}

#[test]
fn builtin_mock_rejects_wasm_only_configuration_controls() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/builtin_mock_unknown_controls");
    std::fs::copy(fixture.join("main.crn"), directory.path().join("main.crn")).unwrap();
    for (key, value) in [("validate_config_trap", "true"), ("reference_table", "'x'")] {
        std::fs::write(
            directory.path().join("providers.crn"),
            format!("provider mock {{ {key} = {value} }}\n"),
        )
        .unwrap();
        for json in [false, true] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
            command.arg("validate").arg(directory.path());
            if json {
                command.arg("--json");
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(1), "{key}: {output:?}");
            let expected = format!("provider mock: {key}: Unknown attribute '{key}'");
            if json {
                let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(report["errors"][0]["message"], expected);
                assert_eq!(report["errors"].as_array().unwrap().len(), 1);
            } else {
                assert_eq!(
                    String::from_utf8(output.stderr).unwrap(),
                    format!("Error: {expected}\n")
                );
            }
        }
    }
}
