//! PTY regression test for issue #3150 (follow-up to #3148 / #3149).
//!
//! #3149 added a blank-line separator between the refresh-progress block and
//! the plan terminal section, but verified it only with piped stdout. A real
//! TTY is required to verify the combined stdout/stderr screen after indicatif
//! applies cursor movement, line clearing, and terminal-width padding. These
//! tests parse the PTY bytes with vt100 and assert against that rendered view.

use std::io::Read;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tempfile::TempDir;

const PTY_ROWS: u16 = 40;
const PTY_COLS: u16 = 120;
const SCROLLBACK_ROWS: usize = 1024;

/// Run `carina <args...>` attached to a real PTY and return the rendered
/// terminal history (stdout+stderr share the user-facing screen).
fn run_on_pty(args: &[&str], cwd: &std::path::Path) -> String {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: PTY_ROWS,
            cols: PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_carina"));
    for a in args {
        cmd.arg(a);
    }
    cmd.cwd(cwd);
    // Keep the child deterministic regardless of the runner's environment.
    // `clean()` strips ANSI either way, but removing these avoids any
    // color/log env skewing the line structure the assertions inspect.
    cmd.env_remove("CLICOLOR_FORCE");
    cmd.env_remove("NO_COLOR");
    cmd.env_remove("RUST_LOG");

    let mut child = pty.slave.spawn_command(cmd).expect("spawn under pty");
    // Drop the slave so EOF is delivered to the reader once the child exits.
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().expect("pty reader");
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).expect("read pty output");
    let status = child.wait().expect("child wait");
    assert!(
        status.success(),
        "carina {args:?} failed under pty.\noutput:\n{}",
        String::from_utf8_lossy(&buf)
    );
    render_screen_with_scrollback(&buf)
}

fn render_screen_with_scrollback(raw: &[u8]) -> String {
    let mut parser = vt100::Parser::new(PTY_ROWS, PTY_COLS, SCROLLBACK_ROWS);
    parser.process(raw);
    parser.set_scrollback(usize::MAX);
    let available_scrollback = parser.screen().scrollback();
    let combined_rows = usize::from(PTY_ROWS)
        .checked_add(available_scrollback)
        .and_then(|rows| u16::try_from(rows).ok())
        .expect("PTY test output fits in a u16-sized vt100 viewport");
    parser.set_size(combined_rows, PTY_COLS);
    parser.set_scrollback(available_scrollback);
    parser.screen().contents()
}

/// The line immediately above the first line that, after trimming, starts
/// with `marker`. Panics with the full output if the marker is missing or is
/// the first line.
fn line_above<'a>(text: &'a str, marker: &str) -> &'a str {
    let lines: Vec<&str> = text.lines().collect();
    let idx = lines
        .iter()
        .position(|l| l.trim_start().starts_with(marker))
        .unwrap_or_else(|| panic!("marker {marker:?} not found in:\n{text}"));
    assert!(
        idx > 0,
        "marker {marker:?} is the first line; expected a refresh block above:\n{text}"
    );
    lines[idx - 1]
}

fn init_project(body: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("main.crn"), body).unwrap();
    run_on_pty(&["init", tmp.path().to_str().unwrap()], tmp.path());
    tmp
}

/// With resources to refresh, the TTY path renders `✓` spinner lines; the
/// `Execution Plan:` header must be separated from the last `✓` line by
/// exactly one blank line.
#[test]
fn execution_plan_separated_from_refresh_bars_on_pty() {
    let tmp = init_project(
        "backend local { path = \"carina.state.json\" }\n\
         provider mock {}\n\
         mock.test.resource { name = \"r1\" }\n",
    );

    let out = run_on_pty(&["plan", tmp.path().to_str().unwrap()], tmp.path());

    assert!(
        out.contains("Execution Plan:"),
        "expected Execution Plan section.\n{out}"
    );
    assert!(
        out.contains('✓'),
        "expected at least one ✓ refresh line on the TTY path.\n{out}"
    );
    assert!(
        line_above(&out, "Execution Plan:").trim().is_empty(),
        "expected exactly one blank line between the refresh ✓ block and \
         the Execution Plan header on a TTY (issue #3150).\n{out}"
    );
}

// NOTE on the "No changes." terminal section under a TTY:
//
// There is intentionally no PTY test that asserts the blank line above
// `No changes. Infrastructure is up-to-date.` *with refresh bars present*.
// Reaching that state needs a resource that exists in state so `plan`
// reports no diff. The CLI loads the mock provider as a WASM plugin
// (`carina-provider-mock/src/main.rs`), whose `create` fails
// (`carina apply` against a mock resource exits non-zero with
// `Apply failed. 0 succeeded, 1 failed.` and persists nothing — verified
// at runtime, not inferred from the in-process `MockProvider` in
// `lib.rs`, which is a different type the CLI does not use). So
// apply-then-plan cannot produce "No changes" while still rendering `✓`
// bars, and a no-AWS test has no other way to get there.
//
// This is not a coverage gap, and the justification rests solely on a
// structural argument, not on the mock's limitation: the progress backend close
// is called once, before `print_plan`, and is *section-agnostic* — it
// closes the refresh phase regardless of which terminal section
// `print_plan` then emits. `Execution Plan:` and `No changes.` are
// printed by the same `print_plan` call below the same closed refresh phase,
// so proving the close for one section proves it for both.
// `execution_plan_separated_from_refresh_bars_on_pty` proves the
// refresh-phase separator on the TTY path; the non-PTY
// `plan_refresh_separator_e2e.rs::no_changes_has_blank_line_after_refresh_block`
// proves the no-changes section gets its separator. Together those two
// plus the section-agnostic single close cover the no-changes-with-bars
// case by construction.

/// `--refresh=false` prints no refresh block, so the terminal section must
/// NOT be preceded by a separator blank line (no regression of #3148's
/// "no spurious blank line" guarantee).
#[test]
fn refresh_false_has_no_separator_on_pty() {
    let tmp = init_project(
        "backend local { path = \"carina.state.json\" }\n\
         provider mock {}\n\
         mock.test.resource { name = \"r1\" }\n",
    );

    let out = run_on_pty(
        &["plan", tmp.path().to_str().unwrap(), "--refresh=false"],
        tmp.path(),
    );

    assert!(
        out.contains("Execution Plan:"),
        "expected Execution Plan section.\n{out}"
    );
    assert!(
        !line_above(&out, "Execution Plan:").trim().is_empty(),
        "with --refresh=false there is no refresh block; the line above \
         Execution Plan must not be blank (issue #3148/#3150).\n{out}"
    );
}

/// Count consecutive empty lines directly above the first line that, after
/// trimming, starts with `marker`.
fn blank_lines_above(text: &str, marker: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let idx = lines
        .iter()
        .position(|l| l.trim_start().starts_with(marker))
        .unwrap_or_else(|| panic!("marker {marker:?} not found in:\n{text}"));
    let mut n = 0;
    while idx > n && lines[idx - 1 - n].trim().is_empty() {
        n += 1;
    }
    n
}

/// Remove rendered line breaks for assertions about text whose diagnostic
/// prefix can push it across the PTY width. Structural assertions must keep
/// using the original rendered output so blank lines remain observable.
fn without_rendered_line_breaks(text: &str) -> String {
    text.chars()
        .filter(|character| !matches!(character, '\r' | '\n'))
        .collect()
}

/// Round-4 regression (#3150): a warning printed after refresh outcomes must
/// still be followed by exactly one backend-owned separator before the plan.
#[test]
fn warning_after_bars_yields_single_blank_line_on_pty() {
    // `mock.test.resource` → refresh bars. The static `for` loop with an
    // unused binding `x` emits a top-level parse warning but resolves
    // immediately (no deferred-for child refresh phase).
    let tmp = init_project(
        "backend local { path = \"carina.state.json\" }\n\
         provider mock {}\n\
         mock.test.resource { name = \"r1\" }\n\
         let resources = for x in [\"a\", \"b\"] {\n\
         \u{20}\u{20}mock.test.resource { name = \"static\" }\n\
         }\n",
    );

    let out = run_on_pty(&["plan", tmp.path().to_str().unwrap()], tmp.path());

    assert!(
        without_rendered_line_breaks(&out).contains("for-loop binding 'x' is unused"),
        "expected the unused-for-binding warning.\n{out}"
    );
    assert!(
        out.contains("Execution Plan:"),
        "expected Execution Plan section.\n{out}"
    );
    assert!(out.contains('✓'), "expected ✓ refresh lines.\n{out}");
    assert_eq!(
        blank_lines_above(&out, "Execution Plan:"),
        1,
        "expected exactly ONE blank line between the warning and the \
         Execution Plan header — two would mean the cumulative bar flag \
         double-counted a region the warning already closed (#3150 \
         Round-4).\n{out}"
    );
}

#[test]
fn warning_from_unbound_loop_after_bars_yields_single_blank_line_on_pty() {
    let tmp = init_project(
        "backend local { path = \"carina.state.json\" }\n\
         provider mock {}\n\
         mock.test.resource { name = \"r1\" }\n\
         for (index, value) in [\"a\", \"b\"] {\n\
         \u{20}\u{20}mock.test.resource { name = value }\n\
         }\n",
    );

    let out = run_on_pty(&["plan", tmp.path().to_str().unwrap()], tmp.path());

    assert!(
        without_rendered_line_breaks(&out).contains("for-loop binding 'index' is unused"),
        "expected the unused-for-binding warning from the unbound loop.\n{out}"
    );
    assert!(
        out.contains("Execution Plan:"),
        "expected Execution Plan section.\n{out}"
    );
    assert!(out.contains('✓'), "expected ✓ refresh lines.\n{out}");
    assert_eq!(
        blank_lines_above(&out, "Execution Plan:"),
        1,
        "expected exactly one blank line between the warning and the \
         Execution Plan header for an unbound loop.\n{out}"
    );
}

#[test]
fn distinct_unused_for_bindings_on_different_lines_are_both_printed() {
    let tmp = init_project(
        "backend local { path = \"carina.state.json\" }\n\
         provider mock {}\n\
         let first = for x in [\"a\", \"b\"] {\n\
         \u{20}\u{20}mock.test.resource { name = \"first\" }\n\
         }\n\
         let second = for x in [\"c\", \"d\"] {\n\
         \u{20}\u{20}mock.test.resource { name = \"second\" }\n\
         }\n",
    );

    let out = run_on_pty(&["plan", tmp.path().to_str().unwrap()], tmp.path());
    let flattened = without_rendered_line_breaks(&out);

    assert_eq!(
        flattened.matches("for-loop binding 'x' is unused").count(),
        2,
        "each authored loop must emit its own CLI warning:\n{out}"
    );
}
