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
| `carina-provider-with-http` | The provider calls an HTTP API through `wasi:http` and declares the hosts it may reach. |

You do not write WIT bindings by hand. The `carina-plugin-sdk` crate defines a `CarinaProvider` trait, and its `export_provider!` macro generates the bindings and connects them to your implementation. The SDK re-exports the request and schema types from `carina-provider-protocol`.

`export_provider!(MyProvider)` exports `carina-provider`. To export `carina-provider-with-http`, declare every hostname the provider may contact:

```rust
carina_plugin_sdk::export_provider!(
    MyProvider,
    http,
    egress: ["api.example.com", "*.service.example.com"],
);
```

Add the `wasi` crate (version 0.14) to the provider's `wasm32` dependencies, because the generated bindings refer to it. The SDK's `carina_plugin_sdk::wasi_http::send_request` then sends an `http::Request` through the host.

The HTTP form embeds a versioned `carina:egress` custom section in the component. The declaration is mandatory. The host reads it from the exact bytes being loaded and permits only consented patterns:

- `api.example.com` matches exactly that host.
- `*.example.com` matches one label, such as `api.example.com`, but not `v1.api.example.com`.
- `**.example.com` matches one or more labels at any depth below `example.com`. It does not match `example.com` itself.

Wildcards are allowed only as the complete leftmost label and cannot sit directly above a public suffix. IP literals, localhost, metadata and link-local names, non-ASCII labels, empty labels, and labels longer than 63 bytes are rejected. DNS resolution is checked separately: loopback, unspecified, and link-local addresses are always denied; metadata is withheld except for a provider kind's own endpoints (currently AWS IMDS and ECS endpoints for AWS-kind providers); and Alibaba or Azure metadata is always withheld. Private IPv4, IPv6, and CGNAT addresses are denied except for AWS-kind providers, which need them for VPC interface endpoints and PrivateLink.

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

Every call must return quickly. The host stops a call whose WebAssembly code has computed for more than 30 seconds in total (time spent waiting in host I/O, such as an HTTP request or a clock sleep, does not count), stops any call that is still running 60 minutes after it began, and limits each HTTP request's connect, first-byte and between-bytes waits to 30 seconds each. A stopped call also breaks that provider instance, so its later calls in the same run fail. A provider never sleeps or polls inside a call. When an operation has not finished, the provider returns a pending result that carries a token it defines and a hint for when to ask again. The host then calls `poll_create`, `poll_update` or `poll_delete` with the original request and that token, until the operation finishes or the resource's time budget (15 minutes by default) runs out.

A pending create is also saved in state. If Carina is interrupted, the next apply calls `resume_create` with the saved token instead of creating the resource again. The token is stored and passed around, so it must hold only what the provider needs to find the operation again, never request values or secrets.

Create results go through a typestate in the SDK, so a provider must say what it knows. There is no conversion from a plain error into a create result; every failure has to be classified:

- `untracked` means nothing was created. Use it only when you know the API never accepted the request, for example when local validation failed or the API rejected it outright. Carina keeps no record of the resource.
- If the request may have reached the API but you have no identifier (a timeout, or a server error with no ID), return `CreateAttempt::pending(token, sent, poll_after_secs)`, which carries no identifier, and settle it in `poll_create`. If a call has a usable continuation but stops with an error, preserve it with the signature for that handle: `CreateAttempt::preserve(token, sent, poll_after_secs, error)`, or `PollAttempt::preserve(token, poll_after_secs, error)`, `ResumeAttempt::preserve(token, poll_after_secs, error)`, `IssuedCreate::preserve(token, poll_after_secs, error)`, or `ResumedCreate::preserve(token, poll_after_secs, error)`. Carina then keeps the attempt and the error instead of forgetting a resource that may exist.
- Call `issued` as soon as you see the resource's identifier. An `IssuedCreate` can then end or transition through `succeeded`, `partial`, `failed`, `discarded`, `pending`, `preserve`, or `canonicalize`; only an explicit `discarded` leaves no record. `canonicalize` replaces the provisional identifier and returns a finishing handle that cannot go back to pending.
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

The provider protocol is versioned. The current development version is 14, and the host also requires at least 14. The SDK records the version it was built with in the provider's `info` response, so you do not set it yourself. Carina v0.9.0, v0.8.0 and v0.7.1 all speak version 12; the next release after v0.9.0 will speak version 14. Providers built for the intermediate version 13 must be rebuilt because version 14 changes the schema identity field and its meaning.

When the versions do not match, the host refuses to load the provider and tells you which side to update: rebuild the provider against the current SDK, or upgrade Carina. [docs/protocol.md](https://github.com/carina-rs/carina/blob/main/docs/protocol.md) records what each version changed, and [Versioning and Compatibility](/providers/writing/versioning/) explains what to do when the version changes.

## Next steps

[Your First Provider](/providers/writing/getting-started/) builds a small provider and uses it from a configuration.
