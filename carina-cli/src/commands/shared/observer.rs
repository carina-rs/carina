//! CLI observer that formats execution events for the shared progress backend.

use std::path::{Path, PathBuf};
use std::time::Duration;

use carina_core::executor::{ExecutionEvent, ExecutionObserver};
use carina_core::plan::Plan;
use carina_core::provider::PartialReadDiagnostic;
use carina_core::resource::ModuleSource;
use carina_core::value::format_value_user_facing;
use carina_core::wait::WaitObservation;
use colored::Colorize;

use crate::commands::shared::progress::{
    EffectProgressKey, FailedOutcome, NonFailureOutcome, ProgressBackend, ProgressSink,
    format_duration, format_progress,
};
use crate::display::format_effect;

/// Render the shared diagnostic details for a mutation whose follow-up read
/// only partially succeeded.
pub(crate) fn partial_read_detail_lines(diagnostic: &PartialReadDiagnostic) -> [String; 3] {
    let missing = if diagnostic.missing_attributes().is_empty() {
        "(none)".to_string()
    } else {
        diagnostic.missing_attributes().join(", ")
    };
    [
        format!("reason: {}", diagnostic.reason()),
        format!("missing attributes: {missing}"),
        "state recorded; re-run apply to complete the read".to_string(),
    ]
}

/// CLI observer that prints colored progress output.
pub(crate) struct CliObserver<S = ModuleSource> {
    progress: ProgressBackend<EffectProgressKey<S>>,
    project_dir: PathBuf,
}

impl<S> CliObserver<S> {
    /// Create a new observer using stderr's terminal state.
    pub(crate) fn new(_plan: &Plan<S>, project_dir: &Path) -> Self
    where
        S: 'static,
    {
        Self {
            progress: ProgressBackend::new(ProgressSink::Human),
            project_dir: project_dir.to_path_buf(),
        }
    }
}

impl<S> ExecutionObserver<S> for CliObserver<S> {
    fn on_event(&self, event: &ExecutionEvent<S>) {
        handle_event(&self.progress, event, &self.project_dir);
    }
}

fn handle_event<S>(
    progress_backend: &ProgressBackend<EffectProgressKey<S>>,
    event: &ExecutionEvent<S>,
    project_dir: &Path,
) {
    match event {
        ExecutionEvent::Waiting { .. } => {
            // Dynamic display: don't show waiting resources.
            // They will appear when they start executing.
        }
        ExecutionEvent::EffectStarted { effect, start: _ } => {
            progress_backend.start_effect(EffectProgressKey::new(effect), format_effect(effect));
        }
        ExecutionEvent::EffectInterrupted { effect, progress } => {
            let key = EffectProgressKey::new(effect);
            let counter = format_progress(progress).dimmed();
            let outcome = NonFailureOutcome::unknown(format!(
                "{} - interrupted (outcome unknown) {}",
                format_effect(effect),
                counter
            ));
            progress_backend.finish(key, outcome);
        }
        ExecutionEvent::EffectSucceeded {
            effect,
            duration,
            progress,
            ..
        } => {
            let key = EffectProgressKey::new(effect);
            let timing = format!("took {}", format_duration(*duration)).dimmed();
            let counter = format_progress(progress).dimmed();
            let outcome = NonFailureOutcome::success(format!(
                "{} {} {}",
                format_effect(effect),
                timing,
                counter
            ));
            progress_backend.finish(key, outcome);
        }
        ExecutionEvent::EffectPartiallySucceeded {
            effect,
            diagnostic,
            duration,
            progress,
            ..
        } => {
            let key = EffectProgressKey::new(effect);
            let timing = format!("took {}", format_duration(*duration)).dimmed();
            let counter = format_progress(progress).dimmed();
            let [reason, missing, state_recorded] = partial_read_detail_lines(diagnostic);
            let outcome = NonFailureOutcome::partial(
                format!("{} (partial) {} {}", format_effect(effect), timing, counter),
                [reason, missing, state_recorded],
            );
            progress_backend.finish(key, outcome);
        }
        ExecutionEvent::EffectFailed {
            effect,
            failure,
            recovery,
            duration,
            progress,
        } => {
            let key = EffectProgressKey::new(effect);
            let timing = format!("took {}", format_duration(*duration)).dimmed();
            let counter = format_progress(progress).dimmed();
            let mut details = vec![failure.to_string()];
            if let Some(recovery) = recovery {
                details.push(recovery.display_with_project_dir(project_dir).to_string());
            }
            let outcome = FailedOutcome::new(
                format!("{} {} {}", format_effect(effect), timing, counter),
                details,
            );
            progress_backend.finish_failed(key, outcome);
        }
        ExecutionEvent::EffectSkipped {
            effect,
            reason,
            progress,
        } => {
            let key = EffectProgressKey::new(effect);
            let counter = format_progress(progress).dimmed();
            let outcome = NonFailureOutcome::skipped(format!(
                "{} - {} {}",
                format_effect(effect),
                reason,
                counter
            ));
            // Skipped effects may not have a spinner (they were never started).
            progress_backend.finish(key, outcome);
        }
        ExecutionEvent::WaitPolling {
            effect,
            observation,
            elapsed,
        } => {
            let key = EffectProgressKey::new(effect);
            progress_backend.in_flight_line(&key, format_wait_polling_line(observation, *elapsed));
        }
        ExecutionEvent::CascadeUpdateSucceeded { id } => {
            progress_backend
                .outcome_line(NonFailureOutcome::success(format!("Update {id} (cascade)")));
        }
        ExecutionEvent::CascadeUpdateFailed { id, error } => {
            progress_backend.failed_line(FailedOutcome::new(
                format!("Update {id} (cascade)"),
                [error.to_string()],
            ));
        }
        ExecutionEvent::RenameSucceeded { id, from, to } => {
            progress_backend.outcome_line(NonFailureOutcome::success(format!(
                "Rename {id} \"{from}\" → \"{to}\""
            )));
        }
        ExecutionEvent::RenameFailed { id, error } => {
            progress_backend.failed_line(FailedOutcome::new(
                format!("Rename {id}"),
                [error.to_string()],
            ));
        }
        ExecutionEvent::RefreshStarted => {
            progress_backend.section_header(format!(
                "{}",
                "Refreshing uncertain resource states...".cyan()
            ));
        }
        ExecutionEvent::RefreshSucceeded { id } => {
            progress_backend.outcome_line(NonFailureOutcome::success(format!("Refresh {id}")));
        }
        ExecutionEvent::RefreshFailed { id, error } => {
            progress_backend.info_line(format!(
                "{} Refresh {} - {}",
                "!".yellow(),
                id,
                carina_core::executor::render_error_chain(*error)
            ));
        }
        ExecutionEvent::ModuleConstraintFailed { failure } => {
            progress_backend.failed_line(FailedOutcome::new(
                "Module constraint",
                [failure.to_string()],
            ));
        }
    }
}

fn format_wait_polling_line(observation: &WaitObservation, elapsed: Duration) -> String {
    let observed = format_wait_observed_attr(observation);
    format!(
        "~ {}: waited {}, {}",
        observation.binding(),
        format_duration(elapsed),
        observed
    )
}

fn format_wait_observed_attr(observation: &WaitObservation) -> String {
    if let Some((attr, value)) = observation.primary() {
        let key = attr.segments().join(".");
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
    use std::collections::{HashMap, HashSet};
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::commands::shared::progress::wait_for_output_contains;
    use carina_core::effect::Effect;
    use carina_core::effect::EffectGeneration;
    use carina_core::executor::ProgressInfo;
    use carina_core::resource::ResolvedResourceId;
    use carina_core::resource::{
        AccessPath, ApplyTimeUnknown, ConcreteValue, DeferredValue, ResolvedResource, Resource,
        ResourceId, Value,
    };
    use carina_core::wait::predicate::{AttrPath, WaitPredicate};
    use indexmap::IndexMap;
    use indicatif::ProgressDrawTarget;

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

    fn plain_output(event: &ExecutionEvent) -> String {
        plain_output_at(event, Path::new("test-project"))
    }

    fn plain_output_at(event: &ExecutionEvent, project_dir: &Path) -> String {
        let output = SharedWriter::default();
        let backend = ProgressBackend::plain(output.clone());
        handle_event(&backend, event, project_dir);
        strip_ansi(&String::from_utf8(output.0.lock().unwrap().clone()).unwrap())
    }

    fn plain_failure_output_at(
        effect: &Effect,
        kind: carina_core::executor::EffectFailureKind,
        context: carina_core::executor::FailureEventContext<'_>,
        project_dir: &Path,
    ) -> String {
        struct Renderer<'a> {
            backend: ProgressBackend<EffectProgressKey>,
            project_dir: &'a Path,
        }
        impl ExecutionObserver for Renderer<'_> {
            fn on_event(&self, event: &ExecutionEvent<'_>) {
                handle_event(&self.backend, event, self.project_dir);
            }
        }
        let output = SharedWriter::default();
        let observer = Renderer {
            backend: ProgressBackend::plain(output.clone()),
            project_dir,
        };
        carina_core::executor::FailureSink::default().record(
            &observer,
            carina_core::executor::FailureReport::Effect {
                effect,
                kind,
                context,
            },
        );
        strip_ansi(&String::from_utf8(output.0.lock().unwrap().clone()).unwrap())
    }

    fn strip_ansi(input: &str) -> String {
        let mut output = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.next_if_eq(&'[').is_some() {
                for code in chars.by_ref() {
                    if code.is_ascii_alphabetic() || code == '~' {
                        break;
                    }
                }
            } else {
                output.push(ch);
            }
        }
        output
    }

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    fn dummy_create_effect() -> Effect {
        Effect::Create(resolved(Resource::new("aws.s3.Bucket", "demo")))
    }

    fn dummy_wait_effect() -> Effect {
        let target = resolved(Resource::new("aws.acm.Certificate", "demo"));
        Effect::Wait {
            identity: carina_core::resource::ResourceIdentity::try_from("demo_ready".to_string())
                .unwrap(),
            target_id: carina_core::wait::ManagedWaitTarget::for_test(&target),
            until: equals_predicate(AttrPath::single("status"), "issued"),
            until_surface: "demo.status == issued".to_string(),
            timeout: Duration::from_secs(300),
            interval: Duration::from_secs(5),
            explicit_dependencies: HashSet::new(),
        }
    }

    fn dummy_delete_effect(generation: EffectGeneration) -> Effect {
        Effect::Delete {
            id: ResolvedResourceId::with_provider_identity("mock", "test.resource", "same", None),
            identifier: crate::fixture_applied_identifier("mock-id"),
            generation,
            directives: Default::default(),
            binding: Some("same".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }
    }

    fn wait_observation<'a>(
        target_id: &'a ResourceId,
        predicate: &'a WaitPredicate,
        last_attrs: &'a HashMap<String, Value>,
    ) -> WaitObservation<'a> {
        WaitObservation::new("demo_ready", target_id, predicate, last_attrs)
    }

    fn equals_predicate(attr: AttrPath, value: &str) -> WaitPredicate {
        WaitPredicate::equals(
            attr,
            Value::Concrete(ConcreteValue::String(value.to_string().into())),
        )
        .unwrap()
    }

    #[test]
    fn wait_observed_attr_is_deterministic() {
        let target_id = ResourceId::with_identity("aws.test.Resource", "demo");
        let attrs = HashMap::from([
            (
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("pending".into())),
            ),
            (
                "arn".to_string(),
                Value::Concrete(ConcreteValue::String("arn:demo".into())),
            ),
        ]);
        let predicate = equals_predicate(AttrPath::single("missing"), "present");
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(format_wait_observed_attr(&observation), "arn=arn:demo");
    }

    #[test]
    fn wait_observed_attr_uses_predicate_attr_when_present() {
        let target_id = ResourceId::with_identity("aws.test.Resource", "demo");
        let status = AttrPath::single("status");
        let predicate = equals_predicate(status, "pending");
        let attrs = HashMap::from([
            (
                "arn".to_string(),
                Value::Concrete(ConcreteValue::String("arn:demo".into())),
            ),
            (
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("pending".into())),
            ),
        ]);
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(format_wait_observed_attr(&observation), "status=pending");
    }

    #[test]
    fn wait_observed_attr_formats_multi_segment_predicate_attr() {
        let target_id = ResourceId::with_identity("aws.test.Resource", "demo");
        let renewal_status =
            AttrPath::try_new(vec!["renewal_summary".into(), "renewal_status".into()]).unwrap();
        let predicate = equals_predicate(renewal_status, "PENDING");
        let attrs = HashMap::from([(
            "renewal_summary".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "renewal_status".to_string(),
                Value::Concrete(ConcreteValue::String("PENDING".into())),
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
        let target_id = ResourceId::with_identity("aws.test.Resource", "demo");
        let xyz = AttrPath::single("xyz");
        let predicate = equals_predicate(xyz, "present");
        let attrs = HashMap::from([
            (
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("pending".into())),
            ),
            (
                "arn".to_string(),
                Value::Concrete(ConcreteValue::String("arn:demo".into())),
            ),
        ]);
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(format_wait_observed_attr(&observation), "arn=arn:demo");
    }

    #[test]
    fn wait_observed_attr_uses_display_formatting() {
        let target_id = ResourceId::with_identity("aws.test.Resource", "demo");
        let attrs = HashMap::from([(
            "arn".to_string(),
            Value::Concrete(ConcreteValue::String(
                "arn:aws:acm:1:certificate/abc".into(),
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
        let target_id = ResourceId::with_identity("aws.test.Resource", "demo");
        let attrs = HashMap::from([(
            "status".to_string(),
            Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
                path: AccessPath::with_fields("bootstrap", "network", vec!["status".into()]),
            })),
        )]);
        let predicate = equals_predicate(AttrPath::single("missing"), "present");
        let observation = wait_observation(&target_id, &predicate, &attrs);

        assert_eq!(
            format_wait_observed_attr(&observation),
            "status=(known after upstream apply: bootstrap.network.status)"
        );
    }

    #[test]
    fn plain_started_and_waiting_write_nothing_immediately() {
        let effect = dummy_create_effect();
        assert!(plain_output(&ExecutionEvent::effect_started_for_test(&effect)).is_empty());
        assert!(
            plain_output(&ExecutionEvent::Waiting {
                effect: &effect,
                pending_dependencies: vec!["x".into()],
            })
            .is_empty()
        );
    }

    #[test]
    fn plain_observer_heartbeat_precedes_success_outcome() {
        let output = SharedWriter::default();
        let backend =
            ProgressBackend::plain_with_interval(output.clone(), Duration::from_millis(10));
        let effect = dummy_create_effect();
        let label = format_effect(&effect);

        handle_event(
            &backend,
            &ExecutionEvent::effect_started_for_test(&effect),
            Path::new("test-project"),
        );
        wait_for_output_contains(&output.0, &format!("Still running: {label}"));

        handle_event(
            &backend,
            &ExecutionEvent::EffectSucceeded {
                effect: &effect,
                state: None,
                duration: Duration::from_millis(123),
                progress: ProgressInfo {
                    completed: 1,
                    total: 1,
                },
            },
            Path::new("test-project"),
        );

        drop(backend);
        let rendered = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        let heartbeat = rendered.rfind(&format!("Still running: {label}")).unwrap();
        let outcome = rendered.find('✓').unwrap();
        assert!(
            heartbeat < outcome,
            "observer heartbeat must precede the outcome: {rendered:?}",
        );
    }

    #[test]
    fn plain_interrupted_finishes_effect_and_stops_heartbeats() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::plain(output.clone());
        let effect = dummy_create_effect();

        handle_event(
            &backend,
            &ExecutionEvent::effect_started_for_test(&effect),
            Path::new("test-project"),
        );
        handle_event(
            &backend,
            &ExecutionEvent::EffectInterrupted {
                effect: &effect,
                progress: ProgressInfo {
                    completed: 1,
                    total: 3,
                },
            },
            Path::new("test-project"),
        );

        assert_eq!(backend.active_count(), 0);
        let after_interrupted = output.0.lock().unwrap().clone();
        assert_eq!(
            String::from_utf8(after_interrupted.clone()).unwrap(),
            "  ? Create aws.s3.Bucket demo - interrupted (outcome unknown) 1/3\n"
        );

        backend.emit_heartbeats_at(std::time::Instant::now() + Duration::from_secs(3_600));
        assert_eq!(*output.0.lock().unwrap(), after_interrupted);
    }

    #[test]
    fn wait_polling_resets_plain_generic_heartbeat_schedule() {
        let output = SharedWriter::default();
        let interval = Duration::from_secs(30);
        let backend = ProgressBackend::plain_with_interval(output.clone(), interval);
        let effect = dummy_wait_effect();
        let key = EffectProgressKey::new(&effect);
        let target_id = ResourceId::with_identity("aws.acm.Certificate", "demo");
        let predicate = equals_predicate(AttrPath::single("status"), "issued");
        let attrs = HashMap::from([(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("pending".into())),
        )]);
        let observation = wait_observation(&target_id, &predicate, &attrs);

        handle_event(
            &backend,
            &ExecutionEvent::effect_started_for_test(&effect),
            Path::new("test-project"),
        );
        handle_event(
            &backend,
            &ExecutionEvent::WaitPolling {
                effect: &effect,
                observation,
                elapsed: Duration::from_secs(30),
            },
            Path::new("test-project"),
        );
        let before_generic_heartbeat = output.0.lock().unwrap().clone();
        let last_reported = backend.plain_last_reported(&key).unwrap();

        backend.emit_heartbeats_at(last_reported + interval - Duration::from_millis(1));

        assert_eq!(*output.0.lock().unwrap(), before_generic_heartbeat);
        assert!(
            String::from_utf8(before_generic_heartbeat)
                .unwrap()
                .contains("~ demo_ready: waited 30.0s, status=pending")
        );
    }

    #[test]
    fn plain_emits_one_line_per_succeeded() {
        let effect = dummy_create_effect();
        let output = plain_output(&ExecutionEvent::EffectSucceeded {
            effect: &effect,
            state: None,
            duration: Duration::from_millis(123),
            progress: ProgressInfo {
                completed: 1,
                total: 3,
            },
        });
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 1);
        let line = lines[0];
        assert!(line.contains("✓"), "missing check mark: {line}");
        assert!(line.contains("Create"), "missing verb: {line}");
        assert!(line.contains("demo"), "missing resource name: {line}");
        assert!(line.contains("took"), "missing `took` label: {line}");
        assert!(line.contains("1/3"), "missing counter: {line}");
    }

    #[test]
    fn plain_partial_success_detail_lines_are_stable() {
        let effect = dummy_create_effect();
        let state = carina_core::resource::State::not_found(ResourceId::with_identity(
            "aws.s3.Bucket",
            "demo",
        ));
        let diagnostic = carina_core::provider::PartialReadDiagnostic::new(
            "provider read was incomplete".to_string(),
            vec!["arn".to_string(), "versioning".to_string()],
        )
        .expect("fixture has missing attributes");

        let output = plain_output(&ExecutionEvent::EffectPartiallySucceeded {
            effect: &effect,
            state: &state,
            diagnostic: &diagnostic,
            duration: Duration::from_millis(123),
            progress: ProgressInfo {
                completed: 1,
                total: 3,
            },
        });
        let lines: Vec<_> = output.lines().collect();

        assert_eq!(
            lines,
            [
                "  ⚠ Create aws.s3.Bucket demo (partial) took 0.1s 1/3",
                "      → reason: provider read was incomplete",
                "      → missing attributes: arn, versioning",
                "      → state recorded; re-run apply to complete the read",
            ]
        );
    }

    #[test]
    fn partial_read_detail_lines_render_none_for_empty_missing_attributes() {
        let diagnostic: PartialReadDiagnostic = serde_json::from_value(serde_json::json!({
            "reason": "provider returned no attributes",
            "missing_attributes": [],
        }))
        .expect("serialized diagnostic shape is valid");

        assert_eq!(
            partial_read_detail_lines(&diagnostic),
            [
                "reason: provider returned no attributes",
                "missing attributes: (none)",
                "state recorded; re-run apply to complete the read",
            ]
        );
    }

    #[test]
    fn cycle_members_and_provider_chains_are_rendered_once_across_progress_and_summary() {
        use carina_core::executor::{
            EffectFailureKind, FailureEventContext, FailureReport, FailureSink,
        };
        use std::collections::{HashMap, HashSet};

        struct Renderer(ProgressBackend<EffectProgressKey>);
        impl ExecutionObserver for Renderer {
            fn on_event(&self, event: &ExecutionEvent<'_>) {
                handle_event(&self.0, event, Path::new("project"));
            }
        }
        let effects: Vec<Effect> = ["blocked", "a", "b", "c", "provider"]
            .into_iter()
            .map(|name| Effect::Create(ResolvedResource::new(Resource::new("test.resource", name))))
            .collect();
        let deps = HashMap::from([
            (0, HashSet::from([3])),
            (1, HashSet::from([2])),
            (2, HashSet::from([3])),
            (3, HashSet::from([1])),
        ]);
        let output = SharedWriter::default();
        let observer = Renderer(ProgressBackend::plain(output.clone()));
        let mut sink = FailureSink::default();
        let mut classified = carina_core::executor::classify_stalled_effects(
            &effects,
            &deps,
            &HashSet::from([0, 1, 2, 3]),
            &HashSet::new(),
        );
        classified.push((
            4,
            EffectFailureKind::provider_with_context(
                "delete failed",
                std::io::Error::other("distinctive provider root"),
            ),
        ));
        for (position, (idx, kind)) in classified.into_iter().enumerate() {
            sink.record(
                &observer,
                FailureReport::Effect {
                    effect: &effects[idx],
                    kind,
                    context: FailureEventContext {
                        recovery: None,
                        duration: Duration::ZERO,
                        progress: ProgressInfo {
                            completed: position + 1,
                            total: effects.len(),
                        },
                    },
                },
            );
        }
        let streamed = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        let summary =
            crate::error::ExecutionFailureReport::new("Apply failed.".into(), sink.failures())
                .to_string();
        let stderr = format!("{streamed}Error: {summary}");
        let members = "{create test.resource.a, create test.resource.b, create test.resource.c}";
        assert_eq!(stderr.matches(members).count(), 1, "{stderr}");
        assert!(
            stderr.find(members).unwrap()
                < stderr
                    .find("blocked by the dependency cycle above")
                    .unwrap(),
            "{stderr}"
        );
        assert_eq!(
            stderr.matches("distinctive provider root").count(),
            1,
            "{stderr}"
        );
        assert!(
            summary.contains("create test.resource.provider: provider error"),
            "{summary}"
        );
        assert!(
            summary.contains("create test.resource.blocked: blocked by the dependency cycle above"),
            "{summary}"
        );
    }

    #[test]
    fn plain_failed_includes_error_on_second_line() {
        let effect = dummy_create_effect();
        let output = plain_failure_output_at(
            &effect,
            carina_core::executor::EffectFailureKind::message("boom"),
            carina_core::executor::FailureEventContext {
                recovery: None,
                duration: Duration::from_millis(50),
                progress: ProgressInfo {
                    completed: 2,
                    total: 3,
                },
            },
            Path::new("test-project"),
        );
        let lines: Vec<_> = output.lines().collect();
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
    fn outcome_unknown_failure_uses_project_scoped_plan_recovery() {
        let effect = dummy_create_effect();
        let recovery =
            carina_core::plan::PlanError::create_outcome_unknown(effect.resource_id().clone());
        let output = plain_failure_output_at(
            &effect,
            carina_core::executor::EffectFailureKind::message("guest call timed out"),
            carina_core::executor::FailureEventContext {
                recovery: Some(&recovery),
                duration: Duration::from_millis(50),
                progress: ProgressInfo {
                    completed: 1,
                    total: 1,
                },
            },
            Path::new("infra/production project"),
        );

        assert!(output.contains("if it exists: import { to = aws.s3.Bucket \"demo\" id = \"…\" }"));
        assert!(output.contains(
            "if it doesn't: carina state forget aws.s3.Bucket demo 'infra/production project'"
        ));
    }

    #[test]
    fn refresh_started_uses_an_empty_line_and_column_zero_header() {
        assert_eq!(
            plain_output(&ExecutionEvent::RefreshStarted),
            "\nRefreshing uncertain resource states...\n",
        );
    }

    #[test]
    fn identical_effect_labels_keep_distinct_live_spinners() {
        let first = dummy_delete_effect(EffectGeneration::Deposed(
            carina_state::DeposedKey::new_unique(),
        ));
        let second = dummy_delete_effect(EffectGeneration::Deposed(
            carina_state::DeposedKey::new_unique(),
        ));
        assert_eq!(format_effect(&first), format_effect(&second));

        let backend = ProgressBackend::tty(
            ProgressDrawTarget::hidden(),
            std::io::sink(),
            std::io::sink(),
        );
        handle_event(
            &backend,
            &ExecutionEvent::effect_started_for_test(&first),
            Path::new("test-project"),
        );
        handle_event(
            &backend,
            &ExecutionEvent::effect_started_for_test(&second),
            Path::new("test-project"),
        );

        assert_eq!(
            backend.active_count(),
            2,
            "equal display labels must not replace each other's spinner",
        );

        handle_event(
            &backend,
            &ExecutionEvent::EffectSucceeded {
                effect: &first,
                state: None,
                duration: Duration::from_millis(1),
                progress: ProgressInfo {
                    completed: 1,
                    total: 2,
                },
            },
            Path::new("test-project"),
        );
        assert_eq!(
            backend.active_count(),
            1,
            "finishing one equal-looking effect must leave the other spinner active",
        );

        handle_event(
            &backend,
            &ExecutionEvent::EffectSucceeded {
                effect: &second,
                state: None,
                duration: Duration::from_millis(1),
                progress: ProgressInfo {
                    completed: 2,
                    total: 2,
                },
            },
            Path::new("test-project"),
        );
        assert_eq!(backend.active_count(), 0);
    }

    #[test]
    fn plain_skipped_includes_reason() {
        let effect = dummy_create_effect();
        let output = plain_output(&ExecutionEvent::EffectSkipped {
            effect: &effect,
            reason: "dependency 'x' failed",
            progress: ProgressInfo {
                completed: 3,
                total: 3,
            },
        });
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("dependency 'x' failed"));
    }
}
