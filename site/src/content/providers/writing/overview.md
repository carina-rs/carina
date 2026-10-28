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
| `carina-provider-with-http` | The provider calls an HTTP API. The host grants outgoing requests through `wasi:http`. |

You do not write WIT bindings by hand. The `carina-plugin-sdk` crate defines a `CarinaProvider` trait, and its `export_provider!` macro generates the bindings and connects them to your implementation. The SDK re-exports the request and schema types from `carina-provider-protocol`.

`export_provider!(MyProvider)` exports `carina-provider`. To export `carina-provider-with-http`, write `export_provider!(MyProvider, http)` and add the `wasi` crate (version 0.14) to the provider's `wasm32` dependencies, because the generated bindings refer to it. The SDK's `carina_plugin_sdk::wasi_http::send_request` then sends an `http::Request` through the host.

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

Every call must return quickly. A provider never sleeps or polls inside a call. When an operation has not finished, the provider returns a pending result that carries a token it defines and a hint for when to ask again. The host then calls `poll_create`, `poll_update` or `poll_delete` with the original request and that token, until the operation finishes or the resource's time budget (15 minutes by default) runs out.

A pending create is also saved in state. If Carina is interrupted, the next apply calls `resume_create` with the saved token instead of creating the resource again. The token is stored and passed around, so it must hold only what the provider needs to find the operation again, never request values or secrets.

Create results go through a typestate in the SDK, so a provider must say what it knows. Before it has a cloud identifier, a failure is `untracked` (nothing exists). After it calls `issued` with an identifier, the attempt can only finish as succeeded, partially succeeded, failed after creation, discarded, or still pending. There is no conversion from a plain error into a create result, so a provider cannot report "nothing was created" after it has created something.

## The sandbox

A provider runs with no filesystem access. It sees only these environment variables:

- `HOME`, `RUST_LOG` and `CARINA_WASI_HTTP_TRACE`, for every provider.
- AWS credential and region variables, only for providers named `aws` or `awscc`.
- `GITHUB_TOKEN`, only for a provider named `github`.

The host chooses these by the name the provider reports from `info`, so do not give a third-party provider one of those names: a user who installs it would hand it their AWS or GitHub credentials. A provider with any other name receives no credentials from the environment. Declare the settings it needs as attributes of its `provider` block instead.

## Protocol version

The provider protocol is versioned. The current version is 12, and the host also requires at least 12. The SDK records the version it was built with in the provider's `info` response, so you do not set it yourself. Carina v0.7.0 speaks version 12.

When the versions do not match, the host refuses to load the provider and tells you which side to update: rebuild the provider against the current SDK, or upgrade Carina. [docs/protocol.md](https://github.com/carina-rs/carina/blob/main/docs/protocol.md) records what each version changed.

## Next steps

[Your First Provider](/providers/writing/getting-started/) builds a small provider and uses it from a configuration.
