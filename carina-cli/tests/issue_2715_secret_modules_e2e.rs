//! Directory module substitutions must preserve protection through persistence.

use std::{fs, path::Path, process::Command};

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn check_directory_module_secrets(enabled: bool) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    copy_directory(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/issue_2715_secret_modules"),
        project,
    );
    if !enabled {
        let main = project.join("main.crn");
        fs::write(
            &main,
            fs::read_to_string(&main)
                .unwrap()
                .replace("enabled = true", "enabled = false"),
        )
        .unwrap();
    }
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env(
                "CARINA_MOCK_STATE_FILE",
                project.join("provider-state.json"),
            )
            .args(args)
            .output()
            .unwrap();
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{args:?}: {rendered}");
        assert!(!rendered.contains("private-"), "{args:?}: {rendered}");
        rendered
    };
    run(&["init", "."]);
    run(&["validate", "."]);
    run(&["plan", "--out", "plan.json", "."]);
    let plan = fs::read_to_string(project.join("plan.json")).unwrap();
    assert!(!plan.contains("private-"), "saved plan contains plaintext");
    run(&["apply", "--auto-approve", "--lock=false", "plan.json"]);
    let state = fs::read_to_string(project.join("carina.state.json")).unwrap();
    assert!(!state.contains("private-"), "state contains plaintext");

    // Verify the protected values were actually substituted and sent, rather
    // than merely dropped or replaced with redaction markers.
    let provider: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(project.join("provider-state.json")).unwrap())
            .unwrap();
    let resources = provider
        .get("states")
        .unwrap_or(&provider)
        .as_object()
        .unwrap();
    let conditional = if enabled {
        "private-module-password-2715"
    } else {
        "private-inactive-2715"
    };
    for (name, expected) in [
        ("direct", "private-module-password-2715"),
        ("local", "private-module-password-2715"),
        ("interpolation", "prefix-private-module-password-2715"),
        ("function", "private-module-password-2715"),
        ("conditional", conditional),
        ("conditional-secret", conditional),
    ] {
        let resource = resources
            .values()
            .find(|value| value["name"] == name)
            .unwrap();
        assert_eq!(resource["comment"], expected, "{name}");
    }
    let direct = resources
        .values()
        .find(|value| value["name"] == "direct")
        .unwrap();
    let reference = resources
        .values()
        .find(|value| value["name"] == "reference")
        .unwrap();
    assert_eq!(reference["comment"], direct["identifier"]);
}

#[test]
fn issue_2715_directory_module_plan_and_state_preserve_secrets_in_then_branch() {
    check_directory_module_secrets(true);
}

#[test]
fn issue_2715_directory_module_plan_and_state_preserve_secrets_in_else_branch() {
    check_directory_module_secrets(false);
}
