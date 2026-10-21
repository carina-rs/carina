# Host-Driven Long-Running Provider Operations: Design

Tracking: carina-rs/carina#3907

## Problem

The host bounds every WASM provider call with `WASM_OPERATION_HARD_TIMEOUT`
(20 min, `carina-plugin-host/src/wasm_factory.rs`). When the bound elapses the
call is dropped and the shared provider instance is **poisoned**: every later
call on that provider (including `read`) fails fast. The constant's
doc-comment states the contract that makes this safe:

> **Provider contract:** a single provider operation must complete within this
> budget. A waiter that needs longer must be expressed as the carina `wait`
> construct (separate short reads the executor drives) rather than a blocking
> loop inside one `create`/`delete` call.

Nothing enforces that contract, and the awscc provider breaks it. Its
CloudControl delete polls inside one call for up to 30 min for
`AWS::EC2::IPAM`, `IPAMPool`, `TransitGateway`, `TransitGatewayAttachment`
and `VPCGatewayAttachment`, and for 20 min (exactly the host bound) for
`NatGateway` (budgets from codegen `operation_config_for_type`, fallback
`default_polling_attempts` in `provider/cloudcontrol.rs`). Every
create/update polls up to 10 min, and retry loops on top re-issue requests
with up to 120 s of backoff each.

The `wait` construct the contract points to does not fit this case: it waits
for a *user-declared resource condition* through `read`, while here the
provider has to finish *its own* create/update/delete request.

### Observed failure (carina#3907)

`ec2_subnet/with_ipam` destroy, reconstructed from CloudTrail and the run log:

| Time (JST) | Event |
|---|---|
| 08:00:58 | Subnet deleted; carina issues CloudControl `DeleteResource` for the VPC-scoped `IPAMPool` and polls inside the `delete` call |
| 08:21:13 | Host hard timeout (20 min) fires; instance poisoned; destroy's extended `wait_for_deletion` reads fail instantly; `Vpc`, top pool and `IPAM` all fail; log ends |

Measurements against real AWS (carina-test-000/001, 2026-10-21):

| Scenario | Result |
|---|---|
| Delete subnet, poll the VPC pool's allocations | IPAM released the deleted subnet's allocation after **1081 s** and **1631 s** in two runs |
| Pool delete issued *after* the allocation was released | CloudControl `SUCCESS` in **47 s** |
| Pool delete issued *immediately* after subnet deletion (what carina does) | CloudControl `SUCCESS` after **1803 s** (allocation released at 1631 s, handler noticed on its next cycle) |

The AWS operation is slow but healthy. The failure is carina's: a ~30 min
operation runs inside one WASM call that the host kills at 20 min, and the
kill takes the whole provider instance down with it.

### Why this is a type-level problem

Raising the hard timeout, or shrinking awscc's in-call budget below it, only
moves the collision point. Today a provider can only express "this takes a
while" by blocking inside the call, so the contract can only be honoured by
convention. The fix gives providers a typed way to say "accepted, not finished
yet", so no operation needs to block, and the host owns the wait.

## Goals

- A provider create/update/delete can return a typed **pending** outcome
  quickly; the host drives completion through short, separate poll calls.
- The total wait budget for an operation is owned by the host, taken from the
  resource schema's `OperationConfig`, and is independent of the per-call
  hard timeout.
- A slow-but-healthy operation never poisons the provider instance.
- awscc stops blocking inside a call for CloudControl request completion,
  and aws stops blocking for its resource-state waiters.
- One wait mechanism: destroy's separate read-based extended wait is removed.
- An apply/destroy that runs longer than the state lock TTL keeps its lock.

## Non-goals

- A create that runs out of budget keeps today's result,
  `ProviderError::Timeout`, and its in-flight identifier is lost as today.
  Recording it needs its own writeback source, create_before_destroy rules and
  next-plan behaviour; tracked in carina#3909.
- Persisting pending operations across carina invocations.
- Cancellation semantics. The first Ctrl+C (graceful) lets in-flight provider
  futures run to completion, which now means up to the operation's budget.
  The second Ctrl+C (cleanup priority) drops them: a drop between polls holds
  no store lock and does not poison; a drop during a WASM call poisons as
  today. Passing a shutdown signal into the `Provider` trait is a separate
  design.
- The `wait` DSL construct (`2026-05-09-wait-construct-design.md`). Its
  non-goal "provider-side `wait()` API" concerns resource conditions and is
  unaffected.
- The WIT `async func` migration (`2026-06-06-3400-wit-async-export-design.md`).
  Async exports would stop a long call from blocking a thread, but the call
  would still be one call under one budget; this design is orthogonal.
- In-call retries of single API calls that are bounded well under the hard
  timeout (aws `retry_aws_operation`, worst case ~6.6 min; ACM DVO 30 s; flow
  log IAM propagation ~1 min; S3 force-delete bucket emptying). They are not
  waits for an operation to finish.
- Duplicate-create risk when the initial CloudControl `CreateResource` send
  fails ambiguously and is re-sent. This exists today and is unchanged.
- Removing the native (non-WASM) JSON-RPC transport. The host no longer
  loads native providers, so it is dead code, but deleting it also touches
  release workflows and the provider resolver's native fallback; that is a
  separate cleanup.
- carina#3908 (non-TTY destroy output). Removing the extended wait deletes
  the timeout-path `multi.println` lines that #3908 lists; any remaining
  non-TTY gaps in destroy's output stay in #3908.
- State and saved-plan formats are unchanged.

## Design

### WIT (`carina-plugin-wit`, protocol v8)

```wit
// types.wit
/// An operation the provider started but has not finished.
record pending-operation {
  /// Opaque to the host; only this provider's matching poll export
  /// interprets it. Must not carry request or secret data: the host passes
  /// the original request back on every poll.
  token: string,
  /// When the provider wants the next poll. The host clamps it to [1, 300].
  poll-after-secs: u32,
}

variant create-outcome {
  success(state),
  partial-success(create-partial-success),   // unchanged
  pending(pending-operation),                // new
}
variant update-outcome {
  success(persisted-state),                  // unchanged arms
  partial-success(update-partial-success),
  pending(pending-operation),                // new
}
variant delete-outcome { deleted, pending(pending-operation) }   // new

// provider.wit
delete: func(id: resource-id, identifier: string, request: delete-request)
    -> result<delete-outcome, provider-error>;      // was result<_, provider-error>
poll-create: func(id: resource-id, request: create-request, pending: pending-operation)
    -> result<create-outcome, provider-error>;
poll-update: func(id: resource-id, identifier: string, request: update-request,
                  pending: pending-operation)
    -> result<update-outcome, provider-error>;
poll-delete: func(id: resource-id, identifier: string, request: delete-request,
                  pending: pending-operation)
    -> result<delete-outcome, provider-error>;
```

- One poll export per operation kind, each returning that kind's own outcome.
  A single `poll(kind, ..)` returning a union would let a create token come
  back as a delete result.
- Every poll receives the original request (and identifier for
  update/delete). A poll that must re-send a request (initial send failed,
  or a retryable failure) has what it needs without the token carrying
  desired attributes. The request records go through the existing
  sealed-secret handling (#3800/#3897); copying them into a token would take
  them out of it. The host cannot leak a token (see the token newtype under
  Host), but a provider should still send nothing it can get back from the
  request.
- A poll may return `pending` again: still running, or the provider re-issued
  a retryable request.

`PROTOCOL_VERSION` and `MIN_SUPPORTED_PROTOCOL_VERSION` move to 8. The check
runs before typed binding (carina#3842), so an old provider fails with the
version error.

### Protocol and SDK (`carina-provider-protocol`, `carina-plugin-sdk`)

The protocol crate's `CreateOutcome` / `UpdateOutcome` stay the *settled*
outcomes, with no pending arm. Pending exists only on the WIT boundary and in
the SDK's own generic types, so the JSON-RPC wire (`methods::CreateResult`
etc.) cannot carry a pending value at all.

The protocol crate owns the one shared pending-operation driver, so no caller
re-derives the clock start, clamp, deadline or final-poll rule:

```rust
pub const DEFAULT_OPERATION_BUDGET: Duration = Duration::from_secs(15 * 60);
pub enum OperationStep<D, P> {
    Done(D),
    Pending { pending: P, poll_after_secs: u32 },
}
pub enum DriveOutcome<D> { Done(D), OverBudget }
pub async fn drive_pending<D, P, E, ...>(
    budget: Duration,
    initial: impl Future<Output = Result<OperationStep<D, P>, E>>,
    poll: impl FnMut(P) -> impl Future<Output = Result<OperationStep<D, P>, E>>,
    sleep: impl FnMut(Duration) -> impl Future<Output = ()>,
) -> Result<DriveOutcome<D>, E>;

pub enum OperationKind { Create, Update, Delete }
impl OperationConfig {
    /// `{create,update,delete}_timeout_secs` for `kind`, or
    /// DEFAULT_OPERATION_BUDGET when unset.
    pub fn budget(&self, kind: OperationKind) -> Duration;
}
```

`drive_pending` starts its `Instant` before polling `initial`, clamps each
requested delay to 1–300 seconds and to the remaining budget, and performs one
final poll after a sleep that reaches the deadline. Sleep is injected, so the
protocol crate has no async-runtime dependency. Every caller gets its budget
through `OperationConfig::budget` (with
`DEFAULT_OPERATION_BUDGET` when the resource type has no config), so no loop
picks a timeout field itself. `OperationConfig` itself is defined once in
`carina-provider-protocol`; `carina-core::schema` publicly re-exports that
same type so native core providers have `budget(kind)` directly.

The SDK trait is generic over each kind's token, chosen per provider. Rust has
no defaults for associated types, so the compiler forces the choice:

```rust
pub trait CarinaProvider {
    type CreateToken: Serialize + DeserializeOwned;
    type UpdateToken: Serialize + DeserializeOwned;
    type DeleteToken: Serialize + DeserializeOwned;

    fn create(&self, id: &ResourceId, req: CreateRequest)
        -> Result<sdk::CreateOutcome<Self::CreateToken>, ProviderError>;
    fn poll_create(&self, id: &ResourceId, req: CreateRequest,
                   pending: sdk::Pending<Self::CreateToken>)
        -> Result<sdk::CreateOutcome<Self::CreateToken>, ProviderError>;
    // update / poll_update, delete / poll_delete likewise
}
```

- The SDK's generic outcomes wrap the settled protocol outcomes:
  `sdk::CreateOutcome<T> = Done(proto::CreateOutcome) | Pending(Pending<T>)`,
  `sdk::UpdateOutcome<T>` likewise, and
  `sdk::DeleteOutcome<T> = Deleted | Pending(Pending<T>)`. The
  `export_provider!` glue converts them to the WIT forms, serialising the
  token to the string and deserialising it in the poll glue. A string that
  does not decode as the provider's token type is a boundary decode error.
- A provider that never defers a kind sets that token to the SDK's
  uninhabited `Never` (an empty enum with serde derives: it serialises with
  `match *self {}` and its deserialisation always fails). `Pending<Never>`
  cannot be constructed, so that kind's `pending` arm cannot be returned, and
  its poll body is `match pending.token {}`.
- The token type is per provider and per operation kind, not per resource
  type. A provider whose tokens cover several resource types (aws: NAT
  gateway, TransitGateway, Organizations account) uses an enum. The host binds
  each pending value to the `ResourceId` it came from (below), so a token only
  returns to the resource that issued it.
- **JSON-RPC dispatch.** The native `run`/`dispatch` transport is sync and
  serves one request at a time over stdio. It drives a pending operation to a
  settled outcome by running the shared async `drive_pending` with a trivial
  ready-only, dependency-free `block_on`; its injected sleep calls
  `std::thread::sleep` and returns a ready future. Its wire methods (including
  `DeleteResult { ok }`) stay unchanged. Its budget comes from a resource type
  → `OperationConfig` map that `run` builds once from `provider.schemas()` at
  startup, falling back to `DEFAULT_OPERATION_BUDGET`.

The driver is invoked in three runtime contexts: the host adapter injects
`tokio::time::sleep`, the SDK JSON-RPC transport injects blocking sleep, and
the Phase 2 native core provider impls inject `tokio::time::sleep`. The loop
rules and clock live only in the protocol driver.

### Host (`carina-plugin-host`)

`WasmProvider` keeps implementing the core `Provider` trait, whose signatures
do not change; the polling loop lives in the adapter:

```rust
drive_pending(
    budget(id.resource_type, OperationKind::Delete),
    call_delete(id, identifier, request),       // one WASM call
    call_poll_delete,                           // one WASM call per poll
    tokio::time::sleep,                         // no store lock held
).await
```

The loop sleeps at most until the deadline and then makes one final poll; if
that poll is still pending the operation is over budget. Over budget is
`ProviderError::Timeout` for every kind, with the instance healthy. For update
the executor already schedules a refresh read; the cloud-side update may
still be running, so the next apply can hit a conflict until it finishes,
reported as an ordinary provider error.

- **Typed pending kinds.** Inside the host, `PendingCreate`, `PendingUpdate`
  and `PendingDelete` are distinct types, each built only by converting that
  kind's outcome, and each owns the `ResourceId`, identifier (update/delete)
  and request it was created for. A poll call takes the pending value alone,
  so a token can be sent neither to another kind's poll nor for another
  resource, and the request passed back is always the one that started it.
- **Token newtype.** Inside those types the token is a host newtype with a
  redacted `Debug`, no `Display` and no serde. Tokens live only in memory for
  the duration of one operation, so the host has no way to log or persist
  one.
- **Hard timeout scope.** `with_operation_timeout` currently wraps the
  store-lock acquisition as well as the guest call, so time queued behind
  sibling operations counts against the 20 min. The timeout moves to the
  guest call after the lock is held. The 20 min value stays as the backstop
  for hung calls. Each create/update/delete/poll call takes and releases the
  store lock on its own, so the sleep between polls never blocks other
  operations on the same provider.
- **Budget source.** The factory and each `WasmProvider` share one
  `Arc<Vec<carina_core::schema::ResourceSchema>>`. Budget lookup finds the core
  schema whose `resource_type` matches `ResourceId.resource_type`, then calls
  its re-exported `OperationConfig::budget(kind)`. There is no parallel
  protocol-config map or second config copy. A missing schema, config or field
  means `DEFAULT_OPERATION_BUDGET` (15 min), which covers every existing
  in-call waiter (longest: aws Organizations account create, 10 min). The
  budget spans all of an operation's polls and re-issues, so awscc's
  `delete_max_retries` / `create_max_retries` are capped by it.

### Destroy (`carina-cli`)

Destroy's extended wait is removed. With the host driving the delete to
completion under the schema budget, `ProviderError::Timeout` from delete is a
genuine failure; keeping the second wait would stack a read-based wait (up to
30 min) on top of the provider budget (up to 60 min for IPAM), and the
read-based approach is the one rejected under Alternatives. In
`carina-cli/src/commands/destroy.rs`:

- the `Err(ProviderError::Timeout)` arm goes away, so a timed-out delete takes
  the generic failure branch and its dependents are skipped as
  "dependent failed";
- `timed_out_resources`, the pre-dispatch timed-out-dependency wait and the
  after-loop wait are deleted;
- `wait_for_deletion`, `WaitResult` and their tests are deleted from
  `shared/retry.rs` / `destroy.rs`;
- the `"Exceeded attempts to wait"` pattern in `is_retryable_delete_error`
  is deleted: it matched awscc's in-call wait exhaustion, which no longer
  exists.

### State lock heartbeat (`carina-cli`, `carina-state`)

The lock TTL is `DEFAULT_LOCK_TIMEOUT_SECS = 3600` (`carina-state/src/lock.rs`),
renewed today only inside `save_state_locked` /
`write_state_locked_for_cleanup`, and the S3 and local backends treat an
expired lock as takeable. A destroy of the IPAM chain (subnet, pool, VPC, top
pool, IPAM) under the new budgets can run past an hour, letting a concurrent
apply take the lock mid-run.

`StateBackend::renew_lock` already renews without a save. Concurrent renewals
are not safe (S3 renew is ETag-conditional, so a timer renewal racing a save
fails that save with `LockNotHeld`; local renew uses one fixed temp path), so:

- one heartbeat task owns renewals for a locked apply/destroy and renews
  every TTL/3. It starts after the pre-execution migration save
  (`load_state_persist_if_migrated` saves right after the lock is acquired)
  and before effects are dispatched. Every other save is after execution
  (there are no incremental saves mid-run), so the heartbeat never needs to
  pause;
- "stopped before any save or release" is enforced by ownership:
  `LockHeartbeat::start(backend, lock, on_lost) -> HeartbeatGuard` moves the
  `LockInfo` in. `guard.stop().await -> Result<LockInfo, LockLost>` joins any
  renewal in progress and returns the most recently renewed lock only when
  ownership was kept. `guard.stop_for_cleanup().await -> Result<LockInfo,
  LockLost>` aborts a possibly hung in-flight renewal and returns the last
  completed lock unless loss was already proven.
  Save and release take `&LockInfo`, so they cannot be reached while the guard
  holds it. For this to hold, `LockInfo` loses `Clone` and its public fields
  and constructor: only `acquire_lock`, `renew_lock` and a successful stop
  produce one. `Drop` aborts the task on `?` and panic paths;
- CLI lock flow uses `StateLock::Disabled(LockingDisabled)` and
  `StateLock::Held(LockInfo)` as witnesses, so `--lock=false` is explicit and
  cannot be confused with an accidentally missing lock. No heartbeat runs for
  `Disabled`;
- `with_lock_heartbeat` returns `HeartbeatOutcome::Kept { result, lock }` or
  `HeartbeatOutcome::Lost { result, lost }`. A renewal failing with
  `LockNotHeld` invokes `on_lost` to request graceful cancellation and yields
  `Lost`, which deliberately carries no lock capability. Apply/destroy map it
  to `AppError::StateLockLost`; they neither save state nor release the lock on
  that path because another process may own it. A transient renewal error is
  warned once per failure streak and retried on the next tick;
- test-only `LockInfo` construction accepts a short TTL, from which the
  heartbeat derives its TTL/3 interval, so tests run in milliseconds.

### `OperationConfig` transport

`OperationConfig` crosses the WASM boundary inside the `schemas()` JSON, not
in WIT. The canonical serde/`Default`/`PartialEq`/`Eq` type lives in
`carina-provider-protocol` (`types.rs`), and `carina-core::schema` re-exports
it. The host's exhaustive `ResourceSchema` conversion clones that same type;
there is no `proto_operation_config_to_core` conversion.

Adding `update_timeout_secs` intentionally leaves Phase 2 compile breakage in
the provider repositories: aws's `convert.rs` `ProtoOperationConfig` literal
must carry the field, and awscc codegen's `CodegenOperationConfig` plus
`emit_option_field` must emit it. Regeneration then updates
`schemas/generated/ec2/*.rs` (as well as the normal `generate-schemas.sh` and
`generate-docs.sh` outputs).

### awscc provider

Token types are enums:

```rust
enum CcCreateToken {
    NotIssued { attempt: u32 },                      // initial send hit a retryable SDK error
    InFlight { request: RequestToken, attempt: u32 },
}
enum CcDeleteToken {
    DetachIgw { request: Option<RequestToken>, attempt: u32 },  // IGW pre-step
    NotIssued { attempt: u32 },
    InFlight { request: RequestToken, attempt: u32 },
}
// CcUpdateToken: same shape as CcCreateToken
```

- `create`/`update`/`delete` send the CloudControl request once and return
  `pending(InFlight)`. A retryable SDK error on the initial send returns
  `pending(NotIssued { attempt + 1 })` with the backoff as `poll-after-secs`,
  instead of sleeping inside the call.
- `poll_*` does one step. `NotIssued` re-sends the request built from the
  request the host passed back, applying the same desired-state
  normalization `create` applies before sending. `InFlight` calls
  `GetResourceRequestStatus` once:
  - `IN_PROGRESS`/`PENDING` → same token, `poll-after-secs = 5`;
  - `SUCCESS` → the existing success handling (read-back for create/update);
  - `FAILED` with a retryable error and attempts left → re-issue, new
    `InFlight` token, backoff (up to 120 s) as `poll-after-secs`;
  - `FAILED` otherwise → the existing `PartialOrFailed` / error handling.
- The IGW pre-delete detach (`special_cases.rs`) becomes the `DetachIgw` phase
  of the delete token instead of a nested blocking update.
- `wait_for_operation_with_attempts` and `default_polling_attempts` are
  deleted. Budgets live only in codegen's `operation_config_for_type`.
  `AWS::EC2::IPAM`/`IPAMPool` delete goes from 1800 s to **3600 s**: the
  measured healthy pool delete took 1803 s, already over the old budget.
- Layering. The SDK trait impl (`main.rs`) wraps async inherent functions of
  the provider through `runtime.block_on`; the native
  `impl carina_core::provider::Provider` (`lib.rs`) is async and cannot call
  back into the SDK trait. So the provider exposes async inherent step
  functions (`create_step`, `poll_create_step`, and so on, returning
  `sdk::CreateOutcome<CcCreateToken>` etc.). The SDK trait impl calls exactly
  one step per WIT call. The native core impl drives the steps with the
  protocol's shared `drive_pending` and `tokio::time::sleep`, using the budget
  from its own `schemas()` through `OperationConfig::budget`, so the
  CloudControl/winterbaume round-trip tests keep calling a blocking-to-done
  `create`/`delete` under the same budgets (3600 s for an IPAM pool delete).

### aws provider

The explicit resource-state waiters (`wait_for_ec2_state` for NAT gateway
create/delete and TransitGateway / attachment create/delete, and the
Organizations account create waiter) become pending operations: the call
issues the API request and returns `pending` with a token naming the waiter
phase; `poll_*` does one describe call. `CreateToken` and `DeleteToken` are
enums over those phases; `UpdateToken` is `Never`, since no update waits.
Their budgets stay within `DEFAULT_OPERATION_BUDGET` (15 min), so aws needs no
`OperationConfig` entries. The layering matches awscc: async inherent step
functions, one step per SDK trait call, and the native core `Provider` impl
(`provider.rs`) drives them with `drive_pending` and `tokio::time::sleep`.

## Phases

Each phase is its own PR, landed in order, using the usual rev-bump flow
(carina-plugin-wit → carina → aws → awscc pinned to aws).

1. **carina-plugin-wit + carina (protocol v8).** WIT types and exports;
   `drive_pending`, `DEFAULT_OPERATION_BUDGET` and the single
   `OperationConfig` type with `budget` in the protocol crate; `LockInfo` made
   non-`Clone` with the heartbeat guard; SDK
   trait with associated token types, `Never` and generic outcomes; JSON-RPC
   dispatch driving pending to settled; host adapter with typed pending kinds,
   token newtype and core-schema budget lookup; `update_timeout_secs`; hard timeout
   scoped to the guest call (made injectable for tests, and the existing
   `HARD_TIMEOUT >= 2 * LONGEST_LEGITIMATE_WAITER_SECS` assertion revisited);
   destroy extended wait removed; lock heartbeat; WASM mock provider updated.
   Tests:
   - shared-driver unit tests: budget starts before the initial call, clamp
     bounds, never sleep past the deadline, and one final poll at the deadline;
   - in `carina-plugin-host/tests` with the WASM mock fixture: an operation
     that returns `pending` N times, then succeeds, for each of create, update
     and delete. The WASM mock gets no env vars (the env allowlist passes none
     to it), so N comes from a mock resource attribute (e.g. `pending_polls`)
     and the remaining count travels in the token; a mock resource type whose
     schema `OperationConfig` has a few-second budget covers the over-budget
     case (ends as `ProviderError::Timeout`, and the instance serves the next
     call);
   - in the same fixture: a sleeping poll loop does not block a concurrent
     operation on the same provider, and lock wait no longer counts against
     an injected short hard timeout;
   - CLI destroy tests use the builtin mock (core `Provider`, no host loop),
     so a new builtin-mock knob that makes delete return
     `ProviderError::Timeout` covers "a timed-out delete skips its dependents
     as dependent failed";
   - with an injected short TTL: the heartbeat renews the lock during a run
     longer than the renewal interval, cannot overlap a save, and cancels the
     run on `LockNotHeld`.
2. **aws + awscc rev bump.** Both providers move to v8 as described above.
   Expected compile breakage includes aws `convert.rs`'s
   `ProtoOperationConfig` literal and awscc's regenerated
   `schemas/generated/ec2/*.rs`; update the latter through codegen
   `emit_option_field`, not by hand. Acceptance: `ec2_subnet/with_ipam` full
   (apply, plan-verify, destroy) passes; the full awscc and aws suites and
   multi-step tests pass.

## Alternatives considered

| Option | Why not |
|---|---|
| Raise the host hard timeout (e.g. 60 min) | Moves the collision point; a hung call takes an hour to surface; a slow call holds the store lock for its whole duration and blocks sibling operations on the same provider. |
| Cap awscc's in-call wait below 20 min and rely on destroy's extended read-wait | Fixes this test, but the contract stays convention-only and creates/updates get no extended wait. |
| Host-side completion check by re-reading the resource (no new exports) | Works for delete (wait for NotFound) but not for create/update, where "exists" is not "finished", and loses the provider's own failure reason from the request. |
| Request payload inside the token, polls without the request | Tokens would carry resolved attributes, secrets included, as an unsealed string outside the sealed-secret path. |
