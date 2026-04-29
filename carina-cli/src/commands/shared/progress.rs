//! Progress UI helpers shared between `apply` and `destroy`.
//!
//! Provides the spinner style, refresh progress tracker, and small formatting
//! helpers for progress counters / durations.

use std::io::IsTerminal;
use std::time::Duration;

use carina_core::executor::ProgressInfo;
use carina_core::resource::ResourceId;
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::error::AppError;

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
pub(crate) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Create the spinner style used by both apply and destroy.
pub(crate) fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.cyan} {msg}...")
        .unwrap()
        .tick_strings(SPINNER_FRAMES)
}

/// Spinner for tracking state refresh progress per resource.
///
/// Shows a spinner while each resource is being read, then displays timing
/// when done. Uses `indicatif` for animated terminal output with a shared
/// `MultiProgress` to support concurrent spinners.
pub(crate) struct RefreshProgress {
    pb: ProgressBar,
    start: std::time::Instant,
}

impl RefreshProgress {
    /// Print the "Refreshing state..." header and prepare for per-resource spinners.
    pub fn start_header() {
        println!("{}", "Refreshing state...".cyan());
    }

    /// Begin tracking a resource read under a shared `MultiProgress`.
    pub fn begin_multi(multi: &MultiProgress, id: &ResourceId) -> Self {
        let pb = multi.add(ProgressBar::new_spinner());
        pb.set_style(spinner_style());
        pb.set_message(format!("{}", id));
        pb.enable_steady_tick(Duration::from_millis(80));
        Self {
            pb,
            start: std::time::Instant::now(),
        }
    }

    /// Finish the spinner with a success checkmark and elapsed time.
    pub fn finish(self) {
        let elapsed = self.start.elapsed();
        let timing = format!("[{}]", format_duration(elapsed)).dimmed();
        let msg = format!("{} {} {}", "✓".green(), self.pb.message(), timing);
        self.pb
            .set_style(ProgressStyle::with_template("  {msg}").unwrap());
        self.pb.finish_with_message(msg);
    }
}

/// Create a `MultiProgress` for concurrent refresh spinners.
///
/// Redirects the draw target to stderr when stdout is not a terminal (e.g., in CI),
/// so that spinner animations are suppressed but `println` messages still appear.
pub(crate) fn refresh_multi_progress() -> MultiProgress {
    let multi = MultiProgress::new();
    if !std::io::stdout().is_terminal() {
        multi.set_draw_target(indicatif::ProgressDrawTarget::stderr());
    }
    multi
}

/// Format a progress counter as a dimmed string like "1/10".
pub(crate) fn format_progress(progress: &ProgressInfo) -> String {
    format!("{}/{}", progress.completed, progress.total)
}

/// The confirmation prompt's "Enter a value: " has no trailing newline, and on
/// Ctrl+C the inner interrupt future fires before the outer run_with_ctrl_c
/// handler — so nothing else emits a newline before the lock-release message.
pub(crate) fn emit_newline_on_interrupt<W: std::io::Write>(
    writer: &mut W,
    result: &Result<String, AppError>,
) {
    if matches!(result, Err(AppError::Interrupted)) {
        let _ = writeln!(writer);
        let _ = writer.flush();
    }
}
