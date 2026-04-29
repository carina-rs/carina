//! CLI observer that prints colored progress output using `indicatif`.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Mutex;
use std::time::Duration;

use carina_core::executor::{ExecutionEvent, ExecutionObserver};
use carina_core::plan::Plan;
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::commands::shared::progress::{format_progress, spinner_style};
use crate::display::format_effect;

use super::progress::format_duration;

/// CLI observer that prints colored progress output using `indicatif`.
///
/// Uses dynamic display: resources appear only when they start executing or
/// complete. No upfront tree is shown. Spinners are created lazily on
/// `EffectStarted` and finished on `EffectSucceeded`/`EffectFailed`.
pub(crate) struct CliObserver {
    multi: MultiProgress,
    /// Map from effect description to its ProgressBar, created lazily when
    /// the effect starts executing. Guarded by a Mutex for concurrent access.
    bars: Mutex<HashMap<String, ProgressBar>>,
}

impl CliObserver {
    /// Create a new observer.
    pub(crate) fn new(_plan: &Plan) -> Self {
        let multi = MultiProgress::new();
        if !std::io::stdout().is_terminal() {
            multi.set_draw_target(indicatif::ProgressDrawTarget::stderr());
        }

        Self {
            multi,
            bars: Mutex::new(HashMap::new()),
        }
    }
}

impl ExecutionObserver for CliObserver {
    fn on_event(&self, event: &ExecutionEvent) {
        match event {
            ExecutionEvent::Waiting { .. } => {
                // Dynamic display: don't show waiting resources.
                // They will appear when they start executing.
            }
            ExecutionEvent::EffectStarted { effect } => {
                let key = format_effect(effect);
                let pb = self.multi.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style());
                pb.set_message(key.clone());
                pb.enable_steady_tick(Duration::from_millis(80));
                self.bars.lock().unwrap().insert(key, pb);
            }
            ExecutionEvent::EffectSucceeded {
                effect,
                duration,
                progress,
                ..
            } => {
                let key = format_effect(effect);
                let timing = format!("[{}]", format_duration(*duration)).dimmed();
                let counter = format_progress(progress).dimmed();
                let msg = format!(
                    "{} {} {} {}",
                    "✓".green(),
                    format_effect(effect),
                    timing,
                    counter
                );
                let mut bars = self.bars.lock().unwrap();
                if let Some(pb) = bars.remove(&key) {
                    pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                    pb.finish_with_message(msg);
                } else {
                    eprintln!("  {msg}");
                }
            }
            ExecutionEvent::EffectFailed {
                effect,
                error,
                duration,
                progress,
            } => {
                let key = format_effect(effect);
                let timing = format!("[{}]", format_duration(*duration)).dimmed();
                let counter = format_progress(progress).dimmed();
                let msg = format!(
                    "{} {} {} {}\n      {} {}",
                    "✗".red(),
                    format_effect(effect),
                    timing,
                    counter,
                    "→".red(),
                    error.red()
                );
                let mut bars = self.bars.lock().unwrap();
                if let Some(pb) = bars.remove(&key) {
                    pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                    pb.finish_with_message(msg.clone());
                }
                // Always print errors to stderr so they're visible even when
                // indicatif's MultiProgress swallows progress bar output.
                eprintln!("  {msg}");
            }
            ExecutionEvent::EffectSkipped {
                effect,
                reason,
                progress,
            } => {
                let key = format_effect(effect);
                let counter = format_progress(progress).dimmed();
                let msg = format!(
                    "{} {} - {} {}",
                    "⊘".yellow(),
                    format_effect(effect),
                    reason,
                    counter
                );
                // Skipped effects may not have a spinner (they were never started).
                let mut bars = self.bars.lock().unwrap();
                if let Some(pb) = bars.remove(&key) {
                    pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                    pb.finish_with_message(msg);
                } else {
                    eprintln!("  {}", msg);
                }
            }
            ExecutionEvent::CascadeUpdateSucceeded { id } => {
                self.multi
                    .println(format!("  {} Update {} (cascade)", "✓".green(), id))
                    .ok();
            }
            ExecutionEvent::CascadeUpdateFailed { id, error } => {
                self.multi
                    .println(format!("  {} Update {} (cascade)", "✗".red(), id))
                    .ok();
                self.multi
                    .println(format!("      {} {}", "→".red(), error.red()))
                    .ok();
            }
            ExecutionEvent::RenameSucceeded { id, from, to } => {
                self.multi
                    .println(format!(
                        "  {} Rename {} \"{}\" → \"{}\"",
                        "✓".green(),
                        id,
                        from,
                        to
                    ))
                    .ok();
            }
            ExecutionEvent::RenameFailed { id, error } => {
                self.multi
                    .println(format!("  {} Rename {}", "✗".red(), id))
                    .ok();
                self.multi
                    .println(format!("      {} {}", "→".red(), error.red()))
                    .ok();
            }
            ExecutionEvent::RefreshStarted => {
                self.multi.println("").ok();
                self.multi
                    .println(format!(
                        "{}",
                        "Refreshing uncertain resource states...".cyan()
                    ))
                    .ok();
            }
            ExecutionEvent::RefreshSucceeded { id } => {
                self.multi
                    .println(format!("  {} Refresh {}", "✓".green(), id))
                    .ok();
            }
            ExecutionEvent::RefreshFailed { id, error } => {
                self.multi
                    .println(format!("  {} Refresh {} - {}", "!".yellow(), id, error))
                    .ok();
            }
        }
    }
}
