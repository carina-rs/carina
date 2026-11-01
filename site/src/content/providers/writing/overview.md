---
title: "Writing a Provider"
description: "What a Carina provider is: a WebAssembly component that receives typed create, read, update and delete requests from the host over a versioned WIT interface."
---

A provider teaches Carina how to manage one kind of infrastructure. The official providers, `carina-rs/aws` and `carina-rs/awscc`, cover AWS. Anyone can write another one with the same SDK they use.

This section covers writing a provider, building it, and using it from a local file or a GitHub repository. Publishing to the provider registry is not open to third parties yet; see [Publishing](/providers/writing/publishing/).

## What a provider is

A provider is a [WebAssembly component](https://component-model.bytecodealliance.org/) built for the `wasm32-wasip2` target. Carina (the host) loads it into a sandbox and calls it through the interface defined in [carina-plugin-wit](https://github.com/carina-rs/carina-plugin-wit). It exports one of two WIT worlds:

| World | Use it when |
|-------|-------------|
| `carina-provider` | The provider needs no network access. |
| `carina-provider-with-http` | The provider calls an HTTP API through `wasi:http`. Only AWS hosts are reachable today; see below. |

You do not write WIT bindings by hand. The `carina-plugin-sdk` crate defines a `CarinaProvider` trait, and its `export_provider!` macro generates the bindings and connects them to your implementation. The SDK re-exports the request and schema types from `carina-provider-protocol`.

`export_provider!(MyProvider)` exports `carina-provider`. To export `carina-provider-with-http`, write `export_provider!(MyProvider, http)` and add the `wasi` crate (version 0.14) to the provider's `wasm32` dependencies, because the generated bindings refer to it. The SDK's `carina_plugin_sdk::wasi_http::send_request` then sends an `http::Request` through the host.

The host checks every outgoing request against a fixed allow-list. Hosts ending in `.amazonaws.com` or `.amazonaws.com.cn` are reachable, while the EC2 and ECS metadata addresses are reachable only by providers the host trusts as AWS. It denies any other host, and neither a provider nor a configuration can extend the list yet, so a provider for a non-AWS API cannot reach that API today. The EKS Pod Identity endpoints (`169.254.170.23` and `fd00:ec2::23`) and arbitrary hosts named by `AWS_CONTAINER_CREDENTIALS_FULL_URI` are not reachable. This is tracked in [carina#4021](https://github.com/carina-rs/carina/issues/4021).

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

Post-create and post-update reads have a separate host-owned budget. It starts at the first `read-back` pending result, defaults to two minutes, and is clipped to the operation's original deadline, so it never extends the operation. A resource schema can set a non-zero `operation_config.read_back.timeout_secs`. The host marks the one poll at the active deadline as `final-attempt` in `poll_create`, `resume_create`, and `poll_update`. Once read-back has begun, that call must return success, partial success, or an error; returning `pending` in either phase is a protocol violation. Returning from `read-back` to `operation` before the deadline is also a violation. Do not put attempt counters in a read-back token—the host owns the deadline and finality.

A pending create is also saved in state. If Carina is interrupted, the next apply calls `resume_create` with the saved token instead of creating the resource again. The token is stored and passed around, so it must hold only what the provider needs to find the operation again, never request values or secrets.

Create results go through a typestate in the SDK, so a provider must say what it knows. There is no conversion from a plain error into a create result; every failure has to be classified:

- `untracked` means nothing was created. Use it only when you know the API never accepted the request, for example when local validation failed or the API rejected it outright. Carina keeps no record of the resource.
- If the request may have reached the API but you have no identifier (a timeout, or a server error with no ID), return `CreateAttempt::pending(token, sent, poll_after_secs)`, which carries no identifier, and settle it in `poll_create`. If a call has a usable continuation but stops with an error, preserve it with the signature for that handle: `CreateAttempt::preserve(token, sent, poll_after_secs, error)`, or `PollAttempt::preserve(token, poll_after_secs, error)`, `ResumeAttempt::preserve(token, poll_after_secs, error)`, `IssuedCreate::preserve(token, poll_after_secs, error)`, or `ResumedCreate::preserve(token, poll_after_secs, error)`. Carina then keeps the attempt and the error instead of forgetting a resource that may exist.
- Call `issued` as soon as you see the resource's identifier. An `IssuedCreate` can then end or transition through `succeeded`, `partial`, `failed`, `discarded`, `pending`, `pending_read_back`, `preserve`, or `canonicalize`; only an explicit `discarded` leaves no record. `canonicalize` replaces the provisional identifier and returns a finishing handle that cannot go back to pending.
- `discarded` means the provider removed the resource it had created, so Carina keeps no record of it. If the resource may still exist, report it with `failed` instead.

## The sandbox

A provider runs with no filesystem access. It sees only these environment variables:

- `HOME`, `RUST_LOG` and `CARINA_WASI_HTTP_TRACE`, for every provider. Setting `CARINA_WASI_HTTP_TRACE=1` prints every request's headers to stderr, including authorization headers, so do not use it with real credentials or in shared logs.
- AWS credential and region variables, only when this user's `carina init` downloaded the exact artifact from the origin for `carina-rs/aws` or `carina-rs/awscc` (including their official GitHub sources) and recorded its source and SHA-256 in the user's trust directory, or for a matching local build explicitly granted the `aws` kind.
- `GITHUB_TOKEN`, only when the same user-owned record exists for the exact `carina-rs/github` artifact (including its official GitHub source), or for a matching local build explicitly granted the `github` kind.

For a remote provider, the host hashes the bytes being loaded and looks for that SHA-256 and canonical source in a user-owned record under `~/.carina/trust/`. It then applies the fixed official-source allow-list. Registry records are written only after the registry signature verifies, and a signer recorded for the source must belong to `carina-rs`. A `carina-providers.lock` file or `.carina/` directory shipped in a repository cannot create this record and does not grant credentials. This also applies in CI: restoring `.carina/` without the trust directory makes official providers fail before their WebAssembly code runs, so the job must run `carina init` or persist the trust directory (`CARINA_TRUST_RECORD_DIR` can relocate it). An absent, unreadable, malformed, or non-`carina-rs` record produces an error that tells the user to run `carina init`.

A local `file://` build receives no credential partition unless its canonical source path is granted with `--trust-local-provider <PATH=KIND>` or `CARINA_TRUST_LOCAL_PROVIDER`; Carina checks that the bytes being loaded still match that source file. A stale entry in `CARINA_TRUST_LOCAL_PROVIDER` makes provider-loading commands fail until it is fixed or removed. Declare other settings as attributes of the `provider` block, and tell users to pass secrets with `secret(env('NAME'))` rather than writing them into `.crn` files: `env` reads the value from the environment, and `secret` keeps it out of plan output and saved plan files.

The same user trust record controls metadata access. Only a recorded artifact from an allow-listed AWS source can reach the EC2 or ECS metadata addresses and read role credentials from them.

## Protocol version

The provider protocol is versioned. The current development version is 14, and the host also requires at least 14. The SDK records the version it was built with in the provider's `info` response, so you do not set it yourself. Carina v0.9.0, v0.8.0 and v0.7.1 all speak version 12; version 14 will ship in the next release after v0.9.0.

When the versions do not match, the host refuses to load the provider and tells you which side to update: rebuild the provider against the current SDK, or upgrade Carina. [docs/protocol.md](https://github.com/carina-rs/carina/blob/main/docs/protocol.md) records what each version changed, and [Versioning and Compatibility](/providers/writing/versioning/) explains what to do when the version changes.

## Next steps

[Your First Provider](/providers/writing/getting-started/) builds a small provider and uses it from a configuration.
