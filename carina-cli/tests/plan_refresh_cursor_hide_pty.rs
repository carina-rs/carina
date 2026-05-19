//! PTY regression test for issues #3153 / #3158.
//!
//! `carina` draws the refresh spinner with indicatif, which manages line
//! clearing and cursor *movement* but never emits DECTCEM cursor hide/show
//! (`\x1b[?25l` / `\x1b[?25h`). Without intervention the caret stays
//! visible the whole run; screenshots show a stray cursor and a parked
//! caret reads as "the command is waiting for input".
//!
//! #3153 added a `CursorGuard`; #3158 widened it to the **whole command
//! run** (a single guard built in `main.rs`, held to process exit) because
//! the refresh-only scope made the cursor flicker (visible during provider
//! load → hidden during refresh → visible again before the result). The
//! plan path has no interactive prompt, so the correct user-facing
//! behavior is: hide once near the start, **no `\x1b[?25h` until the very
//! end** (no mid-run reveal), restore exactly once at exit.
//!
//! This test asserts that against a real PTY (the user-facing reality —
//! under a pipe the spinner is suppressed entirely). It deliberately
//! inspects the **raw** PTY bytes, not a CSI-stripped copy, because the
//! sequences under test are themselves CSI.

use std::io::Read;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tempfile::TempDir;

/// Run `carina <args...>` attached to a real PTY and return the raw bytes it
/// wrote to the terminal (stdout+stderr share one stream on a PTY, which is
/// exactly the user-facing reality this test must reproduce).
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
    cmd.env_remove("CLICOLOR_FORCE");
    cmd.env_remove("NO_COLOR");
    cmd.env_remove("RUST_LOG");

    let mut child = pty.slave.spawn_command(cmd).expect("spawn under pty");
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

fn init_project(body: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("main.crn"), body).unwrap();
    run_on_pty(&["init", tmp.path().to_str().unwrap()], tmp.path());
    tmp
}

/// On a TTY, `carina plan` must hide the cursor once near the start and
/// not reveal it again until the very end — no mid-run flicker (#3158).
/// The plan path has no confirmation prompt, so there must be exactly one
/// hide and exactly one show, the show coming after the final plan output.
#[test]
fn plan_keeps_cursor_hidden_for_whole_command_no_flicker_on_pty() {
    let tmp = init_project(
        "backend local { path = \"carina.state.json\" }\n\
         mock.test.resource { name = \"r1\" }\n",
    );

    let raw = run_on_pty(&["plan", tmp.path().to_str().unwrap()], tmp.path());

    // Sanity: the spinner path actually ran (otherwise the test would
    // vacuously "pass" by never reaching the guarded region).
    assert!(
        raw.contains('✓'),
        "expected at least one ✓ refresh line on the TTY path.\n{raw}"
    );

    let hides: Vec<usize> = raw.match_indices("\x1b[?25l").map(|(i, _)| i).collect();
    let shows: Vec<usize> = raw.match_indices("\x1b[?25h").map(|(i, _)| i).collect();

    // Exactly one hide/show pair: command-wide guard hides once at startup,
    // restores once at exit. More than one show before the end would be the
    // #3158 flicker (a reveal in the middle of the run).
    assert_eq!(
        hides.len(),
        1,
        "expected exactly one cursor-hide (command-wide), got {}: {raw:?}",
        hides.len()
    );
    assert_eq!(
        shows.len(),
        1,
        "expected exactly one cursor-show (restore at exit); more than one \
         means the cursor flickered mid-run (#3158), got {}: {raw:?}",
        shows.len()
    );

    let hide = hides[0];
    let show = shows[0];

    // Hide must come before any refresh output (the guard is installed in
    // main.rs before command dispatch, so it precedes `Refreshing state...`).
    let refreshing = raw
        .find("Refreshing state")
        .unwrap_or_else(|| panic!("expected 'Refreshing state' in output.\n{raw:?}"));
    assert!(
        hide < refreshing,
        "cursor-hide (@{hide}) must precede 'Refreshing state' (@{refreshing}) \
         — the whole command runs with the cursor hidden (#3158).\n{raw:?}"
    );

    // The single show must come after the last visible output (the plan /
    // No-changes terminal section), i.e. it is the exit restore, not a
    // mid-run reveal.
    let last_content = raw
        .rfind("No changes")
        .or_else(|| raw.rfind("Execution Plan"))
        .or_else(|| raw.rfind('✓'))
        .unwrap_or_else(|| panic!("expected a plan terminal section.\n{raw:?}"));
    assert!(
        show > last_content,
        "the only cursor-show (@{show}) must come AFTER the final plan \
         output (@{last_content}); a show before it is the #3158 flicker.\n{raw:?}"
    );

    assert_cursor_not_left_hidden(&raw);
}

/// The terminal must never be left with a hidden cursor: after the *last*
/// hide there must be a show. This is the invariant the SIGINT/panic
/// restore net protects (#3153) — and it must hold on the normal path too.
fn assert_cursor_not_left_hidden(raw: &str) {
    let last_hide = raw.rfind("\x1b[?25l");
    let last_show = raw.rfind("\x1b[?25h");
    match (last_hide, last_show) {
        (None, _) => {} // cursor was never hidden (e.g. non-spinner path)
        (Some(h), Some(s)) => assert!(
            s > h,
            "terminal left with a HIDDEN cursor: a cursor-hide (@{h}) is \
             not followed by any cursor-show (last show @{s}).\n{raw:?}"
        ),
        (Some(h), None) => panic!(
            "terminal left with a HIDDEN cursor: cursor-hide @{h} and no \
             cursor-show at all.\n{raw:?}"
        ),
    }
}

/// Like [`run_on_pty`] but does NOT assert success — for the error-path
/// test, where `carina` is expected to exit non-zero via
/// `main::handle_app_error` → `std::process::exit`.
fn run_on_pty_allow_failure(args: &[&str], cwd: &std::path::Path) -> String {
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
    cmd.env_remove("CLICOLOR_FORCE");
    cmd.env_remove("NO_COLOR");
    cmd.env_remove("RUST_LOG");

    let mut child = pty.slave.spawn_command(cmd).expect("spawn under pty");
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().expect("pty reader");
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).expect("read pty output");
    let _ = child.wait();
    String::from_utf8_lossy(&buf).into_owned()
}

/// On the error path, `carina` exits via `process::exit` (which runs no
/// destructors) — neither the command-wide `CursorGuard`'s `Drop` nor the
/// signal/panic net fires. `main::handle_app_error` must restore the cursor
/// explicitly, or the terminal is left with a hidden cursor on every
/// command error (#3158: command-wide hiding makes this reachable from the
/// very start of the run).
#[test]
fn plan_error_path_does_not_leave_cursor_hidden_on_pty() {
    let tmp = TempDir::new().unwrap();
    // A parse error: forces validation/load failure → `handle_app_error`
    // → `std::process::exit`, bypassing Drop.
    std::fs::write(tmp.path().join("main.crn"), "this is not valid carina {{\n").unwrap();

    let raw = run_on_pty_allow_failure(&["plan", tmp.path().to_str().unwrap()], tmp.path());

    // Non-vacuous by construction: the command-wide guard arms right after
    // Cli::parse() (before dispatch), and this parse error is detected
    // *during* dispatch, so on a PTY the cursor is always hidden before the
    // error — proving the error path genuinely entered the guarded state.
    assert!(
        raw.contains("\x1b[?25l"),
        "expected the cursor to have been hidden before the error \
         (command-wide guard arms before dispatch).\n{raw:?}"
    );
    // The decisive invariant: the error path (process::exit, skips Drop)
    // must still restore the cursor — `handle_app_error` calls
    // `restore_cursor()`.
    assert_cursor_not_left_hidden(&raw);
}

// NOTE on the SIGINT/SIGTERM restore path:
//
// There is intentionally no PTY test that sends Ctrl+C mid-spinner. The
// mock provider has no delay hook, so the refresh completes in
// milliseconds: a `0x03` written right after spawn almost always lands
// *before* the cursor is ever hidden, so the run never enters the guarded
// state and the test would pass vacuously — identically with or without
// `install_restore_handlers`. Forcing a deterministic "spinner has started,
// now signal" handshake would require a test-only delay seam in the
// provider read path, which does not exist and is out of scope for #3153.
//
// The signal-handler write path is instead covered deterministically by a
// unit test that drives `restore_cursor_once(true)` directly (the
// `async_signal_safe == true` `libc::write` branch the SIGINT/SIGTERM
// handler runs) in `carina-cli/src/cursor.rs`, alongside the claim-once
// coordination test. A real end-to-end SIGINT regression test with a
// readiness handshake is tracked as a follow-up issue (#3157).
