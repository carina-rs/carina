//! Regression coverage for progress output when stdout and stderr have
//! different terminal topologies (#3908).
//!
//! `indicatif::MultiProgress` renders to stderr by default. These tests run
//! the real binary under a PTY-hosted shell and redirect each child stream
//! independently, so they exercise the exact `IsTerminal` combinations that
//! ordinary `Command::output()` tests cannot cover.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const BINDINGS: [&str; 3] = ["alpha", "beta", "gamma"];
const PTY_ROWS: u16 = 200;
const PTY_COLS: u16 = 80;
const SCROLLBACK_ROWS: usize = 4096;
const MANY_RESOURCES: usize = 60;

struct Fixture {
    _temp: tempfile::TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

struct PtyOptions {
    stdout_tty: bool,
    stderr_tty: bool,
    merge_streams: bool,
    color: bool,
    rows: u16,
    scrollback_len: usize,
}

impl Fixture {
    fn new() -> Self {
        Self::with_resources(
            r#"let alpha = mock.test.resource {
  name = "alpha"
}

let beta = mock.test.resource {
  name = "beta"
  directives { depends_on = [alpha] }
}

let gamma = mock.test.resource {
  name = "gamma"
}
"#,
        )
    }

    fn with_independent_resources(count: usize) -> Self {
        let mut resources = String::new();
        for index in 0..count {
            resources.push_str(&format!(
                "let resource_{index:03} = mock.test.resource {{\n  name = \"resource-{index:03}\"\n}}\n\n"
            ));
        }
        Self::with_resources(&resources)
    }

    fn with_deferred_for_resources() -> Self {
        Self::with_deferred_for_resources_target("let generated = ")
    }

    fn with_unbound_deferred_for_resources() -> Self {
        Self::with_deferred_for_resources_target("")
    }

    fn with_deferred_for_resources_target(loop_target: &str) -> Self {
        Self::with_resources(
            r#"let source = mock.test.resource {
  name = "source"
}

LOOP_TARGETfor _, resource_name in (source.tags ?? {}) {
  mock.test.resource {
    name = resource_name
  }
}
"#
            .replacen("LOOP_TARGET", loop_target, 1)
            .as_str(),
        )
    }

    fn with_resources(resources: &str) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let mock_state = project.join("mock-provider-state.json");

        fs::write(
            project.join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .expect("write backend fixture");
        fs::write(project.join("providers.crn"), "provider mock {}\n")
            .expect("write provider fixture");
        fs::write(project.join("resources.crn"), resources).expect("write resource fixture");

        let fixture = Self {
            _temp: temp,
            project,
            mock_state,
        };
        assert_success("carina init", &fixture.run_piped(&["init", "."], &[]));
        fixture
    }

    fn apply(&self) {
        assert_success(
            "carina apply fixture setup",
            &self.run_piped(&["apply", "--auto-approve", "."], &[]),
        );
    }

    fn add_second_source_tag(&self) {
        let mut document = serde_json::from_slice::<serde_json::Value>(
            &fs::read(&self.mock_state).expect("read mock provider state"),
        )
        .expect("parse mock provider state");
        document["states"]["test.resource.source"]["tags"] =
            serde_json::json!({ "one": "one", "two": "two" });
        fs::write(
            &self.mock_state,
            serde_json::to_vec_pretty(&document).expect("serialize mock provider state"),
        )
        .expect("write mock provider state");
    }

    fn run_piped(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("RUST_LOG")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .args(args);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.output().expect("run carina with piped streams")
    }

    fn run_with_streams(
        &self,
        args: &[&str],
        stdout_tty: bool,
        stderr_tty: bool,
        extra_env: &[(&str, &str)],
    ) -> PtyRun {
        self.run_with_streams_and_color(args, stdout_tty, stderr_tty, extra_env, false)
    }

    fn run_with_streams_and_color(
        &self,
        args: &[&str],
        stdout_tty: bool,
        stderr_tty: bool,
        extra_env: &[(&str, &str)],
        color: bool,
    ) -> PtyRun {
        self.run_with_streams_and_options(
            args,
            extra_env,
            PtyOptions {
                stdout_tty,
                stderr_tty,
                merge_streams: false,
                color,
                rows: PTY_ROWS,
                scrollback_len: 0,
            },
        )
    }

    fn run_with_merged_output(&self, args: &[&str], extra_env: &[(&str, &str)]) -> PtyRun {
        self.run_with_streams_and_options(
            args,
            extra_env,
            PtyOptions {
                stdout_tty: false,
                stderr_tty: false,
                merge_streams: true,
                color: false,
                rows: PTY_ROWS,
                scrollback_len: 0,
            },
        )
    }

    fn run_with_streams_and_options(
        &self,
        args: &[&str],
        extra_env: &[(&str, &str)],
        options: PtyOptions,
    ) -> PtyRun {
        let stdout_file = self.project.join("command.stdout");
        let stderr_file = self.project.join("command.stderr");
        let _ = fs::remove_file(&stdout_file);
        let _ = fs::remove_file(&stderr_file);

        let mut shell_command = String::from("exec \"$CARINA_BIN\" \"$@\"");
        if options.merge_streams {
            assert!(!options.stdout_tty && !options.stderr_tty);
            shell_command.push_str(" >\"$CARINA_STDOUT_FILE\" 2>&1");
        } else if !options.stdout_tty {
            shell_command.push_str(" >\"$CARINA_STDOUT_FILE\"");
        }
        if !options.merge_streams && !options.stderr_tty {
            shell_command.push_str(" 2>\"$CARINA_STDERR_FILE\"");
        }

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: options.rows,
                cols: PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");

        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg(shell_command);
        command.arg("carina");
        for arg in args {
            command.arg(arg);
        }
        command.cwd(&self.project);
        command.env("CARINA_BIN", env!("CARGO_BIN_EXE_carina"));
        command.env("CARINA_STDOUT_FILE", &stdout_file);
        command.env("CARINA_STDERR_FILE", &stderr_file);
        command.env("CARINA_MOCK_STATE_FILE", &self.mock_state);
        if options.color {
            command.env_remove("NO_COLOR");
        } else {
            command.env("NO_COLOR", "1");
        }
        command.env_remove("CLICOLOR_FORCE");
        command.env_remove("RUST_LOG");
        for (key, value) in extra_env {
            command.env(key, value);
        }

        let mut child = pty
            .slave
            .spawn_command(command)
            .expect("spawn carina under pty");
        drop(pty.slave);

        let mut reader = pty.master.try_clone_reader().expect("clone pty reader");
        let mut raw_pty = Vec::new();
        reader.read_to_end(&mut raw_pty).expect("read pty output");
        let status = child.wait().expect("wait for carina");

        let screen =
            render_screen_with_scrollback(&raw_pty, options.rows, PTY_COLS, options.scrollback_len);

        PtyRun {
            success: status.success(),
            pty_raw: raw_pty,
            screen,
            stdout: read_if_present(&stdout_file),
            stderr: read_if_present(&stderr_file),
        }
    }
}

fn render_screen_with_scrollback(
    raw: &[u8],
    rows: u16,
    cols: u16,
    scrollback_len: usize,
) -> String {
    let mut parser = vt100::Parser::new(rows, cols, scrollback_len);
    parser.process(raw);

    // vt100 exposes scrollback through a viewport offset. Grow the viewport
    // after parsing so it contains the complete scrollback plus the real
    // terminal screen, then render that combined view in one pass.
    parser.set_scrollback(usize::MAX);
    let available_scrollback = parser.screen().scrollback();
    let combined_rows = usize::from(rows)
        .checked_add(available_scrollback)
        .and_then(|rows| u16::try_from(rows).ok())
        .expect("PTY test output fits in a u16-sized vt100 viewport");
    parser.set_size(combined_rows, cols);
    parser.set_scrollback(available_scrollback);
    parser.screen().contents()
}

struct PtyRun {
    success: bool,
    pty_raw: Vec<u8>,
    screen: String,
    stdout: String,
    stderr: String,
}

fn read_if_present(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).expect("redirected output must be UTF-8"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => panic!("read {}: {error}", path.display()),
    }
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_pty_success(label: &str, output: &PtyRun) {
    assert!(
        output.success,
        "{label} failed\nscreen:\n{}\nraw pty:\n{}\nstdout:\n{}\nstderr:\n{}",
        output.screen,
        String::from_utf8_lossy(&output.pty_raw),
        output.stdout,
        output.stderr,
    );
}

fn assert_destroy_outcomes_once(output: &str) {
    for binding in BINDINGS {
        let refresh = format!("✓ mock.test.resource {binding} took");
        assert_eq!(
            output.matches(&refresh).count(),
            1,
            "expected exactly one refresh outcome {refresh:?}\noutput:\n{output}",
        );
    }

    for (symbol, binding) in [("⊘", "alpha"), ("✗", "beta"), ("✓", "gamma")] {
        let outcome = format!("{symbol} Delete mock.test.resource {binding}");
        assert_eq!(
            output.matches(&outcome).count(),
            1,
            "expected exactly one {outcome:?}\noutput:\n{output}",
        );
    }
}

fn assert_apply_create_and_refresh_outcomes_once(output: &str) {
    for binding in BINDINGS {
        let refresh = format!("✓ mock.test.resource {binding} took");
        assert_eq!(
            output.matches(&refresh).count(),
            1,
            "expected exactly one refresh outcome {refresh:?}\noutput:\n{output}",
        );

        let create = format!("✓ Create mock.test.resource {binding} took");
        assert_eq!(
            output.matches(&create).count(),
            1,
            "expected exactly one create outcome {create:?}\noutput:\n{output}",
        );
    }
}

fn assert_destroy_success_and_refresh_outcomes_once(output: &str) {
    for binding in BINDINGS {
        for outcome in [
            format!("✓ mock.test.resource {binding} took"),
            format!("✓ Delete mock.test.resource {binding} took"),
        ] {
            assert_eq!(
                output.matches(&outcome).count(),
                1,
                "expected exactly one outcome {outcome:?}\noutput:\n{output}",
            );
        }
    }
}

fn assert_failed_apply_outcomes_once(output: &str) {
    for binding in BINDINGS {
        let refresh = format!("✓ mock.test.resource {binding} took");
        assert_eq!(
            output.matches(&refresh).count(),
            1,
            "expected exactly one refresh outcome {refresh:?}\noutput:\n{output}",
        );
    }

    for (symbol, binding) in [("✗", "alpha"), ("⊘", "beta"), ("✓", "gamma")] {
        let create = format!("{symbol} Create mock.test.resource {binding}");
        assert_eq!(
            output.matches(&create).count(),
            1,
            "expected exactly one create outcome {create:?}\noutput:\n{output}",
        );
    }
}

fn assert_many_outcomes_once(output: &str, operation: &str) {
    for index in 0..MANY_RESOURCES {
        let binding = format!("resource_{index:03}");
        for outcome in [
            format!("✓ mock.test.resource {binding} took"),
            format!("✓ {operation} mock.test.resource {binding} took"),
        ] {
            assert_eq!(
                output.matches(&outcome).count(),
                1,
                "expected exactly one outcome {outcome:?}\noutput:\n{output}",
            );
        }
    }

    let counter_suffix = format!("/{MANY_RESOURCES}");
    let counters: Vec<usize> = output
        .split_whitespace()
        .filter_map(|token| {
            token
                .strip_suffix(&counter_suffix)
                .and_then(|completed| completed.parse().ok())
        })
        .collect();
    assert_eq!(
        counters,
        (1..=MANY_RESOURCES).collect::<Vec<_>>(),
        "operation counters must render in completion order\noutput:\n{output}",
    );
}

fn assert_marker_on_own_line(output: &str, marker: &str) {
    assert!(
        output.lines().any(|line| line.trim() == marker),
        "expected {marker:?} on its own rendered line\noutput:\n{output}",
    );
}

fn assert_one_blank_line_above(output: &str, marker: &str) {
    let lines: Vec<_> = output.lines().collect();
    let marker_index = lines
        .iter()
        .position(|line| line.trim() == marker)
        .unwrap_or_else(|| {
            panic!("marker {marker:?} not found on its own line\noutput:\n{output}")
        });
    assert!(marker_index > 0, "marker must follow refresh output");
    assert_eq!(
        lines[marker_index - 1].trim(),
        "",
        "expected a blank line above {marker:?}\noutput:\n{output}",
    );
    assert!(
        marker_index < 2 || !lines[marker_index - 2].trim().is_empty(),
        "expected exactly one blank line above {marker:?}\noutput:\n{output}",
    );
}

fn assert_one_blank_line_between_refresh_and_header(output: &str, header: &str) {
    let lines: Vec<_> = output.lines().collect();
    let refresh_index = lines
        .iter()
        .position(|line| line.trim() == "Refreshing state...")
        .unwrap_or_else(|| panic!("refresh header not found on its own line\noutput:\n{output}"));
    let header_index = lines
        .iter()
        .position(|line| line.trim() == header)
        .unwrap_or_else(|| {
            panic!("header {header:?} not found on its own line\noutput:\n{output}")
        });

    assert_eq!(
        &lines[refresh_index + 1..header_index],
        &[""],
        "expected exactly one blank line between the refresh block and {header:?}\noutput:\n{output}",
    );
}

#[test]
fn destroy_routes_failure_to_redirected_stderr_when_stdout_is_a_tty() {
    let fixture = Fixture::new();
    fixture.apply();

    let output = fixture.run_with_streams(
        &["destroy", "--auto-approve", "."],
        true,
        false,
        &[("CARINA_MOCK_DELETE_FAIL_FOR", "test.resource.beta")],
    );

    assert!(!output.success, "forced delete failure must fail destroy");
    for (symbol, binding) in [("⊘", "alpha"), ("✓", "gamma")] {
        let outcome = format!("{symbol} Delete mock.test.resource {binding}");
        assert_eq!(
            output.screen.matches(&outcome).count(),
            1,
            "non-failure outcome must remain on stdout\nscreen:\n{}\nstderr:\n{}",
            output.screen,
            output.stderr,
        );
    }
    assert!(
        !output.screen.contains("✗ Delete mock.test.resource beta"),
        "failure outcome must not be duplicated on stdout\nscreen:\n{}\nstderr:\n{}",
        output.screen,
        output.stderr,
    );
    assert_eq!(
        output
            .stderr
            .matches("✗ Delete mock.test.resource beta")
            .count(),
        1,
        "redirected stderr must contain one identified failure\nstdout screen:\n{}\nstderr:\n{}",
        output.screen,
        output.stderr,
    );
    assert_eq!(
        output
            .stderr
            .matches("CARINA_MOCK_DELETE_FAIL_FOR requested delete failure for test.resource.beta")
            .count(),
        1,
        "redirected stderr must contain one provider detail\nstdout screen:\n{}\nstderr:\n{}",
        output.screen,
        output.stderr,
    );
}

#[test]
fn apply_writes_create_and_refresh_outcomes_when_only_stdout_is_a_tty() {
    let fixture = Fixture::new();

    let output = fixture.run_with_streams(&["apply", "--auto-approve", "."], true, false, &[]);

    assert_pty_success("carina apply", &output);
    assert_apply_create_and_refresh_outcomes_once(&output.screen);
    assert!(
        output.stderr.is_empty(),
        "plain progress must not be written to stderr: {:?}",
        output.stderr,
    );
}

#[test]
fn destroy_writes_each_outcome_once_when_only_stderr_is_a_tty() {
    let fixture = Fixture::new();
    fixture.apply();

    let output = fixture.run_with_streams(
        &["destroy", "--auto-approve", "."],
        false,
        true,
        &[("CARINA_MOCK_DELETE_FAIL_FOR", "test.resource.beta")],
    );

    assert!(!output.success, "forced delete failure must fail destroy");
    assert_destroy_outcomes_once(&output.screen);
}

#[test]
fn apply_writes_all_create_and_refresh_outcomes_when_both_streams_are_piped() {
    let fixture = Fixture::new();

    let output = fixture.run_with_streams(&["apply", "--auto-approve", "."], false, false, &[]);

    assert_pty_success("carina apply", &output);
    assert_apply_create_and_refresh_outcomes_once(&output.stdout);
    assert!(
        output.stderr.is_empty(),
        "plain progress must not be written to stderr: {:?}",
        output.stderr,
    );
}

#[test]
fn apply_routes_full_failure_to_redirected_stderr() {
    let fixture = Fixture::new();

    let output = fixture.run_with_streams(
        &["apply", "--auto-approve", "."],
        true,
        false,
        &[("CARINA_MOCK_CREATE_FAIL_FOR", "test.resource.beta")],
    );

    assert!(!output.success, "forced create failure must fail apply");
    assert!(
        !output.screen.contains("✗ Create mock.test.resource beta"),
        "failure outcome must not be duplicated on stdout\nscreen:\n{}\nstderr:\n{}",
        output.screen,
        output.stderr,
    );
    assert_eq!(
        output
            .stderr
            .matches("✗ Create mock.test.resource beta")
            .count(),
        1,
        "redirected stderr must contain one identified failure\nstdout screen:\n{}\nstderr:\n{}",
        output.screen,
        output.stderr,
    );
    assert_eq!(
        output
            .stderr
            .matches("CARINA_MOCK_CREATE_FAIL_FOR requested create failure for test.resource.beta")
            .count(),
        1,
        "redirected stderr must retain the provider error detail\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr,
    );
}

#[test]
fn apply_merged_output_contains_each_failure_line_and_detail_once() {
    let fixture = Fixture::new();

    let output = fixture.run_with_merged_output(
        &["apply", "--auto-approve", "."],
        &[("CARINA_MOCK_CREATE_FAIL_FOR", "test.resource.alpha")],
    );

    assert!(!output.success, "forced create failure must fail apply");
    assert_eq!(
        output
            .stdout
            .matches("✗ Create mock.test.resource alpha")
            .count(),
        1,
        "merged output must contain one failure line\noutput:\n{}",
        output.stdout,
    );
    assert_eq!(
        output
            .stdout
            .matches("CARINA_MOCK_CREATE_FAIL_FOR requested create failure for test.resource.alpha")
            .count(),
        1,
        "merged output must contain one provider detail\noutput:\n{}",
        output.stdout,
    );
}

#[test]
fn destroy_merged_output_contains_each_failure_line_and_detail_once() {
    let fixture = Fixture::new();
    fixture.apply();

    let output = fixture.run_with_merged_output(
        &["destroy", "--auto-approve", "."],
        &[("CARINA_MOCK_DELETE_FAIL_FOR", "test.resource.beta")],
    );

    assert!(!output.success, "forced delete failure must fail destroy");
    assert_eq!(
        output
            .stdout
            .matches("✗ Delete mock.test.resource beta")
            .count(),
        1,
        "merged output must contain one failure line\noutput:\n{}",
        output.stdout,
    );
    assert_eq!(
        output
            .stdout
            .matches("CARINA_MOCK_DELETE_FAIL_FOR requested delete failure for test.resource.beta")
            .count(),
        1,
        "merged output must contain one provider detail\noutput:\n{}",
        output.stdout,
    );
}

#[test]
fn destroy_writes_outcomes_and_summary_to_stdout_when_both_streams_are_piped() {
    let fixture = Fixture::new();
    fixture.apply();

    let output = fixture.run_with_streams_and_color(
        &["destroy", "--auto-approve", "."],
        false,
        false,
        &[],
        true,
    );

    assert_pty_success("carina destroy", &output);
    assert_destroy_success_and_refresh_outcomes_once(&output.stdout);
    assert!(
        output
            .stdout
            .contains("Destroy complete! 3 resources destroyed."),
        "destroy summary must share stdout with outcomes:\n{}",
        output.stdout,
    );
    assert!(
        !output.stdout.as_bytes().contains(&b'\x1b'),
        "plain stdout must not contain ANSI escapes: {:?}",
        output.stdout,
    );
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
}

#[test]
fn apply_renders_every_outcome_once_when_resources_exceed_pty_rows() {
    let fixture = Fixture::with_independent_resources(MANY_RESOURCES);

    let output = fixture.run_with_streams_and_options(
        &["apply", "--auto-approve", "."],
        &[],
        PtyOptions {
            stdout_tty: true,
            stderr_tty: true,
            merge_streams: false,
            color: false,
            rows: 24,
            scrollback_len: SCROLLBACK_ROWS,
        },
    );

    assert_pty_success("carina apply", &output);
    assert_many_outcomes_once(&output.screen, "Create");
}

#[test]
fn destroy_renders_every_outcome_once_when_resources_exceed_pty_rows() {
    let fixture = Fixture::with_independent_resources(MANY_RESOURCES);
    fixture.apply();

    let output = fixture.run_with_streams_and_options(
        &["destroy", "--auto-approve", "."],
        &[],
        PtyOptions {
            stdout_tty: true,
            stderr_tty: true,
            merge_streams: false,
            color: false,
            rows: 24,
            scrollback_len: SCROLLBACK_ROWS,
        },
    );

    assert_pty_success("carina destroy", &output);
    assert_many_outcomes_once(&output.screen, "Delete");
}

#[test]
fn destroy_keeps_failure_and_dependent_skip_on_the_rendered_tty_screen() {
    let fixture = Fixture::new();
    fixture.apply();

    let output = fixture.run_with_streams(
        &["destroy", "--auto-approve", "."],
        true,
        true,
        &[("CARINA_MOCK_DELETE_FAIL_FOR", "test.resource.beta")],
    );

    assert!(!output.success, "forced delete failure must fail destroy");
    assert_destroy_outcomes_once(&output.screen);
    assert_eq!(
        output
            .screen
            .matches("CARINA_MOCK_DELETE_FAIL_FOR requested delete failure")
            .count(),
        1,
        "TTY screen must contain one provider detail\nscreen:\n{}",
        output.screen,
    );
    assert_marker_on_own_line(&output.screen, "Destroy Plan:");
}

#[test]
fn apply_keeps_failure_and_dependent_skip_on_the_rendered_tty_screen() {
    let fixture = Fixture::new();

    let output = fixture.run_with_streams(
        &["apply", "--auto-approve", "."],
        true,
        true,
        &[("CARINA_MOCK_CREATE_FAIL_FOR", "test.resource.alpha")],
    );

    assert!(!output.success, "forced create failure must fail apply");
    assert_failed_apply_outcomes_once(&output.screen);
    assert_eq!(
        output
            .screen
            .matches("CARINA_MOCK_CREATE_FAIL_FOR requested create failure")
            .count(),
        1,
        "TTY screen must contain one provider detail\nscreen:\n{}",
        output.screen,
    );
    assert_marker_on_own_line(&output.screen, "Execution Plan:");
}

#[test]
fn apply_starts_execution_plan_on_a_fresh_line_after_refresh() {
    let fixture = Fixture::new();

    let output = fixture.run_with_streams(&["apply", "--auto-approve", "."], true, true, &[]);

    assert_pty_success("carina apply", &output);
    assert_marker_on_own_line(&output.screen, "Execution Plan:");
    assert_one_blank_line_above(&output.screen, "Execution Plan:");
}

#[test]
fn destroy_starts_destroy_plan_on_a_fresh_line_after_refresh() {
    let fixture = Fixture::new();
    fixture.apply();

    let output = fixture.run_with_streams(&["destroy", "--auto-approve", "."], true, true, &[]);

    assert_pty_success("carina destroy", &output);
    assert_marker_on_own_line(&output.screen, "Destroy Plan:");
    assert_one_blank_line_above(&output.screen, "Destroy Plan:");
}

#[test]
fn plan_refresh_separator_is_unchanged_on_the_rendered_screen() {
    let fixture = Fixture::new();

    let output = fixture.run_with_streams(&["plan", "."], true, true, &[]);

    assert_pty_success("carina plan", &output);
    assert_one_blank_line_above(&output.screen, "Execution Plan:");
}

#[test]
fn state_refresh_closes_post_expansion_progress_before_its_summary() {
    let fixture = Fixture::with_deferred_for_resources();
    fixture.apply();
    fixture.add_second_source_tag();

    let output = fixture.run_with_streams(&["state", "refresh", "."], true, true, &[]);

    assert_pty_success("carina state refresh", &output);
    assert!(
        output
            .screen
            .contains("mock.test.resource generated.two took"),
        "fixture must exercise the post-expansion progress backend\nscreen:\n{}",
        output.screen,
    );
    assert_one_blank_line_above(&output.screen, "mock.test.resource \"source\":");
    assert_marker_on_own_line(
        &output.screen,
        "State refreshed: 1 resource updated, 0 resources unchanged.",
    );
}

#[test]
fn state_refresh_closes_unbound_post_expansion_progress_before_its_summary() {
    let fixture = Fixture::with_unbound_deferred_for_resources();
    fixture.apply();
    fixture.add_second_source_tag();

    let output = fixture.run_with_streams(&["state", "refresh", "."], true, true, &[]);

    assert_pty_success("carina state refresh", &output);
    assert!(
        output
            .screen
            .contains("mock.test.resource mock_test_resource_"),
        "fixture must render an anonymous post-expansion child\nscreen:\n{}",
        output.screen,
    );
    assert!(
        !output.screen.contains("_for0"),
        "state-refresh progress must not expose a legacy synthetic binding\nscreen:\n{}",
        output.screen,
    );
    assert_one_blank_line_above(&output.screen, "mock.test.resource \"source\":");
    assert_marker_on_own_line(
        &output.screen,
        "State refreshed: 1 resource updated, 0 resources unchanged.",
    );
}

#[test]
fn plan_json_is_the_only_stdout_document_when_both_streams_are_piped() {
    let fixture = Fixture::new();
    fixture.apply();

    let output = fixture.run_with_streams(&["plan", "--json", "."], false, false, &[]);

    assert_pty_success("carina plan --json", &output);
    serde_json::from_str::<serde_json::Value>(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "plan --json stdout must be exactly one JSON document: {error}\nstdout:\n{}\nstderr:\n{}",
            output.stdout, output.stderr,
        )
    });
}

#[test]
fn plan_json_is_the_only_stdout_document_when_stderr_is_a_tty() {
    let fixture = Fixture::new();
    fixture.apply();

    let output = fixture.run_with_streams(&["plan", "--json", "."], false, true, &[]);

    assert_pty_success("carina plan --json", &output);
    serde_json::from_str::<serde_json::Value>(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "plan --json stdout must be exactly one JSON document: {error}\nstdout:\n{}\nPTY screen:\n{}",
            output.stdout, output.screen,
        )
    });
}

#[test]
fn plan_json_with_out_keeps_stdout_and_saved_plan_machine_readable() {
    let fixture = Fixture::new();
    fixture.apply();

    let output = fixture.run_with_streams(
        &["plan", "--json", "--out", "saved-plan.json", "."],
        false,
        false,
        &[],
    );

    assert_pty_success("carina plan --json --out", &output);
    serde_json::from_str::<serde_json::Value>(&output.stdout)
        .expect("plan --json --out stdout must be exactly one JSON document");
    serde_json::from_slice::<serde_json::Value>(
        &fs::read(fixture.project.join("saved-plan.json")).expect("read saved plan"),
    )
    .expect("--out must contain exactly one JSON document");
    assert!(
        !output.stdout.contains("Plan saved to"),
        "saved-plan status belongs on stderr when stdout is JSON: {}",
        output.stdout,
    );
}

#[test]
fn plan_apply_and_destroy_separate_redirected_stdout_when_stderr_is_a_tty() {
    let fixture = Fixture::new();

    let plan = fixture.run_with_streams(&["plan", "."], false, true, &[]);
    assert_pty_success("carina plan", &plan);
    assert_one_blank_line_between_refresh_and_header(&plan.stdout, "Execution Plan:");

    let apply = fixture.run_with_streams(&["apply", "--auto-approve", "."], false, true, &[]);
    assert_pty_success("carina apply", &apply);
    assert_one_blank_line_between_refresh_and_header(&apply.stdout, "Execution Plan:");

    let destroy = fixture.run_with_streams(&["destroy", "--auto-approve", "."], false, true, &[]);
    assert_pty_success("carina destroy", &destroy);
    assert_one_blank_line_between_refresh_and_header(&destroy.stdout, "Destroy Plan:");
}

#[test]
fn plain_stdout_follows_stdout_color_decision() {
    let fixture = Fixture::new();

    let output = fixture.run_with_streams_and_color(
        &["apply", "--auto-approve", "."],
        true,
        false,
        &[],
        true,
    );

    assert_pty_success("carina apply", &output);
    assert_apply_create_and_refresh_outcomes_once(&output.screen);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
}

#[test]
fn cursor_visibility_follows_the_stderr_progress_terminal() {
    let fixture = Fixture::new();

    let stdout_tty = fixture.run_with_streams(&["plan", "."], true, false, &[]);
    assert_pty_success("carina plan with stdout TTY", &stdout_tty);
    assert!(
        !stdout_tty
            .pty_raw
            .windows(b"\x1b[?25l".len())
            .any(|bytes| bytes == b"\x1b[?25l"),
        "cursor must stay visible when the progress stream (stderr) is not a TTY: {:?}",
        String::from_utf8_lossy(&stdout_tty.pty_raw),
    );

    let stderr_tty = fixture.run_with_streams(&["plan", "."], false, true, &[]);
    assert_pty_success("carina plan with stderr TTY", &stderr_tty);
    assert!(
        stderr_tty
            .pty_raw
            .windows(b"\x1b[?25l".len())
            .any(|bytes| bytes == b"\x1b[?25l"),
        "cursor must be hidden on the same stderr TTY as the spinners: {:?}",
        String::from_utf8_lossy(&stderr_tty.pty_raw),
    );
    assert!(
        !stderr_tty.stdout.as_bytes().contains(&b'\x1b'),
        "redirected stdout must not receive cursor control sequences: {:?}",
        stderr_tty.stdout,
    );
}

#[test]
fn plain_stdout_has_no_ansi_when_both_streams_are_piped() {
    let fixture = Fixture::new();

    let output = fixture.run_with_streams_and_color(
        &["apply", "--auto-approve", "."],
        false,
        false,
        &[],
        true,
    );

    assert_pty_success("carina apply", &output);
    assert_apply_create_and_refresh_outcomes_once(&output.stdout);
    assert!(
        !output.stdout.as_bytes().contains(&b'\x1b'),
        "plain stdout must not contain ANSI escapes: {:?}",
        output.stdout,
    );
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
}
