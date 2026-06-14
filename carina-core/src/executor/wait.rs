//! Polling loop for `Effect::Wait`.
//!
//! The wait construct (carina#2825) is dispatched by carina-core's
//! executor — not by the provider — so providers (including WASM
//! plugins) need no contract change beyond the existing
//! [`Provider::read`] trait method.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` §Executor logic.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::executor::{ExecutionEvent, ExecutionObserver};
use crate::provider::{Provider, ProviderError, ReadRequest};
use crate::resource::{ResourceId, State, Value};
use crate::wait::predicate::WaitPredicate;

/// Terminal result of a wait polling loop.
///
/// `Satisfied` carries the observed target snapshot, `Unsatisfiable`
/// represents a static or dynamic skip reason, `Timeout` is real-time
/// exhaustion, and read-side failures distinguish deletion from other errors.
#[derive(Debug)]
pub enum WaitOutcome {
    Satisfied {
        state: State,
    },
    Unsatisfiable(UnsatisfiableReason),
    Timeout {
        last_attrs: HashMap<String, Value>,
        elapsed: Duration,
    },
    NotFound(ProviderError),
    ReadFailed(ProviderError),
}

/// Why a wait condition cannot become true.
///
/// `DependencyFailed` is found before dispatch from failed explicit
/// dependencies. `NoMutatorRemaining` is found dynamically when only waits
/// remain in flight and no other effect can still dispatch.
#[derive(Debug)]
pub enum UnsatisfiableReason {
    DependencyFailed { binding: String },
    NoMutatorRemaining,
}

pub(crate) fn default_heartbeat_gap(interval: Duration) -> Duration {
    Duration::from_secs(30).max(interval * 5)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InFlightKind {
    Wait,
    NonWait,
}

pub(super) fn unsatisfiable_reason_message(reason: &UnsatisfiableReason) -> String {
    match reason {
        UnsatisfiableReason::DependencyFailed { binding } => {
            format!("dependency '{binding}' failed")
        }
        UnsatisfiableReason::NoMutatorRemaining => "no mutator remaining".to_string(),
    }
}

pub(super) fn wait_failure_message(outcome: &WaitOutcome, target_id: &ResourceId) -> String {
    match outcome {
        WaitOutcome::Timeout {
            last_attrs,
            elapsed,
        } => format!(
            "wait on {} timed out after {:?} (last observed attributes: {:?})",
            target_id, elapsed, last_attrs
        ),
        WaitOutcome::NotFound(err) | WaitOutcome::ReadFailed(err) => err.to_string(),
        WaitOutcome::Satisfied { .. } | WaitOutcome::Unsatisfiable(_) => {
            unreachable!("satisfied and unsatisfiable waits are not failures")
        }
    }
}

pub(super) fn cancel_waits_if_terminal(
    in_flight_kinds: &HashMap<usize, InFlightKind>,
    undispatched_count: usize,
    wait_cancellers: &HashMap<usize, tokio::sync::watch::Sender<bool>>,
) {
    let only_waits = !in_flight_kinds.is_empty()
        && in_flight_kinds
            .values()
            .all(|kind| matches!(kind, InFlightKind::Wait));
    if only_waits && undispatched_count == 0 {
        for cancel in wait_cancellers.values() {
            let _ = cancel.send(true);
        }
    }
}

/// Run the wait polling loop:
///
/// 1. `read()` the target's current state.
/// 2. If the read returns `not_found`, fail immediately with
///    [`ProviderError::NotFound`] — mid-poll target deletion is a
///    real divergence the user should see right away.
/// 3. Evaluate `until` against the returned attribute map.
/// 4. If true, return the captured state.
/// 5. Otherwise, if the elapsed time has passed `timeout`, return
///    [`ProviderError::Timeout`] whose message includes the unmet
///    predicate, the last observed attribute snapshot, and the
///    elapsed time.
/// 6. Otherwise, sleep for `interval` and repeat.
#[allow(clippy::too_many_arguments)]
pub async fn execute_wait_effect(
    provider: &dyn Provider,
    target_id: &ResourceId,
    target_identifier: Option<&str>,
    until: &WaitPredicate,
    timeout: Duration,
    interval: Duration,
    cancel: tokio::sync::watch::Receiver<bool>,
    observer: &dyn ExecutionObserver,
) -> WaitOutcome {
    execute_wait_effect_with_heartbeat_gap(
        provider,
        target_id.name_str(),
        target_id,
        target_identifier,
        until,
        timeout,
        interval,
        cancel,
        observer,
        default_heartbeat_gap(interval),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_wait_effect_with_heartbeat_gap(
    provider: &dyn Provider,
    binding: &str,
    target_id: &ResourceId,
    target_identifier: Option<&str>,
    until: &WaitPredicate,
    timeout: Duration,
    interval: Duration,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    observer: &dyn ExecutionObserver,
    heartbeat_gap: Duration,
) -> WaitOutcome {
    let start = Instant::now();
    let mut last_heartbeat_at: Option<Instant> = None;
    loop {
        let state = match provider
            .read(target_id, target_identifier, ReadRequest)
            .await
        {
            Ok(state) => state,
            Err(err @ ProviderError::NotFound(_)) => return WaitOutcome::NotFound(err),
            Err(err) => return WaitOutcome::ReadFailed(err),
        };
        if !state.exists {
            return WaitOutcome::NotFound(
                ProviderError::not_found(format!(
                    "wait target {} not found (deleted out-of-band?)",
                    target_id
                ))
                .for_resource(target_id.clone()),
            );
        }

        let now = Instant::now();
        let should_emit_heartbeat = last_heartbeat_at
            .map(|last| now.duration_since(last) >= heartbeat_gap)
            .unwrap_or(true);
        if should_emit_heartbeat {
            observer.on_event(&ExecutionEvent::WaitPolling {
                binding,
                target_id,
                elapsed: start.elapsed(),
                last_attrs: &state.attributes,
            });
            last_heartbeat_at = Some(now);
        }

        if until.evaluate(&state.attributes) {
            return WaitOutcome::Satisfied { state };
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return WaitOutcome::Timeout {
                last_attrs: state.attributes,
                elapsed,
            };
        }
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_ok() && *cancel.borrow() {
                    return WaitOutcome::Unsatisfiable(UnsatisfiableReason::NoMutatorRemaining);
                }
            }
            () = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecutionEvent, ExecutionObserver};
    use crate::provider::{
        BoxFuture, CreateRequest, DeleteRequest, ProviderResult, ReadRequest, UpdateRequest,
    };
    use crate::resource::{ConcreteValue, DataSource, Value};
    use crate::wait::predicate::{AttrPath, WaitPredicate};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::watch;

    /// Minimal test provider: returns a queue of pre-canned read
    /// responses in order, then repeats the last one. Counts every
    /// `read` call so tests can assert polling behaviour.
    struct ReadSequenceProvider {
        responses: Mutex<Vec<State>>,
        reads: Mutex<usize>,
    }

    impl ReadSequenceProvider {
        fn new(responses: Vec<State>) -> Self {
            Self {
                responses: Mutex::new(responses),
                reads: Mutex::new(0),
            }
        }

        fn read_count(&self) -> usize {
            *self.reads.lock().unwrap()
        }
    }

    impl Provider for ReadSequenceProvider {
        fn name(&self) -> &str {
            "mock-wait"
        }

        fn read(
            &self,
            _id: &ResourceId,
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async move {
                let mut reads = self.reads.lock().unwrap();
                *reads += 1;
                let mut responses = self.responses.lock().unwrap();
                if responses.len() > 1 {
                    Ok(responses.remove(0))
                } else if let Some(last) = responses.last() {
                    Ok(last.clone())
                } else {
                    Err(ProviderError::api_error("no canned response"))
                }
            })
        }

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    fn state_with_status(status: &str) -> State {
        let mut attrs = HashMap::new();
        attrs.insert(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String(status.to_string())),
        );
        State::existing(ResourceId::new("acm.Certificate", "cert"), attrs)
    }

    fn equals_status(value: &str) -> WaitPredicate {
        WaitPredicate::Equals {
            attr: AttrPath::single("status"),
            value: Value::Concrete(ConcreteValue::String(value.to_string())),
        }
    }

    struct NoopWaitObserver;

    impl ExecutionObserver for NoopWaitObserver {
        fn on_event(&self, _event: &ExecutionEvent) {}
    }

    struct HeartbeatObserver {
        heartbeats: Mutex<Vec<Instant>>,
    }

    impl HeartbeatObserver {
        fn new() -> Self {
            Self {
                heartbeats: Mutex::new(Vec::new()),
            }
        }

        fn heartbeats(&self) -> Vec<Instant> {
            self.heartbeats.lock().unwrap().clone()
        }
    }

    impl ExecutionObserver for HeartbeatObserver {
        fn on_event(&self, event: &ExecutionEvent) {
            if let ExecutionEvent::WaitPolling { .. } = event {
                self.heartbeats.lock().unwrap().push(Instant::now());
            }
        }
    }

    #[test]
    fn default_heartbeat_gap_uses_maximum_of_floor_and_interval_multiple() {
        assert_eq!(
            default_heartbeat_gap(Duration::from_millis(1)),
            Duration::from_secs(30)
        );
        assert_eq!(
            default_heartbeat_gap(Duration::from_secs(60)),
            Duration::from_secs(300)
        );
    }

    #[tokio::test]
    async fn wait_returns_immediately_when_until_already_true() {
        let provider = ReadSequenceProvider::new(vec![state_with_status("ISSUED")]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let observer = NoopWaitObserver;
        let result = execute_wait_effect(
            &provider,
            &target,
            None,
            &pred,
            Duration::from_secs(60),
            Duration::from_millis(10),
            cancel_rx,
            &observer,
        )
        .await;
        let WaitOutcome::Satisfied { state } = result else {
            panic!("expected satisfied wait, got {result:?}");
        };
        assert_eq!(
            state.attributes.get("status"),
            Some(&Value::Concrete(ConcreteValue::String(
                "ISSUED".to_string()
            )))
        );
        assert_eq!(provider.read_count(), 1);
    }

    #[tokio::test]
    async fn wait_polls_until_predicate_becomes_true() {
        let provider = ReadSequenceProvider::new(vec![
            state_with_status("PENDING_VALIDATION"),
            state_with_status("PENDING_VALIDATION"),
            state_with_status("ISSUED"),
        ]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let observer = NoopWaitObserver;
        let result = execute_wait_effect(
            &provider,
            &target,
            None,
            &pred,
            Duration::from_secs(60),
            Duration::from_millis(1),
            cancel_rx,
            &observer,
        )
        .await;
        assert!(
            matches!(result, WaitOutcome::Satisfied { .. }),
            "wait should succeed after 3 reads, got {result:?}"
        );
        assert_eq!(provider.read_count(), 3);
    }

    #[tokio::test]
    async fn wait_returns_timeout_when_predicate_stays_false() {
        let provider = ReadSequenceProvider::new(vec![state_with_status("PENDING_VALIDATION")]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let observer = NoopWaitObserver;
        let result = execute_wait_effect(
            &provider,
            &target,
            None,
            &pred,
            Duration::from_millis(10),
            Duration::from_millis(2),
            cancel_rx,
            &observer,
        )
        .await;
        let WaitOutcome::Timeout {
            last_attrs,
            elapsed,
        } = result
        else {
            panic!("expected Timeout, got {result:?}");
        };
        assert!(
            elapsed >= Duration::from_millis(10),
            "timeout should report elapsed time, got {elapsed:?}"
        );
        assert_eq!(
            last_attrs.get("status"),
            Some(&Value::Concrete(ConcreteValue::String(
                "PENDING_VALIDATION".to_string()
            )))
        );
    }

    #[tokio::test]
    async fn wait_returns_not_found_when_target_disappears() {
        let provider = ReadSequenceProvider::new(vec![
            state_with_status("PENDING_VALIDATION"),
            State::not_found(ResourceId::new("acm.Certificate", "cert")),
        ]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let observer = NoopWaitObserver;
        let result = execute_wait_effect(
            &provider,
            &target,
            None,
            &pred,
            Duration::from_secs(60),
            Duration::from_millis(1),
            cancel_rx,
            &observer,
        )
        .await;
        assert!(
            matches!(result, WaitOutcome::NotFound(_)),
            "expected NotFound, got {result:?}"
        );
    }

    #[tokio::test]
    async fn wait_returns_unsatisfiable_when_cancelled() {
        let provider = ReadSequenceProvider::new(vec![state_with_status("PENDING_VALIDATION")]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let observer = NoopWaitObserver;
        let reads_seen = AtomicUsize::new(0);

        let result = tokio::join!(
            async {
                loop {
                    if provider.read_count() >= 3 {
                        reads_seen.store(provider.read_count(), Ordering::SeqCst);
                        let _ = cancel_tx.send(true);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            },
            execute_wait_effect(
                &provider,
                &target,
                None,
                &pred,
                Duration::from_secs(60),
                Duration::from_millis(1),
                cancel_rx,
                &observer,
            )
        )
        .1;

        assert!(
            reads_seen.load(Ordering::SeqCst) >= 3,
            "test should cancel only after a few polls"
        );
        assert!(
            matches!(
                result,
                WaitOutcome::Unsatisfiable(UnsatisfiableReason::NoMutatorRemaining)
            ),
            "expected unsatisfiable wait after cancellation, got {result:?}"
        );
    }

    #[tokio::test]
    async fn wait_emits_heartbeat_at_max_interval() {
        let provider = ReadSequenceProvider::new(vec![state_with_status("PENDING_VALIDATION")]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let observer = HeartbeatObserver::new();

        let result = execute_wait_effect_with_heartbeat_gap(
            &provider,
            "cert_issued",
            &target,
            None,
            &pred,
            Duration::from_millis(100),
            Duration::from_millis(1),
            cancel_rx,
            &observer,
            Duration::from_millis(5),
        )
        .await;

        assert!(
            matches!(result, WaitOutcome::Timeout { .. }),
            "heartbeat test should end by timeout, got {result:?}"
        );
        let heartbeats = observer.heartbeats();
        assert!(
            !heartbeats.is_empty(),
            "wait should emit at least one WaitPolling heartbeat"
        );
        for pair in heartbeats.windows(2) {
            let gap = pair[1].duration_since(pair[0]);
            assert!(
                gap >= Duration::from_millis(5),
                "heartbeat gap should respect max(30s, interval * 5) with test seam, got {gap:?}"
            );
        }
    }
}
