//! Terminal cursor visibility for the whole command run (#3153, #3158).
//!
//! `indicatif` clears lines and moves the cursor but never emits DECTCEM
//! hide/show. A visible caret during a command can look like an input prompt
//! and appears in screenshots. One [`CursorGuard`] therefore hides it for the
//! entire command lifetime, from CLI parsing until exit. #3153 originally hid
//! it only during refresh; #3158 widened this to command-wide hiding to avoid
//! flicker between provider loading, refresh, and printing the result.
//!
//! Three restore paths cooperate through the invocation's [`CursorState`]:
//!
//! 1. [`CursorGuard::drop`] restores on normal completion and `?` error unwind.
//! 2. Explicit [`CursorState::restore`] calls cover `std::process::exit`, which
//!    runs no destructors, including the shutdown supervisor's signal exits.
//! 3. The executable's panic hook restores before delegating to the previous
//!    hook, covering `panic = "abort"` where the guard cannot unwind.
//!
//! Each claims the same atomic hidden flag with `swap(false)`, so only the
//! first claimant emits the restore. Clones share that invocation's flag;
//! separate invocations own independent state.
//!
//! [`CursorReveal`] temporarily shows the caret for synchronous apply/destroy
//! confirmation prompts and re-hides it afterward, so the user can see where
//! they type while the command-wide guard remains in place.
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// DECTCEM cursor-hide control sequence (`ESC [ ? 25 l`).
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
/// DECTCEM cursor-show control sequence (`ESC [ ? 25 h`).
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";

/// Invocation-owned flag, true from cursor hide until a restore claims it.
/// The RAII guard, explicit exits, and panic hook share this claim-once state.
#[derive(Clone, Default)]
pub struct CursorState(Arc<AtomicBool>);

impl CursorState {
    /// Start with a visible cursor and no restore owed.
    pub fn new() -> Self {
        Self::default()
    }
    /// Restore before `std::process::exit` or from the executable's panic hook.
    ///
    /// Every exit reachable while the command-wide guard is active must call
    /// this, including early command errors (#3158). It is idempotent with the
    /// guard and the other restore paths, so a path that also unwinds is safe.
    pub fn restore(&self) {
        self.restore_once();
    }
    /// Claim and perform a restore only when hidden; report whether it happened.
    fn restore_once(&self) -> bool {
        if !self.0.swap(false, Ordering::SeqCst) {
            return false;
        }
        use std::io::Write;
        let mut out = std::io::stderr();
        let _ = out.write_all(CURSOR_SHOW);
        let _ = out.flush();
        true
    }
}

/// Terminal mode shared by cursor handling and the progress backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressMode {
    Tty,
    Plain,
}
impl ProgressMode {
    pub(crate) fn is_tty(self) -> bool {
        matches!(self, Self::Tty)
    }
}

/// Resolve the progress terminal once from stderr, indicatif's render stream.
pub fn progress_mode() -> ProgressMode {
    static MODE: OnceLock<ProgressMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        if std::io::stderr().is_terminal() {
            ProgressMode::Tty
        } else {
            ProgressMode::Plain
        }
    })
}

/// Command-lifetime RAII guard that hides on construction and restores on drop.
///
/// Construction emits `\x1b[?25l` and arms the supplied [`CursorState`]. Drop
/// emits `\x1b[?25h` only if it claims the hidden flag before an explicit exit
/// or the panic hook. Hiding follows stderr's shared progress mode; a non-TTY
/// guard is fully inert.
pub struct CursorGuard<W: std::io::Write> {
    state: CursorState,
    writer: W,
    should_hide: bool,
}
impl<W: std::io::Write> CursorGuard<W> {
    /// Hide now if enabled, using an explicit writer as the test seam.
    pub(crate) fn new(state: CursorState, mut writer: W, should_hide: bool) -> Self {
        if should_hide {
            let _ = writer.write_all(CURSOR_HIDE);
            let _ = writer.flush();
            state.0.store(true, Ordering::SeqCst);
        }
        Self {
            state,
            writer,
            should_hide,
        }
    }
}
impl CursorGuard<std::io::Stderr> {
    /// Hide for the whole command on the stderr progress terminal.
    /// Inert when stderr is not a terminal.
    pub fn progress(state: &CursorState) -> Self {
        Self::new(state.clone(), std::io::stderr(), progress_mode().is_tty())
    }
}
impl<W: std::io::Write> Drop for CursorGuard<W> {
    fn drop(&mut self) {
        // Use the guard's writer only if no exit/panic path already claimed
        // the restore by clearing this invocation's flag.
        if self.should_hide && self.state.0.swap(false, Ordering::SeqCst) {
            let _ = self.writer.write_all(CURSOR_SHOW);
            let _ = self.writer.flush();
        }
    }
}

/// Scoped inverse of [`CursorGuard`] for interactive confirmation prompts.
///
/// Construction claims the hidden state with `swap(false)` and shows the
/// cursor, keeping the exit/panic paths from double-restoring during the
/// prompt. Drop re-hides and returns the hidden state to the command-wide
/// guard. If the cursor was already visible, this reveal stays inert.
pub(crate) struct CursorReveal {
    state: CursorState,
    /// True only when this reveal performed the show and owes a re-hide.
    revealed: bool,
}
impl CursorReveal {
    /// Reveal for this value's lifetime, reusing the claim-once restore path.
    pub(crate) fn new(state: &CursorState) -> Self {
        Self {
            state: state.clone(),
            revealed: state.restore_once(),
        }
    }
}
impl Drop for CursorReveal {
    fn drop(&mut self) {
        if self.revealed {
            // This reveal is the sole mutator while its synchronous,
            // single-threaded prompt runs: it claimed the flag in `new`, and
            // a signal-driven exit terminates before this drop can run.
            // Unconditional `store(true)` is safe only under that invariant;
            // it hands the hidden state back to the command guard/restore net.
            use std::io::Write;
            let mut out = std::io::stderr();
            let _ = out.write_all(CURSOR_HIDE);
            let _ = out.flush();
            self.state.0.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_cursor_states_keep_other_invocations_hidden() {
        let a = CursorState::new();
        let b = CursorState::new();
        let mut a_output = Vec::new();
        let mut b_output = Vec::new();
        let a_guard = CursorGuard::new(a.clone(), &mut a_output, true);
        let b_guard = CursorGuard::new(b.clone(), &mut b_output, true);
        std::thread::spawn(move || a.restore()).join().unwrap();
        assert!(b.0.load(Ordering::SeqCst));
        drop(a_guard);
        drop(b_guard);
        assert_eq!(a_output, CURSOR_HIDE);
        assert_eq!(b_output, [CURSOR_HIDE, CURSOR_SHOW].concat());
    }

    #[test]
    fn guard_hides_on_construct_and_shows_on_drop_when_enabled() {
        let state = CursorState::new();
        let mut buf: Vec<u8> = Vec::new();
        // The guard borrows `buf`, so the hide-then-show ordering can only
        // be asserted after the scope ends.
        {
            let _guard = CursorGuard::new(state.clone(), &mut buf, true);
            assert!(state.0.load(Ordering::SeqCst), "flag armed on hide");
        }
        assert_eq!(buf, b"\x1b[?25l\x1b[?25h");
        assert!(
            !state.0.load(Ordering::SeqCst),
            "flag cleared once restored"
        );
    }

    #[test]
    fn guard_writes_nothing_when_disabled() {
        let state = CursorState::new();
        let mut buf: Vec<u8> = Vec::new();
        {
            let _guard = CursorGuard::new(state.clone(), &mut buf, false);
        }
        assert!(
            buf.is_empty(),
            "non-TTY guard must emit no DECTCEM sequence, got {buf:?}"
        );
    }

    #[test]
    fn guard_restores_cursor_on_early_error_unwind() {
        // Simulate a `?`-style early return: the guard is dropped while the
        // surrounding fallible operation bails out. The show sequence must
        // still be emitted so the terminal is left usable.
        fn refresh_phase(buf: &mut Vec<u8>) -> Result<(), &'static str> {
            let _guard = CursorGuard::new(CursorState::new(), buf, true);
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
        // Simulate an explicit exit path firing first: the flag is armed,
        // that path claims the restore, and the guard's later drop must stay
        // silent so the sequence is not emitted twice.
        let state = CursorState::new();
        state.0.store(true, Ordering::SeqCst);
        assert!(state.restore_once(), "first claimant performs the restore");
        assert!(
            !state.restore_once(),
            "second claimant must observe the cleared flag and do nothing"
        );

        // And a guard dropping after an explicit exit path already restored:
        // no write.
        let state = CursorState::new();
        state.0.store(true, Ordering::SeqCst);
        assert!(state.restore_once());
        let mut buf: Vec<u8> = Vec::new();
        {
            // should_hide=true but the flag was already cleared by the
            // explicit exit path above → Drop's `swap` sees false → no write.
            let g = CursorGuard {
                state: state.clone(),
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
        let state = CursorState::new();
        state.0.store(true, Ordering::SeqCst);
        assert!(
            state.restore_once(),
            "armed: the path claims and performs the restore"
        );
        assert!(
            !state.0.load(Ordering::SeqCst),
            "flag cleared after the restore"
        );
        assert!(
            !state.restore_once(),
            "not armed: a second restore call must be a no-op"
        );
    }

    #[test]
    fn panic_restoration_callback_restores_hidden_cursor_once() {
        let state = CursorState::new();
        state.0.store(true, Ordering::SeqCst);
        // Exercise the callback used by the binary hook without installing a
        // process-wide handler in the test runner.
        state.restore();
        assert!(!state.0.load(Ordering::SeqCst));
    }

    #[test]
    fn reveal_shows_then_rehides_when_cursor_was_hidden() {
        // Command-wide guard has hidden the cursor (flag armed).
        let state = CursorState::new();
        state.0.store(true, Ordering::SeqCst);
        {
            let r = CursorReveal::new(&state);
            assert!(r.revealed, "reveal claims the hidden state and shows");
            assert!(
                !state.0.load(Ordering::SeqCst),
                "flag taken from the exit/panic net for the prompt's duration"
            );
            // A restore path arriving during the prompt finds the flag already
            // false and must not double-restore.
            assert!(
                !state.restore_once(),
                "restore during prompt is a no-op (reveal owns the state)"
            );
            drop(r);
        }
        assert!(
            state.0.load(Ordering::SeqCst),
            "after the prompt the hidden state is handed back to the guard"
        );
    }

    #[test]
    fn reveal_is_inert_when_cursor_not_hidden() {
        // Non-TTY / inert command-wide guard: nothing was hidden.
        let state = CursorState::new();
        {
            let r = CursorReveal::new(&state);
            assert!(!r.revealed, "nothing to reveal when not hidden");
        }
        assert!(
            !state.0.load(Ordering::SeqCst),
            "an inert reveal must not arm the flag on drop"
        );
    }
}
