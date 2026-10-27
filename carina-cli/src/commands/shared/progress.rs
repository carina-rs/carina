//! Progress UI shared by command paths.
//!
//! All `indicatif` ownership lives here. Callers report semantic progress
//! events through [`ProgressBackend`] and cannot accidentally select a draw
//! target or finish a spinner without the plain-output fallback.

use std::collections::HashMap;
use std::hash::Hash;
use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::time::Duration;

use carina_core::executor::ProgressInfo;
use carina_core::resource::ResourceId;
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::error::AppError;

enum ProgressBackendInner<K> {
    Tty {
        multi: MultiProgress,
        bars: Mutex<TtyBars<K>>,
        separator_writer: Mutex<Box<dyn Write + Send>>,
        bar_region_writer: Mutex<Box<dyn Write + Send>>,
    },
    Plain {
        writer: Mutex<Box<dyn Write + Send>>,
        refresh_phase_closed: Mutex<bool>,
    },
}

struct TtyBars<K> {
    active: HashMap<K, ProgressBar>,
    refresh_phase_closed: bool,
    bar_region_open: bool,
}

impl<K> Default for TtyBars<K> {
    fn default() -> Self {
        Self {
            active: HashMap::new(),
            refresh_phase_closed: false,
            bar_region_open: false,
        }
    }
}

/// Keyed progress output rendered interactively on stderr or plainly on stdout.
///
/// `indicatif` 0.17's [`MultiProgress`] always renders to stderr by default,
/// so stderr is the stream whose terminal state selects interactive mode. When
/// stderr is redirected, finished outcomes are regular stdout lines so command
/// output remains redirectable without losing progress results.
pub(crate) struct ProgressBackend<K> {
    inner: ProgressBackendInner<K>,
}

impl<K> ProgressBackend<K>
where
    K: Eq + Hash,
{
    /// Build the command progress backend from the stream indicatif renders to.
    pub(crate) fn new() -> Self {
        if io::stderr().is_terminal() {
            Self::with_tty(MultiProgress::new(), io::stdout(), io::stderr())
        } else {
            Self::with_plain_writer(io::stdout())
        }
    }

    #[cfg(test)]
    pub(crate) fn plain(writer: impl Write + Send + 'static) -> Self {
        Self::with_plain_writer(writer)
    }

    fn with_plain_writer(writer: impl Write + Send + 'static) -> Self {
        Self {
            inner: ProgressBackendInner::Plain {
                writer: Mutex::new(Box::new(writer)),
                refresh_phase_closed: Mutex::new(false),
            },
        }
    }

    fn with_tty(
        multi: MultiProgress,
        separator_writer: impl Write + Send + 'static,
        bar_region_writer: impl Write + Send + 'static,
    ) -> Self {
        Self {
            inner: ProgressBackendInner::Tty {
                multi,
                bars: Mutex::new(TtyBars::default()),
                separator_writer: Mutex::new(Box::new(separator_writer)),
                bar_region_writer: Mutex::new(Box::new(bar_region_writer)),
            },
        }
    }

    #[cfg(test)]
    fn tty(
        draw_target: indicatif::ProgressDrawTarget,
        separator_writer: impl Write + Send + 'static,
        bar_region_writer: impl Write + Send + 'static,
    ) -> Self {
        Self::with_tty(
            MultiProgress::with_draw_target(draw_target),
            separator_writer,
            bar_region_writer,
        )
    }

    /// Start an interactive spinner for `key`. Plain output waits for the
    /// corresponding outcome so each operation is emitted exactly once.
    pub(crate) fn start(&self, key: K, label: impl Into<String>) {
        let ProgressBackendInner::Tty { multi, bars, .. } = &self.inner else {
            return;
        };

        let pb = multi.add(ProgressBar::new_spinner());
        pb.set_style(spinner_style());
        pb.set_message(label.into());
        pb.enable_steady_tick(Duration::from_millis(80));
        let mut bars = bars.lock().unwrap();
        bars.bar_region_open = true;
        if let Some(previous) = bars.active.insert(key, pb) {
            previous.finish_and_clear();
            multi.remove(&previous);
        }
    }

    /// Finish the spinner for `key`, or emit the outcome directly when there
    /// is no spinner (including every plain-output invocation).
    pub(crate) fn finish(&self, key: K, line: impl Into<String>) {
        let line = line.into();
        match &self.inner {
            ProgressBackendInner::Tty { multi, bars, .. } => {
                let mut bars = bars.lock().unwrap();
                if let Some(pb) = bars.active.remove(&key) {
                    pb.finish_and_clear();
                    multi.remove(&pb);
                }
                bars.bar_region_open = true;
                let _ = multi.println(format!("  {line}"));
            }
            ProgressBackendInner::Plain { writer, .. } => {
                let _ = writeln!(writer.lock().unwrap(), "  {line}");
            }
        }
    }

    /// Emit a line that is not tied to a spinner.
    ///
    /// Interactive output is printed above the live spinner area.
    pub(crate) fn line(&self, line: impl AsRef<str>) {
        match &self.inner {
            ProgressBackendInner::Tty { multi, bars, .. } => {
                let mut bars = bars.lock().unwrap();
                bars.bar_region_open = true;
                let _ = multi.println(format!("  {}", line.as_ref()));
            }
            ProgressBackendInner::Plain { writer, .. } => {
                let _ = writeln!(writer.lock().unwrap(), "  {}", line.as_ref());
            }
        }
    }

    /// Record that an external stderr line terminated the interactive bar region.
    ///
    /// A later progress event reopens the region. Plain stdout output is
    /// independent from stderr and needs no corresponding state change.
    pub(crate) fn note_external_line(&self) {
        if let ProgressBackendInner::Tty { bars, .. } = &self.inner {
            bars.lock().unwrap().bar_region_open = false;
        }
    }

    /// Close the refresh phase with exactly one blank separator line.
    pub(crate) fn finish_refresh_phase(&self) {
        match &self.inner {
            ProgressBackendInner::Tty {
                bars,
                separator_writer,
                bar_region_writer,
                ..
            } => {
                let mut bars = bars.lock().unwrap();
                if bars.refresh_phase_closed {
                    return;
                }
                bars.refresh_phase_closed = true;
                let bar_region_open = bars.bar_region_open;
                bars.bar_region_open = false;
                drop(bars);

                if bar_region_open {
                    let mut writer = bar_region_writer.lock().unwrap();
                    let _ = writeln!(writer);
                    let _ = writer.flush();
                }

                let mut writer = separator_writer.lock().unwrap();
                let _ = writeln!(writer);
                let _ = writer.flush();
            }
            ProgressBackendInner::Plain {
                writer,
                refresh_phase_closed,
            } => {
                let mut refresh_phase_closed = refresh_phase_closed.lock().unwrap();
                if *refresh_phase_closed {
                    return;
                }
                *refresh_phase_closed = true;
                let mut writer = writer.lock().unwrap();
                let _ = writeln!(writer);
                let _ = writer.flush();
            }
        }
    }
}

/// Format a duration as a human-readable string like "3.2s" or "1m 5.3s".
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{:.1}s", secs)
    } else {
        let mins = secs as u64 / 60;
        let remaining = secs - (mins as f64 * 60.0);
        format!("{}m {:.1}s", mins, remaining)
    }
}

/// Braille spinner frames for animated progress display.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Create the spinner style used by both apply and destroy.
fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.cyan} {msg}...")
        .unwrap()
        .tick_strings(SPINNER_FRAMES)
}

/// Spinner for tracking state refresh progress per resource.
///
/// Shows a spinner while each resource is being read, then displays timing
/// when done. Uses `indicatif` for animated terminal output with a shared
/// `MultiProgress` to support concurrent spinners.
pub(crate) struct RefreshProgress<'a> {
    backend: &'a ProgressBackend<ResourceId>,
    key: ResourceId,
    label: String,
    start: std::time::Instant,
}

impl<'a> RefreshProgress<'a> {
    /// Print the "Refreshing state..." header and prepare for per-resource spinners.
    pub fn start_header() {
        println!("{}", "Refreshing state...".cyan());
    }

    /// Begin tracking a resource read under a shared progress backend.
    pub fn begin(backend: &'a ProgressBackend<ResourceId>, id: &ResourceId) -> Self {
        let key = id.clone();
        let label = id.human().to_string();
        backend.start(key.clone(), label.clone());
        Self {
            backend,
            key,
            label,
            start: std::time::Instant::now(),
        }
    }

    /// Finish the spinner with a success checkmark and elapsed time.
    pub fn finish(self) {
        let elapsed = self.start.elapsed();
        let timing = format!("took {}", format_duration(elapsed)).dimmed();
        let msg = format!("{} {} {}", "✓".green(), self.label, timing);
        self.backend.finish(self.key, msg);
    }
}

/// Format a progress counter as a dimmed string like "1/10".
pub(crate) fn format_progress(progress: &ProgressInfo) -> String {
    format!("{}/{}", progress.completed, progress.total)
}

/// The confirmation prompt's "Enter a value: " has no trailing newline, and on
/// Ctrl+C the prompt interrupt can fire before any other path emits a newline
/// before the lock-release message.
pub(crate) fn emit_newline_on_interrupt<W: std::io::Write>(
    writer: &mut W,
    result: &Result<String, AppError>,
) {
    if matches!(result, Err(AppError::Interrupted)) {
        let _ = writeln!(writer);
        let _ = writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use indicatif::{ProgressDrawTarget, TermLike};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Debug, Default)]
    struct RecordingTerm(Arc<Mutex<Vec<u8>>>);

    impl RecordingTerm {
        fn push(&self, bytes: impl AsRef<[u8]>) {
            self.0.lock().unwrap().extend_from_slice(bytes.as_ref());
        }

        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl TermLike for RecordingTerm {
        fn width(&self) -> u16 {
            80
        }

        fn height(&self) -> u16 {
            20
        }

        fn move_cursor_up(&self, n: usize) -> io::Result<()> {
            self.push(format!("\x1b[{n}A"));
            Ok(())
        }

        fn move_cursor_down(&self, n: usize) -> io::Result<()> {
            self.push(format!("\x1b[{n}B"));
            Ok(())
        }

        fn move_cursor_right(&self, n: usize) -> io::Result<()> {
            self.push(format!("\x1b[{n}C"));
            Ok(())
        }

        fn move_cursor_left(&self, n: usize) -> io::Result<()> {
            self.push(format!("\x1b[{n}D"));
            Ok(())
        }

        fn write_line(&self, line: &str) -> io::Result<()> {
            self.push(line);
            self.push("\r\n");
            Ok(())
        }

        fn write_str(&self, text: &str) -> io::Result<()> {
            self.push(text);
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            self.push("\r\x1b[2K");
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn tty_backend_emits_finished_and_untied_lines_once() {
        let terminal = RecordingTerm::default();
        let backend: ProgressBackend<usize> = ProgressBackend::tty(
            ProgressDrawTarget::term_like(Box::new(terminal.clone())),
            io::sink(),
            io::sink(),
        );

        backend.start(1, "running");
        backend.finish(1, "finished after start");
        backend.finish(2, "finished without start");
        backend.line("standalone event");

        let raw = String::from_utf8(terminal.bytes()).unwrap();
        for outcome in [
            "finished after start",
            "finished without start",
            "standalone event",
        ] {
            assert_eq!(
                raw.matches(outcome).count(),
                1,
                "expected one emitted {outcome:?}\nraw output:\n{raw}",
            );
        }
    }

    #[test]
    fn tty_backend_splits_bar_terminator_and_refresh_separator() {
        let terminal = RecordingTerm::default();
        let stdout = SharedWriter::default();
        let stderr = SharedWriter::default();
        let backend: ProgressBackend<usize> = ProgressBackend::tty(
            ProgressDrawTarget::term_like(Box::new(terminal)),
            stdout.clone(),
            stderr.clone(),
        );

        backend.start(1, "running");
        backend.finish(1, "finished");
        backend.finish_refresh_phase();
        backend.finish_refresh_phase();

        assert_eq!(stdout.0.lock().unwrap().as_slice(), b"\n");
        assert_eq!(stderr.0.lock().unwrap().as_slice(), b"\n");
    }

    #[test]
    fn tty_backend_skips_bar_terminator_without_rendered_progress() {
        let stdout = SharedWriter::default();
        let stderr = SharedWriter::default();
        let backend: ProgressBackend<usize> =
            ProgressBackend::tty(ProgressDrawTarget::hidden(), stdout.clone(), stderr.clone());

        backend.finish_refresh_phase();

        assert_eq!(stdout.0.lock().unwrap().as_slice(), b"\n");
        assert!(stderr.0.lock().unwrap().is_empty());
    }

    #[test]
    fn tty_backend_skips_bar_terminator_after_external_line() {
        let stdout = SharedWriter::default();
        let stderr = SharedWriter::default();
        let backend: ProgressBackend<usize> =
            ProgressBackend::tty(ProgressDrawTarget::hidden(), stdout.clone(), stderr.clone());

        backend.start(1, "running");
        backend.finish(1, "finished");
        backend.note_external_line();
        backend.finish_refresh_phase();

        assert_eq!(stdout.0.lock().unwrap().as_slice(), b"\n");
        assert!(stderr.0.lock().unwrap().is_empty());
    }

    #[test]
    fn plain_backend_emits_refresh_separator_once() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::<usize>::plain(output.clone());

        backend.finish_refresh_phase();
        backend.finish_refresh_phase();

        assert_eq!(output.0.lock().unwrap().as_slice(), b"\n");
    }

    #[test]
    fn plain_backend_emits_each_finished_or_untied_event_once() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::plain(output.clone());

        backend.start(1, "started but not emitted");
        backend.finish(1, "finished after start");
        backend.finish(2, "finished without start");
        backend.line("standalone event");

        assert_eq!(
            String::from_utf8(output.0.lock().unwrap().clone()).unwrap(),
            concat!(
                "  finished after start\n",
                "  finished without start\n",
                "  standalone event\n",
            )
        );
    }

    #[test]
    fn plain_backend_does_not_rewrite_line_contents() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::plain(output.clone());

        backend.finish(1, "\x1b[31mfailed\x1b[0m");
        backend.line("\x1b[32mstandalone\x1b[0m");

        assert_eq!(
            String::from_utf8(output.0.lock().unwrap().clone()).unwrap(),
            "  \x1b[31mfailed\x1b[0m\n  \x1b[32mstandalone\x1b[0m\n"
        );
    }
}
