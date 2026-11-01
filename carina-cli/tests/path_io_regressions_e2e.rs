use std::process::Command;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command.env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    command
}

#[test]
fn fmt_preserves_relative_argument_spelling_in_output_and_diffs() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("main.crn"),
        "exports{ marker:String=\"hello\"}\n",
    )
    .unwrap();
    let output = command()
        .current_dir(temp.path())
        .args(["fmt", ".", "--diff"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Diff for ./main.crn:"),
        "{output:?}"
    );
    let output = command()
        .current_dir(temp.path())
        .args(["fmt", "."])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Formatted: ./main.crn"),
        "{output:?}"
    );
}

#[cfg(unix)]
#[test]
fn fmt_recursive_directory_symlink_loop_terminates_and_validate_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("dir")).unwrap();
    std::fs::write(
        temp.path().join("dir/main.crn"),
        "exports { marker: String = \"hello\" }\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("..", temp.path().join("dir/loop")).unwrap();
    let output = command()
        .args(["fmt", "--recursive"])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = command()
        .arg("validate")
        .arg(temp.path().join("dir"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}
