use std::process::Command;

#[test]
fn conversion_json_keeps_one_typed_owner_and_main_no_as_output() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let validate = || {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(directory.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(["validate", ".", "--json"])
            .output()
            .unwrap();
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        (output.status.code(), json)
    };
    // Main's successful empty inventory omits both diagnostic arrays.
    let (status, json) = validate();
    assert_eq!(status, Some(0));
    assert_eq!(
        json,
        serde_json::json!({"status": "ok", "resource_count": 0, "resources": []})
    );

    let converted = "let converted = 'ok' as String\n";
    std::fs::write(directory.path().join("sibling.crn"), converted).unwrap();
    std::fs::write(directory.path().join("main.crn"), "let first = mock.test.resource { name = converted }\nlet second = mock.test.resource { name = converted }\n").unwrap();
    let (status, json) = validate();
    assert_eq!(status, Some(1));
    assert_eq!(json["status"], "error");
    let errors = json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{json}");
    assert_eq!(errors[0]["code"], "conversion_target");
    assert_eq!(errors[0]["severity"], "error");
    assert_eq!(
        errors[0]["message"],
        "conversion target must be a loaded named scalar type"
    );
    assert!(
        errors[0]["location"]["file"]
            .as_str()
            .unwrap()
            .ends_with("sibling.crn")
    );
    assert_eq!(
        errors[0]["location"]["range"],
        serde_json::json!({
            "start": {"line": 1, "column": 17},
            "end": {"line": 1, "column": converted.trim_end().len() + 1}
        })
    );
}
