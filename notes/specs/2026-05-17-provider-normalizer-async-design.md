# ProviderNormalizer async-ification: Design

<!-- derived-from #root-cause -->

## Goal

Make the `ProviderNormalizer` trait **async** (returning `BoxFuture`,
mirroring the already-async `Provider` trait) so the WASM normalizer
host implementation `.await`s the guest call directly, and the
**synchronous-method-with-nested-`block_on`** anti-pattern is removed
*at the type level* for all four normalizer methods at once.

This is the design document only. Implementation (carina core +
`carina-provider-aws` + `carina-provider-awscc`) follows in separate
PRs after this design merges, per the repo's split-PR policy for large
refactors.

## Root cause

<!-- constrained-by ../../CLAUDE.md -->

`carina apply` against `carina-rs/infra registry/dev/registry`
deadlocks. Tracked as carina#3112. The hang is **not** the
`create`/`read` path (carina#3106) and **not** route53 read
(carina-provider-aws#333) — it is `ProviderNormalizer::normalize_desired`
invoked from the apply-path `renormalize` added by carina#3060
(commit `7116989a`).

### Empirical evidence (not inferred)

Instrumented build, real apply, phase logging:

```
Refreshing state...
[DIAG normalize_desired] ENTER (11) ... WASM call RETURNED (ok=true)   # plan: 3x OK
Applying changes...
[DIAG renormalize] id=...c6d54263 -> normalize_desired
[DIAG normalize_desired] ENTER (1) ... WASM call RETURNED (ok=true)   # A record: 3x OK
[DIAG renormalize] id=...c6d54263 DONE
[DIAG renormalize] id=...5bd529ba -> normalize_desired
[DIAG normalize_desired] ENTER (1) ... WASM call RETURNED (ok=true)   # AAAA: 1st OK
[DIAG normalize_desired] ENTER (1)
[DIAG normalize_desired] converted to wit, entering block_on
[DIAG normalize_desired] acquiring store lock...                      # HANGS — no "ACQUIRED"
```

Stack of the hung process (100% of samples):

```
run_apply (#[tokio::main] thread, Runtime::block_on)
 -> execute_basic_effect -> resolve_resource -> renormalize           (carina#3060)
  -> ProviderRouter as ProviderNormalizer::normalize_desired
   -> WasmProviderNormalizer::normalize_desired
    -> tokio::task::block_in_place
     -> Handle::current().block_on(async { self.instance.store.lock().await; ... })
      -> park -> parking_lot::Condvar::wait_until_internal  (forever)
```

### Why it deadlocks

`ProviderNormalizer` is a **synchronous** trait:

```rust
pub trait ProviderNormalizer: Send + Sync {
    fn normalize_desired(&self, _resources: &mut [Resource]) {}
    fn normalize_state(&self, _current_states: &mut HashMap<ResourceId, State>) {}
    fn hydrate_read_state(&self, _current_states: &mut HashMap<ResourceId, State>,
                          _saved_attrs: &SavedAttrs) {}
    fn merge_default_tags(&self, resources: &mut [Resource],
                          default_tags: &IndexMap<String, Value>,
                          registry: &SchemaRegistry);
}
```

`WasmProviderNormalizer` drives the async WASM guest from inside each
sync method with `tokio::task::block_in_place` +
`Handle::current().block_on(async { store.lock().await; ... })`.

Before carina#3060 this ran only during **synchronous plan
preparation** — safe. carina#3060's `renormalize`
(`carina-core/src/executor/basic.rs`) re-applies the plan-time
normalization pipeline on the apply path and calls the sync
`normalize_desired` **from inside the apply async execution loop**
(`execute_basic_effect` is `async fn`, driven by the `#[tokio::main]`
thread's `Runtime::block_on`), and `renormalize` invokes it **multiple
times in sequence** per resource (canonicalize → normalize_desired →
resolve_enum_aliases, and `normalize_desired` itself recurses per
attribute pass). A `tokio::sync::Mutex` `MutexGuard` from one nested
`block_on` is not released before the next nested `block_on` tries to
re-acquire the same store `Mutex` — a self-deadlock, observed
deterministically on the AAAA RecordSet's second `normalize_desired`.

This is the single explanation consistent with **all** the prior
failed fixes: carina#3106 (create/read timeout) is never reached;
carina-provider-aws#333 (route53 read) is downstream of the wedge; and
any timeout future on the wedged runtime is itself never polled — which
is why "just add a timeout" cannot work and was correctly rejected.

## Non-goals

- **Reverting carina#3060.** Its apply-path re-normalization fixed the
  aws#315-class phantom-diff series (#3060/#3063/aws#315/#319). The
  deadlock is in *how* `WasmProviderNormalizer` bridges sync→async, not
  in re-normalizing on apply. The async redesign **preserves** #3060's
  behavior; reverting would reintroduce resolved phantom-diff bugs.
- **Changing the WIT contract.** `provider.wit`'s `normalize-desired`,
  `normalize-state`, `hydrate-read-state`, `merge-default-tags` funcs
  are already host-driven-async (guest runs synchronously; the host
  decides how to drive). Only the **host-side Rust trait** changes.
  No WIT change, no provider plugin rebuild required for the contract.
- **Folding the normalizer into the `Provider` trait.** They are
  distinct responsibilities (one mutates desired/state in place; the
  other performs CRUD). Keep them separate traits; only align the
  async shape.
- **carina#3109 as a separate fix.** carina#3109 (normalizer path
  lacks the carina#3106 timeout/poison guard) is the *same* sync-nested
  -`block_on` class. Async-ifying the trait removes the anti-pattern
  wholesale, so #3109 is **subsumed** by this design, not fixed
  independently. (A bounded timeout may still be layered on the async
  normalizer later as defense-in-depth; out of scope here.)
- **Touching the synchronous parts of plan preparation that do not
  call a WASM normalizer.** Only call paths that reach
  `WasmProviderNormalizer` need to become async.

## Design

### 1. Trait shape (mirror the async `Provider` trait)

`Provider` is already object-safe-async without the `async-trait`
macro, via `BoxFuture`:

```rust
fn read(&self, id: &ResourceId, identifier: Option<&str>,
        request: ReadRequest) -> BoxFuture<'_, ProviderResult<State>>;
```

Apply the identical pattern to `ProviderNormalizer`. The methods mutate
their arguments in place and return nothing, so they borrow `&mut`
across an `.await`; the returned future must carry that borrow's
lifetime:

```rust
pub trait ProviderNormalizer: Send + Sync {
    fn normalize_desired<'a>(&'a self, resources: &'a mut [Resource])
        -> BoxFuture<'a, ()>;
    fn normalize_state<'a>(&'a self, current_states: &'a mut HashMap<ResourceId, State>)
        -> BoxFuture<'a, ()>;
    fn hydrate_read_state<'a>(&'a self, current_states: &'a mut HashMap<ResourceId, State>,
                              saved_attrs: &'a SavedAttrs) -> BoxFuture<'a, ()>;
    fn merge_default_tags<'a>(&'a self, resources: &'a mut [Resource],
                              default_tags: &'a IndexMap<String, Value>,
                              registry: &'a SchemaRegistry) -> BoxFuture<'a, ()>;
}
```

Notes:

- No default no-op bodies any more (a `BoxFuture`-returning method
  can't have an empty `{}` default). Provide a tiny helper
  `fn ready_noop<'a>() -> BoxFuture<'a, ()> { Box::pin(async {}) }` and
  have `NoopNormalizer` and any "I don't normalize" impl return it
  explicitly. The trait already removed implicit `merge_default_tags`
  defaults for exactly this "silent no-op" hazard
  (carina-provider-awscc#192) — extend that discipline to all four.
- `&'a mut [Resource]` held across `.await` is sound: the future is
  driven to completion before the borrow ends (callers `.await` it
  immediately; no concurrent aliasing).

### 2. `WasmProviderNormalizer` — delete the nested `block_on`

Each of the four impls drops `tokio::task::block_in_place(|| Handle::
current().block_on(async { ... }))` and becomes a normal `async` body
wrapped in `Box::pin`:

```rust
fn normalize_desired<'a>(&'a self, resources: &'a mut [Resource])
    -> BoxFuture<'a, ()>
{
    Box::pin(async move {
        let wit_resources = /* convert */;
        let mut store = self.instance.store.lock().await;   // plain .await
        store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);
        match self.instance.bindings
            .call_normalize_desired(&mut store, &wit_resources).await
        { Ok(r) => /* write back */, Err(e) => log::error!(...) }
    })
}
```

The self-deadlock is **structurally impossible** afterward: there is no
nested runtime; the single store `Mutex` is acquired and released
within one polled future, the same way the async `Provider` methods
already do safely. (Whether to also bound it with `tokio::time::timeout`
+ the carina#3106 poison guard is a follow-up — the deadlock is gone
without it.)

### 3. `ProviderRouter` — sequence the per-normalizer futures

`ProviderRouter` fans out to `self.normalizers`. Async form awaits each
in order (order preserved — normalizers are not commutative):

```rust
fn normalize_desired<'a>(&'a self, resources: &'a mut [Resource])
    -> BoxFuture<'a, ()>
{
    Box::pin(async move {
        for ext in &self.normalizers {
            ext.normalize_desired(resources).await;
        }
    })
}
```

`&mut resources` is re-borrowed per iteration across the `.await` —
sound because the futures run sequentially, never concurrently.

### 4. Propagate `.await` to callers (~19 carina call sites)

The call graph that must thread `.await`:

- `carina-core/src/executor/basic.rs`: `renormalize` →
  `resolve_resource` / `resolve_resource_with_source` →
  `execute_basic_effect`. `execute_basic_effect` is **already
  `async fn`**, so the change is local: `renormalize` and the two
  `resolve_resource*` become `async fn` and their three call sites
  inside `execute_basic_effect` / replace path gain `.await`.
- Plan-build path (`carina-cli/src/wiring`, `commands/apply/mod.rs`,
  `commands/state.rs`, `fixture_plan.rs`): these run inside the
  top-level `#[tokio::main]` async context, so adding `.await` is
  mechanical, not a runtime-context change.
- Test normalizers (`provider.rs` `SchemaOnlyProvider`/`TestNormalizer`,
  `executor/tests.rs` `CanonicalizingNormalizer`,
  `wiring/tests.rs`): return `Box::pin(async { ... })`.

`resolve_resource` / `resolve_resource_with_source` currently being
**synchronous** is the only non-mechanical part. All 8 call sites were
audited (`carina-core/src/executor/{basic,replace,phased}.rs`); every
one is inside an `async fn` already: `execute_basic_effect`,
`execute_cbd_replace_parallel`, `execute_dbd_replace_parallel`,
`execute_effects_phased`. There is **no** synchronous leaf caller, so
the change is `.await`-threading through already-async frames, not a
sync→async inversion anywhere.

### 5. Provider repos (separate follow-up PRs)

`carina-provider-aws` (`AwsNormalizer`) and `carina-provider-awscc`
implement `ProviderNormalizer`. Their bodies are pure/sync
(enum resolution, dns-name strip, tag merge — no I/O), so each method
becomes `fn name<'a>(...) -> BoxFuture<'a, ()> { Box::pin(async move {
/* existing sync body */ }) }`. Mechanical, no logic change. These
land **after** the core design+impl, in per-repo PRs, with a
`carina-core` pin bump (the established cross-repo order:
carina → aws → awscc).

## Blast radius (measured)

| Surface | Count | Nature |
| --- | --- | --- |
| `ProviderNormalizer` impls (carina-core) | `NoopNormalizer`, `ProviderRouter`, `WasmProviderNormalizer` + 3 test impls | sig change; only `WasmProviderNormalizer` is non-mechanical (delete `block_on`) |
| Call sites (carina) | ~19 across `provider.rs`, `executor/basic.rs`, `cli/wiring`, `commands/apply`, `commands/state`, `fixture_plan` | add `.await`; all callers already in async context |
| Provider repos | `carina-provider-aws`, `carina-provider-awscc` (1 impl each) | wrap sync body in `Box::pin(async move {})`; separate PRs |
| WIT | 0 | contract unchanged |

## Migration / PR sequence

1. **This PR** — design doc only (`refs #3112`). No code.
2. **carina impl PR** — trait → async, all carina impls + call sites,
   tests. `Closes #3112`. Pre-PR: real-infra smoke
   (`aws-vault exec carina-registry-dev -- carina apply
   registry/dev/registry`) must reach a normal plan/apply without the
   deadlock — the original acceptance condition.
3. **carina-provider-aws PR** — `AwsNormalizer` async + `carina-core`
   pin bump.
4. **carina-provider-awscc PR** — same for awscc.

Each provider repo's main must stay green after its pin bump; the
meta-tracker (carina#3112) is closed only when all three repos' mains
build against the new trait.

## Alternatives considered (and rejected)

- **Revert carina#3060.** Stops the deadlock immediately but
  reintroduces the aws#315-class phantom-diff series #3060 fixed.
  Trades one regression for several; rejected (Non-goals).
- **Per-site minimal fix in `renormalize`** (e.g. ensure the
  `MutexGuard` drops between calls). Symptom-targeted: the
  sync-nested-`block_on` anti-pattern survives in `normalize_state` /
  `hydrate_read_state` / `merge_default_tags` (same class, carina#3109)
  and re-breaks from any future async caller. The trait contract stays
  "works only if called from a sync context", which is unenforceable
  and exactly what produced this regression. Rejected: not root-cause.
- **`spawn_blocking` the whole normalizer on a dedicated thread.**
  Removes the *nested* runtime but keeps a sync trait that internally
  blocks; the WASM store `Mutex` is not `Send`-friendly across the
  spawn boundary in the way the async pattern is, and it leaves the
  trait shape lying about its execution model. The async trait is the
  shape the rest of the provider boundary (`Provider`) already uses —
  consistency and type-enforced correctness win long-term.
- **`async-trait` macro.** The codebase deliberately uses hand-written
  `BoxFuture` returns for `Provider` (object safety, no proc-macro in
  the hot trait). Match that; do not introduce `async-trait` for one
  trait.
