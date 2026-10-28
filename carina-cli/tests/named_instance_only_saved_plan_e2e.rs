use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn copy_fixture(destination: &Path) {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/named_instance_only_saved_plan");
    for entry in fs::read_dir(source).expect("read fixture directory") {
        let entry = entry.expect("read fixture entry");
        fs::copy(entry.path(), destination.join(entry.file_name())).expect("copy fixture file");
    }
}

fn run(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina")
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn named_instance_only_provider_kind_round_trips_through_saved_plan() {
    let project = tempfile::tempdir().expect("create temporary project");
    copy_fixture(project.path());

    assert_success("carina init", &run(project.path(), &["init", "."]));
    assert_success(
        "carina plan --out",
        &run(project.path(), &["plan", "--out", "saved-plan.json", "."]),
    );
    let saved_plan: serde_json::Value = serde_json::from_slice(
        &fs::read(project.path().join("saved-plan.json")).expect("read saved plan"),
    )
    .expect("decode saved plan");
    assert_eq!(
        saved_plan["provider_artifacts"],
        serde_json::json!({}),
        "a named instance without a kind default must not imply a loaded artifact"
    );
    assert_success(
        "carina apply saved plan",
        &run(
            project.path(),
            &["apply", "saved-plan.json", "--auto-approve", "--lock=false"],
        ),
    );
}
