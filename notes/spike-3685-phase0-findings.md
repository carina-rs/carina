# carina#3685 Phase 0 findings: WASI 0.3 provider boundary

Date: 2026-08-02

Branch/worktree: issue-3685-wasip3-spike

Status: spike only; no commits or pushes

## Executive result

The guest/artifact feasibility gate is positive, with an important qualification:

- The mock provider builds as a component that imports the final WASI 0.3 clocks and reduced HTTP client interfaces.
- Mixed sync/async WIT is accepted. Exactly initialize, read, read-data-source, create, update, and delete are canonically async exports; metadata, validation, and normalizer exports remain synchronous.
- The result was verified from the built binary with wasm-tools component wit, not inferred from source or tests.
- The working build is the permitted fallback: wasm32-wasip2 for Rust std plus the wasip3 0.7.0+wasi-0.3.0 support crate for p3 clocks/HTTP and canonical async exports. It is therefore a hybrid component, not a native wasm32-wasip3 build: stable Rust std still contributes WASI 0.2 CLI/I/O/random imports.

The overall Phase 0 feasibility gate is **GO, with a deliberately narrow
claim**:

- carina-plugin-host builds against exact wasmtime, wasmtime-wasi, and
  wasmtime-wasi-http 47.0.3.
- The existing integration harness runs initialization and the full mock
  create/read/update/delete path successfully on that stack: 9/9 ordinary
  integration tests passed.
- All five restored HTTP initialization stress shapes passed in 20 measured
  full-suite iterations: 100/100 stress test executions, 249.631 seconds total,
  12.425–12.555 seconds per suite, and no timeout or hang signature.
- Saved stress logs contain no NeedWork, StartImplicit, livelock, panic, or
  failure marker. No scheduler trace rerun was triggered because nothing was
  suspicious.
- Final crate-scoped nextest verification passed 169/169 tests across the host,
  SDK, and mock.

This is sufficient evidence that the mock provider boundary is feasible on the
final WASI 0.3 / Wasmtime 47 stack. It is **not** evidence that the June
real-infrastructure livelock is fixed: no p3 AWS/AWSCC artifact exists for that
stronger test, and the handoff explicitly established that mock green alone is
insufficient for production safety.

## Historical evidence carried forward

I read these before making changes:

- notes/specs/2026-06-06-3400-handoff.md
- notes/specs/2026-06-06-3400-wit-async-export-design.md

The June Stage A work had green mock coverage but did not eliminate the real-infrastructure NeedWork/StartImplicit scheduler livelock. Its decisive evidence gap was failure to inspect the built artifact. This report treats artifact inspection as the guest gate and does not treat mock results as a substitute for real-infrastructure evidence.

## Exact toolchain and dependency combination

Installed tools:

    rustc 1.95.0 (59807616e 2026-04-14)
    cargo 1.95.0 (f2d3ce0bd 2026-03-21)
    rustc 1.99.0-nightly (73dc9167f 2026-08-01)
    host: aarch64-apple-darwin
    LLVM version: 22.1.8
    wasm-tools 1.246.1

The nightly target list contains:

    wasm32-wasip3

Installed nightly components:

    cargo-aarch64-apple-darwin
    rust-src
    rust-std-aarch64-apple-darwin
    rustc-aarch64-apple-darwin

As expected for the Tier 3 target, there is no installed prebuilt wasm32-wasip3 sysroot.

### Native wasm32-wasip3 attempt

I first tested the requested native shape with a minimal mixed-interface component:

    cargo +nightly build \
      -Zbuild-std=std,panic_abort \
      --target wasm32-wasip3 \
      --offline

The build reached component linking, and the generated linker command included the expected canonical-ABI marker:

    "--export" "[async-lift]spike:test/api#operation"

It then failed:

    error: linking with wasm-component-ld failed
    ...
    "--cooperative-threading"
    ...
    "-l" "c"
    ...
    rust-lld: error: unknown argument: --cooperative-threading
    rust-lld: error: unable to find library -lc
    error: failed to invoke LLD

The nightly target spec requires --cooperative-threading, while the available linker/sysroot combination cannot satisfy it. The nightly sysroot has wasm-component-ld 0.5.27 but no rustlib/wasm32-wasip3 target sysroot. This is the precise native-target blocker on this machine.

### Working fallback combination

The working guest combination is:

- Host Cargo/rustc: stable 1.95.0.
- Compilation target: wasm32-wasip2.
- Rust standard library ABI: WASI 0.2, from the installed stable target.
- Carina WIT generator: wit-bindgen 0.54.0 with features macros and async.
- WASI 0.3 support: wasip3 0.7.0+wasi-0.3.0 with std, bitflags, and, in the SDK, http-compat.
- wasip3 runtime bindings: its wit-bindgen 0.57.1 dependency.
- Offline source fallback: wasip3 0.7.0 and wit-bindgen 0.57.1 copied from the nightly rust-src vendor tree through command-line Cargo patches.
- Component WIT: final wasi:http@0.3.0 client/types and wasi:clocks@0.3.0.

Exact successful build:

    env RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo \
      --config 'patch.crates-io.wasip3.path="/Users/mizzy/.rustup/toolchains/nightly-aarch64-apple-darwin/lib/rustlib/src/rust/library/vendor/wasip3-0.7.0+wasi-0.3.0"' \
      --config 'patch.crates-io.wit-bindgen.path="/Users/mizzy/.rustup/toolchains/nightly-aarch64-apple-darwin/lib/rustlib/src/rust/library/vendor/wit-bindgen-0.57.1"' \
      build -p carina-provider-mock \
      --target wasm32-wasip2 \
      --offline

Result:

    Finished dev profile [unoptimized + debuginfo] target(s) in 9.91s

Artifact:

    target/wasm32-wasip2/debug/carina-provider-mock.wasm

I also tried making wit-bindgen 0.57.1 the direct Carina generator. That route needed the uncached wasm-encoder 0.247.0 graph and could not proceed without network access. Keeping the already cached 0.54.0 direct generator while allowing wasip3 to use its patched 0.57.1 runtime support worked and emitted the correct async canonical ABI.

Cargo.lock is left normalized to registry sources and exact checksums. The command-line patches above are required only on this offline machine.

### Cargo.toml changes

carina-plugin-sdk:

- Added futures-executor 0.3 for the native JSON-RPC compatibility dispatcher.
- Enabled wit-bindgen 0.54 features macros,async.
- Added wasip3 0.7.0 with default features disabled and std,bitflags,http-compat.
- Added wasm32 futures-util alloc, bytes, and http-body-util dependencies.

carina-provider-mock:

- Bumped its direct wasm32 wit-bindgen from 0.51 to 0.54 with macros,async.
- Added wasip3 0.7.0 with std,bitflags and http 1.

carina-plugin-host:

- Pinned wasmtime, wasmtime-wasi, and wasmtime-wasi-http to exactly 47.0.3.
- Enabled the p3 features on wasmtime-wasi and wasmtime-wasi-http.
- Added rustls 0.23 with aws_lc_rs for Wasmtime 47's default p3 HTTP transport.
- Added Hyper/Tokio test features needed by the resurrected local HTTP stress server.

## WIT and guest implementation

The WIT submodule working tree was edited in place and was not pushed.

- Replaced the vendored p2 CLI/clocks/filesystem/HTTP/random/sockets WIT with the final p3 definitions from nightly rust-src.
- Removed the p2 wasi:io dependency; WASI 0.3 has no wasi:io package.
- Changed the HTTP world import from wasi:http/outgoing-handler@0.2.6 to wasi:http/client@0.3.0.
- Marked exactly the six requested provider operations async.
- wasm-tools component wit carina-plugin-wit/wit accepts the mixed interface. Whole-interface async escalation was not needed.

The SDK uses a spike-only parallel p3 HTTP adapter rather than claiming a full SDK/AWS Smithy transport port:

- carina-plugin-sdk/src/wasi_http_p3.rs awaits wasip3::http::client::send and collects the response.
- The existing p2 adapter remains in the tree for comparison.
- The provider trait returns boxed futures for the six I/O methods.
- The guest macro implements the six generated async exports and uses an async mutex for provider state.
- Synchronous metadata/normalizer exports use a fail-fast try_lock; they do not block an executor thread behind an in-flight async operation.

The mock initialize method recognizes the reverted Stage A stress attributes and issues genuinely async p3 HTTP client calls. The old shape names are retained for comparison, but p3 has no pollable.block path.

## Mandatory built-artifact verification

Command:

    wasm-tools component wit \
      target/wasm32-wasip2/debug/carina-provider-mock.wasm \
      > /private/tmp/carina-3685-final-component.wit

Restricting the check to the exported provider interface finds exactly six
occurrences of async func and exactly the six requested names. The full world
also has the expected async p3 imports wait-for and send, for eight async
declarations total; those are imports, not provider exports.

Artifact SHA-256:

    efb145d4a3350ac43dd3cb605586dd2890b68f42370189e71168ba46c3af6ab8

The artifact imports show its hybrid nature:

    import wasi:clocks/types@0.3.0;
    import wasi:http/types@0.3.0;
    import wasi:http/client@0.3.0;
    import wasi:clocks/monotonic-clock@0.3.0;
    import wasi:cli/environment@0.2.9;
    import wasi:cli/exit@0.2.9;
    import wasi:io/error@0.2.9;
    import wasi:io/poll@0.2.9;
    import wasi:io/streams@0.2.9;
    import wasi:cli/stdin@0.2.9;
    import wasi:cli/stdout@0.2.9;
    import wasi:cli/stderr@0.2.9;
    import wasi:cli/terminal-input@0.2.9;
    import wasi:cli/terminal-output@0.2.9;
    import wasi:cli/terminal-stdin@0.2.9;
    import wasi:cli/terminal-stdout@0.2.9;
    import wasi:cli/terminal-stderr@0.2.9;
    import wasi:random/insecure-seed@0.2.9;
    export wasi:cli/run@0.2.0;
    export carina:provider/provider@0.1.0;

The p2 CLI/I/O imports above come from the wasm32-wasip2 standard library. The provider HTTP path and canonical async exports are p3.

### Verbatim provider section from wasm-tools component wit

    interface provider {
      use types.{resource-id, state, create-outcome, update-outcome, resource-def, value, create-request, read-request, update-request, delete-request, provider-error, type-identity, binding-pattern};

      enum plan-op {
        create,
        read,
        update,
        delete,
      }

      info: func() -> string;

      schemas: func() -> string;

      provider-config-attribute-types: func() -> string;

      validate-config: func(attrs: list<tuple<string, value>>) -> result<_, provider-error>;

      initialize: async func(attrs: list<tuple<string, value>>) -> result<_, provider-error>;

      read: async func(id: resource-id, identifier: option<string>, request: read-request) -> result<state, provider-error>;

      read-data-source: async func(res: resource-def) -> result<state, provider-error>;

      create: async func(id: resource-id, request: create-request) -> result<create-outcome, provider-error>;

      update: async func(id: resource-id, identifier: string, request: update-request) -> result<update-outcome, provider-error>;

      delete: async func(id: resource-id, identifier: string, request: delete-request) -> result<_, provider-error>;

      required-permissions: func(id: resource-id, operation: plan-op) -> list<string>;

      satisfier-hint: func(target-id: resource-id, attr-path: list<string>) -> list<binding-pattern>;

      provider-config-completions: func() -> string;

      identity-attributes: func() -> list<string>;

      validate-custom-type: func(ty: type-identity, value: string) -> result<_, provider-error>;

      get-enum-aliases: func() -> string;

      normalize-desired: func(resources: list<resource-def>) -> list<resource-def>;

      normalize-state: func(states: list<tuple<string, state>>) -> list<tuple<string, state>>;

      hydrate-read-state: func(states: list<tuple<string, state>>, saved-attrs: list<tuple<string, list<tuple<string, value>>>>) -> list<tuple<string, state>>;

      merge-default-tags: func(resources: list<resource-def>, default-tags: list<tuple<string, value>>) -> list<resource-def>;
    }

This is the hard evidence missing from Stage A: the six exports are async-lifted in the built component.

## Host port

The host source now expresses the Wasmtime 47 p3 design:

- wasm_component_model_async(true) is enabled.
- The HTTP bindgen maps wasi:http to wasmtime_wasi_http::p3::bindings::http and no longer maps p3 HTTP through wasi:io.
- The six async provider calls run through Store::run_concurrent and an Accessor, with owned arguments.
- The hybrid guest linker keeps p2 CLI/std support, then adds p3 clocks and p3 HTTP.
- HostState implements the p3 HTTP view.
- Engine creation installs the rustls 0.23 AWS-LC provider used by the default p3 transport.
- The old p2 HTTP view and policy implementation remain in the tree as the
  unported baseline. The hybrid component separately needs the p2 WASI
  CLI/I/O/random linker for its standard-library imports, not p2 HTTP.

### Exact Wasmtime 47 build

The manifest and Cargo.lock contain exact 47.0.3 pins and no temporary 45 pin.
The first real 47 check compiled the entire dependency graph and found one
source incompatibility left by the earlier cached-45 probe:

    error[E0433]: cannot find LinkOptions in exit
    error[E0061]: this function takes 2 arguments but 3 arguments were supplied

Wasmtime 47's p2 CLI exit linker no longer takes interface LinkOptions. Removing
that obsolete argument was the only exact-47 compile correction:

    cli::exit::add_to_linker::<T, WasiCli>(linker, T::cli)?;

The final command and result were:

    env RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= \
      cargo check -p carina-plugin-host --offline --locked

    Checking carina-plugin-host v0.4.0 (...)
    Finished dev profile [unoptimized + debuginfo] target(s) in 0.90s

### Exact Wasmtime 47 end-to-end result

Command:

    env RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= \
      cargo test -p carina-plugin-host \
      --test wasm_integration_test \
      test_wasm_mock_provider \
      --offline --locked -- --nocapture

Actual test output:

    running 9 tests
    test test_wasm_mock_provider_merge_default_tags_empty_short_circuits ... ok
    test test_wasm_mock_provider_required_permissions_dispatches_through_wit ... ok
    test test_wasm_mock_provider_update_and_delete ... ok
    test test_wasm_mock_provider_factory ... ok
    test test_wasm_mock_provider_read_data_source_dispatches_override ... ok
    test test_wasm_mock_provider_merge_default_tags_preserves_order ... ok
    test test_wasm_mock_provider_create_and_read ... ok
    test test_wasm_mock_provider_normalizer ... ok
    test test_wasm_mock_provider_merge_default_tags_dispatches_through_wit ... ok

    test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured;
    5 filtered out; finished in 7.17s

This instantiates the verified hybrid component through the real p2-std plus
p3-clocks/HTTP linkers and exercises initialize, create, read, update, and
delete. There was no guest, linker, or scheduler failure.

A later broad nextest pass initially exposed nine stale p2 diagnostic
expectations, not runtime failures: eight unit fixtures and one
wasm_error_reporting fixture still expected host version 0.2.6 or treated
0.2.x as the compatible track. The fixtures now use p3 client/types names and
0.3.x compatibility, and the production diagnostic renders its compatibility
track from the host version rather than hard-coding 0.2.x.

## Stress work and results

The five multi-instance initialize stress cases from 443e5260 were restored and adapted:

- sequential
- poll-batch
- tokio-join
- spawn-await
- sleep-interleave

Each creates five provider instances and exercises bounded p3 HTTP initialization against a delayed local HTTP server. The mock retains the old shape labels, while their implementation uses native async send futures because p3 has no pollable.block.

The loopback server worked. A smoke run produced:

    running 5 tests
    test multi_instance_wasi_http_initialize_concurrent_completes_bounded ... ok
    test multi_instance_wasi_http_initialize_tokio_join_shape_completes_bounded ... ok
    test multi_instance_wasi_http_initialize_spawn_await_shape_completes_bounded ... ok
    test multi_instance_wasi_http_initialize_sleep_interleave_shape_completes_bounded ... ok
    test multi_instance_wasi_http_initialize_sequential_completes_bounded ... ok

    test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured;
    9 filtered out; finished in 12.25s

I then ran 20 additional full-suite iterations. Each cargo invocation had an
external 180-second alarm and wrote a separate log. These are the outer
wall-clock measurements, including Cargo/test-process overhead:

| Iteration | Seconds | Iteration | Seconds |
| ---: | ---: | ---: | ---: |
| 1 | 12.458 | 11 | 12.545 |
| 2 | 12.444 | 12 | 12.532 |
| 3 | 12.486 | 13 | 12.463 |
| 4 | 12.507 | 14 | 12.429 |
| 5 | 12.425 | 15 | 12.538 |
| 6 | 12.488 | 16 | 12.436 |
| 7 | 12.495 | 17 | 12.441 |
| 8 | 12.448 | 18 | 12.461 |
| 9 | 12.503 | 19 | 12.555 |
| 10 | 12.529 | 20 | 12.448 |

Final measured stress accounting:

| Evidence | Result |
| --- | --- |
| Required repeated suites | at least 20 |
| Measured full suites | 20/20 passed |
| Stress test executions | 100/100 passed |
| Additional smoke suite | 5/5 passed in 12.25s |
| Total measured loop time | 249.631s |
| Mean suite wall time | 12.4816s |
| Minimum / maximum | 12.425s / 12.555s |
| Timing spread | 0.130s |
| External 180s alarms | 0 |
| NeedWork/StartImplicit markers | 0 |
| Livelock, panic, or failure markers | 0 |

There was no suspicious timing outlier or failure, so the requested conditional
RUST_LOG=wasmtime::runtime::component::concurrent=trace rerun was not triggered.
The 0.130-second spread is stable rather than hang-like.

This is strong evidence for the mock boundary and directly improves on Stage A
because the exercised artifact is proven async-lifted. It remains mock evidence:
it does not establish that the AWS real-infrastructure workload which previously
livelocked is safe.

## WASI 0.3 HTTP hooks assessment (Risk 4)

Wasmtime's p3 host surface still has a viable per-request seam.

The 47 p3 add_to_linker API requires a WasiHttpCtxView whose hooks member is a WasiHttpHooks implementation:

- https://docs.wasmtime.dev/api/wasmtime_wasi_http/p3/fn.add_to_linker.html
- https://docs.wasmtime.dev/api/wasmtime_wasi_http/p3/trait.WasiHttpHooks.html
- https://docs.wasmtime.dev/api/wasmtime_wasi_http/p3/bindings/http/client/trait.HostWithStore.html

The hook receives the HTTP request plus optional RequestOptions and controls the future that produces the response. That supports the existing Carina policies:

- **Allow-list:** inspect URI authority before delegating; reject disallowed hosts with an ErrorCode.
- **Tracing:** open the tracing span before delegation and instrument/wrap both request dispatch and response futures.
- **Per-request timeout:** clamp RequestOptions connect/first-byte/between-bytes limits and/or wrap the delegated transport future in a Tokio timeout.

The p3 equivalent should be an AllowListHttpHooksP3 implementation that wraps the default p3 send path. The spike code deliberately installs Default hooks so the scheduler path can be isolated; the allow-list/tracing/timeout policy itself has not been ported or runtime-tested.

The HTTP hook is not a replacement for the host's existing 20-minute
whole-provider-call deadline and instance poisoning. It only governs HTTP
work. A guest can wait forever elsewhere, and a component scheduler livelock
can prevent the HTTP future from making progress. Retain the hard call
deadline/poisoning policy until end-to-end evidence justifies changing it.

Wasmtime documents the p3 module as experimental, unstable/incomplete, and outside normal semver guarantees:

- https://docs.rs/wasmtime-wasi-http/latest/wasmtime_wasi_http/p3/index.html

Risk 4 is therefore **technically feasible, with the default exact-47 transport
runtime-proven, but not retired**: the seam exists and carried the stress
traffic, while the Carina allow-list/tracing/timeout hook port itself still
needs implementation and tests.

## Verification ledger

Successful:

| Command | Result |
| --- | --- |
| wasm-tools component wit carina-plugin-wit/wit | WIT package validates |
| patched cargo build -p carina-provider-mock --target wasm32-wasip2 --offline | passed, 9.91s |
| patched cargo check -p carina-plugin-sdk --offline | passed, 2.28s |
| patched cargo test -p carina-plugin-sdk --offline | passed, 18/18, 1.68s |
| patched cargo check -p carina-provider-mock --offline | passed, 6.65s |
| wasm-tools component wit built artifact | exactly six async provider exports verified |
| cargo check -p carina-plugin-host --offline --locked | exact 47.0.3 passed, 0.90s after dependency build |
| ordinary wasm_integration_test filter | 9/9 passed, 7.17s |
| stress smoke suite | 5/5 passed, 12.25s |
| 20 measured stress suites | 100/100 passed, 249.631s total |
| cargo test -p carina-plugin-host --lib wasm_factory::tests | 48/48 passed, 3.05s |
| cargo test -p carina-plugin-host --test wasm_error_reporting_test | 2/2 passed, 1.04s |
| cargo nextest run for host, SDK, and mock | 169/169 passed, 17.769s |
| cargo fmt --all -- --check | passed |
| git diff --check | passed |

Diagnostic failures encountered:

| Command/goal | Result |
| --- | --- |
| native nightly wasm32-wasip3 build-std | linker lacks cooperative-threading support and libc/sysroot |
| first exact-47 host check | obsolete p2 cli::exit LinkOptions argument; removed, final check passed |
| first broad nextest run | eight stale p2 diagnostic fixtures; updated to p3 |
| second broad nextest run | one remaining stale external diagnostic fixture; updated |
| final broad nextest run | 169/169 passed |

The selected host/SDK/mock crates are green. A workspace-wide run was not
needed for this spike and the p2 provider source path is intentionally not
claimed source-compatible after replacing the single WIT world and changing
the SDK trait's six I/O methods to futures.

## Compatibility posture

The current spike hard-replaces the WIT dependency set and changes the canonical types of six exports. An existing p2 provider component cannot instantiate as the typed p3 world, and existing provider source does not implement the new future-returning trait.

Recommended Phase 1 posture: **transitional dual-world host**.

- Keep the current p2 binding/linker and its policy hooks for existing aws/awscc/provider artifacts.
- Add a separately versioned p3 WIT world and generated host binding rather than silently changing the meaning of the existing provider package.
- Detect/select the world by artifact metadata or explicit provider manifest/protocol version.
- Move providers one at a time, retaining rollback and side-by-side comparison.
- Remove p2 only after aws, awscc, and other supported providers have p3 artifacts and real-infrastructure plan/apply evidence.

A hard-break p3-only release is reasonable only with a coordinated provider rebuild and protocol rollout. Doing it before that would turn this feasibility experiment into an ecosystem-wide flag day and eliminate the known-working fallback while p3 is still described as experimental.

## Go/no-go input and next evidence required

Decision for carina#3685 Phase 0 today:

- **GO for technical feasibility and a transitional Phase 1 prototype.** The
  component is artifact-proven async, exact Wasmtime 47 compiles, CRUD works,
  and the required repeated mock stress gate is green with stable timings.
- **NO-GO for a p3-only production hard break or removal of existing timeout /
  poisoning guardrails.** The workload that exposed the historical livelock
  was real AWS infrastructure, not this mock.

Evidence still required before a production decision:

1. Implement and test the p3 allow-list/tracing/request-timeout hooks rather
   than Default, then repeat the stress loop through those hooks.
2. Add the transitional dual-world selection mechanism and prove existing p2
   providers remain operational.
3. Once p3 AWS/AWSCC artifacts exist, perform the real-infrastructure plan that
   Stage A lacked. Treat it as a separate and stronger gate than mock stress.
4. Retain the concurrent-scheduler trace procedure for any real-provider stall;
   specifically look for NeedWork/StartImplicit cycling.
5. Revisit native wasm32-wasip3 once its Tier 3 linker/sysroot path supports
   cooperative threading; the hybrid support-crate route is enough for this
   feasibility result but is not the clean final toolchain.

The central positive finding is now end-to-end: the built component contains
the six native async canonical exports and progresses through exact Wasmtime 47
under 20 repeated multi-instance HTTP stress suites. The central unresolved
production question is whether the same scheduler progresses under Carina's
real AWS provider workload without the historical livelock.
