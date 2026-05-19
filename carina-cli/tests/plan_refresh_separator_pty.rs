//! PTY regression test for issue #3150 (follow-up to #3148 / #3149).
//!
//! #3149 added a blank-line separator between the refresh-progress block and
//! the plan terminal section, but verified it only with piped stdout. Under a
//! pipe the `✓` spinner lines are redirected to stderr and the plain
//! `Refreshing state...` header (a `println!`, newline-terminated) is the only
//! refresh output on stdout, so a single `\n` separator looked correct.
//!
//! Under a real TTY, indicatif draws the spinner bars to stderr (its default
//! draw target) and leaves the **last** finished bar line *without a
//! terminating newline* (the cursor is parked at the end of
//! `✓ <name> [<elapsed>s]`). Because stdout and stderr are the same terminal
//! device on a TTY, the single separator `\n` written to stdout is then
//! entirely consumed just terminating that open bar line, so zero blank
//! lines appear between the refresh block and `Execution Plan:` /
//! `No changes.`. Only a PTY-backed test can observe this (a piped stdout
//! splits the two fds, hiding it) — which is the whole point of this file.

use std::io::Read;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tempfile::TempDir;

/// Run `carina <args...>` attached to a real PTY and return everything it
/// wrote to the terminal (stdout+stderr are the same stream on a PTY, which
/// is exactly the user-facing reality this test must reproduce).
fn run_on_pty(args: &[&str], cwd: &std::path::Path) -> String {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
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
    String::from_utf8_lossy(&buf).into_owned()
}

/// Strip CSI escape sequences and drop carriage returns so the assertions
/// can reason about newline-delimited line structure.
///
/// This is deliberately narrow, not a terminal emulator. It removes CSI
/// sequences (`ESC [ … final-byte`) — which is all indicatif emits here
/// (SGR colors, `\x1b[2K` erase-line, cursor moves) — and drops `\r`
/// instead of treating it as a column reset. The cleaned text is therefore
/// a *concatenation of redraw frames*, NOT a reconstruction of the final
/// screen. That is sufficient — and only sufficient — for the single thing
/// every assertion here checks: whether the line immediately above a
/// terminal-section marker (`Execution Plan:` / `No changes.`) is an empty
/// `\n`-delimited line. The decisive separators in that region are true
/// `\n`s emitted after the spinner finishes, so frame concatenation on the
/// `✓` line above cannot mask or fabricate that blank line. Do not reuse
/// `clean()` for full-screen assertions. indicatif does not emit OSC
/// sequences on this path, so OSC is intentionally not handled (a lone
/// non-CSI ESC just drops the following byte).
fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                // CSI: ESC [ … <final byte in @-~ range>.
                chars.next();
                for x in chars.by_ref() {
                    if x.is_ascii_alphabetic() || x == '~' {
                        break;
                    }
                }
            } else {
                // Non-CSI ESC (not emitted by indicatif here): drop the
                // ESC and its single following byte.
                chars.next();
            }
            continue;
        }
        if c == '\r' {
            continue;
        }
        out.push(c);
    }
    out
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
         mock.test.resource { name = \"r1\" }\n",
    );

    let out = clean(&run_on_pty(
        &["plan", tmp.path().to_str().unwrap()],
        tmp.path(),
    ));

    assert!(
        out.contains("Execution Plan:"),
        "expected Execution Plan section.\n{out}"
    );
    assert!(
        out.contains('✓'),
        "expected at least one ✓ refresh line on the TTY path.\n{out}"
    );
    assert_eq!(
        line_above(&out, "Execution Plan:"),
        "",
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
// structural argument, not on the mock's limitation: `finish_refresh_bar_region`
// is called once, before `print_plan`, and is *section-agnostic* — it
// closes the indicatif bar region regardless of which terminal section
// `print_plan` then emits. `Execution Plan:` and `No changes.` are
// printed by the same `print_plan` call below the same closed bar region,
// so proving the close for one section proves it for both.
// `execution_plan_separated_from_refresh_bars_on_pty` proves the
// bar-region close on the TTY path; the non-PTY
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
         mock.test.resource { name = \"r1\" }\n",
    );

    let out = clean(&run_on_pty(
        &["plan", tmp.path().to_str().unwrap(), "--refresh=false"],
        tmp.path(),
    ));

    assert!(
        out.contains("Execution Plan:"),
        "expected Execution Plan section.\n{out}"
    );
    assert_ne!(
        line_above(&out, "Execution Plan:"),
        "",
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
    while idx > n && lines[idx - 1 - n].is_empty() {
        n += 1;
    }
    n
}

/// Round-4 regression (#3150): when refresh bars are drawn AND a parse
/// warning is printed AND there is no deferred-for child refresh, the
/// printed `⚠` line already closes indicatif's open bar region. A
/// cumulative "any bar drawn" flag would make `finish_refresh_bar_region`
/// emit a second newline → TWO blank lines before `Execution Plan:`
/// (warning terminator + spurious close + #3149 separator). The running
/// `bar_region_open` flag (reset when `print_warnings` emits) must keep it
/// at exactly ONE blank line.
#[test]
fn warning_after_bars_yields_single_blank_line_on_pty() {
    // `mock.test.resource` → refresh bars. The static `for` loop with an
    // unused binding `x` emits a top-level parse warning but resolves
    // immediately (no deferred-for child refresh phase).
    let tmp = init_project(
        "backend local { path = \"carina.state.json\" }\n\
         mock.test.resource { name = \"r1\" }\n\
         for x in [\"a\", \"b\"] {\n\
         \u{20}\u{20}mock.test.resource { name = \"static\" }\n\
         }\n",
    );

    let out = clean(&run_on_pty(
        &["plan", tmp.path().to_str().unwrap()],
        tmp.path(),
    ));

    assert!(
        out.contains("for-loop binding 'x' is unused"),
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
