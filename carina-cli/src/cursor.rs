//! Terminal cursor visibility control for the refresh-progress phase (#3153).
//!
//! `indicatif` manages line clearing and cursor *movement* but never emits
//! DECTCEM hide/show. So while the refresh spinner runs the terminal's
//! caret stays visible, parked on the active spinner row; screenshots of
//! `carina plan` show a stray cursor and it reads as "the command is
//! waiting for input".
//!
//! Two cooperating mechanisms restore the cursor, coordinated by one
//! process-global [`CURSOR_HIDDEN`] flag so exactly one of them emits the
//! restore sequence:
//!
//! 1. [`CursorGuard`] — an RAII guard covering the normal exit and `?`
//!    error-unwind paths (stack unwinding runs its `Drop`).
//! 2. [`install_restore_handlers`] — a SIGINT/SIGTERM signal handler plus a
//!    panic hook that cover the abnormal exits unwinding can't: `plan` has
//!    no Ctrl+C cancellation (it holds no state lock, so unlike
//!    apply/destroy it is not wrapped in `run_with_ctrl_c` — see #3111), a
//!    second Ctrl+C force-exits via `std::process::exit`, and a panic with
//!    `panic = "abort"` does not unwind. In all of these `Drop` never runs,
//!    so without this net the terminal is left with a hidden cursor.
//!
//! The signal handler runs in an async-signal context: it may only perform
//! a raw `write(2)` and an `AtomicBool` swap, nothing else (no allocation,
//! no `std::io`, no locks).

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

/// DECTCEM cursor-hide control sequence (`ESC [ ? 25 l`).
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
/// DECTCEM cursor-show control sequence (`ESC [ ? 25 h`).
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";

/// True between the moment the cursor is hidden and the moment it is
/// restored. The restore is performed by whichever of the RAII guard, the
/// signal handler, or the panic hook observes a `true → false` transition
/// first (via [`AtomicBool::swap`]) — so the sequence is emitted exactly
/// once no matter which exit path fires.
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Restore the cursor *iff* it is currently hidden, claiming the restore so
/// no other path repeats it. `true` means this call performed the restore.
///
/// `async_signal_safe` selects the write path: from a signal handler only a
/// raw `libc::write(2)` is permitted (no `std::io`, no allocation); from the
/// guard `Drop` / panic hook the buffered `std::io::stdout` is used so the
/// sequence interleaves correctly with indicatif's own terminal writes.
fn restore_cursor_once(async_signal_safe: bool) -> bool {
    if !CURSOR_HIDDEN.swap(false, Ordering::SeqCst) {
        return false;
    }
    if async_signal_safe {
        // SAFETY: `write` is async-signal-safe. fd 1 is stdout; a short
        // partial/failed write is acceptable here — a best-effort cursor
        // restore must never block or panic in a signal context.
        unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                CURSOR_SHOW.as_ptr() as *const libc::c_void,
                CURSOR_SHOW.len(),
            );
        }
    } else {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(CURSOR_SHOW);
        let _ = out.flush();
    }
    true
}

/// RAII guard that hides the terminal cursor for the lifetime of the
/// refresh-progress phase and restores it on drop.
///
/// Construction emits `\x1b[?25l` and arms [`CURSOR_HIDDEN`]; `Drop` emits
/// `\x1b[?25h` (claiming the restore via [`restore_cursor_once`]) on the
/// normal-completion and `?`-error-unwind paths. The abnormal exits are
/// covered by [`install_restore_handlers`] reading the same flag.
///
/// Hiding is gated on `stdout().is_terminal()`, matching
/// [`crate::wiring::finish_refresh_bar_region`]'s gate: when stdout is not a
/// TTY (CI capture, redirection to a file) nothing is emitted, so captured
/// logs stay clean. With `should_hide` false the guard is fully inert.
pub(crate) struct CursorGuard<W: std::io::Write> {
    writer: W,
    should_hide: bool,
}

impl<W: std::io::Write> CursorGuard<W> {
    /// Construct a guard over an explicit writer, hiding the cursor now iff
    /// `should_hide`. Used by tests; production code uses [`Self::stdout`].
    pub(crate) fn new(mut writer: W, should_hide: bool) -> Self {
        if should_hide {
            let _ = writer.write_all(CURSOR_HIDE);
            let _ = writer.flush();
            CURSOR_HIDDEN.store(true, Ordering::SeqCst);
        }
        Self {
            writer,
            should_hide,
        }
    }
}

impl CursorGuard<std::io::Stdout> {
    /// Hide the cursor on stdout for the refresh phase, restoring it on drop.
    ///
    /// Inert (writes nothing, ever) when stdout is not a terminal.
    pub(crate) fn stdout() -> Self {
        let should_hide = std::io::stdout().is_terminal();
        Self::new(std::io::stdout(), should_hide)
    }
}

impl<W: std::io::Write> Drop for CursorGuard<W> {
    fn drop(&mut self) {
        if !self.should_hide {
            return;
        }
        // If the global flag is still set, this guard owns the restore;
        // write the sequence to our own writer (the test seam) and clear
        // the flag so the signal/panic net does not repeat it. If a signal
        // already claimed it, `swap` returned false and we stay silent.
        if CURSOR_HIDDEN.swap(false, Ordering::SeqCst) {
            let _ = self.writer.write_all(CURSOR_SHOW);
            let _ = self.writer.flush();
        }
    }
}

/// Install the SIGINT/SIGTERM handler and panic hook that restore the
/// cursor on the exit paths `Drop` cannot reach. Idempotent-safe to call
/// once at startup; a no-op when stdout is not a terminal (nothing ever
/// hides the cursor in that case, so nothing needs restoring).
pub fn install_restore_handlers() {
    if !std::io::stdout().is_terminal() {
        return;
    }

    // SIGINT (Ctrl+C) and SIGTERM. The handler restores the cursor (only if
    // still hidden) and then re-runs the signal's default disposition so
    // the process still terminates with the conventional behavior — we are
    // only inserting a cursor restore, not swallowing the signal.
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        // SAFETY: the closure is async-signal-safe — it performs only an
        // `AtomicBool` swap, a raw `write(2)`, and signal-hook's own
        // `emulate_default_handler` (documented async-signal-safe).
        let res = unsafe {
            signal_hook::low_level::register(sig, move || {
                restore_cursor_once(true);
                let _ = signal_hook::low_level::emulate_default_handler(sig);
            })
        };
        let _ = res;
    }

    // Panic path: `panic = "abort"` does not unwind, so the guard's `Drop`
    // never runs. Restore the cursor (if hidden) then delegate to the
    // previous hook so the normal panic message / abort still happens.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_cursor_once(false);
        prev(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate the process-global `CURSOR_HIDDEN`. They must not
    // run concurrently with each other; nextest's process-per-test model
    // isolates them, and within a process they are ordered by this mutex.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn guard_hides_on_construct_and_shows_on_drop_when_enabled() {
        let _l = TEST_LOCK.lock().unwrap();
        CURSOR_HIDDEN.store(false, Ordering::SeqCst);
        let mut buf: Vec<u8> = Vec::new();
        // The guard borrows `buf`, so the hide-then-show ordering can only
        // be asserted after the scope ends.
        {
            let _guard = CursorGuard::new(&mut buf, true);
            assert!(CURSOR_HIDDEN.load(Ordering::SeqCst), "flag armed on hide");
        }
        assert_eq!(buf, b"\x1b[?25l\x1b[?25h");
        assert!(
            !CURSOR_HIDDEN.load(Ordering::SeqCst),
            "flag cleared once restored"
        );
    }

    #[test]
    fn guard_writes_nothing_when_disabled() {
        let _l = TEST_LOCK.lock().unwrap();
        CURSOR_HIDDEN.store(false, Ordering::SeqCst);
        let mut buf: Vec<u8> = Vec::new();
        {
            let _guard = CursorGuard::new(&mut buf, false);
        }
        assert!(
            buf.is_empty(),
            "non-TTY guard must emit no DECTCEM sequence, got {buf:?}"
        );
    }

    #[test]
    fn guard_restores_cursor_on_early_error_unwind() {
        let _l = TEST_LOCK.lock().unwrap();
        CURSOR_HIDDEN.store(false, Ordering::SeqCst);
        // Simulate a `?`-style early return: the guard is dropped while the
        // surrounding fallible operation bails out. The show sequence must
        // still be emitted so the terminal is left usable.
        fn refresh_phase(buf: &mut Vec<u8>) -> Result<(), &'static str> {
            let _guard = CursorGuard::new(buf, true);
            Err("provider read failed")?;
            unreachable!()
        }
        let mut buf: Vec<u8> = Vec::new();
        let res = refresh_phase(&mut buf);
        assert!(res.is_err());
        assert_eq!(
            buf, b"\x1b[?25l\x1b[?25h",
            "cursor must be restored even when the refresh phase errors out"
        );
    }

    #[test]
    fn restore_is_claimed_exactly_once() {
        let _l = TEST_LOCK.lock().unwrap();
        // Simulate "signal fired first": the flag is armed, the signal path
        // claims the restore, and the guard's later drop must stay silent
        // so the sequence is not emitted twice.
        CURSOR_HIDDEN.store(true, Ordering::SeqCst);
        assert!(
            restore_cursor_once(false),
            "first claimant performs the restore"
        );
        assert!(
            !restore_cursor_once(false),
            "second claimant must observe the cleared flag and do nothing"
        );

        // And a guard dropping after the signal already restored: no write.
        CURSOR_HIDDEN.store(true, Ordering::SeqCst);
        assert!(restore_cursor_once(false));
        let mut buf: Vec<u8> = Vec::new();
        {
            // should_hide=true but the flag was already cleared by the
            // signal path above → Drop's `swap` sees false → no write.
            let g = CursorGuard {
                writer: &mut buf,
                should_hide: true,
            };
            drop(g);
        }
        assert!(
            buf.is_empty(),
            "guard must not re-emit a restore the signal path already did, got {buf:?}"
        );
    }
}
