#[test]
fn github_admission_json_matches_shared_diagnostics() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/github_backend_admission");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", "--json"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors = json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 3, "{json}");
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("expected.json")).unwrap()).unwrap();
    for (error, expected) in errors.iter().zip(expected.as_array().unwrap()) {
        assert_eq!(error["message"], expected["message"]);
    }
    for (error, field, line) in [
        (&errors[0], "branch", 3),
        (&errors[1], "path", 4),
        (&errors[2], "repository", 2),
    ] {
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains(&format!("Invalid attribute '{field}'"))
        );
        assert_eq!(
            error["location"]["file"],
            root.join("backend.crn").to_str().unwrap()
        );
        assert_eq!(error["location"]["range"]["start"]["line"], line);
    }
}

#[test]
fn github_admission_validates_concrete_expressions_without_credentials_or_clients() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("main.crn"),
        "let state_branch = 'states/prod'\n",
    )
    .unwrap();
    std::fs::write(root.path().join("providers.crn"), "// no providers\n").unwrap();
    std::fs::write(root.path().join("backend.crn"),"backend github { repository = env('STATE_REPOSITORY') branch = state_branch path = 'prod.json' }\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", "--json"])
        .arg(root.path())
        .env("STATE_REPOSITORY", "owner/repo")
        .env("GITHUB_TOKEN", "")
        .env("PATH", "/no-credential-helper")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["resource_count"], 0);
}

#[test]
fn github_saved_plan_backend_round_trip_contains_only_destination_settings() {
    let raw = carina_core::parser::parse_file(
        "backend github { repository = 'owner/repo' branch = 'states/prod' path = 'prod.json' }",
        std::path::Path::new("backend.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap()
    .backend
    .unwrap();
    let config = carina_state::BackendConfig::try_from(&raw).unwrap();
    let saved =
        carina_cli::commands::plan::SavedBackendConfig::from(Some(config.to_parser_config()));
    let wire = serde_json::to_vec(&saved).unwrap();
    let restored: carina_cli::commands::plan::SavedBackendConfig =
        serde_json::from_slice(&wire).unwrap();
    let lock = restored.selection_source().backend_lock().unwrap();
    assert_eq!(lock, carina_state::BackendLock::from_config(&config));
    let value: serde_json::Value = serde_json::from_slice(&wire).unwrap();
    assert_eq!(value["backend_type"], "github");
    assert_eq!(value["attributes"].as_object().unwrap().len(), 3);
}

#[test]
fn github_offline_migration_check_rejects_remote_before_constructing_any_client() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
        .args([
            "state",
            "migrate",
            "--check",
            ".",
            "--state-url",
            "github://owner/repo/prod.json?branch=states%2Fprod",
        ])
        .env("GITHUB_TOKEN", "")
        .env("PATH", "/no-credential-helper")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8(output.stderr).unwrap();
    assert!(message.contains("local state file"), "{message}");
    assert!(!message.contains("credentials"), "{message}");
}
