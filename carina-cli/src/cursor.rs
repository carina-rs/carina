//! Terminal cursor visibility control for the whole command run
//! (#3153, #3158).
//!
//! `indicatif` manages line clearing and cursor *movement* but never emits
//! DECTCEM hide/show. So while a `carina` command runs the terminal's caret
//! stays visible; screenshots show a stray cursor and a parked caret reads
//! as "the command is waiting for input".
//!
//! The cursor is hidden for the **entire command lifetime** — a single
//! [`CursorGuard`] is constructed once at startup (`main.rs`, right after
//! CLI parse) and held until process exit. #3153 originally scoped this to
//! just the refresh-spinner phase, which made the cursor flicker (visible
//! during provider load → hidden during refresh → visible again before the
//! result printed); #3158 widened it to command-wide.
//!
//! Three cooperating mechanisms manage the cursor, coordinated by one
//! process-global [`CURSOR_HIDDEN`] flag so the restore sequence is emitted
//! exactly once:
//!
//! 1. [`CursorGuard`] — the command-lifetime RAII guard. Hides on
//!    construct, restores on `Drop` (covers normal exit and `?`
//!    error-unwind).
//! 2. [`install_panic_restore_hook`] — a panic hook that covers the abnormal
//!    panic exit path unwinding can't: a panic with `panic = "abort"` does
//!    not unwind. SIGINT/SIGTERM cursor restoration is handled by the unified
//!    shutdown listener before its second-signal `std::process::exit` path.
//! 3. [`CursorReveal`] — a scoped *inverse* guard for interactive
//!    confirmation prompts (apply/destroy "Enter a value:" / "Type 'yes'").
//!    With the cursor hidden command-wide the user would otherwise type
//!    blind; `CursorReveal` temporarily shows the cursor for the prompt and
//!    re-hides it on drop. This is the one intentional, user-driven reveal
//!    (not flicker — it is exactly where the user is being asked to type).

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

/// DECTCEM cursor-hide control sequence (`ESC [ ? 25 l`).
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
/// DECTCEM cursor-show control sequence (`ESC [ ? 25 h`).
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";

/// True between the moment the cursor is hidden and the moment it is
/// restored. The restore is performed by whichever of the RAII guard, the
/// explicit exit path, or the panic hook observes a `true → false`
/// transition first (via [`AtomicBool::swap`]) — so the sequence is emitted
/// exactly once no matter which exit path fires.
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Restore the cursor before a `std::process::exit`.
///
/// `process::exit` runs no destructors, so the command-wide [`CursorGuard`]'s
/// `Drop` never fires, and it is not a panic so
/// [`install_panic_restore_hook`]'s net does not catch it either. Every
/// `process::exit` site that can be reached while the cursor is hidden —
/// notably `main::handle_app_error`, the universal command-error path —
/// must call this first, or the terminal is left with a hidden cursor on
/// error (#3158: command-wide hiding makes this reachable from the very
/// start of the run). Idempotent and claim-once with the guard/exit/panic
/// paths, so calling it on a path that *also* unwinds is harmless.
pub fn restore_cursor() {
    restore_cursor_once();
}

/// Restore the cursor *iff* it is currently hidden, claiming the restore so
/// no other path repeats it. `true` means this call performed the restore.
fn restore_cursor_once() -> bool {
    if !CURSOR_HIDDEN.swap(false, Ordering::SeqCst) {
        return false;
    }
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(CURSOR_SHOW);
    let _ = out.flush();
    true
}

/// RAII guard that hides the terminal cursor for the lifetime of the
/// command and restores it on drop.
///
/// Construction emits `\x1b[?25l` and arms [`CURSOR_HIDDEN`]; `Drop` emits
/// `\x1b[?25h` (claiming the restore via [`restore_cursor_once`]) on the
/// normal-completion and `?`-error-unwind paths. The abnormal exits are
/// covered by explicit `restore_cursor()` calls and
/// [`install_panic_restore_hook`] reading the same flag.
///
/// Hiding is gated on `stdout().is_terminal()`, matching
/// [`crate::wiring::finish_refresh_bar_region`]'s gate: when stdout is not a
/// TTY (CI capture, redirection to a file) nothing is emitted, so captured
/// logs stay clean. With `should_hide` false the guard is fully inert.
pub struct CursorGuard<W: std::io::Write> {
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
    /// Hide the cursor on stdout for the whole command, restoring it on drop.
    ///
    /// Inert (writes nothing, ever) when stdout is not a terminal.
    pub fn stdout() -> Self {
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
        // the flag so the exit/panic net does not repeat it. If another
        // path already claimed it, `swap` returned false and we stay silent.
        if CURSOR_HIDDEN.swap(false, Ordering::SeqCst) {
            let _ = self.writer.write_all(CURSOR_SHOW);
            let _ = self.writer.flush();
        }
    }
}

/// Scoped *inverse* of [`CursorGuard`]: temporarily reveal the cursor while
/// an interactive confirmation prompt is on screen, then re-hide it.
///
/// With [`CursorGuard`] hiding the cursor command-wide, the apply/destroy
/// confirmation prompts ("Enter a value:" / "Type 'yes' to confirm.")
/// would have the user typing blind. Wrapping the prompt in a
/// `CursorReveal` shows the caret for the duration of the prompt and
/// restores the hidden state on drop. This is the single intentional,
/// user-driven reveal — it lands exactly where the user is asked to type,
/// so it is expected, not the #3158 flicker.
///
/// Coordination with [`CURSOR_HIDDEN`] / [`restore_cursor_once`]:
///
/// - On construct, if the cursor is currently hidden it is *claimed*
///   (`swap(false)`) and `\x1b[?25h` is written — taking the flag away
///   from the exit/panic net for the prompt's duration so they can't
///   double-emit.
/// - On drop, if this reveal performed the show, it re-hides
///   (`\x1b[?25l`) and re-arms the flag, returning ownership to the
///   command-wide guard.
/// - If the cursor was not hidden at construct (non-TTY, or the
///   command-wide guard is inert), the reveal is fully inert.
pub(crate) struct CursorReveal {
    /// True iff this reveal performed the show and therefore owes a re-hide.
    revealed: bool,
}

impl CursorReveal {
    /// Reveal the cursor for the lifetime of this value, if it is hidden.
    pub(crate) fn new() -> Self {
        // Claiming the hidden state and emitting show is exactly
        // `restore_cursor_once`: it `swap(false)`s the flag and, iff it won
        // that transition, writes `CURSOR_SHOW` via buffered stdout. Reusing
        // it keeps the claim-once protocol in one place; the inverse half
        // (re-hide + re-arm) has no equivalent and lives in `Drop`.
        let revealed = restore_cursor_once();
        Self { revealed }
    }
}

impl Drop for CursorReveal {
    fn drop(&mut self) {
        if !self.revealed {
            return;
        }
        // Re-hide and hand the flag back to the command-wide guard / net.
        // Unconditional `store(true)` (not a `swap`) is the one place the
        // strict "only the swap-winner mutates" discipline is relaxed: this
        // reveal claimed the flag in `new`, the prompt is synchronous and
        // single-threaded, and a SIGINT during it terminates the process
        // before this drop runs — so nothing else can have touched the flag
        // between `new` and here. Safe only under that sole-mutator invariant.
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(CURSOR_HIDE);
        let _ = out.flush();
        CURSOR_HIDDEN.store(true, Ordering::SeqCst);
    }
}

/// Install the panic hook that restores the cursor on the panic exit path
/// `Drop` cannot reach. Idempotent-safe to call once at startup; a no-op
/// when stdout is not a terminal (nothing ever hides the cursor in that case,
/// so nothing needs restoring).
pub fn install_panic_restore_hook() {
    if !std::io::stdout().is_terminal() {
        return;
    }

    install_panic_restore_hook_inner();
}

fn install_panic_restore_hook_inner() {
    // Panic path: `panic = "abort"` does not unwind, so the guard's `Drop`
    // never runs. Restore the cursor (if hidden) then delegate to the
    // previous hook so the normal panic message / abort still happens.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_cursor_once();
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

    fn reset_cursor_hidden_for_test(hidden: bool) {
        CURSOR_HIDDEN.store(hidden, Ordering::SeqCst);
    }

    fn install_panic_restore_hook_for_test() {
        install_panic_restore_hook_inner();
    }

    fn is_cursor_hidden_for_test() -> bool {
        CURSOR_HIDDEN.load(Ordering::SeqCst)
    }

    #[test]
    fn guard_hides_on_construct_and_shows_on_drop_when_enabled() {
        let _l = TEST_LOCK.lock().unwrap();
        reset_cursor_hidden_for_test(false);
        let mut buf: Vec<u8> = Vec::new();
        // The guard borrows `buf`, so the hide-then-show ordering can only
        // be asserted after the scope ends.
        {
            let _guard = CursorGuard::new(&mut buf, true);
            assert!(is_cursor_hidden_for_test(), "flag armed on hide");
        }
        assert_eq!(buf, b"\x1b[?25l\x1b[?25h");
        assert!(!is_cursor_hidden_for_test(), "flag cleared once restored");
    }

    #[test]
    fn guard_writes_nothing_when_disabled() {
        let _l = TEST_LOCK.lock().unwrap();
        reset_cursor_hidden_for_test(false);
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
        reset_cursor_hidden_for_test(false);
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
        // Simulate an explicit exit path firing first: the flag is armed,
        // that path claims the restore, and the guard's later drop must stay
        // silent so the sequence is not emitted twice.
        reset_cursor_hidden_for_test(true);
        assert!(restore_cursor_once(), "first claimant performs the restore");
        assert!(
            !restore_cursor_once(),
            "second claimant must observe the cleared flag and do nothing"
        );

        // And a guard dropping after an explicit exit path already restored:
        // no write.
        reset_cursor_hidden_for_test(true);
        assert!(restore_cursor_once());
        let mut buf: Vec<u8> = Vec::new();
        {
            // should_hide=true but the flag was already cleared by the
            // explicit exit path above → Drop's `swap` sees false → no write.
            let g = CursorGuard {
                writer: &mut buf,
                should_hide: true,
            };
            drop(g);
        }
        assert!(
            buf.is_empty(),
            "guard must not re-emit a restore the explicit exit path already did, got {buf:?}"
        );
    }

    #[test]
    fn restore_path_claims_once_and_is_idempotent() {
        let _l = TEST_LOCK.lock().unwrap();
        reset_cursor_hidden_for_test(true);
        assert!(
            restore_cursor_once(),
            "armed: the path claims and performs the restore"
        );
        assert!(
            !is_cursor_hidden_for_test(),
            "flag cleared after the restore"
        );
        assert!(
            !restore_cursor_once(),
            "not armed: a second restore call must be a no-op"
        );
    }

    #[test]
    fn install_panic_restore_hook_restores_hidden_cursor_once() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_cursor_hidden_for_test(true);
        install_panic_restore_hook_for_test();

        let result = std::panic::catch_unwind(|| {
            panic!("cursor restore hook test");
        });

        assert!(result.is_err());
        assert!(!is_cursor_hidden_for_test());
    }

    #[test]
    fn reveal_shows_then_rehides_when_cursor_was_hidden() {
        let _l = TEST_LOCK.lock().unwrap();
        // Command-wide guard has hidden the cursor (flag armed).
        reset_cursor_hidden_for_test(true);
        {
            let r = CursorReveal::new();
            assert!(r.revealed, "reveal claims the hidden state and shows");
            assert!(
                !is_cursor_hidden_for_test(),
                "flag taken from the exit/panic net for the prompt's duration"
            );
            // A restore path arriving during the prompt finds the flag already
            // false and must not double-restore.
            assert!(
                !restore_cursor_once(),
                "restore during prompt is a no-op (reveal owns the state)"
            );
            drop(r);
        }
        assert!(
            is_cursor_hidden_for_test(),
            "after the prompt the hidden state is handed back to the guard"
        );
    }

    #[test]
    fn reveal_is_inert_when_cursor_not_hidden() {
        let _l = TEST_LOCK.lock().unwrap();
        // Non-TTY / inert command-wide guard: nothing was hidden.
        reset_cursor_hidden_for_test(false);
        {
            let r = CursorReveal::new();
            assert!(!r.revealed, "nothing to reveal when not hidden");
        }
        assert!(
            !is_cursor_hidden_for_test(),
            "an inert reveal must not arm the flag on drop"
        );
    }
}
