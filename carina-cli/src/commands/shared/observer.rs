//! CLI observer that prints colored progress output using `indicatif`.
//!
//! Two output modes:
//!
//! * **TTY mode** (default when stdout or stderr is a terminal): renders
//!   per-effect spinners through `indicatif::MultiProgress`.
//! * **Plain mode** (both stdout and stderr non-TTY, e.g. CI logs): emits
//!   one line per event via `println!` / `eprintln!`. This is necessary
//!   because `indicatif` suppresses all drawing on non-TTY targets, which
//!   otherwise leaves CI logs blank between "Applying changes..." and
//!   "Apply complete!" (#2883).

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Mutex;
use std::time::Duration;

use carina_core::executor::{ExecutionEvent, ExecutionObserver};
use carina_core::plan::Plan;
use carina_core::value::format_value_user_facing;
use carina_core::wait::WaitObservation;
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::commands::shared::progress::{format_progress, spinner_style};
use crate::display::format_effect;

use super::progress::format_duration;

enum Backend {
    Tty {
        multi: MultiProgress,
        bars: Mutex<HashMap<String, ProgressBar>>,
    },
    Plain,
}

/// CLI observer that prints colored progress output.
pub(crate) struct CliObserver {
    backend: Backend,
}

impl CliObserver {
    /// Create a new observer. Picks `Tty` mode when stdout or stderr is a
    /// terminal, otherwise `Plain` mode (one line per event, no spinners).
    pub(crate) fn new(_plan: &Plan) -> Self {
        let stdout_tty = std::io::stdout().is_terminal();
        let stderr_tty = std::io::stderr().is_terminal();
        let backend = if stdout_tty || stderr_tty {
            let multi = MultiProgress::new();
            if !stdout_tty {
                multi.set_draw_target(indicatif::ProgressDrawTarget::stderr());
            }
            Backend::Tty {
                multi,
                bars: Mutex::new(HashMap::new()),
            }
        } else {
            Backend::Plain
        };
        Self { backend }
    }
}

impl ExecutionObserver for CliObserver {
    fn on_event(&self, event: &ExecutionEvent) {
        match &self.backend {
            Backend::Tty { multi, bars } => handle_tty(multi, bars, event),
            Backend::Plain => handle_plain(event),
        }
    }
}

fn handle_tty(
    multi: &MultiProgress,
    bars: &Mutex<HashMap<String, ProgressBar>>,
    event: &ExecutionEvent,
) {
    match event {
        ExecutionEvent::Waiting { .. } => {
            // Dynamic display: don't show waiting resources.
            // They will appear when they start executing.
        }
        ExecutionEvent::EffectStarted { effect } => {
            let key = format_effect(effect);
            let pb = multi.add(ProgressBar::new_spinner());
            pb.set_style(spinner_style());
            pb.set_message(key.clone());
            pb.enable_steady_tick(Duration::from_millis(80));
            bars.lock().unwrap().insert(key, pb);
        }
        ExecutionEvent::EffectSucceeded {
            effect,
            duration,
            progress,
            ..
        } => {
            let key = format_effect(effect);
            let timing = format!("took {}", format_duration(*duration)).dimmed();
            let counter = format_progress(progress).dimmed();
            let msg = format!(
                "{} {} {} {}",
                "✓".green(),
                format_effect(effect),
                timing,
                counter
            );
            let mut bars = bars.lock().unwrap();
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
            let timing = format!("took {}", format_duration(*duration)).dimmed();
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
            let mut bars = bars.lock().unwrap();
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
            let mut bars = bars.lock().unwrap();
            if let Some(pb) = bars.remove(&key) {
                pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                pb.finish_with_message(msg);
            } else {
                eprintln!("  {}", msg);
            }
        }
        ExecutionEvent::WaitPolling {
            observation,
            elapsed,
        } => {
            multi
                .println(format_wait_polling_line(observation, *elapsed))
                .ok();
        }
        ExecutionEvent::CascadeUpdateSucceeded { id } => {
            multi
                .println(format!("  {} Update {} (cascade)", "✓".green(), id))
                .ok();
        }
        ExecutionEvent::CascadeUpdateFailed { id, error } => {
            multi
                .println(format!("  {} Update {} (cascade)", "✗".red(), id))
                .ok();
            multi
                .println(format!("      {} {}", "→".red(), error.red()))
                .ok();
        }
        ExecutionEvent::RenameSucceeded { id, from, to } => {
            multi
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
            multi.println(format!("  {} Rename {}", "✗".red(), id)).ok();
            multi
                .println(format!("      {} {}", "→".red(), error.red()))
                .ok();
        }
        ExecutionEvent::RefreshStarted => {
            multi.println("").ok();
            multi
                .println(format!(
                    "{}",
                    "Refreshing uncertain resource states...".cyan()
                ))
                .ok();
        }
        ExecutionEvent::RefreshSucceeded { id } => {
            multi
                .println(format!("  {} Refresh {}", "✓".green(), id))
                .ok();
        }
        ExecutionEvent::RefreshFailed { id, error } => {
            multi
                .println(format!("  {} Refresh {} - {}", "!".yellow(), id, error))
                .ok();
        }
    }
}

fn handle_plain(event: &ExecutionEvent) {
    for line in format_plain(event) {
        println!("{}", line);
    }
}

/// Render an `ExecutionEvent` as zero or more plain-mode lines.
///
/// Pulled out so the non-TTY rendering can be unit-tested without driving
/// stdout. `EffectStarted` / `Waiting` produce no lines in plain mode —
/// they would otherwise duplicate the matching Succeeded / Failed entry.
fn format_plain(event: &ExecutionEvent) -> Vec<String> {
    match event {
        ExecutionEvent::Waiting { .. } | ExecutionEvent::EffectStarted { .. } => Vec::new(),
        ExecutionEvent::EffectSucceeded {
            effect,
            duration,
            progress,
            ..
        } => {
            let timing = format!("took {}", format_duration(*duration));
            let counter = format_progress(progress);
            vec![format!(
                "  ✓ {} {} {}",
                format_effect(effect),
                timing,
                counter
            )]
        }
        ExecutionEvent::EffectFailed {
            effect,
            error,
            duration,
            progress,
        } => {
            let timing = format!("took {}", format_duration(*duration));
            let counter = format_progress(progress);
            vec![
                format!("  ✗ {} {} {}", format_effect(effect), timing, counter),
                format!("      → {}", error),
            ]
        }
        ExecutionEvent::EffectSkipped {
            effect,
            reason,
            progress,
        } => {
            let counter = format_progress(progress);
            vec![format!(
                "  ⊘ {} - {} {}",
                format_effect(effect),
                reason,
                counter
            )]
        }
        ExecutionEvent::WaitPolling {
            observation,
            elapsed,
        } => vec![format_wait_polling_line(observation, *elapsed)],
        ExecutionEvent::CascadeUpdateSucceeded { id } => {
            vec![format!("  ✓ Update {} (cascade)", id)]
        }
        ExecutionEvent::CascadeUpdateFailed { id, error } => vec![
            format!("  ✗ Update {} (cascade)", id),
            format!("      → {}", error),
        ],
        ExecutionEvent::RenameSucceeded { id, from, to } => {
            vec![format!("  ✓ Rename {} \"{}\" → \"{}\"", id, from, to)]
        }
        ExecutionEvent::RenameFailed { id, error } => {
            vec![format!("  ✗ Rename {}", id), format!("      → {}", error)]
        }
        ExecutionEvent::RefreshStarted => vec![
            String::new(),
            "Refreshing uncertain resource states...".to_string(),
        ],
        ExecutionEvent::RefreshSucceeded { id } => vec![format!("  ✓ Refresh {}", id)],
        ExecutionEvent::RefreshFailed { id, error } => {
            vec![format!("  ! Refresh {} - {}", id, error)]
        }
    }
}

fn format_wait_polling_line(observation: &WaitObservation, elapsed: Duration) -> String {
    let observed = format_wait_observed_attr(observation);
    format!(
        "  ~ {}: waited {}, {}",
        observation.binding(),
        format_duration(elapsed),
        observed
    )
}

fn format_wait_observed_attr(observation: &WaitObservation) -> String {
    if let Some((attr, value)) = observation.primary() {
        let key = attr.segments.join(".");
        return format!("{key}={}", format_value_user_facing(value));
    }

    let mut observed: Vec<_> = observation.last_attrs().iter().collect();
    observed.sort_by_key(|(key, _)| *key);
    let Some((key, value)) = observed.first() else {
        return "no observed attributes".to_string();
    };
    format!("{key}={}", format_value_user_facing(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::effect::Effect;
    use carina_core::executor::ProgressInfo;
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Resource, ResourceId, UnknownReason, Value,
    };
    use carina_core::wait::predicate::{AttrPath, WaitPredicate};
    use indexmap::IndexMap;

    fn dummy_create_effect() -> Effect {
        Effect::Create(Resource::new("aws.s3.Bucket", "demo"))
    }

    fn wait_observation<'a>(
        target_id: &'a ResourceId,
        predicate: &'a WaitPredicate,
        last_attrs: &'a HashMap<String, Value>,
    ) -> WaitObservation<'a> {
        WaitObservation::new("demo_ready", target_id, predicate, last_attrs)
    }

    fn equals_predicate(attr: AttrPath, value: &str) -> WaitPredicate {
        WaitPredicate::Equals {
            attr,
            value: Value::Concrete(ConcreteValue::String(value.to_string())),
        }
    }

    #[test]
    fn wait_observed_attr_is_deterministic() {
        let target_id = ResourceId::new("aws.test.Resource", "demo");
        let attrs = HashMap::from([
            (
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("pending".to_string())),
            ),
            (
                "arn".to_string(),
                Value::Concrete(ConcreteValue::String("arn:demo".to_string())),
            ),
        ]);
        let predicate = equals_predicate(AttrPath::single("missing"), "present");
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(format_wait_observed_attr(&observation), "arn=arn:demo");
    }

    #[test]
    fn wait_observed_attr_uses_predicate_attr_when_present() {
        let target_id = ResourceId::new("aws.test.Resource", "demo");
        let status = AttrPath::single("status");
        let predicate = equals_predicate(status, "pending");
        let attrs = HashMap::from([
            (
                "arn".to_string(),
                Value::Concrete(ConcreteValue::String("arn:demo".to_string())),
            ),
            (
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("pending".to_string())),
            ),
        ]);
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(format_wait_observed_attr(&observation), "status=pending");
    }

    #[test]
    fn wait_observed_attr_formats_multi_segment_predicate_attr() {
        let target_id = ResourceId::new("aws.test.Resource", "demo");
        let renewal_status = AttrPath {
            segments: vec!["renewal_summary".to_string(), "renewal_status".to_string()],
        };
        let predicate = equals_predicate(renewal_status, "PENDING");
        let attrs = HashMap::from([(
            "renewal_summary".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "renewal_status".to_string(),
                Value::Concrete(ConcreteValue::String("PENDING".to_string())),
            )]))),
        )]);
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(
            format_wait_observed_attr(&observation),
            "renewal_summary.renewal_status=PENDING"
        );
    }

    #[test]
    fn wait_observed_attr_falls_back_to_sort_first_when_predicate_attr_missing() {
        let target_id = ResourceId::new("aws.test.Resource", "demo");
        let xyz = AttrPath::single("xyz");
        let predicate = equals_predicate(xyz, "present");
        let attrs = HashMap::from([
            (
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("pending".to_string())),
            ),
            (
                "arn".to_string(),
                Value::Concrete(ConcreteValue::String("arn:demo".to_string())),
            ),
        ]);
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(format_wait_observed_attr(&observation), "arn=arn:demo");
    }

    #[test]
    fn wait_observed_attr_uses_display_formatting() {
        let target_id = ResourceId::new("aws.test.Resource", "demo");
        let attrs = HashMap::from([(
            "arn".to_string(),
            Value::Concrete(ConcreteValue::String(
                "arn:aws:acm:1:certificate/abc".to_string(),
            )),
        )]);
        let predicate = equals_predicate(AttrPath::single("missing"), "present");
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(
            format_wait_observed_attr(&observation),
            "arn=arn:aws:acm:1:certificate/abc"
        );
    }

    #[test]
    fn wait_observed_attr_handles_unknown_value() {
        let target_id = ResourceId::new("aws.test.Resource", "demo");
        let attrs = HashMap::from([(
            "status".to_string(),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
        )]);
        let predicate = equals_predicate(AttrPath::single("missing"), "present");
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(
            format_wait_observed_attr(&observation),
            "status=(known after upstream apply)"
        );
    }

    #[test]
    fn plain_skips_started_and_waiting() {
        let effect = dummy_create_effect();
        assert!(format_plain(&ExecutionEvent::EffectStarted { effect: &effect }).is_empty());
        assert!(
            format_plain(&ExecutionEvent::Waiting {
                effect: &effect,
                pending_dependencies: vec!["x".into()],
            })
            .is_empty()
        );
    }

    #[test]
    fn plain_emits_one_line_per_succeeded() {
        let effect = dummy_create_effect();
        let lines = format_plain(&ExecutionEvent::EffectSucceeded {
            effect: &effect,
            state: None,
            duration: Duration::from_millis(123),
            progress: ProgressInfo {
                completed: 1,
                total: 3,
            },
        });
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(line.contains("✓"), "missing check mark: {line}");
        assert!(line.contains("Create"), "missing verb: {line}");
        assert!(line.contains("demo"), "missing resource name: {line}");
        assert!(line.contains("took"), "missing `took` label: {line}");
        assert!(line.contains("1/3"), "missing counter: {line}");
    }

    #[test]
    fn plain_failed_includes_error_on_second_line() {
        let effect = dummy_create_effect();
        let lines = format_plain(&ExecutionEvent::EffectFailed {
            effect: &effect,
            error: "boom",
            duration: Duration::from_millis(50),
            progress: ProgressInfo {
                completed: 2,
                total: 3,
            },
        });
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("✗"));
        assert!(
            lines[0].contains("took"),
            "missing `took` label: {}",
            lines[0]
        );
        assert!(lines[1].contains("boom"));
    }

    #[test]
    fn plain_skipped_includes_reason() {
        let effect = dummy_create_effect();
        let lines = format_plain(&ExecutionEvent::EffectSkipped {
            effect: &effect,
            reason: "dependency 'x' failed",
            progress: ProgressInfo {
                completed: 3,
                total: 3,
            },
        });
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("dependency 'x' failed"));
    }
}
