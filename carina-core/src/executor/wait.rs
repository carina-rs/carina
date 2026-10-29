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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;

use crate::effect::Effect;
use crate::executor::scheduler::FailureView;
use crate::executor::{ExecutionEvent, ExecutionObserver, ProgressInfo};
use crate::provider::{AppliedIdentifier, Provider, ProviderError, ReadRequest};
#[cfg(test)]
use crate::provider::{ReadOutcome, fixture_read_outcome_for_identifier};
#[cfg(test)]
use crate::resource::ResolvedResourceId;
use crate::resource::{Current, ResourceId, State, Value};
use crate::shutdown::{CleanupAwareLoop, CleanupInterrupted, LoopStep};
use crate::value::format_value_user_facing;
use crate::wait::predicate::WaitPredicate;
use crate::wait::{ManagedWaitTarget, WaitObservation};

pub(crate) type WaitIdentifierResolver<'a> =
    dyn Fn(&ResourceId) -> Option<AppliedIdentifier> + Send + Sync + 'a;
pub(crate) type SharedWaitIdentifiers = Arc<Mutex<HashMap<ResourceId, Option<AppliedIdentifier>>>>;

pub(super) fn initial_wait_identifiers(
    managed_current: &HashMap<ResourceId, Current>,
) -> SharedWaitIdentifiers {
    let identifiers: HashMap<ResourceId, Option<AppliedIdentifier>> = managed_current
        .iter()
        .map(|(id, current)| {
            let identifier = match current {
                Current::Absent(_state) => None,
                Current::Present(present) => Some(present.identifier().clone()),
                Current::CreateInFlight(_pending) => None,
                Current::MissingIdentifier(_state) => None,
            };
            (id.clone(), identifier)
        })
        .collect();
    Arc::new(Mutex::new(identifiers))
}

pub(crate) fn resolve_wait_identifier(
    shared: &SharedWaitIdentifiers,
    target_id: &ResourceId,
) -> Option<AppliedIdentifier> {
    shared
        .lock()
        .expect("wait identifier map poisoned")
        .get(target_id)
        .cloned()
        .flatten()
}

/// Outcome of polling a Wait effect. The variants distinguish:
/// - `Satisfied`: the wait condition was met; carries the resource state.
/// - `Cancelled`: external cancel observed mid-poll; abort early without failure.
/// - `Abandoned`: cleanup priority interrupted the wait; abort without failure.
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
    Abandoned,
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

/// Returns the longer of 30s and five polling intervals to limit heartbeat
/// spam. In the loop, the triggering `now` sample is taken on the first poll
/// at or after this period has elapsed since the last emit.
pub(crate) fn default_heartbeat_cadence(interval: Duration) -> Duration {
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
        WaitOutcome::Satisfied { .. }
        | WaitOutcome::Cancelled
        | WaitOutcome::Abandoned
        | WaitOutcome::Unsatisfiable(_) => unreachable!(
            "satisfied, cancelled, abandoned, and unsatisfiable waits are not failures"
        ),
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
    for tx in wait_cancellers.values() {
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
/// already visible through `view` will be skipped on its next dispatch attempt
/// without producing work; treating it as still "undispatched" for the
/// terminal-with-pending-waits check would keep waits polling forever when
/// their consumer downstream is blocked on a failing sibling. (carina#3544)
pub(super) fn count_effectively_undispatched<S>(
    actionable_indices: &[usize],
    dispatched: &HashSet<usize>,
    view: &FailureView<'_, S>,
) -> usize {
    actionable_indices
        .iter()
        .filter(|&&idx| !dispatched.contains(&idx))
        .filter(|&&idx| !view.is_effectively_pre_skipped(idx))
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
    started: Arc<Mutex<HashMap<usize, ProgressInfo>>>,
}

/// Records the progress value at the same point an in-flight effect emits its
/// `EffectStarted` event.
pub(super) struct InFlightStart {
    idx: usize,
    started: Arc<Mutex<HashMap<usize, ProgressInfo>>>,
}

impl InFlightStart {
    pub(super) fn emit_started<S>(
        self,
        observer: &dyn ExecutionObserver<S>,
        effect: &Effect<S>,
        progress: ProgressInfo,
    ) {
        self.started
            .lock()
            .expect("in-flight start registry poisoned")
            .insert(self.idx, progress);
        observer.on_event(&ExecutionEvent::EffectStarted { effect });
    }
}

impl<'fut, R> WaitAwareInFlight<'fut, R> {
    /// Create an empty wait-aware in-flight set.
    pub(super) fn new() -> Self {
        Self {
            inner: FuturesUnordered::new(),
            kinds: HashMap::new(),
            cancellers: HashMap::new(),
            started: Arc::new(Mutex::new(HashMap::new())),
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
    pub(super) fn push_non_wait<MkFut, F>(&mut self, idx: usize, make_future: MkFut)
    where
        MkFut: FnOnce(InFlightStart) -> F,
        F: Future<Output = (usize, R)> + 'fut,
    {
        let start = InFlightStart {
            idx,
            started: Arc::clone(&self.started),
        };
        self.inner.push(Box::pin(make_future(start)));
        self.kinds.insert(idx, InFlightKind::NonWait);
    }

    /// Dispatch a wait future and register its cancellation sender.
    pub(super) fn push_wait<MkFut>(&mut self, idx: usize, mk_fut: MkFut)
    where
        MkFut: FnOnce(
            tokio::sync::watch::Receiver<WaitSignal>,
            InFlightStart,
        ) -> Pin<Box<dyn Future<Output = (usize, R)> + 'fut>>,
    {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(WaitSignal::Continue);
        let start = InFlightStart {
            idx,
            started: Arc::clone(&self.started),
        };
        self.cancellers.insert(idx, cancel_tx);
        self.kinds.insert(idx, InFlightKind::Wait);
        self.inner.push(mk_fut(cancel_rx, start));
    }

    /// Drop every unresolved future and return only effects that had emitted
    /// `EffectStarted`, paired with the progress value used by their terminal
    /// event siblings.
    pub(super) fn abandon(self) -> Vec<(usize, ProgressInfo)> {
        let mut interrupted = {
            let started = self
                .started
                .lock()
                .expect("in-flight start registry poisoned");
            self.kinds
                .keys()
                .filter_map(|idx| started.get(idx).copied().map(|progress| (*idx, progress)))
                .collect::<Vec<_>>()
        };
        interrupted.sort_by_key(|(idx, _)| *idx);
        interrupted
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
        self.parent
            .started
            .lock()
            .expect("in-flight start registry poisoned")
            .remove(&idx);
        Some((idx, result))
    }

    /// Explicitly drop this ready handle without awaiting a completion.
    pub(super) fn drop_without_awaiting(self) {}
}

/// Poll `provider.read()` until `until` is satisfied, the wait fails or times
/// out, cancellation makes it unsatisfiable, or cleanup priority abandons it.
///
/// * `cleanup_loop` – shutdown state sampled immediately before every poll. If
///   cleanup priority is already active, the function returns
///   [`WaitOutcome::Abandoned`] before resolving the target identifier or
///   calling `provider.read()`. Cleanup priority can also interrupt either an
///   in-flight provider read or the following interval sleep.
/// * `timeout` – maximum elapsed time before returning [`WaitOutcome::Timeout`].
/// * `interval` – sleep duration between provider reads.
///
/// A missing target or provider read error returns immediately rather than
/// waiting for another interval.
#[allow(clippy::too_many_arguments)]
pub async fn execute_wait_effect<S>(
    cleanup_loop: &CleanupAwareLoop<'_>,
    provider: &dyn Provider,
    effect: &Effect<S>,
    target: &ManagedWaitTarget,
    identifier_resolver: &WaitIdentifierResolver<'_>,
    until: &WaitPredicate,
    timeout: Duration,
    interval: Duration,
    cancel: tokio::sync::watch::Receiver<WaitSignal>,
    observer: &dyn ExecutionObserver<S>,
) -> WaitOutcome {
    let target_id = target.id();
    execute_wait_effect_with_heartbeat_cadence(
        cleanup_loop,
        provider,
        effect,
        target_id.identity_str(),
        target_id,
        identifier_resolver,
        until,
        timeout,
        interval,
        cancel,
        observer,
        default_heartbeat_cadence(interval),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_wait_effect_with_heartbeat_cadence<S>(
    cleanup_loop: &CleanupAwareLoop<'_>,
    provider: &dyn Provider,
    effect: &Effect<S>,
    binding: &str,
    target_id: &ResourceId,
    identifier_resolver: &WaitIdentifierResolver<'_>,
    until: &WaitPredicate,
    timeout: Duration,
    interval: Duration,
    mut cancel: tokio::sync::watch::Receiver<WaitSignal>,
    observer: &dyn ExecutionObserver<S>,
    heartbeat_cadence: Duration,
) -> WaitOutcome {
    let start = Instant::now();
    let mut last_heartbeat_at: Option<Instant> = None;
    loop {
        let cleanup_wait = match cleanup_loop.step() {
            LoopStep::Continue(wait) => wait,
            LoopStep::Abandon => return WaitOutcome::Abandoned,
        };
        let iteration = async {
            let Some(target_identifier) = identifier_resolver(target_id) else {
                return Some(WaitOutcome::ReadFailed(
                    ProviderError::internal(format!(
                        "wait target {} has no provider identifier",
                        target_id
                    ))
                    .for_resource(target_id.clone()),
                ));
            };
            let state = match provider
                .read(target_id, &target_identifier, ReadRequest)
                .await
            {
                Ok(outcome) => outcome.into_state(),
                Err(err @ ProviderError::NotFound(_)) => {
                    return Some(WaitOutcome::NotFound(err));
                }
                Err(err) => return Some(WaitOutcome::ReadFailed(err)),
            };
            if !state.exists {
                return Some(WaitOutcome::NotFound(
                    ProviderError::not_found(format!(
                        "wait target {} not found (deleted out-of-band?)",
                        target_id
                    ))
                    .for_resource(target_id.clone()),
                ));
            }

            // Sample `now` before the sleep deliberately: it timestamps this poll,
            // not the following wakeup. With `>=`, the cadence is a floor, not a
            // target: emits are never early but can be late when the interval does
            // not divide 30s or provider/scheduler work delays a poll.
            // Observer-side timestamps can drift further through event construction
            // and callback delay.
            let now = Instant::now();
            let should_emit_heartbeat = last_heartbeat_at
                .map(|last| now.duration_since(last) >= heartbeat_cadence)
                .unwrap_or(true);
            if should_emit_heartbeat {
                let observation =
                    WaitObservation::new(binding, target_id, until, &state.attributes);
                observer.on_event(&ExecutionEvent::WaitPolling {
                    effect,
                    observation,
                    elapsed: start.elapsed(),
                });
                last_heartbeat_at = Some(now);
            }

            if until.evaluate(&state.attributes) {
                return Some(WaitOutcome::Satisfied { state });
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Some(WaitOutcome::Timeout {
                    last_attrs: state.attributes,
                    elapsed,
                });
            }
            tokio::select! {
                biased;
                changed = cancel.changed() => {
                    if changed.is_ok() {
                        match *cancel.borrow() {
                            WaitSignal::Cancelled => Some(WaitOutcome::Cancelled),
                            WaitSignal::NoMutatorRemaining => {
                                Some(WaitOutcome::Unsatisfiable(
                                    UnsatisfiableReason::NoMutatorRemaining
                                ))
                            }
                            WaitSignal::Continue => None,
                        }
                    } else {
                        None
                    }
                }
                () = tokio::time::sleep(interval) => None,
            }
        };
        match cleanup_wait.until_cleanup_priority(iteration).await {
            CleanupInterrupted::Completed(Some(outcome)) => return outcome,
            CleanupInterrupted::Completed(None) => {}
            CleanupInterrupted::Abandoned => return WaitOutcome::Abandoned,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecutionEvent, ExecutionObserver};
    use crate::provider::{
        AppliedState, BoxFuture, CreateRequest, DeleteRequest, ProviderResult, ReadRequest,
        UpdateRequest,
    };
    use crate::resource::{ConcreteValue, DeferredValue, ResolvedResource, UnknownReason, Value};
    use crate::wait::predicate::{AttrPath, WaitPredicate};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::{oneshot, watch};

    const CLEANUP_TEST_BOUND: Duration = Duration::from_secs(1);

    fn managed_wait_target() -> ManagedWaitTarget {
        ManagedWaitTarget::new(&ResolvedResource::from_id(ResolvedResourceId::new(
            ResourceId::with_identity("acm.Certificate", "cert"),
        )))
    }

    fn wait_effect() -> Effect {
        Effect::Wait {
            identity: crate::resource::ResourceIdentity::new("cert_issued"),
            target_id: managed_wait_target(),
            until: equals_status("ISSUED"),
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(1),
            explicit_dependencies: HashSet::new(),
        }
    }

    fn provider_identifier(_: &ResourceId) -> Option<AppliedIdentifier> {
        Some(
            AppliedIdentifier::new("wait-target-id")
                .expect("fixture provider identifier is non-empty"),
        )
    }

    #[test]
    fn initial_wait_identifiers_include_managed_current() {
        let id = ResourceId::with_identity("test.resource", "target");
        let managed_current = HashMap::from([(
            id.clone(),
            Current::from_state(
                State::existing(id.clone(), HashMap::new()).with_identifier("managed-id"),
            ),
        )]);

        let identifiers = initial_wait_identifiers(&managed_current);

        assert_eq!(
            resolve_wait_identifier(&identifiers, &id)
                .expect("managed identifier must be available to waits")
                .as_str(),
            "managed-id"
        );
    }

    /// Minimal test provider: returns a queue of pre-canned read
    /// responses in order, then repeats the last one. Counts every
    /// `read` call so tests can assert polling behaviour.
    struct ReadSequenceProvider {
        responses: Mutex<Vec<State>>,
        reads: Mutex<usize>,
        pending: bool,
    }

    impl ReadSequenceProvider {
        fn new(responses: Vec<State>) -> Self {
            Self {
                responses: Mutex::new(responses),
                reads: Mutex::new(0),
                pending: false,
            }
        }

        fn pending() -> Self {
            Self {
                responses: Mutex::new(Vec::new()),
                reads: Mutex::new(0),
                pending: true,
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
            identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            let identifier = identifier.clone();
            Box::pin(async move {
                {
                    let mut reads = self.reads.lock().unwrap();
                    *reads += 1;
                }
                if self.pending {
                    return std::future::pending().await;
                }
                let mut responses = self.responses.lock().unwrap();
                if responses.len() > 1 {
                    Ok(fixture_read_outcome_for_identifier(
                        responses.remove(0),
                        &identifier,
                    ))
                } else if let Some(last) = responses.last() {
                    Ok(fixture_read_outcome_for_identifier(
                        last.clone(),
                        &identifier,
                    ))
                } else {
                    Err(ProviderError::api_error("no canned response"))
                }
            })
        }

        fn read_data_source(
            &self,
            resource: &crate::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
            _attempt: crate::provider::CreateAttempt,
        ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>>
        {
            let id = id.clone();
            Box::pin(async move {
                Ok(crate::provider::CreateOutcome::fixture_success(
                    AppliedState::new(id, "created-id", HashMap::new())
                        .expect("fixture identifier is non-empty"),
                ))
            })
        }

        fn resume_create(
            &self,
            _request: crate::provider::ResumeCreateRequest,
        ) -> BoxFuture<'_, Result<crate::provider::ResumeOutcome, crate::provider::ResumeFailure>>
        {
            Box::pin(async { panic!("unexpected resume_create") })
        }

        fn update(
            &self,
            id: &ResourceId,
            identifier: &crate::provider::AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
            let id = id.clone();
            let identifier = identifier.to_string();
            Box::pin(async move {
                Ok(crate::provider::UpdateOutcome::Success {
                    state: AppliedState::new(id, identifier, HashMap::new())
                        .expect("provider input identifier is non-empty"),
                })
            })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &crate::provider::AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async move { Ok(()) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    fn state_with_status(status: &str) -> State {
        let mut attrs = HashMap::new();
        attrs.insert(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String(status.to_string().into())),
        );
        State::existing(ResourceId::with_identity("acm.Certificate", "cert"), attrs)
    }

    fn equals_status(value: &str) -> WaitPredicate {
        WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String(value.to_string().into())),
        )
        .unwrap()
    }

    struct NoopWaitObserver;

    impl ExecutionObserver for NoopWaitObserver {
        fn on_event(&self, _event: &ExecutionEvent) {}
    }

    struct HeartbeatObserver {
        heartbeats: Mutex<Vec<(ResourceId, Vec<AttrPath>)>>,
    }

    impl HeartbeatObserver {
        fn new() -> Self {
            Self {
                heartbeats: Mutex::new(Vec::new()),
            }
        }

        fn heartbeats(&self) -> Vec<(ResourceId, Vec<AttrPath>)> {
            self.heartbeats.lock().unwrap().clone()
        }
    }

    impl ExecutionObserver for HeartbeatObserver {
        fn on_event(&self, event: &ExecutionEvent) {
            if let ExecutionEvent::WaitPolling {
                effect,
                observation,
                elapsed: _,
            } = event
            {
                let watched_attrs = observation
                    .watched_attrs()
                    .iter()
                    .map(|attr| (*attr).clone())
                    .collect();
                self.heartbeats
                    .lock()
                    .unwrap()
                    .push((effect.resource_id().clone(), watched_attrs));
            }
        }
    }

    #[test]
    fn default_heartbeat_cadence_uses_maximum_of_floor_and_interval_multiple() {
        assert_eq!(
            default_heartbeat_cadence(Duration::from_millis(1)),
            Duration::from_secs(30)
        );
        assert_eq!(
            default_heartbeat_cadence(Duration::from_secs(60)),
            Duration::from_secs(300)
        );
    }

    #[tokio::test]
    async fn wait_abandons_in_flight_provider_read_when_cleanup_is_prioritized() {
        tokio::time::timeout(CLEANUP_TEST_BOUND, async {
            let provider = ReadSequenceProvider::pending();
            let pred = equals_status("ISSUED");
            let target = managed_wait_target();
            let effect = wait_effect();
            let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
            let observer = NoopWaitObserver;
            let (trigger, shutdown) = crate::shutdown::testing::shutdown_channel();
            let cleanup_loop = shutdown.cleanup_aware_loop();

            let wait = execute_wait_effect(
                &cleanup_loop,
                &provider,
                &effect,
                &target,
                &provider_identifier,
                &pred,
                Duration::from_secs(60),
                Duration::from_secs(60),
                cancel_rx,
                &observer,
            );
            tokio::pin!(wait);

            std::future::poll_fn(|cx| {
                assert!(
                    wait.as_mut().poll(cx).is_pending(),
                    "wait returned before provider.read was in flight"
                );
                std::task::Poll::Ready(())
            })
            .await;
            assert_eq!(
                provider.read_count(),
                1,
                "the test must park inside the first provider.read"
            );

            trigger.prioritize_cleanup();
            let result = std::future::poll_fn(|cx| match wait.as_mut().poll(cx) {
                std::task::Poll::Ready(outcome) => std::task::Poll::Ready(outcome),
                std::task::Poll::Pending => {
                    panic!("cleanup priority did not abandon the in-flight provider.read")
                }
            })
            .await;
            assert!(
                matches!(result, WaitOutcome::Abandoned),
                "cleanup priority returned the wrong outcome: {result:?}"
            );
        })
        .await
        .expect("in-flight cleanup regression test exceeded its one-second bound");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_abandons_while_parked_in_sleep_interval() {
        tokio::time::timeout(CLEANUP_TEST_BOUND, async {
            let provider = ReadSequenceProvider::new(vec![state_with_status("PENDING")]);
            let pred = equals_status("ISSUED");
            let target = managed_wait_target();
            let effect = wait_effect();
            let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
            let observer = NoopWaitObserver;
            let (trigger, shutdown) = crate::shutdown::testing::shutdown_channel();
            let cleanup_loop = shutdown.cleanup_aware_loop();

            let wait = execute_wait_effect(
                &cleanup_loop,
                &provider,
                &effect,
                &target,
                &provider_identifier,
                &pred,
                Duration::from_secs(600),
                Duration::from_secs(30),
                cancel_rx,
                &observer,
            );
            tokio::pin!(wait);

            std::future::poll_fn(|cx| {
                assert!(
                    wait.as_mut().poll(cx).is_pending(),
                    "wait returned before parking in the polling interval"
                );
                std::task::Poll::Ready(())
            })
            .await;
            assert_eq!(
                provider.read_count(),
                1,
                "the first provider.read must complete before the wait parks in sleep"
            );

            trigger.prioritize_cleanup();
            let result = std::future::poll_fn(|cx| match wait.as_mut().poll(cx) {
                std::task::Poll::Ready(outcome) => std::task::Poll::Ready(outcome),
                std::task::Poll::Pending => {
                    panic!("cleanup priority did not abandon the wait parked in sleep")
                }
            })
            .await;
            assert!(
                matches!(result, WaitOutcome::Abandoned),
                "cleanup priority returned the wrong sleep-parked outcome: {result:?}"
            );
        })
        .await
        .expect("sleep-parked cleanup regression test exceeded its one-second bound");
    }

    #[tokio::test]
    async fn wait_abandons_before_read_when_cleanup_is_already_prioritized() {
        let provider = Arc::new(ReadSequenceProvider::new(vec![state_with_status("ISSUED")]));
        let (trigger, shutdown) = crate::shutdown::testing::shutdown_channel();
        trigger.prioritize_cleanup();
        assert_eq!(
            shutdown.phase(),
            crate::shutdown::ShutdownPhase::CleanupPriority,
            "the test trigger and shutdown token must share cleanup-priority state"
        );

        let worker_provider = Arc::clone(&provider);
        let (result_tx, result_rx) = oneshot::channel();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("failed to build wait-test runtime");
            let result = runtime.block_on(async move {
                let pred = equals_status("ISSUED");
                let target = managed_wait_target();
                let effect = wait_effect();
                let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
                let observer = NoopWaitObserver;
                let cleanup_loop = shutdown.cleanup_aware_loop();

                execute_wait_effect(
                    &cleanup_loop,
                    worker_provider.as_ref(),
                    &effect,
                    &target,
                    &provider_identifier,
                    &pred,
                    Duration::from_secs(60),
                    Duration::from_secs(60),
                    cancel_rx,
                    &observer,
                )
                .await
            });
            let _ = result_tx.send(result);
        });

        let result = tokio::time::timeout(CLEANUP_TEST_BOUND, result_rx)
            .await
            .expect("already-active cleanup wait did not return within one second")
            .expect("already-active cleanup wait worker exited without an outcome");
        worker
            .join()
            .expect("already-active cleanup wait worker panicked");

        assert!(
            matches!(result, WaitOutcome::Abandoned),
            "already-active cleanup priority returned the wrong outcome: {result:?}"
        );
        assert_eq!(
            provider.read_count(),
            0,
            "already-active cleanup priority must abandon before provider.read; got {result:?}"
        );
    }

    #[tokio::test]
    async fn wait_reaches_satisfied_during_graceful_shutdown() {
        tokio::time::timeout(CLEANUP_TEST_BOUND, async {
            let provider = ReadSequenceProvider::new(vec![state_with_status("ISSUED")]);
            let pred = equals_status("ISSUED");
            let target = managed_wait_target();
            let effect = wait_effect();
            let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
            let observer = NoopWaitObserver;
            let (trigger, shutdown) = crate::shutdown::testing::shutdown_channel();
            trigger.request_graceful_shutdown();
            assert_eq!(
                shutdown.phase(),
                crate::shutdown::ShutdownPhase::Graceful,
                "the wait must start while graceful shutdown is active"
            );
            let cleanup_loop = shutdown.cleanup_aware_loop();

            let result = execute_wait_effect(
                &cleanup_loop,
                &provider,
                &effect,
                &target,
                &provider_identifier,
                &pred,
                Duration::from_secs(60),
                Duration::from_secs(30),
                cancel_rx,
                &observer,
            )
            .await;

            assert!(
                matches!(result, WaitOutcome::Satisfied { .. }),
                "graceful shutdown abandoned a satisfiable wait: {result:?}"
            );
            assert_eq!(
                provider.read_count(),
                1,
                "graceful shutdown must allow the provider read to complete"
            );
        })
        .await
        .expect("graceful-shutdown wait test exceeded its one-second bound");
    }

    #[tokio::test]
    async fn wait_returns_immediately_when_until_already_true() {
        let provider = ReadSequenceProvider::new(vec![state_with_status("ISSUED")]);
        let pred = equals_status("ISSUED");
        let target = managed_wait_target();
        let effect = wait_effect();
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
        let observer = NoopWaitObserver;
        let shutdown = crate::shutdown::ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();
        let result = execute_wait_effect(
            &cleanup_loop,
            &provider,
            &effect,
            &target,
            &provider_identifier,
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
            Some(&Value::Concrete(ConcreteValue::String("ISSUED".into())))
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
        let target = managed_wait_target();
        let effect = wait_effect();
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
        let observer = NoopWaitObserver;
        let shutdown = crate::shutdown::ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();
        let result = execute_wait_effect(
            &cleanup_loop,
            &provider,
            &effect,
            &target,
            &provider_identifier,
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
        let target = managed_wait_target();
        let effect = wait_effect();
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
        let observer = NoopWaitObserver;
        let shutdown = crate::shutdown::ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();
        let result = execute_wait_effect(
            &cleanup_loop,
            &provider,
            &effect,
            &target,
            &provider_identifier,
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
                "PENDING_VALIDATION".into()
            )))
        );
    }

    #[test]
    fn wait_timeout_failure_message_uses_display_formatting_for_last_attrs() {
        let target = ResolvedResourceId::new(ResourceId::with_identity("acm.Certificate", "cert"));
        let outcome = WaitOutcome::Timeout {
            last_attrs: HashMap::from([
                (
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("pending".into())),
                ),
                (
                    "arn".to_string(),
                    Value::Concrete(ConcreteValue::String(
                        "arn:aws:acm:1:certificate/abc".into(),
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
        let target = ResolvedResourceId::new(ResourceId::with_identity("acm.Certificate", "cert"));
        let outcome = WaitOutcome::Timeout {
            last_attrs: HashMap::new(),
            elapsed: Duration::from_secs(1),
        };

        let message = wait_failure_message(&outcome, &target);

        assert!(message.contains("last observed attributes: no observed attributes"));
    }

    #[test]
    fn wait_timeout_failure_message_handles_unknown_and_deferred_attrs() {
        let target = ResolvedResourceId::new(ResourceId::with_identity("acm.Certificate", "cert"));
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
            State::not_found(ResourceId::with_identity("acm.Certificate", "cert")),
        ]);
        let pred = equals_status("ISSUED");
        let target = managed_wait_target();
        let effect = wait_effect();
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
        let observer = NoopWaitObserver;
        let shutdown = crate::shutdown::ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();
        let result = execute_wait_effect(
            &cleanup_loop,
            &provider,
            &effect,
            &target,
            &provider_identifier,
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
        let target = managed_wait_target();
        let effect = wait_effect();
        let (cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
        let observer = NoopWaitObserver;
        let reads_seen = AtomicUsize::new(0);
        let shutdown = crate::shutdown::ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();

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
                &cleanup_loop,
                &provider,
                &effect,
                &target,
                &provider_identifier,
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

    #[tokio::test(start_paused = true)]
    async fn wait_emits_exact_heartbeat_count_for_cadence() {
        let provider = ReadSequenceProvider::new(vec![state_with_status("PENDING_VALIDATION")]);
        let pred = equals_status("ISSUED");
        let target = ResolvedResourceId::new(ResourceId::with_identity("acm.Certificate", "cert"));
        let (_cancel_tx, cancel_rx) = watch::channel(WaitSignal::Continue);
        let observer = HeartbeatObserver::new();
        let shutdown = crate::shutdown::ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();
        let effect: crate::effect::Effect = crate::effect::Effect::Wait {
            identity: crate::resource::ResourceIdentity::new("cert_issued"),
            target_id: managed_wait_target(),
            until: equals_status("ISSUED"),
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: Duration::from_millis(100),
            interval: Duration::from_millis(1),
            explicit_dependencies: HashSet::new(),
        };

        let result = execute_wait_effect_with_heartbeat_cadence(
            &cleanup_loop,
            &provider,
            &effect,
            "cert_issued",
            &target,
            &provider_identifier,
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
        // With Tokio time paused, each 1ms sleep advances exactly one virtual
        // millisecond. The loop therefore reads at 0ms through 100ms inclusive
        // and emits every 5ms from 0ms through 100ms inclusive: 21 heartbeats.
        // The final emit at 100ms occurs only because heartbeat emission
        // precedes the timeout check; with those checks reversed, the loop
        // would exit first and emit 20 heartbeats.
        // These exact counts guard against both a silent and a noisy cadence
        // without imposing a scheduler-latency floor on real time.
        const EXPECTED_READS: usize = 101;
        const EXPECTED_HEARTBEATS: usize = 21;
        assert_eq!(provider.read_count(), EXPECTED_READS);
        assert_eq!(
            heartbeats.len(),
            EXPECTED_HEARTBEATS,
            "5ms heartbeat cadence over a virtual 100ms timeout should emit \
             exactly {EXPECTED_HEARTBEATS} heartbeats"
        );
        assert_eq!(heartbeats[0].0, effect.resource_id().clone());
        assert_eq!(heartbeats[0].1, vec![AttrPath::single("status")]);
    }
}
