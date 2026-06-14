//! Polling loop for `Effect::Wait`.
//!
//! The wait construct (carina#2825) is dispatched by carina-core's
//! executor — not by the provider — so providers (including WASM
//! plugins) need no contract change beyond the existing
//! [`Provider::read`] trait method.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` §Executor logic.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};

use crate::deps::find_failed_dependency;
use crate::effect::Effect;
use crate::executor::{ExecutionEvent, ExecutionObserver};
use crate::provider::{Provider, ProviderError, ReadRequest};
use crate::resource::{ResourceId, State, Value};
use crate::value::format_value_user_facing;
use crate::wait::WaitObservation;
use crate::wait::predicate::WaitPredicate;

/// Outcome of polling a Wait effect. The variants distinguish:
/// - `Satisfied`: the wait condition was met; carries the resource state.
/// - `Cancelled`: external cancel observed mid-poll; abort early without failure.
/// - `Unsatisfiable`: the wait can never be satisfied, such as when no
///   remaining mutator can change the target.
/// - `Timeout`: wait window elapsed without satisfying the condition.
/// - `NotFound`: provider reported the resource missing during polling.
/// - `ReadFailed`: provider read failed mid-poll.
#[derive(Debug)]
pub enum WaitOutcome {
    Satisfied {
        state: State,
    },
    Cancelled,
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

/// Signal sent on the watch channel that controls an in-flight Wait future.
/// - `Continue`: initial sentinel, polling proceeds normally.
/// - `NoMutatorRemaining`: dispatching loop has no other effects that could
///   resolve the dependency; the Wait should abort as Unsatisfiable.
/// - `Cancelled`: external cancel observed; the Wait should abort and report
///   `WaitOutcome::Cancelled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitSignal {
    Continue,
    NoMutatorRemaining,
    Cancelled,
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
        } => {
            let observed = format_wait_last_attrs(last_attrs);
            format!(
                "wait on {} timed out after {:?} (last observed attributes: {})",
                target_id, elapsed, observed
            )
        }
        WaitOutcome::NotFound(err) | WaitOutcome::ReadFailed(err) => err.to_string(),
        WaitOutcome::Satisfied { .. } | WaitOutcome::Cancelled | WaitOutcome::Unsatisfiable(_) => {
            unreachable!("satisfied, cancelled, and unsatisfiable waits are not failures")
        }
    }
}

fn format_wait_last_attrs(last_attrs: &HashMap<String, Value>) -> String {
    let mut keys: Vec<_> = last_attrs.keys().collect();
    keys.sort();
    if keys.is_empty() {
        return "no observed attributes".to_string();
    }
    keys.into_iter()
        .map(|key| format!("{key}={}", format_value_user_facing(&last_attrs[key])))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Skip reason emitted on `EffectSkipped` when an effect is dropped or
/// aborted due to cancel observation. Consumers may match on this exact
/// string to render cancelled effects distinctly from cascade failures.
pub(super) const SKIP_REASON_CANCELLED: &str = "cancelled";

/// Signal all in-flight Wait effect cancellers, causing their polling to
/// abort early. Used when cancel observation needs to drain in-flight Wait
/// effects without waiting for their natural timeout.
///
/// `send` errors are intentionally ignored: a dropped receiver means the
/// Wait future has already completed and removed itself from
/// `wait_cancellers`, so signaling that channel is a no-op.
pub(super) fn signal_in_flight_waits(
    wait_cancellers: &HashMap<usize, tokio::sync::watch::Sender<WaitSignal>>,
) {
    for (_idx, tx) in wait_cancellers.iter() {
        // Receiver dropped == Wait future completed; nothing to cancel.
        let _ = tx.send(WaitSignal::Cancelled);
    }
}

pub(super) fn cancel_waits_if_terminal(
    in_flight_kinds: &HashMap<usize, InFlightKind>,
    undispatched_count: usize,
    wait_cancellers: &HashMap<usize, tokio::sync::watch::Sender<WaitSignal>>,
) {
    let only_waits = !in_flight_kinds.is_empty()
        && in_flight_kinds
            .values()
            .all(|kind| matches!(kind, InFlightKind::Wait));
    if only_waits && undispatched_count == 0 {
        for cancel in wait_cancellers.values() {
            let _ = cancel.send(WaitSignal::NoMutatorRemaining);
        }
    }
}

/// Count effects that are not yet dispatched and not effectively
/// pre-skipped by a failed dependency. An effect whose direct dependency is
/// already in `failed_bindings` will be skipped on its next dispatch attempt
/// without producing work; treating it as still "undispatched" for the
/// terminal-with-pending-waits check would keep waits polling forever when
/// their consumer downstream is blocked on a failing sibling. (carina#3544)
pub(super) fn count_effectively_undispatched(
    actionable_indices: &[usize],
    dispatched: &HashSet<usize>,
    effects: &[Effect],
    failed_bindings: &HashSet<String>,
) -> usize {
    actionable_indices
        .iter()
        .filter(|&&idx| !dispatched.contains(&idx))
        .filter(|&&idx| find_failed_dependency(&effects[idx], failed_bindings).is_none())
        .count()
}

/// Wait-aware in-flight future set with typestate-guarded terminal checks.
///
/// You cannot push while a [`TerminalCheck`] or [`NextReady`] is alive because
/// both hold a mutable borrow of this set. The only way to await the next
/// completed future is through `cancel_if_terminal().next_completed()`, so the
/// terminal wait cancellation check cannot be skipped.
#[allow(clippy::type_complexity)]
pub(super) struct WaitAwareInFlight<'fut, R> {
    inner: FuturesUnordered<Pin<Box<dyn Future<Output = (usize, R)> + 'fut>>>,
    kinds: HashMap<usize, InFlightKind>,
    cancellers: HashMap<usize, tokio::sync::watch::Sender<WaitSignal>>,
}

impl<'fut, R> WaitAwareInFlight<'fut, R> {
    /// Create an empty wait-aware in-flight set.
    pub(super) fn new() -> Self {
        Self {
            inner: FuturesUnordered::new(),
            kinds: HashMap::new(),
            cancellers: HashMap::new(),
        }
    }

    /// Return `true` when no futures are currently in flight.
    pub(super) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Return the number of futures currently in flight.
    pub(super) fn len(&self) -> usize {
        self.inner.len()
    }

    /// Signal every in-flight wait to stop because external cancellation was observed.
    pub(super) fn signal_in_flight_waits(&self) {
        signal_in_flight_waits(&self.cancellers);
    }

    /// Dispatch a non-wait future and record its in-flight kind.
    pub(super) fn push_non_wait<F>(&mut self, idx: usize, fut: F)
    where
        F: Future<Output = (usize, R)> + 'fut,
    {
        self.inner.push(Box::pin(fut));
        self.kinds.insert(idx, InFlightKind::NonWait);
    }

    /// Dispatch a wait future and register its cancellation sender.
    pub(super) fn push_wait<MkFut>(&mut self, idx: usize, mk_fut: MkFut)
    where
        MkFut: FnOnce(
            tokio::sync::watch::Receiver<WaitSignal>,
        ) -> Pin<Box<dyn Future<Output = (usize, R)> + 'fut>>,
    {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(WaitSignal::Continue);
        self.cancellers.insert(idx, cancel_tx);
        self.kinds.insert(idx, InFlightKind::Wait);
        self.inner.push(mk_fut(cancel_rx));
    }

    /// Begin the required terminal wait cancellation check before awaiting.
    #[must_use = "TerminalCheck must be consumed via cancel_if_terminal() to advance the loop"]
    pub(super) fn check_terminal(
        &mut self,
        undispatched_count: usize,
    ) -> TerminalCheck<'_, 'fut, R> {
        TerminalCheck {
            parent: self,
            undispatched_count,
        }
    }
}

/// Terminal-check typestate handle that blocks further pushes while alive.
#[must_use = "TerminalCheck must be consumed via cancel_if_terminal()"]
pub(super) struct TerminalCheck<'a, 'fut, R> {
    parent: &'a mut WaitAwareInFlight<'fut, R>,
    undispatched_count: usize,
}

impl<'a, 'fut, R> TerminalCheck<'a, 'fut, R> {
    /// Cancel waits if only waits remain and no undispatched effects exist.
    pub(super) fn cancel_if_terminal(self) -> NextReady<'a, 'fut, R> {
        cancel_waits_if_terminal(
            &self.parent.kinds,
            self.undispatched_count,
            &self.parent.cancellers,
        );
        NextReady {
            parent: self.parent,
        }
    }
}

/// Ready-to-await typestate handle produced after terminal checking.
#[must_use = "NextReady must be awaited or explicitly dropped"]
pub(super) struct NextReady<'a, 'fut, R> {
    parent: &'a mut WaitAwareInFlight<'fut, R>,
}

impl<'a, 'fut, R> NextReady<'a, 'fut, R> {
    /// Await the next completion and clean up its wait-aware bookkeeping.
    pub(super) async fn next_completed(self) -> Option<(usize, R)> {
        let (idx, result) = self.parent.inner.next().await?;
        self.parent.kinds.remove(&idx);
        self.parent.cancellers.remove(&idx);
        Some((idx, result))
    }

    /// Explicitly drop this ready handle without awaiting a completion.
    pub(super) fn drop_without_awaiting(self) {}
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
    cancel: tokio::sync::watch::Receiver<WaitSignal>,
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
    mut cancel: tokio::sync::watch::Receiver<WaitSignal>,
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
            let observation = WaitObservation::new(binding, target_id, until, &state.attributes);
            observer.on_event(&ExecutionEvent::WaitPolling {
                observation,
                elapsed: start.elapsed(),
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
            biased;
            changed = cancel.changed() => {
                if changed.is_ok() {
                    match *cancel.borrow() {
                        WaitSignal::Cancelled => return WaitOutcome::Cancelled,
                        WaitSignal::NoMutatorRemaining => {
                            return WaitOutcome::Unsatisfiable(
                                UnsatisfiableReason::NoMutatorRemaining
                            );
                        }
                        WaitSignal::Continue => {}
                    }
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
    use crate::resource::{ConcreteValue, DataSource, DeferredValue, UnknownReason, Value};
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

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> Vec<String> {
            Vec::new()
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
        heartbeats: Mutex<Vec<(Instant, Vec<AttrPath>)>>,
    }

    impl HeartbeatObserver {
        fn new() -> Self {
            Self {
                heartbeats: Mutex::new(Vec::new()),
            }
        }

        fn heartbeats(&self) -> Vec<(Instant, Vec<AttrPath>)> {
            self.heartbeats.lock().unwrap().clone()
        }
    }

    impl ExecutionObserver for HeartbeatObserver {
        fn on_event(&self, event: &ExecutionEvent) {
            if let ExecutionEvent::WaitPolling { observation, .. } = event {
                let watched_attrs = observation
                    .watched_attrs()
                    .iter()
                    .map(|attr| (*attr).clone())
                    .collect();
                self.heartbeats
                    .lock()
                    .unwrap()
                    .push((Instant::now(), watched_attrs));
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
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
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
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
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
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
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

    #[test]
    fn wait_timeout_failure_message_uses_display_formatting_for_last_attrs() {
        let target = ResourceId::new("acm.Certificate", "cert");
        let outcome = WaitOutcome::Timeout {
            last_attrs: HashMap::from([
                (
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("pending".to_string())),
                ),
                (
                    "arn".to_string(),
                    Value::Concrete(ConcreteValue::String(
                        "arn:aws:acm:1:certificate/abc".to_string(),
                    )),
                ),
            ]),
            elapsed: Duration::from_secs(1),
        };

        let message = wait_failure_message(&outcome, &target);

        assert!(message.contains(
            "last observed attributes: arn=arn:aws:acm:1:certificate/abc, status=pending"
        ));
        assert!(!message.contains("Concrete"));
        assert!(!message.contains("String("));
    }

    #[test]
    fn wait_timeout_failure_message_handles_empty_last_attrs() {
        let target = ResourceId::new("acm.Certificate", "cert");
        let outcome = WaitOutcome::Timeout {
            last_attrs: HashMap::new(),
            elapsed: Duration::from_secs(1),
        };

        let message = wait_failure_message(&outcome, &target);

        assert!(message.contains("last observed attributes: no observed attributes"));
    }

    #[test]
    fn wait_timeout_failure_message_handles_unknown_and_deferred_attrs() {
        let target = ResourceId::new("acm.Certificate", "cert");
        let outcome = WaitOutcome::Timeout {
            last_attrs: HashMap::from([
                (
                    "status".to_string(),
                    Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
                ),
                (
                    "target".to_string(),
                    Value::resource_ref("vpc", "id", vec![]),
                ),
            ]),
            elapsed: Duration::from_secs(1),
        };

        let message = wait_failure_message(&outcome, &target);

        assert!(
            message.contains(
                "last observed attributes: status=(known after upstream apply), target=vpc.id"
            ),
            "{message}"
        );
        assert!(!message.contains("Deferred"));
        assert!(!message.contains("Unknown("));
        assert!(!message.contains("ResourceRef"));
    }

    #[tokio::test]
    async fn wait_returns_not_found_when_target_disappears() {
        let provider = ReadSequenceProvider::new(vec![
            state_with_status("PENDING_VALIDATION"),
            State::not_found(ResourceId::new("acm.Certificate", "cert")),
        ]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
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
        let (cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
        let observer = NoopWaitObserver;
        let reads_seen = AtomicUsize::new(0);

        let result = tokio::join!(
            async {
                loop {
                    if provider.read_count() >= 3 {
                        reads_seen.store(provider.read_count(), Ordering::SeqCst);
                        let _ = cancel_tx.send(WaitSignal::NoMutatorRemaining);
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
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
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
        assert_eq!(heartbeats[0].1, vec![AttrPath::single("status")]);
        for pair in heartbeats.windows(2) {
            let gap = pair[1].0.duration_since(pair[0].0);
            assert!(
                gap >= Duration::from_millis(5),
                "heartbeat gap should respect max(30s, interval * 5) with test seam, got {gap:?}"
            );
        }
    }
}
