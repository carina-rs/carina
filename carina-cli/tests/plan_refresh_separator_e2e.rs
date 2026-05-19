//! End-to-end test for issue #3148.
//!
//! `carina plan` prints the state-refresh progress block (`Refreshing
//! state...` header + `✓ <name> [<elapsed>s]` lines) immediately above the
//! plan's terminal section. Without a separator the last refresh line and the
//! terminal section (`Execution Plan:` or `No changes. Infrastructure is
//! up-to-date.`) sit on adjacent rows and read as a visual run-on.
//!
//! These tests run the real `carina plan` binary against the bundled mock
//! provider (no AWS needed) and assert the blank-line separator on actual
//! stdout — the unit tests in `display::tests` pin the pure helper contract,
//! but only the real binary proves the refresh block and the plan terminal
//! section are actually adjacent on the same stream.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

/// Pin the child to plain-text output so the marker assertions are
/// environment-independent. Without this, an inherited `CLICOLOR_FORCE=1`
/// makes `carina plan` emit ANSI escapes and the `starts_with` matching
/// (which compares against uncolored `Execution Plan:` / `No changes.`)
/// fails for reasons unrelated to the separator under test.
fn plain_text_command(bin: &str) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    cmd
}

fn init_project(dir: &std::path::Path, body: &str) {
    fs::write(dir.join("main.crn"), body).unwrap();
    let status = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .args(["init", dir.to_str().unwrap()])
        .status()
        .expect("failed to execute carina init");
    assert!(status.success(), "carina init failed");
}

fn plan_stdout(dir: &std::path::Path, extra_args: &[&str]) -> String {
    let mut args = vec!["plan", dir.to_str().unwrap()];
    args.extend_from_slice(extra_args);
    let output = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .args(&args)
        .output()
        .expect("failed to execute carina plan");
    assert!(
        output.status.success(),
        "carina plan failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("stdout is not UTF-8")
}

/// Return the line immediately preceding the first line that starts with
/// `marker` (after trimming the leading indentation the plan terminal
/// sections don't use, so this matches `Execution Plan:` / `No changes.`).
fn line_before<'a>(stdout: &'a str, marker: &str) -> &'a str {
    let lines: Vec<&str> = stdout.lines().collect();
    let idx = lines
        .iter()
        .position(|l| l.starts_with(marker))
        .unwrap_or_else(|| panic!("marker {marker:?} not found in stdout:\n{stdout}"));
    assert!(
        idx > 0,
        "marker {marker:?} is the first line; expected a refresh block above it:\n{stdout}"
    );
    lines[idx - 1]
}

/// With refresh on (the default), the no-changes terminal section is
/// separated from the refresh block by exactly one blank line.
#[test]
fn no_changes_has_blank_line_after_refresh_block() {
    let tmp = TempDir::new().unwrap();
    init_project(
        tmp.path(),
        r#"backend local { path = "carina.state.json" }
"#,
    );

    let stdout = plan_stdout(tmp.path(), &[]);

    assert!(
        stdout.contains("No changes. Infrastructure is up-to-date."),
        "expected the no-changes terminal section.\nstdout:\n{stdout}"
    );
    assert_eq!(
        line_before(&stdout, "No changes."),
        "",
        "expected a blank line directly above the no-changes summary \
         (issue #3148).\nstdout:\n{stdout}"
    );
}

/// With refresh on (the default), the `Execution Plan:` header is separated
/// from the refresh block by exactly one blank line.
#[test]
fn execution_plan_has_blank_line_after_refresh_block() {
    let tmp = TempDir::new().unwrap();
    init_project(
        tmp.path(),
        r#"backend local { path = "carina.state.json" }
mock.test.resource { name = "r1" }
"#,
    );

    let stdout = plan_stdout(tmp.path(), &[]);

    assert!(
        stdout.contains("Execution Plan:"),
        "expected the Execution Plan terminal section.\nstdout:\n{stdout}"
    );
    assert_eq!(
        line_before(&stdout, "Execution Plan:"),
        "",
        "expected a blank line directly above the Execution Plan header \
         (issue #3148).\nstdout:\n{stdout}"
    );
}

/// With `--refresh=false` there is no refresh block above the plan, so no
/// separator blank line is introduced — the terminal section must not be
/// preceded by a spurious empty line.
#[test]
fn refresh_false_has_no_separator_blank_line() {
    let tmp = TempDir::new().unwrap();
    init_project(
        tmp.path(),
        r#"backend local { path = "carina.state.json" }
mock.test.resource { name = "r1" }
"#,
    );

    let stdout = plan_stdout(tmp.path(), &["--refresh=false"]);

    assert!(
        stdout.contains("Execution Plan:"),
        "expected the Execution Plan terminal section.\nstdout:\n{stdout}"
    );
    assert_ne!(
        line_before(&stdout, "Execution Plan:"),
        "",
        "with --refresh=false there is no refresh block, so the line above \
         the Execution Plan header must not be blank (issue #3148).\n\
         stdout:\n{stdout}"
    );
}
