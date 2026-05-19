//! PTY regression test for issue #3153.
//!
//! `carina plan` (and `apply` / `destroy`) draws the refresh spinner with
//! indicatif, which manages line clearing and cursor *movement* but never
//! emits DECTCEM cursor hide/show (`\x1b[?25l` / `\x1b[?25h`). So while the
//! spinner runs the terminal's text caret stays visible, parked on the
//! active spinner row; screenshots of `carina plan` show a stray cursor
//! and it reads as "the command is waiting for input".
//!
//! The fix wraps the refresh phase in a `CursorGuard` (emits `\x1b[?25l`
//! on entry, `\x1b[?25h` on the normal / `?`-error drop) backed by a
//! SIGINT/SIGTERM + panic restore net for the non-unwinding exits (see
//! `carina-cli/src/cursor.rs`; the signal/panic coordination is unit-tested
//! there). This test asserts the happy path against a real PTY (the
//! user-facing reality — under a pipe the spinner is suppressed entirely):
//! both sequences present, in stream order. It deliberately inspects the
//! **raw** PTY bytes, not a CSI-stripped copy, because the sequences under
//! test are themselves CSI.

use std::io::{Read, Write};

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

/// On a TTY, `carina plan` with resources to refresh must hide the cursor
/// before the spinner runs and restore it after, in that order (#3153).
#[test]
fn plan_hides_and_restores_cursor_around_refresh_on_pty() {
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

    let hide = raw.find("\x1b[?25l").unwrap_or_else(|| {
        panic!("expected DECTCEM cursor-hide (ESC[?25l) during refresh.\n{raw:?}")
    });
    let show = raw.rfind("\x1b[?25h").unwrap_or_else(|| {
        panic!("expected DECTCEM cursor-show (ESC[?25h) after refresh.\n{raw:?}")
    });

    assert!(
        hide < show,
        "cursor must be hidden (ESC[?25l) before it is restored (ESC[?25h); \
         got hide@{hide} show@{show}.\n{raw:?}"
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

/// Ctrl+C during `carina plan` must still leave the cursor restored. The
/// RAII guard's `Drop` cannot help here — `plan` holds no state lock so it
/// is not wrapped in `run_with_ctrl_c` (#3111), and the default SIGINT
/// disposition terminates the process without unwinding. The
/// SIGINT-handler restore net (`cursor::install_restore_handlers`) is what
/// makes this pass.
///
/// This is timing-tolerant by construction: it asserts only the
/// end-state invariant "cursor not left hidden", which holds whether the
/// Ctrl+C landed mid-spinner (signal handler restores) or after the guard
/// already restored. It does not assert *which* path restored it, so there
/// is no race to lose.
#[test]
fn plan_ctrl_c_does_not_leave_cursor_hidden_on_pty() {
    let tmp = init_project(
        "backend local { path = \"carina.state.json\" }\n\
         mock.test.resource { name = \"r1\" }\n",
    );

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_carina"));
    cmd.arg("plan");
    cmd.arg(tmp.path().to_str().unwrap());
    cmd.cwd(tmp.path());
    cmd.env_remove("CLICOLOR_FORCE");
    cmd.env_remove("NO_COLOR");
    cmd.env_remove("RUST_LOG");

    let mut child = pty.slave.spawn_command(cmd).expect("spawn under pty");
    drop(pty.slave);

    // Send the terminal INTR character (Ctrl+C = 0x03). The PTY line
    // discipline translates it into SIGINT for the foreground process
    // group, exactly as a real interactive Ctrl+C would — no extra
    // dependency, no direct kill(2).
    let mut writer = pty.master.take_writer().expect("pty writer");
    let _ = writer.write_all(&[0x03]);
    let _ = writer.flush();

    let mut reader = pty.master.try_clone_reader().expect("pty reader");
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).expect("read pty output");
    let _ = child.wait();

    let raw = String::from_utf8_lossy(&buf);
    // Whether or not Ctrl+C landed before the spinner started, the decisive
    // property is the same: the run must not end with a hidden cursor.
    assert_cursor_not_left_hidden(&raw);
}
