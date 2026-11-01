---
title: "Writing a Provider"
description: "What a Carina provider is: a WebAssembly component that receives typed create, read, update and delete requests from the host over a versioned WIT interface."
---

A provider teaches Carina how to manage one kind of infrastructure. The official providers, `carina-rs/aws` and `carina-rs/awscc`, cover AWS. Anyone can write another one with the same SDK they use.

This section covers writing a provider, building it, and using it from a local file or a GitHub repository. Publishing to the provider registry is not open to third parties yet; see [Publishing](/providers/writing/publishing/).

## What a provider is

A provider is a [WebAssembly component](https://component-model.bytecodealliance.org/) built for the `wasm32-wasip2` target. Carina (the host) loads it into a sandbox and calls it through the interface defined in [carina-plugin-wit](https://github.com/carina-rs/carina-plugin-wit). It exports one of three WIT worlds:

| World | Use it when |
|-------|-------------|
| `carina-provider` | The provider needs no network access. |
| `carina-provider-with-http` | The provider calls an HTTP API through `wasi:http` and declares the hosts it may reach. |
| `carina-provider-aws` | An AWS-kind provider needs `wasi:http` plus host-resolved AWS base credentials. |

The `carina-plugin-sdk` crate defines a `CarinaProvider` trait and supplies bindings generated in Carina-owned crates from the SDK revision's WIT and binding generator. Its `export_provider!` macro connects those bindings to your implementation. Your provider needs no WIT submodule, `wit-bindgen` dependency, or direct dependency on the binding crates. The SDK re-exports the request and schema types from `carina-provider-protocol`.

`export_provider!(MyProvider)` exports `carina-provider`. To export `carina-provider-with-http`, declare every hostname the provider may contact:

```rust
carina_plugin_sdk::export_provider!(
    MyProvider,
    http,
    egress: ["api.example.com", "*.service.example.com"],
);
```

The SDK supplies the WASI imports. Its `carina_plugin_sdk::wasi_http::send_request` sends an `http::Request` through the host. Add a direct `wasi` dependency only if your own code calls WASI APIs.

The HTTP form embeds a versioned `carina:egress` custom section in the component. The declaration is mandatory. The host reads it from the exact bytes being loaded and permits only consented patterns:

- `api.example.com` matches exactly that host.
- `*.example.com` matches one label, such as `api.example.com`, but not `v1.api.example.com`.
- `**.example.com` matches one or more labels at any depth below `example.com`. It does not match `example.com` itself.

Wildcards are allowed only as the complete leftmost label and cannot sit directly above a public suffix. IP literals, localhost, metadata and link-local names, non-ASCII labels, empty labels, and labels longer than 63 bytes are rejected. DNS resolution is checked separately: loopback, unspecified, link-local, and cloud metadata addresses are always denied. Private IPv4, IPv6, and CGNAT addresses are denied except for AWS-kind providers, which need them for VPC interface endpoints and PrivateLink.

An AWS provider uses the same egress declaration but selects the AWS world:

```rust
carina_plugin_sdk::export_provider!(
    MyAwsProvider,
    aws,
    egress: ["**.amazonaws.com", "**.amazonaws.com.cn"],
);
```

Call `carina_plugin_sdk::aws_credentials::current()` to obtain the current base credentials. The SDK-side type exposes accessors but redacts secret fields from `Debug`. Native builds return a typed error so provider unit tests still compile. At runtime, only a component loaded with the AWS trust kind can link this import. Schema-only loading links the import without resolving credentials and returns `ProviderFailed` if the provider calls it while schemas are loading. See [AWS Credentials](/guides/aws-credentials/) for the user-facing chain and refresh behavior.

In native provider tests, validate declaration patterns with `carina_plugin_sdk::protocol::HostPattern::parse`.

`carina init` copies the normalized declaration into `carina-providers.lock`. A declaration from a verified, allow-listed `carina-rs` provider is recorded without a prompt. Other providers require explicit consent; see [`carina init`](/reference/cli/init/#provider-egress-consent). Runtime operations fail closed when the record is absent or stale, while schema loading for `validate`, `fmt`, and the LSP never receives network access.

### Non-Rust components

`wasm-tools` 1.246 does not have a command that adds an arbitrary custom section. Add the annotation to the language toolchain's core module in text form before wrapping it as a component:

```bash
# core.wasm: the language toolchain's core module output
wasm-tools print core.wasm > core.wat
# Insert this annotation before the module's final ")":
#   (@custom "carina:egress" "{\"version\":1,\"hosts\":[\"api.example.com\"]}")
wasm-tools parse core.wat -o core-with-egress.wasm
wasm-tools component embed carina-plugin-wit/wit --world carina-provider-with-http \
  core-with-egress.wasm -o embedded.wasm
wasm-tools component new embedded.wasm -o provider.wasm
```

For a toolchain that already emits a component, add the same annotation to its core module before `component new`. Do not run `wasm-tools strip` on the finished provider unless the command is configured to preserve `carina:egress`; both the default strip operation and `strip --all` remove the declaration, and an HTTP provider without it is rejected at load time.

## How the host calls a provider

When a command such as `carina plan` loads a provider, the host:

1. Calls `info` to get the provider's name and the protocol version it was built against, and rejects it if that version is not supported (see [Protocol version](#protocol-version)).
2. Calls `schemas` to get every resource type the provider manages. Schemas cross the boundary as JSON, and the host validates them when it decodes them, so a malformed schema fails at load, not halfway through an apply.
3. Validates the `provider` block's attributes against the types from `provider_config_attribute_types`, then calls `validate_config` for checks the types cannot express, and `initialize` once for each `provider` block, with that block's configuration.
4. Calls `read`, `create`, `update` and `delete` as the plan requires.

## The provider executes; Carina decides

Carina computes the plan itself: it compares the configuration with state and with what `read` returns, and turns the difference into effects. A provider never decides what should change. It receives one typed request per operation and carries it out:

| Operation | The provider receives |
|-----------|----------------------|
| `read` | The resource's identifier. It returns `NotFound`, or `Found` with the current attributes. |
| `create` | The desired resource with resolved attribute values. |
| `update` | The identifier, the last persisted state (`from`), and a patch. |
| `delete` | The identifier and Carina's directives for the resource. |

The identifier is the provider's own name for the resource, such as a cloud ID. A provider returns it from `create` and receives it on every later call. It is never empty: the type that carries it, `AppliedIdentifier`, cannot be built from an empty string.

An update never receives the full desired resource. It receives a patch, a list of operations on top-level attributes:

| Patch op | Meaning |
|----------|---------|
| `Add` | The attribute was not set and now has a value. |
| `Replace` | The attribute's value changed. |
| `Remove` | The attribute was removed from the configuration. |
| `Reassert` | A write-only attribute that has not changed, sent again because the API cannot read it back and the update would otherwise drop it. |

The patch is the whole update. A provider must not change any attribute the patch does not mention.

A write-only attribute (one the API accepts but never returns, such as a password) still gets `Add`, `Replace` and `Remove` when its value changes: Carina records which write-only values it last sent and compares the configuration with that record. `Reassert` is different. It carries a write-only attribute whose value has not changed, and Carina adds it only to an update that something else already caused.

## Short calls; the host waits

Every call must return quickly. The host stops a call whose WebAssembly code has computed for more than 30 seconds in total (time spent waiting in host I/O, such as an HTTP request or a clock sleep, does not count), stops any call that is still running 60 minutes after it began, and limits each HTTP request's connect, first-byte and between-bytes waits to 30 seconds each. A stopped call also breaks that provider instance, so its later calls in the same run fail. A provider never sleeps or polls inside a call. When a create or update has not finished, the provider returns a pending result that carries a token it defines, an `operation` or `read-back` phase, and a hint for when to ask again. Delete pending is phase-less because delete has no post-apply read-back. The host then calls `poll_create`, `poll_update` or `poll_delete` with the original request and that token, until the operation finishes or the resource's time budget (15 minutes by default) runs out.

Post-create and post-update reads have a separate host-owned budget. It starts at the first `read-back` pending result, defaults to two minutes, and is clipped to the operation's original deadline, so it never extends the operation. A resource schema can set a non-zero `operation_config.read_back.timeout_secs`. The host marks the one poll at the active deadline as `final-attempt` in `poll_create`, `resume_create`, and `poll_update`. The phase of the continuation sent that request controls the result: pending after a final attempt sent to an `operation` continuation is over budget, even if the response moves to `read-back`; pending in either phase after a final attempt sent to a `read-back` continuation is a protocol violation. Returning from `read-back` to `operation` at any time is also a violation. Do not put attempt counters in a read-back token—the host owns the deadline and finality.

A pending create is also saved in state. If Carina is interrupted, the next apply calls `resume_create` with the saved token instead of creating the resource again. The token is stored and passed around, so it must hold only what the provider needs to find the operation again, never request values or secrets. State retains the token and identifier but no poll scheduling metadata, so the host sends `operating` on the first resume call. A saved read-back token with an identifier can resume from a fresh provider instance through an identifier-bearing handle. Its next pending or preserved result reports read-back progress. SDK dispatch rejects only a backward transition: host progress is read-back while the token is operation-phase. For create poll and resume this returns `Undetermined` and keeps the previous continuation; update decoding rejects the same backward pairing. An identifier-less read-back token ends create polling with `READ_BACK_WITHOUT_IDENTIFIER`.

Create results go through a typestate in the SDK, so a provider must say what it knows. There is no conversion from a plain error into a create result; every failure has to be classified:

- `untracked` means nothing was created. Use it only when you know the API never accepted the request, for example when local validation failed or the API rejected it outright. Carina keeps no record of the resource.
- If the request may have reached the API but you have no identifier (a timeout, or a server error with no ID), return `pending` with an operation payload and settle it in `poll_create`. When a usable continuation also has an error to report, return `preserve`. Carina keeps the attempt and the error.
- Call `issued` as soon as you see the resource's identifier. An `IssuedCreate` can then end or transition through `succeeded`, `partial`, `failed`, `discarded`, `pending`, `preserve`, or `canonicalize`; only an explicit `discarded` leaves no record. `canonicalize` replaces the provisional identifier and returns a finishing handle that cannot go back to pending.
- `discarded` means the provider removed the resource it had created, so Carina keeps no record of it. If the resource may still exist, report it with `failed` instead.

Create and update tokens implement `PhasedToken` with an associated operation payload `type Operation`, an infallible `from_operation`, a consuming `into_operation`, and a borrowed `operation` view. The default `phase()` derives `Operation` from `Some` and `ReadBack` from `None`; providers do not write a second phase match. Identifier-bearing `pending` and `preserve` accept the full token. Handles without an identifier take `T::Operation` directly, and `Poll::Unissued` / `Resume::Unissued` carry that payload. Provider code constructs operation payloads without a fallible wrapper; only SDK dispatch narrows host tokens. Updates use `Pending::new(token, poll_after_secs)` and derive their phase through `Pending::phase()`. `Never` implements the trait for providers that never defer an operation.

For a token type `T: PhasedToken`, the continuation signatures are:

| Handle | `pending` | `preserve` |
|--------|-----------|------------|
| `CreateAttempt` | `(T::Operation, Sent, u32) -> CreateStep<T>` | `(T::Operation, Sent, u32, ProviderError) -> CreateVerdict<T>` |
| `PollAttempt` | `(T::Operation, u32) -> CreateStep<T>` | `(T::Operation, u32, ProviderError) -> PollVerdict<T>` |
| `ResumeAttempt` | `(T::Operation, u32) -> ResumeStep<T>` | `(T::Operation, u32, ProviderError) -> ResumeVerdict<T>` |
| `IssuedCreate` | `(T, u32) -> CreateStep<T>` | `(T, u32, ProviderError) -> CreateVerdict<T>` |
| `ResumedCreate` | `(T, u32) -> ResumeStep<T>` | `(T, u32, ProviderError) -> ResumeVerdict<T>` |

Each method consumes its handle. The `u32` is the poll delay in seconds.

## The sandbox

A provider runs with no filesystem access. It sees only these environment variables:

- `HOME`, `RUST_LOG` and `CARINA_WASI_HTTP_TRACE`, for every provider. Setting `CARINA_WASI_HTTP_TRACE=1` prints every request's header names to stderr, shows values only for a fixed allowlist of non-credential headers (`host`, `content-type`, `content-length`, `transfer-encoding`, `expect`, `user-agent`, `x-amz-date`), redacts all other header values, and removes userinfo, fragments and query values from URIs.
- `AWS_REGION`, `AWS_DEFAULT_REGION`, and `AWS_ENDPOINT_URL`, only for an AWS-kind provider. AWS credential variables are never passed; the AWS world receives credentials through its typed import.
- `GITHUB_TOKEN`, only when the same user-owned record exists for the exact `carina-rs/github` artifact (including its official GitHub source), or for a matching local build explicitly granted the `github` kind.

For a remote provider, the host hashes the bytes being loaded and looks for that SHA-256 and canonical source in a user-owned record under `~/.carina/trust/`. It then applies the fixed official-source allow-list. Registry records are written only after the registry signature verifies, and a signer recorded for the source must belong to `carina-rs`. A `carina-providers.lock` file or `.carina/` directory shipped in a repository cannot create this record and does not grant credentials. This also applies in CI: restoring `.carina/` without the trust directory makes official providers fail before their WebAssembly code runs, so the job must run `carina init` or persist the trust directory (`CARINA_TRUST_RECORD_DIR` can relocate it). An absent, unreadable, malformed, or non-`carina-rs` record produces an error that tells the user to run `carina init`.

A local `file://` build receives no credential partition unless its canonical source path is granted with `--trust-local-provider <PATH=KIND>` or `CARINA_TRUST_LOCAL_PROVIDER`; Carina checks that the bytes being loaded still match that source file. A stale entry in `CARINA_TRUST_LOCAL_PROVIDER` makes provider-loading commands fail until it is fixed or removed. Declare other settings as attributes of the `provider` block, and tell users to pass secrets with `secret(env('NAME'))` rather than writing them into `.crn` files: `env` reads the value from the environment, and `secret` keeps it out of plan output and saved plan files.

Cloud metadata addresses are withheld from every provider kind. The host may use ECS or EC2 metadata as part of its own AWS SDK credential chain, but those endpoints and credentials are never exposed through guest networking or environment variables.

## Protocol version

The provider protocol is versioned. The current development version is 16, and the host also requires at least 16. The SDK records the version it was built with in the provider's `info` response, so you do not set it yourself. Carina v0.9.0, v0.8.0, and v0.7.1 all speak version 12; versions after 12 are development-only until the next release. Protocol 16 changes the schema identity field to a required mode and uses declared identity hashing, so providers built for versions 14 or 15 must be rebuilt.

When the versions do not match, the host refuses to load the provider and tells you which side to update: rebuild the provider against the current SDK, or upgrade Carina. [docs/protocol.md](https://github.com/carina-rs/carina/blob/main/docs/protocol.md) records what each version changed, and [Versioning and Compatibility](/providers/writing/versioning/) explains what to do when the version changes.

## Next steps

[Your First Provider](/providers/writing/getting-started/) builds a small provider and uses it from a configuration.
