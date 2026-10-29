use std::process::{Command, Output};

use tempfile::TempDir;

const TYPE_ANNOTATION_REQUIRED: &str = "export 'x': type annotation required";

fn fixture() -> TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        fixture.path().join("main.crn"),
        "exports { x = [1, 'a'] }\n",
    )
    .expect("write fixture");
    fixture
}

fn run(fixture: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(args)
        .current_dir(fixture.path())
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .expect("run carina")
}

fn assert_type_annotation_required(command: &str, output: &Output) -> String {
    assert!(
        !output.status.success(),
        "carina {command} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .find_map(|line| {
            line.find(TYPE_ANNOTATION_REQUIRED)
                .map(|start| line[start..].to_owned())
        })
        .unwrap_or_else(|| {
            panic!(
                "carina {command} did not report `{TYPE_ANNOTATION_REQUIRED}`\nstderr:\n{stderr}"
            )
        })
}

#[test]
fn validate_rejects_export_that_requires_type_annotation() {
    let fixture = fixture();
    let path = fixture.path().to_str().expect("UTF-8 fixture path");

    assert_type_annotation_required("validate", &run(&fixture, &["validate", path]));
}

#[test]
fn plan_rejects_export_that_requires_type_annotation() {
    let fixture = fixture();
    let path = fixture.path().to_str().expect("UTF-8 fixture path");

    let expected = assert_type_annotation_required("validate", &run(&fixture, &["validate", path]));
    let actual =
        assert_type_annotation_required("plan", &run(&fixture, &["plan", path, "--refresh=false"]));
    assert_eq!(actual, expected);
}

#[test]
fn apply_rejects_export_that_requires_type_annotation() {
    let fixture = fixture();
    let path = fixture.path().to_str().expect("UTF-8 fixture path");

    let expected = assert_type_annotation_required("validate", &run(&fixture, &["validate", path]));
    let actual = assert_type_annotation_required(
        "apply",
        &run(&fixture, &["apply", path, "--auto-approve", "--lock=false"]),
    );
    assert_eq!(actual, expected);
}
