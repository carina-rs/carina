use carina_core::provider::{ProviderPlan, ProviderValidationInputs};
use carina_core::provider_text::{
    GuestInputInventory, GuestInputStore, GuestStoreData, RedactionSet,
};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wasmtime::component::ResourceTable;
use wasmtime::{CallHook, Engine, Store, StoreLimits};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView};

use super::{AllowListHttpHooks, CallEgressRecorder, GuestNetwork, HttpTestRoute, ProviderLabel};

/// Period between increments of the wasmtime engine epoch.
pub(in crate::wasm_factory) const EPOCH_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum total guest computation consumed by one guest call in production.
const GUEST_COMPUTE_BUDGET: Duration = Duration::from_secs(30);

/// Wall-clock backstop for every WASM guest call in production.
///
/// This includes guest computation and time parked in host imports. It bounds
/// host-side waits that never complete, which would otherwise retain a store
/// lock or a factory initialization guard indefinitely (carina#3106).
///
/// # Sizing — why this is 60 minutes, not the 30s compute budget
///
/// The largest enumerated current-provider single-call worst case is the AWS
/// VPC flow-log IAM-propagation loop at about 21 minutes; legacy provider
/// releases from before the bounded SDK policy can take about 41 minutes. The
/// test `production_wall_budget_exceeds_each_provider_single_call_worst_case`
/// pins this value above both models and every enumerated current-provider
/// case. The remaining headroom is a finite hang backstop for work the model
/// cannot bound, including an uncapped TLS handshake, a body that keeps
/// trickling chunks, and unbounded in-call loops such as `empty_s3_bucket`
/// paging during bucket deletion.
const WASM_OPERATION_HARD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The compute and wall-clock limits for one WASM guest call.
///
/// `compute` bounds the total guest computation of one guest call; time parked
/// in host imports is excluded. `wall` bounds every guest call, including time
/// spent in host imports.
#[derive(Clone, Copy, Debug)]
pub struct GuestCallBudget {
    compute: Duration,
    wall: Duration,
}

impl GuestCallBudget {
    pub const PRODUCTION: Self = Self {
        compute: GUEST_COMPUTE_BUDGET,
        wall: WASM_OPERATION_HARD_TIMEOUT,
    };

    #[cfg(any(test, feature = "test-utils"))]
    pub const fn new(compute: Duration, wall: Duration) -> Self {
        Self { compute, wall }
    }

    #[cfg(test)]
    pub(in crate::wasm_factory) const fn compute(self) -> Duration {
        self.compute
    }

    #[cfg(test)]
    pub(in crate::wasm_factory) const fn wall(self) -> Duration {
        self.wall
    }
}

pub(in crate::wasm_factory) use super::super::diagnostics::WallBudgetExceeded;

struct GuestComputeState {
    remaining: Duration,
    resumed_at: Instant,
    host_depth: u32,
}

impl GuestComputeState {
    fn new() -> Self {
        Self {
            remaining: Duration::ZERO,
            resumed_at: Instant::now(),
            host_depth: 0,
        }
    }
}

struct GuestStoreState {
    #[cfg(test)]
    armed_wall_budget: Option<(tokio::time::Instant, tokio::time::Instant)>,
    budget: GuestCallBudget,
    compute: GuestComputeState,
}

impl GuestStoreState {
    fn new(budget: GuestCallBudget) -> Self {
        Self {
            #[cfg(test)]
            armed_wall_budget: None,
            budget,
            compute: GuestComputeState::new(),
        }
    }
}

pub(in crate::wasm_factory) type HostState = GuestStoreData<HostRuntimeState>;

pub(in crate::wasm_factory) struct HostRuntimeState {
    wasi_ctx: WasiCtx,
    http_ctx: Option<WasiHttpCtx>,
    table: ResourceTable,
    http_hooks: AllowListHttpHooks,
    limits: StoreLimits,
    guest: GuestStoreState,
    stderr: super::guest_stderr::HostOutput,
    provider_name: String,
    diagnostic_imports: usize,
    protected_bytes: usize,
    protected_values: usize,
}

impl HostRuntimeState {
    pub(super) fn accept_diagnostic_import(&mut self) -> bool {
        if self.diagnostic_imports >= 256 {
            return false;
        }
        self.diagnostic_imports += 1;
        true
    }
    pub(super) fn accept_protected_material(&mut self, bytes: usize, values: usize) -> bool {
        if bytes > (256usize * 1024).saturating_sub(self.protected_bytes)
            || values > 1024usize.saturating_sub(self.protected_values)
        {
            return false;
        }
        self.protected_bytes += bytes;
        self.protected_values += values;
        true
    }
}

impl WasiView for HostRuntimeState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostRuntimeState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: self
                .http_ctx
                .as_mut()
                .expect("HTTP not enabled for this provider"),
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

/// A Wasmtime store whose raw inner store cannot escape this module except
/// through [`GuestStore::call`].
///
/// A wall timeout leaves the store mid-call: neither the call hook's
/// `host_depth` nor the component instance state is unwound. The store must
/// not be reused afterward. Shared owners enforce that by replacing the
/// complete live-instance slot (store plus bindings) with a typed poisoned
/// state while its mutex is held; unshared initialization paths discard the
/// store directly.
pub(in crate::wasm_factory) struct GuestStore {
    #[cfg(any(test, feature = "test-utils"))]
    deadline_observer: Option<super::PollObserverForTest>,
    store: GuestInputStore<HostRuntimeState>,
    next_secrets: Option<ProviderValidationInputs>,
    config_secrets: Option<ProviderValidationInputs>,
}

impl GuestStore {
    /// Create a store with every invariant needed for guest calls.
    ///
    /// The call hook charges guest-resident intervals outside host imports to
    /// one top-level call while excluding time parked in host imports. Nested
    /// guest code entered while a host import is in flight (such as realloc or
    /// post-return during result lowering) is re-armed from the remaining
    /// budget, so epochs accumulated during a long host wait cannot interrupt
    /// it, but that nested interval is not subtracted from the remaining
    /// compute time.
    pub(in crate::wasm_factory) fn new(
        engine: &Engine,
        label: Arc<ProviderLabel>,
        network: GuestNetwork,
        http_ctx: Option<WasiHttpCtx>,
        limits: StoreLimits,
        budget: GuestCallBudget,
    ) -> Self {
        Self::new_with_http_test_route(engine, label, network, http_ctx, limits, budget, None)
    }

    pub(in crate::wasm_factory) fn new_with_http_test_route(
        engine: &Engine,
        label: Arc<ProviderLabel>,
        network: GuestNetwork,
        http_ctx: Option<WasiHttpCtx>,
        limits: StoreLimits,
        budget: GuestCallBudget,
        http_test_route: Option<HttpTestRoute>,
    ) -> Self {
        let stderr = super::guest_stderr::HostOutput::new(super::host_stderr());
        let provider_name = label.name.clone();
        let inputs = ProviderPlan::state_only()
            .session(&provider_name, None, None)
            .guest_inventory();
        let capabilities = network.runtime_capabilities().cloned();
        let mut http_hooks = AllowListHttpHooks::with_test_route(label, network, http_test_route);
        http_hooks.stderr = stderr.clone();
        let host_state = HostRuntimeState {
            wasi_ctx: wasmtime_wasi::WasiCtxBuilder::new().build(),
            http_ctx,
            table: ResourceTable::new(),
            http_hooks,
            limits,
            guest: GuestStoreState::new(budget),
            stderr: stderr.clone(),
            provider_name,
            diagnostic_imports: 0,
            protected_bytes: 0,
            protected_values: 0,
        };
        let mut store = GuestStoreData::new_store(engine, host_state, inputs);
        let wasi_ctx =
            super::build_sandboxed_wasi_ctx_for_store(store.data(), capabilities.as_ref(), |key| {
                std::env::var(key).ok()
            });
        store.data_mut().wasi_ctx = wasi_ctx;
        store.limiter(|state| &mut state.limits);
        store.call_hook(|mut store, transition| {
            let now = Instant::now();
            let remaining_to_arm = {
                let state = &mut store.data_mut().guest;
                match transition {
                    CallHook::CallingWasm => {
                        if state.compute.host_depth == 0 {
                            state.compute.remaining = state.budget.compute;
                            state.compute.resumed_at = now;
                        }
                        Some(state.compute.remaining)
                    }
                    CallHook::CallingHost => {
                        if state.compute.host_depth == 0 {
                            let elapsed = now.saturating_duration_since(state.compute.resumed_at);
                            state.compute.remaining =
                                state.compute.remaining.saturating_sub(elapsed);
                        }
                        state.compute.host_depth += 1;
                        None
                    }
                    CallHook::ReturningFromHost => {
                        debug_assert!(state.compute.host_depth > 0);
                        state.compute.host_depth -= 1;
                        if state.compute.host_depth == 0 {
                            state.compute.resumed_at = now;
                            Some(state.compute.remaining)
                        } else {
                            None
                        }
                    }
                    CallHook::ReturningFromWasm => None,
                }
            };
            if let Some(remaining) = remaining_to_arm {
                store.set_epoch_deadline(compute_epoch_ticks(remaining));
            }
            Ok(())
        });
        Self {
            #[cfg(any(test, feature = "test-utils"))]
            deadline_observer: None,
            store,
            next_secrets: None,
            config_secrets: None,
        }
    }

    pub(in crate::wasm_factory) fn set_input_inventory(&mut self, inputs: GuestInputInventory) {
        self.store.data_mut().set_input_inventory(inputs);
    }

    pub(in crate::wasm_factory) fn set_configuration_secrets(
        &mut self,
        secrets: ProviderValidationInputs,
    ) {
        self.config_secrets = Some(secrets.clone());
        self.set_secrets(secrets);
    }

    pub(in crate::wasm_factory) fn set_secrets(&mut self, secrets: ProviderValidationInputs) {
        self.next_secrets = Some(secrets);
    }

    /// Runtime cache identity is the supplied session, while the instance
    /// inventory also retains host credentials and previously observed inputs.
    pub(in crate::wasm_factory) fn configuration_secrets(
        &self,
    ) -> Option<ProviderValidationInputs> {
        self.config_secrets.clone()
    }

    pub(in crate::wasm_factory) fn known_secrets(&self) -> Option<RedactionSet> {
        self.store.data().stderr.known_secrets()
    }

    /// Run guest code under this store's wall-clock budget.
    ///
    /// This is the only API that exposes the raw Wasmtime store, and the
    /// closure cannot outlive the timeout future. The timeout is observed at
    /// await points; it cannot interrupt a host import that blocks its runtime
    /// thread synchronously.
    pub(in crate::wasm_factory) async fn call<'s, T, F>(
        &'s mut self,
        f: impl FnOnce(&'s mut Store<HostState>) -> F,
    ) -> Result<T, WallBudgetExceeded>
    where
        T: 'static,
        F: Future<Output = T> + 's,
    {
        let secrets = self
            .next_secrets
            .take()
            .or_else(|| self.config_secrets.clone());
        let source = secrets.clone().unwrap_or_else(|| {
            ProviderPlan::state_only()
                .session(&self.store.data().provider_name, None, None)
                .guest_inputs()
        });
        self.store
            .data_mut()
            .set_input_inventory(source.guest_inventory());
        let inputs = self.store.data().input_inventory();
        let _input_call = inputs.observe(&source);
        let _stderr_call = (secrets.is_some() || !inputs.is_empty()).then(|| {
            self.store
                .data()
                .stderr
                .begin_inventory_call(&source, inputs)
        });
        self.call_with_start(tokio::time::Instant::now(), f).await
    }

    // Supplying the origin separately lets tests model synchronous setup time
    // with the documented paused clock, without polling an advance future.
    async fn call_with_start<'s, T, F>(
        &'s mut self,
        started: tokio::time::Instant,
        f: impl FnOnce(&'s mut Store<HostState>) -> F,
    ) -> Result<T, WallBudgetExceeded>
    where
        T: 'static,
        F: Future<Output = T> + 's,
    {
        let wall = self.store.data().guest.budget.wall;
        let deadline = started.checked_add(wall);
        #[cfg(test)]
        self.record_armed_wall_budget_for_test(started, deadline);
        let store = &mut self.store;
        let body = async move { f(store).await };
        let call = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, body),
            // Delegate unrepresentable deadlines to Tokio so its far-future
            // fallback stays identical to timeout's behavior.
            None => tokio::time::timeout(wall, body),
        };
        #[cfg(any(test, feature = "test-utils"))]
        if let Some(observer) = &self.deadline_observer {
            observer(super::PollEventForTest::GuestCallDeadlineCreated { wall });
        }
        call.await.map_err(|_elapsed| WallBudgetExceeded { wall })
    }

    #[cfg(test)]
    fn record_armed_wall_budget_for_test(
        &mut self,
        started: tokio::time::Instant,
        deadline: Option<tokio::time::Instant>,
    ) {
        self.store.data_mut().guest.armed_wall_budget =
            deadline.map(|deadline| (started, deadline));
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(in crate::wasm_factory) fn set_deadline_observer_for_test(
        &mut self,
        observer: Option<super::PollObserverForTest>,
    ) {
        self.deadline_observer = observer;
    }

    pub(in crate::wasm_factory) fn begin_create_call(&mut self, recorder: CallEgressRecorder) {
        let active = &mut self.store.data_mut().http_hooks.call_egress;
        assert!(active.is_none(), "guest calls cannot overlap on one store");
        *active = Some(recorder);
    }

    pub(in crate::wasm_factory) fn finish_create_call(&mut self) {
        let active = &mut self.store.data_mut().http_hooks.call_egress;
        assert!(
            active.take().is_some(),
            "a create call scope must be active"
        );
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(in crate::wasm_factory) fn set_budget(&mut self, budget: GuestCallBudget) {
        self.store.data_mut().guest.budget = budget;
    }

    #[cfg(test)]
    pub(in crate::wasm_factory) fn set_epoch_deadline_for_test(&mut self, ticks: u64) {
        self.store.set_epoch_deadline(ticks);
    }
}

// confinement-allow-return: Outbound credential wire handoff; only opaque SecretPayload inputs reach this adapter.
pub(in crate::wasm_factory) fn handoff_credential(
    store: &HostState,
    value: &super::super::GuestCredential,
) -> String {
    super::super::handoff_credential(store, value)
}

impl HostRuntimeState {
    pub(in crate::wasm_factory) fn network(&self) -> &GuestNetwork {
        &self.http_hooks.network
    }
}

fn compute_epoch_ticks(remaining: Duration) -> u64 {
    let ticks = (remaining.as_nanos() / EPOCH_TICK_INTERVAL.as_nanos()).saturating_add(1);
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_provider_label() -> Arc<ProviderLabel> {
        Arc::new(ProviderLabel {
            name: "guest-store-test".to_string(),
            source: carina_provider_resolver::ProviderLoadSource::local_file(
                &carina_core::paths::ResolvedPath::new("/test-providers/guest-store-test.wasm")
                    .unwrap(),
                carina_core::paths::DisplayPath::new("guest-store-test.wasm").unwrap(),
            ),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn wall_budget_includes_synchronous_guest_call_setup() {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).expect("test engine should be valid");
        let limits = wasmtime::StoreLimitsBuilder::new().build();
        let mut store = GuestStore::new(
            &engine,
            test_provider_label(),
            GuestNetwork::SchemaOnly,
            None,
            limits,
            GuestCallBudget::new(Duration::from_secs(30), Duration::from_millis(20)),
        );

        let call_start = tokio::time::Instant::now();
        // Model 50ms of synchronous setup after the origin was captured. The
        // 20ms deadline is already past when the closure returns its future.
        tokio::time::advance(Duration::from_millis(50)).await;
        let result = store
            .call_with_start(call_start, |_store| {
                // This is the synchronous setup closure, before it has
                // returned a future. The actual timeout deadline must already
                // be armed from the call's start, before any setup executes.
                let (started, deadline) = _store
                    .data()
                    .guest
                    .armed_wall_budget
                    .expect("the deadline must be armed before synchronous setup");
                assert_eq!(started, call_start);
                assert_eq!(deadline, call_start + Duration::from_millis(20));
                std::future::pending::<()>()
            })
            .await;

        assert!(
            result.is_err(),
            "synchronous setup before the returned future must consume the wall budget"
        );
        assert_eq!(
            tokio::time::Instant::now(),
            call_start + Duration::from_millis(50)
        );

        // Match Tokio's far-future fallback for an unrepresentable budget.
        store.set_budget(GuestCallBudget::new(Duration::from_secs(30), Duration::MAX));
        let call = store.call(|_| std::future::pending::<()>());
        tokio::pin!(call);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(call.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(call.as_mut().poll(&mut context).is_pending());
    }

    #[tokio::test(start_paused = true)]
    async fn wall_budget_expires_at_the_configured_deadline() {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).unwrap();
        let budget = GuestCallBudget::new(Duration::from_secs(30), Duration::from_secs(2));
        let mut store = GuestStore::new(
            &engine,
            test_provider_label(),
            GuestNetwork::SchemaOnly,
            None,
            wasmtime::StoreLimitsBuilder::new().build(),
            budget,
        );
        let call = store.call(|_| std::future::pending::<()>());
        tokio::pin!(call);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());

        assert!(call.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(budget.wall() - Duration::from_millis(1)).await;
        assert!(call.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(Duration::from_millis(1)).await;
        let error = call
            .await
            .expect_err("a pending call must exhaust its budget");
        assert_eq!(error.wall, budget.wall());
    }

    #[test]
    fn guest_compute_budget_epoch_ticks_do_not_expire_early() {
        assert_eq!(compute_epoch_ticks(Duration::ZERO), 1);
        assert_eq!(compute_epoch_ticks(Duration::from_millis(1)), 1);
        assert_eq!(compute_epoch_ticks(Duration::from_secs(1)), 2);
        assert_eq!(compute_epoch_ticks(Duration::from_millis(1_001)), 2);
    }
}
