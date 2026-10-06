use std::path::Path;
use std::process::{Command, Output};

fn run_fmt(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .arg("fmt")
        .args(args)
        .output()
        .expect("run carina fmt")
}

#[test]
fn diff_only_prints_preview_without_rewriting_file() {
    let project = tempfile::tempdir().unwrap();
    let main = project.path().join("main.crn");
    let original = b"exports   {\n}\n";
    std::fs::write(&main, original).unwrap();

    let output = run_fmt(project.path(), &["--diff"]);

    assert!(
        output.status.success(),
        "carina fmt --diff failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Diff for"), "stdout:\n{stdout}");
    assert!(stdout.contains("-exports   {"), "stdout:\n{stdout}");
    assert!(stdout.contains("+exports {"), "stdout:\n{stdout}");
    assert_eq!(std::fs::read(&main).unwrap(), original);
    assert!(
        stdout.contains("1 file(s) would be reformatted."),
        "stdout:\n{stdout}"
    );
    assert!(!stdout.contains("Formatted:"), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("Formatted 1 file(s)."),
        "stdout:\n{stdout}"
    );
}

#[test]
fn check_with_diff_reports_dirty_file_without_rewriting_it() {
    let project = tempfile::tempdir().unwrap();
    let main = project.path().join("main.crn");
    let original = b"exports   {\n}\n";
    std::fs::write(&main, original).unwrap();

    let output = run_fmt(project.path(), &["--check", "--diff"]);

    assert!(
        !output.status.success(),
        "carina fmt --check --diff unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Diff for"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("The following files need formatting:"),
        "stdout:\n{stdout}"
    );
    assert_eq!(std::fs::read(&main).unwrap(), original);
}

#[test]
fn check_reports_dirty_file_without_rewriting_it() {
    let project = tempfile::tempdir().unwrap();
    let main = project.path().join("main.crn");
    let original = b"exports   {\n}\n";
    std::fs::write(&main, original).unwrap();

    let output = run_fmt(project.path(), &["--check"]);

    assert!(
        !output.status.success(),
        "carina fmt --check unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(std::fs::read(&main).unwrap(), original);
}

#[test]
fn fmt_without_flags_rewrites_file() {
    let project = tempfile::tempdir().unwrap();
    let main = project.path().join("main.crn");
    let original = b"exports   {\n}\n";
    std::fs::write(&main, original).unwrap();

    let output = run_fmt(project.path(), &[]);

    assert!(
        output.status.success(),
        "carina fmt failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(std::fs::read(&main).unwrap(), b"exports {\n}\n");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Formatted:"), "stdout:\n{stdout}");
    assert!(stdout.contains("Formatted 1 file(s)."), "stdout:\n{stdout}");
}
