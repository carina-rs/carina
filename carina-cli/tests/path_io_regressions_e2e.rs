use std::process::Command;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command.env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    command
}

#[cfg(unix)]
#[test]
fn recursive_fmt_skips_unrelated_looping_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let nested = temp.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(
        nested.join("main.crn"),
        "exports{ marker:String=\"hello\"}\n",
    )
    .unwrap();
    for directory in [temp.path(), nested.as_path()] {
        std::os::unix::fs::symlink("notes.txt", directory.join("notes.txt")).unwrap();
    }

    let output = command()
        .current_dir(temp.path())
        .args(["fmt", "--recursive", "."])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Formatted: ./nested/main.crn"),
        "{output:?}"
    );
    let output = command()
        .current_dir(temp.path())
        .args(["fmt", "--recursive", ".", "--check"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn lint_parent_components_keep_root_tag_style_warnings() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("scratch")).unwrap();
    std::fs::create_dir(temp.path().join("infra")).unwrap();
    std::fs::write(temp.path().join("infra/main.crn"),
        "attributes {\n  tags = {\n    Name = \"application\"\n    Environment = \"production\"\n    managed_by = \"carina\"\n  }\n}\n").unwrap();
    assert!(!temp.path().join("infra/scratch").exists());

    let output = command()
        .current_dir(temp.path())
        .args(["lint", "scratch/../infra"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "mixed tag styles must fail lint: {output:?}"
    );
    assert!(
        stderr.contains("Tag key 'managed_by' doesn't match the dominant style (PascalCase)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("warning: scratch/../infra/main.crn:5"),
        "{stderr}"
    );
    assert!(stderr.contains("Found 1 lint warning(s)."), "{stderr}");
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
