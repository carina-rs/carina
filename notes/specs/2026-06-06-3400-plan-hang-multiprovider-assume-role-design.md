# carina#3400: `plan` hangs on multi-provider stack with cross-account assume_role — root-cause investigation and design

Status: investigation in progress (2026-06-06)
Issue: https://github.com/carina-rs/carina/issues/3400

This document captures what is known so far about the carina#3400
livelock, the candidate root causes, and the path to a root-cause fix.
It is a working note: the fix itself has not landed and the final
strategy is still pending the version-bump experiment described under
"Next experiment".

## What we have observed

The repro is `aws-vault exec carina-registry-dev -- carina plan` in
`carina-rs/infra/envs/registry/dev/infra` (the registry-dev Provider
Registry stack). The stack has five provider instances:

| Instance | Provider | Region | Notes |
| --- | --- | --- | --- |
| awscc (default) | awscc | ap-northeast-1 | — |
| awscc_us (alias) | awscc | us-east-1 | — |
| aws (default) | aws | ap-northeast-1 | — |
| us (alias) | aws | us-east-1 | — |
| management (alias) | aws | ap-northeast-1 | cross-account `assume_role` into 412038850359 |

With `--refresh=false` (so no per-resource provider RPC happens) `carina
plan` prints the five "Using ..." announcement lines and then hangs.
Killed after 240 s with no further output, no error, no completion.
This was reproduced against the worktree binary at
`9ad...` (issue-3400-plan-hang-multiprovider branch off main `a46355c8`).

The "Using" line is printed by `instantiate_provider_into_router` in
`carina-cli/src/wiring/mod.rs:1122` immediately before
`factory.create_provider(...).await`. That `await` for the **fifth**
(`management`) instance never returns. The first four instances all
create cleanly.

### What the wasmtime trace shows

With `RUST_LOG=...wasmtime=trace,wasmtime_wasi=trace,wasmtime_wasi_http=trace`
plus `CARINA_WASI_HTTP_TRACE=1`:

- Exactly four `POST https://sts.ap-northeast-1.amazonaws.com/` requests
  complete with `status=200`. AWS SDK level credential resolution is
  succeeding on the wire.
- After the fourth STS response, no further HTTP traffic is emitted.
- `wasmtime::runtime::component::concurrent` enters a tight loop, at a
  rate of thousands of cycles per second, that never ends:

  ```
  new host task HostTask(3)
  delete host task HostTask(3)
  exit RuntimeInstance { instance: ComponentInstanceId(0), index: RuntimeComponentInstanceIndex(0) }
  ready to delete? false ( ... host_future_state: Live)
  suspend fiber: NeedWork
  resume_fiber: restore current thread None
  resume_fiber: suspend reason Some(NeedWork)
  ready to delete? true ( ... host_future_state: Dropped)
  delete guest task GuestTask(0)
  queueing call QualifiedThreadId(0, 2)
  push high priority: GuestCall(..., GuestCall { thread: QualifiedThreadId(0, 2), kind: StartImplicit })
  handle work item GuestCall(..., GuestCall { thread: QualifiedThreadId(0, 2), kind: StartImplicit })
  call GuestCall { ..., kind: StartImplicit } ready? true (do_not_enter: false; backpressure: 0)
  resume_fiber: save current thread None
  sync/async-stackful call: replaced None with QualifiedThreadId(0, 2) as current thread
  enter RuntimeInstance { instance: ComponentInstanceId(0), index: RuntimeComponentInstanceIndex(0) }
  ```

Two signals stand out:

- `HostTask(3)` is the *same* task ID created and deleted every cycle.
  Nothing is making forward progress, but the same conceptual host
  future is being re-attached on every poll.
- `host_future_state` alternates `Live ↔ Dropped` on every cycle of the
  loop. In `GuestTask::ready_to_delete` (wasmtime
  `src/runtime/component/concurrent.rs:4521`) `Live` blocks deletion
  and `Dropped` releases it, so the guest task is repeatedly created
  → suspended → dropped → recreated.

The 30 s WASM operation timeout (`WASM_OPERATION_TIMEOUT_SECS = 30` in
`carina-plugin-host/src/wasm_factory.rs:79`) does not fire because
epoch interruption only counts WASM compute time, not host-side waits.
The 20 minute wall-clock backstop (`WASM_OPERATION_HARD_TIMEOUT`) does
fire — but the user reports killing the process at 5 minutes; the
backstop converting hangs into bounded errors is not the same as
fixing them.

## Why this looks like a wasmtime concurrent-runtime bug exposed by our
guest pattern, not a plain network hang

- 4 STS responses arrived at status 200 in well under a second total.
  AWS authentication is fine.
- The loop persists with `--refresh=false`, so no per-resource provider
  RPC is the cause; the hang is during `call_initialize` for the
  fifth instance.
- `app-deploy` (a sibling stack in the same repo: 2 provider instances,
  no `assume_role`) plans cleanly in 16 seconds against identical
  provider lock SHAs. The differences vs. the failing stack are
  (a) five instances vs. two and (b) one of the five carries a
  cross-account `assume_role`.

The guest provider (`carina-provider-aws/.../main.rs:173 fn initialize`)
runs a `tokio::runtime::Runtime` it owns (on wasm32: `new_current_thread`
with time enabled only) and calls `runtime.block_on(...)` on
`AwsProvider::new_with_account_guard(...)`. When `assume_role` is set,
that delegates to `aws_config::sts::AssumeRoleProvider`, which in turn
requires a *chained* credential resolution: the base credential
provider must produce credentials *before* the STS `AssumeRole` call
can be signed. That chain involves multiple concurrent host-future
allocations against `wasi:http/outgoing-handler`.

The other four instances also use `wasi:http` (to call STS for caller
identity, region resolution, IMDS probes etc.), but with shorter
chains. The fifth instance's chain is the longest by construction.

We have not yet *proven* that "chain length" rather than "fifth WASM
component instance" is the trigger.

## Candidate root causes (ranked)

### H1: sync-export-calls-async-import livelock exposed by wasmtime 43

The most plausible explanation, supported by upstream research:

- The provider WIT (`carina-plugin-wit/wit/provider.wit`) declares
  `initialize` as a sync `func`, not an `async` one.
- The guest runs an embedded `tokio` reactor in `block_on` and drives
  `wasi:http/outgoing-handler` (an async-shaped import surface) inside
  that sync export.
- WebAssembly component-model spec PR
  [WebAssembly/component-model#578](https://github.com/WebAssembly/component-model/pull/578)
  declares that pattern (a sync task synchronously calling an async
  import before returning) is a trap, and wasmtime
  [PR #12043](https://github.com/bytecodealliance/wasmtime/pull/12043)
  implements that trap. It was merged 2025-12-09 and backported to
  40.x via #12144; it is **not** present in 43.0.2.
- In 43.0.2 the lack of the trap means the concurrent runtime keeps
  trying to make progress on a configuration the spec now considers
  invalid. The `HostTask(3)` recycle + `host_future_state` flip we see
  is consistent with the runtime endlessly re-arming a future for a
  task that has structurally no way to complete.

The first four instances "work" probably because they finish their
internal-runtime work before the runtime reaches the problematic
re-entry state — i.e., they are luck rather than evidence of
correctness. This part is a hypothesis.

### H2: per-instance leak in `shared_instances`

`WasmProviderFactory::get_or_create_shared_instance`
(`carina-plugin-host/src/wasm_factory.rs:1633`) holds the
`shared_instances` async `Mutex` *across* the entire
`create_initialized_instance` await — which in turn awaits the WASM
`call_initialize`. If `call_initialize` for the fifth instance is the
hang, the Mutex stays held and any other binding's instantiation that
queues behind it is also blocked, but that is a *symptom* of H1, not an
independent cause. We would not expect this pattern to livelock on its
own.

### H3: `traced_send_request_handler` future leak

The `CARINA_WASI_HTTP_TRACE=1` path wraps the request in an extra
`wasmtime_wasi::runtime::spawn` and returns `HostFutureIncomingResponse::pending(handle)`.
If the inner future panicked the JoinHandle would carry the panic, but
the trace shows clean 200 OK lines, so this is unlikely to be the
cause. The hang reproduces with `CARINA_WASI_HTTP_TRACE` unset; the
default path (`default_send_request`) hangs identically.

## What we are not going to do

- **Per-instance bandaid.** A symptom-level fix that special-cases the
  five-instance / assume-role stack would not address the underlying
  pattern. Any new caller that wires a long async-chain inside a sync
  export would re-hit the same bug. Per the carina core rule, we fix
  the upstream invariant, not each consumer.
- **Raise `WASM_OPERATION_HARD_TIMEOUT`.** The backstop turning the
  hang into a `ProviderError::timeout` is a degradation, not a fix.
  The user wants `plan` to *succeed*.
- **Tell users to flatten their stacks.** carina advertises the
  multi-provider + `assume_role` pattern as supported; making that
  unusable in practice is not a fix.

## Next experiment (before final design)

The single highest-leverage check is to bump `wasmtime` /
`wasmtime-wasi` / `wasmtime-wasi-http` from 43 to **44 or 45** and
re-run the repro. Three possible outcomes:

1. **Trap with a clear message** at the `assume_role` boundary —
   confirms H1, and the fix is "make the WIT export `async` (or restructure
   the guest so it does not drive an async-shaped import from a sync
   export)". This is a workspace-wide change but the trap will pinpoint
   every call site that needs updating.
2. **Plan completes** — wasmtime fixed the underlying concurrent-runtime
   livelock without changing the spec. The fix becomes "bump
   wasmtime" plus a regression test in `carina-plugin-host` that
   instantiates ≥2 WASM provider instances each running multiple
   in-flight `wasi:http` requests inside a sync `initialize`.
3. **Same hang** — H1 is wrong, look at H2/H3 more carefully and
   write a minimal repro inside `carina-plugin-host` using
   `carina-provider-mock`.

Outcome 1 is the most useful even though it requires the most work,
because it gives us a *type-level* guarantee that the buggy pattern is
detected at every call site — exactly the "make the broken state
unrepresentable" criterion in `CLAUDE.md`'s root-cause rule.

## Reproduction signal we need to keep

Regardless of which outcome lands the fix, the final PR must include a
regression test that fails on the **current** code and passes on the
fix. The shape that matches the production failure mode is roughly:

- `carina-plugin-host` integration test
- Spin up ≥3 WASM provider instances against the same factory using a
  modified `carina-provider-mock` whose `initialize` issues several
  `wasi:http/outgoing-handler` requests to a localhost test HTTP
  server before returning
- Assert each instance's `create_provider().await` returns within a
  bounded time (e.g. 10 s)

If H1 is the cause and outcome 1 is the path, this test would be
expressed as "the modified mock guest panics with the `12043` trap at
build-time" — the test still proves we removed the buggy pattern.

If H2/H3 turns out to be the cause, the same regression test catches
it; we then thread the fix into the host.

This regression test is a **prerequisite** for the fix PR, not a
follow-up — without it a future refactor could silently re-introduce
the bug. CLAUDE.md's "make the broken state unrepresentable" rule
applies: the fix should ideally also be type-level, but at minimum the
test exists.

## Open items

- Run the wasmtime 44 / 45 experiment.
- If outcome 3, write the minimal mock-guest repro and bisect inside
  `carina-plugin-host`.
- Decide whether the WIT change to `async` exports is in-PR or split
  (almost certainly in-PR, given the root-cause rule; measure radius
  with `cargo check --workspace --all-targets 2>&1 | grep error | wc -l`
  after stubbing the WIT change).
- Confirm via dagayn `get_impact_radius` how many call sites need to
  thread the new async signature; this informs the workspace-vs-PR
  split decision.

## Codex investigation addendum: Task A - wasmtime 44/45 bump feasibility

Command attempted for 44:

```sh
cargo check -p carina-plugin-host 2>&1 | tee /tmp/codex-3400-check44.log
```

Result: the check did not reach Rust compilation. Cargo attempted to
update the crates.io index and failed because this sandbox cannot
resolve `index.crates.io`. `grep -c '^error' /tmp/codex-3400-check44.log`
returned `1`; that one error is the dependency-resolution failure, not
a source compile error:

```text
error: failed to get `aws-config` as a dependency of package `carina-cli v0.4.0 (...)`
Caused by:
  failed to download from `https://index.crates.io/config.json`
Caused by:
  [6] Couldn't resolve host name (Could not resolve host: index.crates.io)
```

Command attempted for 45:

```sh
cargo check -p carina-plugin-host 2>&1 | tee /tmp/codex-3400-check45.log
```

Result: identical dependency-resolution failure before Rust
compilation. `grep -c '^error' /tmp/codex-3400-check45.log` returned
`1`; again this is not a source compile error.

Temporary manifest changes were reverted manually because this sandbox
cannot write the Git worktree index (`git checkout -- ...` failed on
`.git/worktrees/.../index.lock`). Verification:

```sh
git diff -- carina-plugin-host/Cargo.toml Cargo.lock
```

returned no diff after each temporary bump.

Local registry availability:

- 43 sources are present under
  `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/wasmtime-43.0.2`,
  `wasmtime-wasi-43.0.2`, and `wasmtime-wasi-http-43.0.2`.
- No `wasmtime-*`, `wasmtime-wasi-*`, or `wasmtime-wasi-http-*` 44/45
  source directories or crate archives were present in
  `~/.cargo/registry/src` or `~/.cargo/registry/cache`.

Known 43 API surface currently used by `carina-plugin-host`:

- `wasmtime_wasi_http::p2::WasiHttpHooks::send_request` in 43.0.2 is
  `fn send_request(&mut self, hyper::Request<body::HyperOutgoingBody>, types::OutgoingRequestConfig) -> HttpResult<types::HostFutureIncomingResponse>`.
  Carina implements that exact signature in
  `carina-plugin-host/src/wasm_factory.rs:343`.
- `wasmtime_wasi_http::p2::default_send_request` in 43.0.2 is
  `pub fn default_send_request(request, config) -> types::HostFutureIncomingResponse`.
  Carina calls it in `carina-plugin-host/src/wasm_factory.rs:398`.
- `wasmtime_wasi::runtime::spawn` in 43.0.2 is
  `pub fn spawn<F>(f: F) -> AbortOnDropJoinHandle<F::Output> where F: Future + Send + 'static, F::Output: Send + 'static`.
  Carina uses it in the trace wrapper and local copied handler paths
  (`carina-plugin-host/src/wasm_factory.rs:377`, `:514`, `:529`).
- The generated bindgen constructors are called as
  `CarinaProvider::instantiate_async(&mut store, component, &linker).await`
  and `CarinaProviderWithHttp::instantiate_async(&mut store, component, &linker).await`
  in `carina-plugin-host/src/wasm_factory.rs:1019` and `:1170`.
- Linker/store usage remains the 43 shape:
  `Store::new(engine, host_state)`, `store.limiter(...)`,
  `store.set_epoch_deadline(...)`, `Linker::new(engine)`,
  `add_only_http_to_linker_async(&mut linker)`.

API delta summary for 44/45: not measured from local registry sources.
The requested registry paths do not exist locally, and Cargo could not
download them in this sandbox. A future run with network access should
repeat the exact same commands and then compare these four symbols
against the 43 signatures above.

Feasibility conclusion:

- 44: **blocked / unclassified** from this run. The build never reached
  Rust compile errors, and the 44 source was not locally available, so a
  truthful `trivial` / `moderate` / `major rewrite` label cannot be
  assigned from evidence.
- 45: **blocked / unclassified** from this run for the same reason.
  Do not infer bump cost from the single `error` count; it is a Cargo
  network/index error, not an API migration error.

## Codex investigation addendum: Task B - H1 guest cross-check

Criterion 1: is `initialize` async-lifted in WIT?

No. The WIT declaration is a plain sync function:

```text
carina-plugin-wit/wit/provider.wit:29
initialize: func(attrs: list<tuple<string, value>>) -> result<_, provider-error>;
```

There is no `async` modifier. The SDK guest export wrapper is also a
plain `fn initialize(...) -> Result<...>` and directly calls the
provider trait implementation (`carina-plugin-sdk/src/wasm_guest.rs:872`
through `:878` for the HTTP world).

Criterion 2: does the guest call an async-shaped import inside that sync
export?

Yes. The AWS provider's wasm32 config path installs
`carina_plugin_sdk::wasi_http::WasiHttpClient` as the AWS SDK HTTP
client:

```text
carina-provider-aws/src/lib.rs:168
use carina_plugin_sdk::wasi_http::WasiHttpClient;
carina-provider-aws/src/lib.rs:171
.http_client(WasiHttpClient::new())
```

That SDK client imports and calls `wasi:http/outgoing-handler`:

```text
carina-plugin-sdk/src/wasi_http.rs:38
use wasi::http::outgoing_handler;
carina-plugin-sdk/src/wasi_http.rs:305
let future_response = outgoing_handler::handle(outgoing_req, options)
```

The guest binding explicitly maps the HTTP import:

```text
carina-plugin-sdk/src/wasm_guest.rs:93
"wasi:http/outgoing-handler@0.2.6": ::wasi::http::outgoing_handler,
```

Criterion 3: does the guest block waiting for that import's response?

Yes. `initialize` calls the async AWS initialization and account guard
through an internal runtime:

```text
carina-provider-aws/src/main.rs:36
let runtime = tokio::runtime::Builder::new_current_thread()
carina-provider-aws/src/main.rs:187
let provider = self.runtime.block_on(AwsProvider::new_with_account_guard(...));
carina-provider-aws/src/main.rs:196
self.runtime.block_on(provider.verify_account_id())?;
```

The AWS config path awaits config loading and, when `assume_role` is
present, awaits `wrap_with_assume_role`:

```text
carina-provider-aws/src/lib.rs:169-176
let base = aws_config::defaults(...).http_client(WasiHttpClient::new()).load().await;
match assume_role {
    None => base,
    Some(ar) => Self::wrap_with_assume_role(base, ar).await,
}
```

The `wasi:http` response path then waits synchronously on the future
response:

```text
carina-plugin-sdk/src/wasi_http.rs:319-320
let pollable = future_response.subscribe();
pollable.block();
```

Verdict: H1 is **consistent with the observed livelock**. The strongest
evidence is the combination of a sync WIT export (`initialize: func`),
the guest's internal `current_thread` `block_on`, and the SDK HTTP
transport calling `outgoing_handler::handle` followed by
`subscribe().block()` before `initialize` returns. I found no direct
contradictory evidence: WIT does not declare `initialize` async, and
the guest does block on the import response path.

## Concrete next-step procedure (added by Codex)

Because this sandbox could not download wasmtime 44/45, the cleanest
next experiment is to run the bump on a machine with crates.io access
and use wasmtime 45, which includes the newer trap code
`CannotBlockSyncTask` documented as "A synchronous task attempted to
make a potentially blocking call prior to returning."

Run:

```sh
cd /Users/mizzy/src/github.com/carina-rs/carina/.worktrees/issue-3400-plan-hang-multiprovider

apply_patch <<'PATCH'
*** Begin Patch
*** Update File: carina-plugin-host/Cargo.toml
@@
-wasmtime = { version = "43", features = ["component-model"] }
-wasmtime-wasi = "43"
-wasmtime-wasi-http = "43"
+wasmtime = { version = "45", features = ["component-model"] }
+wasmtime-wasi = "45"
+wasmtime-wasi-http = "45"
*** End Patch
PATCH

cargo check -p carina-plugin-host 2>&1 | tee /tmp/codex-3400-check45.log
grep -c '^error' /tmp/codex-3400-check45.log

# If the host compiles, rebuild the provider component and rerun the issue #3400 repro.
# Use the same environment Opus used for the successful reproduction, including AWS
# credentials and CARINA_WASI_HTTP_TRACE=1 if detailed host HTTP phases are useful.
cargo build -p carina-cli
CARINA_WASI_HTTP_TRACE=1 cargo run -p carina-cli -- plan <path-to-five-provider-repro-stack> \
  2>&1 | tee /tmp/codex-3400-plan45.log
```

Exact Cargo.toml diff:

```diff
diff --git a/carina-plugin-host/Cargo.toml b/carina-plugin-host/Cargo.toml
--- a/carina-plugin-host/Cargo.toml
+++ b/carina-plugin-host/Cargo.toml
@@
-wasmtime = { version = "43", features = ["component-model"] }
-wasmtime-wasi = "43"
-wasmtime-wasi-http = "43"
+wasmtime = { version = "45", features = ["component-model"] }
+wasmtime-wasi = "45"
+wasmtime-wasi-http = "45"
```

Expected verdict matrix:

- If the repro traps with `CannotBlockSyncTask` or the text
  `A synchronous task attempted to make a potentially blocking call
  prior to returning`, H1 is confirmed. Fix path: make the provider WIT
  export async where it can drive `wasi:http`, or restructure the guest
  so sync exports never wait on async-shaped imports before returning.
- If `plan` completes with the same five-provider stack, the practical
  fix is the wasmtime bump itself. Add a regression test in
  `carina-plugin-host` that instantiates multiple HTTP-capable WASM
  provider instances whose `initialize` issues several
  `wasi:http/outgoing-handler` requests, and assert bounded completion.
- If the same hang occurs on 45, H1 is wrong or incomplete. Next
  investigation: instrument `shared_instances` lock hold time and the
  host `HostFutureIncomingResponse` lifecycle around
  `default_send_request`, then build a minimal `carina-provider-mock`
  repro that removes AWS SDK credential-chain complexity.
- If the 45 bump produces a large API migration before the repro can run,
  classify the bump by the actual compiler diagnostics and use one of
  the smaller experiments below.

Smaller alternatives if the bump is a major rewrite:

- Modify `carina-provider-mock` so sync `initialize` performs multiple
  `wasi:http/outgoing-handler` requests using the existing
  `carina-plugin-sdk::wasi_http` path, then instantiate at least three
  provider instances against a local HTTP server. The goal is to
  reproduce the same NeedWork/StartImplicit trace pattern without AWS.
- Patch only the AWS guest locally so `initialize` skips
  `verify_account_id()` while still building clients with `assume_role`.
  If the fifth-provider hang disappears, the blocking STS response path
  is the narrow trigger; if it remains, the trigger is earlier in
  credential/config loading.
- Patch the AWS guest locally to remove `assume_role` wrapping but keep
  the five-provider shape and account guard. If the hang disappears only
  when `wrap_with_assume_role` is removed, H1 remains plausible and the
  STS credential-provider HTTP call count is the stressor to model in
  the mock repro.

## Experiment results (Opus, 2026-06-06) — corrected after misread

Ran the Codex-prescribed wasmtime 45 bump against the real repro.

### Build outcome

- `carina-plugin-host/Cargo.toml`: changed `wasmtime`, `wasmtime-wasi`,
  `wasmtime-wasi-http` from `"43"` to `"45"`.
- `cargo check -p carina-plugin-host`: compiles cleanly with **zero**
  source-level changes (`Finished dev profile in 3.44s`).
- `cargo build --release -p carina-cli`: completes successfully.
- The Codex run's `cargo check` failed because the local
  `sccache-wrapper` `RUSTC_WRAPPER` is incompatible with the build
  invocation here; unsetting `RUSTC_WRAPPER` made the build pass.
  This is a local-tooling artifact, not an API-level break in
  wasmtime-wasi-http 45.

### Repro outcome — the livelock is NOT gone (Opus misread)

`aws-vault exec carina-registry-dev -- carina plan --refresh=false`
against `carina-rs/infra/envs/registry/dev/infra` "finished" in
17 seconds on the first wasmtime-45 build, and I (Opus) read that as
"the livelock is gone". That was wrong.

What actually happened: wasmtime-wasi-http 45 transitively pulled in
rustls 0.23, which requires `CryptoProvider::install_default()` before
any HTTPS exchange. The 17-second termination was a **panic** in
`tokio-rt-worker` at the very first STS request, not a clean plan
completion. Re-checking the stderr log makes this obvious:

```
thread 'tokio-rt-worker' panicked at .../rustls-0.23.36/src/crypto/mod.rs:249:14:
Could not automatically determine the process-level CryptoProvider from
Rustls crate features.
```

After Codex added `install_default_rustls_crypto_provider()` (Once-guard
calling `rustls_0_23::crypto::aws_lc_rs::default_provider().install_default()`
from every `WasmProviderFactory` constructor) and re-running the real-infra
repro, the rustls panic is gone — and the **original livelock comes back
unchanged**: 120 s without progress, output stuck on the five "Using ..."
lines exactly as on wasmtime 43.

So the corrected verdict for the bump experiment is:

- wasmtime 43: hang (livelock, from the start)
- wasmtime 45 with no rustls init: panic at first HTTPS request
- wasmtime 45 + `install_default` for `aws_lc_rs`: **same hang as wasmtime 43**

The version bump alone does NOT fix carina#3400.

### Why the regression test passes anyway

The new
`multi_instance_wasi_http_initialize_completes_bounded` test in
`carina-plugin-host/tests/wasm_integration_test.rs` passes in ~2.3 s.
It creates three mock-provider instances, each of which issues three
plain-HTTP requests to a localhost test server inside its sync
`initialize`. That happens to never enter the failure mode — likely
because the AWS SDK's credential chain (especially `assume_role`)
constructs many more concurrent in-flight host futures and runs
through more components of the wasi:http stack than a small loop of
hand-written `outgoing_handler::handle` calls. The mock guest reproduces
the *shape* of the bug Opus had in mind, not the *intensity* that
triggers the wasmtime concurrent-runtime livelock.

So:

- The mock test does not catch the bug today and would not catch it on
  a future regression either. It is currently dead weight.
- A genuine regression test needs to either (a) drive the carina-provider-aws
  initialise path against a stubbed STS server, or (b) reverse-engineer
  exactly which guest-side future shape triggers the runtime livelock
  and reproduce that minimally in the mock.

### Implications for the fix path

H1 ("sync export calls async import" pattern as the structural cause)
is **back to being the leading hypothesis**. Bumping wasmtime did not
defuse it because the underlying spec violation is in our guest code,
not in wasmtime's handling of valid guest code. The longer term fix
involves making the relevant WIT exports async-lifted so the guest
can return its Future to the host while the wasi:http response is
still pending, rather than synchronously waiting on it via
`subscribe().block()` before returning.

The Codex implementation as it stands ships:

- A wasmtime 43→45 bump (no behavioural change for this bug).
- A rustls 0.23 `CryptoProvider` install (only needed *because of* the
  wasmtime bump; with the bump reverted this is also unneeded).
- A regression test that does not actually catch the bug.

Sending this PR as-is would be a bandaid that lands tooling churn
without fixing the user's hang. The right move from here is to abandon
the bump-only direction and design a fix that addresses H1 directly —
either an async WIT export and the guest restructure that implies, or
a host-side change that breaks the spec-violating pattern apart so the
runtime is no longer stuck on it.

### Decision: do NOT ship the current diff as a PR

The Codex-implemented changes (Cargo.toml bump, install_default, mock
test, mock guest http loop) stay on this branch for now as artifacts
of the experiment, but they will not become a PR in their current
form. The next Opus turn re-opens the investigation with the corrected
verdict above as input.

## Path forward: C → A

User direction (2026-06-06): "carina-rs としては根本対応してくれればよい、
やり方は任せる". So this section commits to the root-cause path.

### Stage C — establish a carina-only repro that actually hangs

The current Codex mock test (3 instances × 3 plain HTTP requests in
`initialize`) does **not** reproduce the bug. Without a failing test,
any later fix can only be validated by re-running the real-infra
`carina plan` repro, which is slow, requires AWS credentials, and is
not runnable in CI. A carina-only repro is the prerequisite for the
real fix.

Concrete steps:

1. Read `carina-provider-aws/src/lib.rs` `wrap_with_assume_role` and
   the AWS SDK credential chain to enumerate which `wasi:http` shapes
   accumulate — the count and concurrency pattern of in-flight host
   futures during `assume_role` initialisation. We want the *shape*
   of in-flight futures, not the wire-level requests they make.
2. Stretch the mock guest's `initialize` to match that shape: spawn N
   concurrent `outgoing_handler::handle` calls from inside a
   `block_on`, holding all their `future-incoming-response` handles
   simultaneously, before draining them. Hold time / concurrency
   matters; don't just loop sequentially.
3. Run the new test against wasmtime 43 (revert the bump) — it should
   hang. Cap with `tokio::time::timeout(30s)` so it fails as a test,
   not as a hang.
4. Run the same test against wasmtime 45 + `install_default` — it
   should also hang (matching the real-infra observation). This
   confirms the bug is structural, not version-specific to 43.
5. The test is now the regression contract: any later fix must make
   it pass on whatever wasmtime version we ship.

Two things this stage does not do:

- Drag in AWS SDK or any AWS auth state. The guest pattern, not the
  wire content, is what triggers wasmtime's concurrent-runtime
  livelock — keep the test self-contained.
- Try to "fix" the test if it doesn't hang. If a faithful mock of the
  guest pattern doesn't hang, H1 is wrong and we need to revisit
  H2/H3 instead. The test failing-to-fail is itself information.

### Stage A — async WIT exports as the structural fix

Only entered once stage C has a reliably-hanging test. The fix is to
declare the relevant exports (`initialize`, plausibly also `read` /
`create` / `update` / `delete` / `read-data-source` since they
internally drive AWS SDK in the same shape) `async` in the WIT,
restructure carina-plugin-sdk so guests return Futures, and restructure
each provider crate's implementation to drop its embedded
`tokio::runtime::Runtime` + `block_on`. This is a workspace-wide
breaking change covering:

- `carina-plugin-wit/wit/provider.wit` (WIT declarations)
- `carina-plugin-sdk/src/wasm_guest.rs` (export macros)
- `carina-plugin-sdk/src/lib.rs` (CarinaProvider trait shape)
- `carina-plugin-host/src/wasm_factory.rs` (host call shape)
- `carina-provider-mock/src/main.rs`
- `carina-provider-aws/.../main.rs` and any internal block_on sites
- `carina-provider-awscc/.../main.rs` and any internal block_on sites

Will likely need one carina PR (host + SDK + WIT + mock + regression
test from stage C) coordinated with two follow-on provider PRs
(carina-provider-aws and carina-provider-awscc) that re-pin to the new
SDK rev. The provider PRs must merge before the carina PR can be
verified against the real infra, but the carina PR's own CI just needs
the mock provider + regression test.

The "make broken state unrepresentable" criterion is satisfied at the
type level: once exports are async-lifted, the guest physically cannot
synchronously block on an async-shaped import before returning — the
async return *is* the return — so a future contributor cannot
reintroduce the spec-violating pattern without going back to sync
exports first, which is a visible WIT-level decision.

If stage C's test does not hang, this whole stage is wrong; rethink.

What `plan` did on 45 — short version: it got past the management
provider's `call_initialize` and **panicked at a different point**
on a rustls 0.23 initialisation requirement:

```
thread 'tokio-rt-worker' panicked at
.../rustls-0.23.36/src/crypto/mod.rs:249:14:
Could not automatically determine the process-level CryptoProvider from
Rustls crate features. Call CryptoProvider::install_default() before
this point to select a provider manually, or make sure exactly one of
the 'aws-lc-rs' and 'ring' features is enabled.
```

`wasmtime-wasi-http 45` depends on rustls 0.23 (43 used 0.22), and
0.23 requires the embedder to pick a `CryptoProvider` explicitly
when multiple are available in features. This is purely a follow-up
init step in `carina-plugin-host`, not part of the hang.

### Implications for the fix

H1 ("sync export calls async import" being the structural cause) is
**no longer the leading hypothesis**. The wasmtime concurrent-runtime
livelock at 43 was real, but 45 makes forward progress with the
same WIT shape and the same guest code — i.e. the fix on the
embedder side is the version bump, not a WIT rework. The spec
change (PR #578) is still relevant long-term, but it does not need
to land in the same PR.

What the fix PR has to contain:

1. Bump `wasmtime` / `wasmtime-wasi` / `wasmtime-wasi-http` in
   `carina-plugin-host/Cargo.toml` from `"43"` to `"45"`.
2. Install a default `rustls` `CryptoProvider` exactly once, before
   any `wasmtime-wasi-http` outbound request can run. Two reasonable
   shapes (decide in implementation):
   - `rustls::crypto::aws_lc_rs::default_provider().install_default()`
     (matches what carina's local `traced_send_request_handler`
     already uses for the host-side rustls 0.22 stack — pick the
     same family for consistency); call once from a
     `std::sync::Once`-guarded helper invoked at
     `WasmProviderFactory` construction time.
   - or pin the rustls 0.23 feature flags so exactly one of
     `aws-lc-rs` / `ring` is enabled (less explicit, but no init
     code change).
3. Sweep other workspace crates that pin `rustls` / `tokio-rustls`
   to make sure their version constraints line up with whatever
   wasmtime-wasi-http 45 transitively pulls.
4. Regression test: a `carina-plugin-host` integration test that
   instantiates multiple HTTP-capable WASM provider instances whose
   `initialize` issues several `wasi:http/outgoing-handler` requests
   against a localhost test server, and asserts each
   `create_provider().await` completes within a bounded time
   (10 s). Without this, a future wasmtime downgrade or refactor
   could silently re-introduce the hang.

The "make the broken state unrepresentable" criterion in CLAUDE.md
is partly satisfied here by the wasmtime version pin (43 had the
bug, ≥45 does not), but the regression test is what actually
prevents a future regression in code we control.

### What we are explicitly NOT doing in this PR

- We are **not** changing the WIT to declare `initialize` (or any
  other export) `async`. That would be a workspace-wide breaking
  change to the guest SDK + every provider; it is not necessary to
  fix this issue given the wasmtime bump works.
- We are **not** lengthening `WASM_OPERATION_HARD_TIMEOUT`. The
  bump removes the hang, not the user's need for fast feedback.
- We are **not** filing a follow-up for the type-level "sync
  export can't call async import" guarantee. PR #12043 will
  eventually trap that pattern at runtime on a future wasmtime
  bump; carina inherits the guarantee from upstream rather than
  re-implementing it.

### Open question for the implementation PR

Whether to also bump `rustls` from `"0.22"` to match what
wasmtime-wasi-http 45 transitively uses. The current
`traced_send_request_handler` in `carina-plugin-host/src/wasm_factory.rs`
uses `rustls 0.22` and `tokio-rustls 0.25`; if we bump those too,
the local handler and the wasmtime-wasi-http internal handler
share one TLS stack again. Measure the radius before deciding
(`cargo check --workspace --all-targets` after the change).

## Stage C implementation attempt (Codex, 2026-06-06)
<!-- derived-from #stage-c--establish-a-carina-only-repro-that-actually-hangs -->

Implemented the mock-only guest shape requested for Stage C:

- `carina-provider-mock/src/main.rs` now accepts
  `__mock_initialize_http_concurrency`.
- `concurrency = 1` keeps the existing sequential
  `carina_plugin_sdk::wasi_http::send_request` path.
- `concurrency > 1` starts a batch of raw
  `wasi:http/outgoing-handler.handle` requests, holds all returned
  `future-incoming-response` resources simultaneously, subscribes all
  of them, and drains readiness with `wasi:io/poll.poll`.
- `carina-plugin-host/tests/wasm_integration_test.rs` has a bounded
  sequential boundary test (`5 instances, 16 requests_each,
  concurrency 1`) and an ignored concurrent repro candidate
  (`5 instances, 16 requests_each, concurrency 16`) with a 30 s timeout.

Verification completed in this sandbox:

```sh
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo build -p carina-provider-mock --target wasm32-wasip2
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo nextest run -p carina-plugin-host
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo nextest run -p carina-plugin-host multi_instance_wasi_http_initialize_concurrent_repro_candidate_times_out --run-ignored only
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo test --workspace --doc
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings
```

All commands passed, but the localhost HTTP integration tests are not
conclusive in this sandbox: binding `127.0.0.1:0` returns
`Operation not permitted (os error 1)`, so the helper prints
`SKIP: sandbox does not permit binding localhost HTTP test server` and
returns early. As a result, no mock-only hanging configuration has been
verified here. The next run needs a local environment where loopback
bind is allowed, then explicitly try the ignored repro candidate and
reduce from `(concurrency 16, instances 5, requests_each 16)` toward
`concurrency 8` and `4` if it hangs.

## Stage C follow-up: concurrent mock resource drop-order fix (Codex, 2026-06-06)
<!-- derived-from #stage-c--establish-a-carina-only-repro-that-actually-hangs -->

Opus ran the ignored concurrent repro candidate on a loopback-capable
machine and found it did not hang: it trapped after ~1.93 s while
dropping `FutureIncomingResponse` from the guest's
`run_concurrent_initialize_http_requests` path.

The root cause was a guest-side resource-lifecycle bug in the mock,
not the carina#3400 livelock. `PendingResponse` stored the
`FutureIncomingResponse` before the `Pollable` produced by
`future.subscribe()`. When a ready item was removed from the pending
batch, the code took the response and then let the whole
`PendingResponse` drop at scope exit. Rust field drop order therefore
attempted to drop the parent `FutureIncomingResponse` while the child
pollable was still live, which violates the component resource
ownership shape and traps in the guest drop chain.

The mock now removes a ready item, explicitly drops the `Pollable`
before calling `future.get()`, and then explicitly drops the
`FutureIncomingResponse` immediately after the response has been taken,
before checking status or consuming the response body. This keeps the
concurrent path aligned with the sequential SDK helper's resource
lifetime discipline: hand the request to `outgoing-handler.handle`,
finish the outgoing body, wait, take exactly one response, drop the
future promptly, and fully finish the incoming body.

Verification in this sandbox remains unclassified because loopback bind
is denied and the integration helper returns through its existing
`SKIP: sandbox does not permit binding localhost HTTP test server` path.
The guest does rebuild successfully:

```sh
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo build -p carina-provider-mock --target wasm32-wasip2
```

The next loopback-capable run should rerun:

```sh
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo nextest run -p carina-plugin-host multi_instance_wasi_http_initialize_concurrent_repro_candidate_times_out --run-ignored only --no-fail-fast
```

If that fixed candidate passes, continue the Stage C search by adding
ignored variants for higher per-instance concurrency (`32`, then `64`),
higher instance counts (`8`, then `10` with generated binding names),
and finally an inter-request fan-out shape that starts a smaller batch,
drains completions, and starts more requests while preserving
bookkeeping across batch boundaries. If it hangs, remove `#[ignore]`
from the hanging test and keep its timeout assertion as the regression
contract.

## Stage C AWS-SDK-shape mock variants (Codex, 2026-06-06)
<!-- derived-from #stage-c--establish-a-carina-only-repro-that-actually-hangs -->

Re-read the AWS guest and locked AWS SDK sources to narrow the async
shape. The current AWS process provider creates a wasm32
`tokio::runtime::Builder::new_current_thread().enable_time()` runtime
and `initialize` calls `self.runtime.block_on(...)` twice: first for
`AwsProvider::new_with_account_guard(...)`, then for
`provider.verify_account_id()`. In the provider library,
`new_with_account_guard` awaits `build_config`, `build_config` awaits
`aws_config::defaults(...).http_client(WasiHttpClient::new()).load()`,
and `wrap_with_assume_role` builds an
`aws_config::sts::AssumeRoleProvider`.

The locked `aws-config 1.8.12` credential chain does not use
`tokio::join!`, `try_join!`, `try_join_all`, `select_all`, or a
production-path `tokio::spawn` in the default credential chain or
`AssumeRoleProvider`. `CredentialsProviderChain::credentials` loops
providers and awaits one provider at a time. `AssumeRoleProvider`
constructs an STS fluent builder and later awaits
`fluent_builder.clone().send()`. The most plausible shape is therefore
a sequential dependency chain: base credentials are resolved first, the
AssumeRole request is signed from that result, then
`verify_account_id` performs a second STS `GetCallerIdentity` request
with the assumed-role credentials. Sleep futures exist in SDK retry /
timeout support and IMDS/test paths, so a sleep-interleave mock remains
worth probing, but it is not the primary evidence.

The mock provider now accepts `__mock_initialize_http_shape`:

| Shape | Mock behavior |
| --- | --- |
| `sequential` | Existing one-at-a-time SDK `wasi_http::send_request` path. |
| `poll-batch` | Existing raw `outgoing-handler` batch with poll-list drain. |
| `tokio-join` | Starts a batch, polls readiness interleaved with guest CPU work. |
| `spawn-await` | Holds one in-flight response while a nested helper owns and drops another response. |
| `sleep-interleave` | Starts/polls a batch with `wasi:clocks/monotonic-clock.subscribe-duration(...).block()` between HTTP polls. |

Stage C run table:

| Shape | Instances | Requests each | Concurrency | Outcome in this sandbox |
| --- | ---: | ---: | ---: | --- |
| `poll-batch` | 5 | 16 | 16 | PASS on loopback-capable machine after drop-order fix (~1.9 s; reported by Opus) |
| `tokio-join` | 5 | 16 | 4 | sandbox-blocked (`127.0.0.1:0` bind denied with `Operation not permitted`) |
| `spawn-await` | 5 | 8 | 2 | sandbox-blocked (`127.0.0.1:0` bind denied with `Operation not permitted`) |
| `sleep-interleave` | 5 | 16 | 4 | sandbox-blocked (`127.0.0.1:0` bind denied with `Operation not permitted`) |

The new ignored tests compile and the mock WASM guest builds in this
sandbox, but the localhost server cannot bind here, so these three new
variants have not been classified as real PASS/HANG/TRAP locally.

## Stage C verdict (no hang reproducible in mock)
<!-- derived-from #stage-c-aws-sdk-shape-mock-variants-codex-2026-06-06 -->

No carina-only mock hang has been found in this sandbox. The strongest
previous mock candidate now passes after the guest resource drop-order
fix, and the three AWS-SDK-shape variants added here are only
compile-verified because loopback bind is blocked. Until those variants
are run on a loopback-capable machine, the honest verdict is: **no
mock-only hanging regression contract exists yet**.

Candidate next steps:

- Instrument wasmtime's concurrent runtime and carina's
  `HostFutureIncomingResponse` path in the real repro to identify which
  host task is being re-armed in the NeedWork/StartImplicit loop.
- Build a stripped-down AWS-SDK-like guest fixture that carries the
  actual `aws-sdk-sts` / smithy client path into the mock and points it
  at a local STS-compatible server.
- Accept the cost and proceed with Stage A WIT async refactor based on
  the wasmtime trace evidence and the remaining real-infra repro, while
  keeping the mock variants as stress coverage rather than the primary
  regression contract.

### Loopback-capable run of the new variants (Opus, 2026-06-06)

Ran the three new Stage C variants on my workstation, which can bind
`127.0.0.1`:

```sh
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo nextest run -p carina-plugin-host \
  multi_instance_wasi_http_initialize_tokio_join_shape_times_out \
  multi_instance_wasi_http_initialize_spawn_await_shape_times_out \
  multi_instance_wasi_http_initialize_sleep_interleave_shape_times_out \
  --run-ignored only --no-fail-fast
```

All three **PASS within 5 seconds**. Updated table:

| Shape | Instances | Requests each | Concurrency | Outcome |
| --- | ---: | ---: | ---: | --- |
| `poll-batch` | 5 | 16 | 16 | PASS ~1.9 s |
| `tokio-join` | 5 | 16 | 4 | PASS ~5.0 s |
| `spawn-await` | 5 | 8 | 2 | PASS ~5.0 s |
| `sleep-interleave` | 5 | 16 | 4 | PASS ~5.0 s |

The verdict stands: **no mock-only configuration reproduces the
real-infra hang**. The mock variants exercise the sync-export +
concurrent / nested / interleaved wasi:http patterns that H1 and its
neighbours predict should livelock, and none do. H1 is wrong as a
sufficient cause; whatever AWS SDK actually does on the production path
is different in a way the mock has not captured.

### Decision: proceed with Stage A WIT async refactor on trace evidence

I am picking the third candidate next step. The real-infra repro is
reliable and the wasmtime trace (`HostTask(3)` recycle +
`host_future_state Live↔Dropped`) is concrete; we have enough evidence
to justify the structural fix even without a mock-side repro.
Implementation follows the Option C plan in
`./2026-06-06-3400-wit-async-export-design.md`. The Stage C tests added
on this branch stay as stress coverage; they are not the regression
contract, and removing them or weakening them later is acceptable.

The success criterion for the Stage A fix is unambiguous: the existing
real-infra `aws-vault exec carina-registry-dev -- carina plan
--refresh=false` against `carina-rs/infra/envs/registry/dev/infra` must
complete within the same bound as the sibling `app-deploy` stack (~30 s
ballpark). If async-lifted exports do not restore that, we revisit
hypotheses; that branch of the decision is captured under the design
doc's Risk and rollback section.
