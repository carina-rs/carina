---
title: "Your First Provider"
description: "Build a minimal Carina provider with carina-plugin-sdk, compile it to a WebAssembly component, and use it from a configuration through a file:// source."
---

This page builds `hello`, a provider with one resource type, `hello.greeting`. It calls no API: a greeting's message is derived from its name. That keeps the example small enough to read in full while still going through every call the host makes, so you can run `plan`, `apply` and `destroy` against it.

The code on this page was compiled and run against the carina repository at commit `6726a1cc`, with `carina-plugin-wit` at `5ebd5ce6`.

## Prerequisites

- Rust with the `wasm32-wasip2` target: `rustup target add wasm32-wasip2`
- The [`carina` CLI](/getting-started/installation/), at a version that speaks the same protocol version as the SDK you build against (see [Protocol version](/providers/writing/overview/#protocol-version))

## Project layout

```text
carina-provider-hello/          # repository root
├── Cargo.toml                  # workspace
├── carina-plugin-wit/          # git submodule
└── carina-provider-hello/
    ├── Cargo.toml
    └── src/main.rs
```

The provider crate sits one directory below the repository root because `export_provider!` reads the WIT files from `../carina-plugin-wit/wit`, relative to the crate. The official providers use the same layout.

Create the repository and add the WIT definitions as a submodule:

```bash
mkdir carina-provider-hello && cd carina-provider-hello
git init
git submodule add https://github.com/carina-rs/carina-plugin-wit.git carina-plugin-wit
```

The WIT files must match the SDK. Check out the `carina-plugin-wit` commit that the carina repository pins at the revision you depend on. For `6726a1cc`, that is `5ebd5ce6`:

```bash
git -C carina-plugin-wit checkout 5ebd5ce6
```

For another revision, look up the pinned commit with the GitHub CLI: `gh api "repos/carina-rs/carina/contents/carina-plugin-wit?ref=<rev>" --jq .sha`.

## Cargo manifests

The workspace manifest at the repository root:

```toml
[workspace]
members = ["carina-provider-hello"]
resolver = "2"
```

The crate manifest, `carina-provider-hello/Cargo.toml`:

```toml
[package]
name = "carina-provider-hello"
version = "0.1.0"
edition = "2024"

[dependencies]
carina-plugin-sdk = { git = "https://github.com/carina-rs/carina", rev = "6726a1cc" }
serde = "1"
serde_json = "1"

[target.'cfg(target_arch = "wasm32")'.dependencies]
wit-bindgen = { version = "0.54", default-features = false, features = ["macros"] }
```

The SDK is not on crates.io; depend on it from git and pin `rev`. `serde`, `serde_json` and `wit-bindgen` are needed because the code that `export_provider!` generates uses them from your crate.

## The provider

`carina-provider-hello/src/main.rs`:

```rust
// The provider only runs as a WebAssembly component; on the host target
// nothing calls it.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::HashMap;

use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, Never, Pending, Poll,
    PollVerdict, Resume, ResumeStep, ResumeVerdict, Sent, UpdateOutcome,
};

/// A provider with one resource, `hello.greeting`. It talks to no API:
/// the greeting is derived from its name, so it keeps no state of its own.
#[derive(Default)]
struct HelloProvider;

fn string_type() -> AttributeType {
    AttributeType::String {
        pattern: None,
        length: None,
        validate: None,
        to_dsl: None,
        identity: None,
    }
}

fn attribute(name: &str, required: bool, create_only: bool, read_only: bool) -> AttributeSchema {
    AttributeSchema {
        name: name.to_string(),
        attr_type: string_type(),
        required,
        default: None,
        description: None,
        create_only,
        read_only,
        write_only: false,
        block_name: None,
        provider_name: None,
        removable: None,
        identity: required,
        deferred_populate: false,
    }
}

fn error(kind: ProviderErrorKind, message: impl Into<String>) -> ProviderError {
    ProviderError {
        kind,
        message: message.into(),
        resource_id: None,
        cause: None,
        provider_name: Some("hello".to_string()),
        operation: None,
        status: None,
        code: None,
        request_id: None,
    }
}

fn greeting(name: &str) -> HashMap<String, Value> {
    HashMap::from([
        ("name".to_string(), Value::String(name.to_string())),
        ("message".to_string(), Value::String(format!("Hello, {name}!"))),
    ])
}

impl CarinaProvider for HelloProvider {
    // This provider finishes every operation in one call, so it never
    // hands the host a continuation token.
    type CreateToken = Never;
    type UpdateToken = Never;
    type DeleteToken = Never;

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "hello".to_string(),
            display_name: "Hello".to_string(),
            capabilities: vec![],
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![ResourceSchema {
            resource_type: "greeting".to_string(),
            attributes: HashMap::from([
                ("name".to_string(), attribute("name", true, true, false)),
                ("message".to_string(), attribute("message", false, false, true)),
            ]),
            description: Some("A greeting derived from a name".to_string()),
            kind: SchemaKind::Managed,
            unique_name: UniqueNameSpec::default(),
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![],
            defs: Default::default(),
        }]
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attrs: &HashMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> Result<ReadOutcome, ProviderError> {
        Ok(ReadOutcome::Found {
            state: AppliedState {
                id: id.clone(),
                identifier: identifier.clone(),
                attributes: greeting(identifier.as_str()),
            },
        })
    }

    fn read_data_source(&self, _resource: &Resource) -> Result<State, ProviderError> {
        Err(error(ProviderErrorKind::InvalidInput, "hello has no data sources"))
    }

    fn create(
        &self,
        _id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> Result<CreateStep<Never>, CreateVerdict> {
        let name = match request.resource.attributes.get("name") {
            Some(Value::String(name)) if !name.is_empty() => name.clone(),
            _ => {
                return Err(attempt.untracked(error(
                    ProviderErrorKind::InvalidInput,
                    "name must be a non-empty string",
                )));
            }
        };
        let identifier = AppliedIdentifier::new(name.clone()).expect("name is non-empty");
        Ok(attempt
            .issued(identifier, Sent::AllRequestAttributes)
            .succeeded(greeting(&name)))
    }

    fn poll_create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        poll: Poll<Never>,
    ) -> Result<CreateStep<Never>, PollVerdict> {
        match poll {
            Poll::Unissued(_, token) | Poll::Issued(_, token) => match token {},
        }
    }

    fn resume_create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        resume: Resume<Never>,
    ) -> Result<ResumeStep<Never>, ResumeVerdict> {
        match resume {
            Resume::Unissued(_, token) | Resume::Issued(_, token) => match token {},
        }
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: UpdateRequest,
    ) -> Result<UpdateOutcome<Never>, ProviderError> {
        // `name` is create-only and `message` is read-only, so the host
        // replaces the resource instead of updating it.
        Err(error(ProviderErrorKind::Internal, "greeting cannot be updated"))
    }

    fn poll_update(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: UpdateRequest,
        pending: Pending<Never>,
    ) -> Result<UpdateOutcome<Never>, ProviderError> {
        match pending.token {}
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: DeleteRequest,
    ) -> Result<DeleteOutcome<Never>, ProviderError> {
        Ok(DeleteOutcome::Deleted)
    }

    fn poll_delete(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: DeleteRequest,
        pending: Pending<Never>,
    ) -> Result<DeleteOutcome<Never>, ProviderError> {
        match pending.token {}
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_plugin_sdk::PlanOp,
    ) -> Result<Vec<String>, ProviderError> {
        Ok(vec![])
    }
}

#[cfg(target_arch = "wasm32")]
carina_plugin_sdk::export_provider!(HelloProvider);

fn main() {}
```

The parts to notice:

- `info` returns the name the configuration uses in `provider hello { ... }` and in resource types. A resource type `greeting` becomes `hello.greeting` in the DSL.
- `name` is required and create-only, so changing it replaces the resource instead of updating it. It is also marked `identity`, so an anonymous `hello.greeting` (one without `let`) gets a stable address from its name.
- `message` is read-only: the provider sets it, and the configuration cannot.
- `create` returns the identifier through the create typestate. `issued` records the identifier the provider reported; `succeeded` finishes the create with its attributes. `Sent` says which top-level attributes the accepted API request carried; `Sent::AllRequestAttributes` is right here because nothing is sent separately. A provider that sets some attributes through a later call should name only the ones it sent, with `Sent::only([...])`.
- `read` rebuilds the attributes from the identifier. A provider for a real API would call the API here and return `ReadOutcome::NotFound` when the resource is gone.
- The token types are `Never`, an empty type, because every operation finishes in one call. The poll and resume methods still have to exist, but they can never be called, and `match token {}` proves that to the compiler.
- `export_provider!` builds the provider with `Default`, so the type must implement it. `main` is empty; the host calls the exported functions, not `main`.
- The macro only expands for `wasm32`, so on the host target (`cargo check`, rust-analyzer) nothing uses the provider. The `cfg_attr` line at the top keeps that from producing dead-code warnings.

## Build

```bash
cargo build --target wasm32-wasip2 --release
```

The component is at `target/wasm32-wasip2/release/carina-provider-hello.wasm`.

## Use it from a configuration

In a separate directory, create `main.crn` and point a `provider` block at the component with a `file://` URL. The path must be absolute. With no `backend` block, state goes to `carina.state.json` in that directory.

```crn
provider hello {
  source = 'file:///path/to/carina-provider-hello/target/wasm32-wasip2/release/carina-provider-hello.wasm'
}

let greeting = hello.greeting {
  name = 'world'
}
```

Install the provider, then plan and apply:

```bash
carina init
carina plan
carina apply
```

`carina plan` shows the greeting to be created, with `message` as `(known after apply)`. After `apply`, another `plan` reports no changes, because `read` returns the same attributes. Change `name` to `'carina'` and `plan` shows the greeting must be replaced, because `name` is create-only. `carina destroy` deletes it.

`carina init` installs the component into `.carina/providers/` (as a hard link when it can, otherwise a copy) and records its checksum in `carina-providers.lock`. Later commands use that copy, so after you rebuild the provider, run `carina init` again to pick up the new build and update the checksum.

## Use it from GitHub

Once the provider is in a GitHub repository, a configuration can install it from there:

```crn
provider hello {
  source  = 'github.com/your-org/carina-provider-hello'
  version = '0.1.0'
}
```

`version` is a semver constraint, as for any provider. `carina init` considers all of the repository's releases, picks the newest matching version, and preserves the release's exact tag. Tags may be either `v0.1.0` or `0.1.0`. The release must contain exactly one asset whose name ends in `.wasm`; the asset may otherwise have any name. The lookup uses `GITHUB_TOKEN` when set or the token from a logged-in `gh` when available, so it also works with private repositories. `carina-providers.lock` records the exact tag and GitHub asset ID, alongside the semver version and checksum, so later installs fetch and verify the same asset.

Lock files written by older Carina versions for `github.com/...` version sources do not record the release tag. Run `carina init` once to rewrite those entries; `carina init --locked`, plan, and apply report the legacy entry until it has been rewritten.

With `revision` instead of `version`, `carina init` installs the CI build for a branch, tag or commit. It resolves the revision to a commit and looks at the successful GitHub Actions runs for that commit. Only runs triggered by a `push` to the repository itself count; pull request runs, including those from forks, are ignored. Across the artifacts those runs uploaded, the `.wasm` files must all have the same content, so have one workflow upload the built component. The artifact may have any name; the file inside it must end in `.wasm`. Byte-identical copies count as one build, and the newest run's copy is used. Every artifact of those runs is downloaded and inspected, so an artifact larger than 512 MiB, or a `.wasm` file larger than 256 MiB once unpacked, makes the install fail. When no build or more than one is found, the error lists the candidates. This call does need a token, `GITHUB_TOKEN` or a logged-in `gh`.

`carina-providers.lock` records the commit, the artifact ID and the checksum, and later installs download that artifact without choosing again. GitHub deletes artifacts after their retention period. When the recorded artifact is gone, `carina init` looks for a build at the same commit and accepts it only if its checksum matches the lock. In lock-writing modes it records the new artifact ID. With `--locked` it leaves the lock unchanged, installs the build and suggests running `carina init`, which then records the new ID. If every artifact for the commit has expired, re-run CI for that commit or point `revision` at a newer one. Unless your builds are reproducible, the rebuilt `.wasm` will not match the recorded checksum; Carina refuses it, and you accept the new build by removing the provider's entry from `carina-providers.lock` and running `carina init`.

Lock entries written by older Carina versions for `revision` sources have no artifact ID. `carina init` selects the build again at the recorded commit, checks it against the recorded checksum and adds the ID; `carina init --locked`, plan, and apply report the legacy entry until then.

A GitHub source is not signature-checked, but `carina init` records the checksum of what it downloaded in the lock file.

## Next steps

[Implementing Operations](/providers/writing/implementing-operations/) covers a provider for a real, asynchronous API: pending creates, polling, updates from a patch, and errors. The mock provider in the carina repository, [`carina-provider-mock`](https://github.com/carina-rs/carina/tree/main/carina-provider-mock), is a test fixture that exercises every create outcome.
