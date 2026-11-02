---
title: "Versioning and Compatibility"
description: "How the Carina provider protocol version decides which Carina releases can load a provider, and what to do when the protocol changes."
---

A provider has two version numbers. Its own release version is yours to choose. Its protocol version is fixed by the SDK it was built with, and it decides which Carina releases can load it.

## The protocol version

The host and the provider talk through a versioned protocol. `carina-provider-protocol` defines two numbers: `PROTOCOL_VERSION`, the version this build speaks, and `MIN_SUPPORTED_PROTOCOL_VERSION`, the oldest protocol version the host still loads from a provider. The SDK writes the version it was built with into the provider's `info` response; you never set it yourself.

Every time Carina starts a provider, for `validate`, `plan`, `apply` or any other command that uses it, it checks that version first and refuses to go further if it is out of range. `carina init` only downloads and records the provider without starting it, so a mismatch shows up at the first command after `init`:

```text
provider 'hello' was built against protocol version 18 but this host requires at least version 19; rebuild the provider against the current carina protocol
```

```text
provider 'hello' was built against protocol version 20 but this host supports up to version 19; upgrade carina to load this provider
```

Since v0.5.0, the first release that checks the version, every release has set the minimum equal to the protocol version it speaks, so a provider has to be built for exactly the protocol version of the Carina that loads it:

| Carina release | Protocol version | Oldest provider protocol it loads |
|----------------|------------------|-----------------------------------|
| Next release after v0.9.0 (unreleased) | 19 | 19 |
| Development snapshot after v0.9.0 (unreleased) | 18 | 18 |
| Development snapshot after v0.9.0 (unreleased) | 17 | 17 |
| Development snapshot after v0.9.0 (unreleased) | 16 | 16 |
| Development snapshot after v0.9.0 (unreleased) | 15 | 15 |
| Development snapshot after v0.9.0 (unreleased) | 14 | 14 |
| v0.9.0 | 12 | 12 |
| v0.8.0 | 12 | 12 |
| v0.7.1 | 12 | 12 |
| v0.7.0 | 12 | 12 |
| v0.6.0 | 10 | 10 |
| v0.5.0 | 10 | 10 |

Only protocol 19 is current. Protocols 14 through 18 were development snapshots, and their changes remain part of protocol 19:

| Development protocol | Change |
|----------------------|--------|
| 19 | Desired normalization uses structural trees with per-leaf, exactly-once deferred handles; CRUD encoding is unchanged. |
| 18 | Typed reference schema leaves and batched plan-time reference resolution. |
| 17 | Required list element policy and canonical-value uniqueness validation. |
| 16 | Required identity mode field; declared identity hashing and nested create-only diffing. |
| 15 | Create-progress variants, poll finality, and separate read-back budgets. |
| 14 | Host-resolved AWS credentials through the typed AWS world. |

Protocol 17 introduced the required list element policy. The host checks unique lists during validation, planning, and LSP diagnostics, using canonical element values. Protocol 19 adds per-leaf desired normalization; providers built for protocol 18 or earlier must rebuild against the current SDK.

Protocol 18 adds string-valued reference schema leaves and the initialized provider's `resolve-references` export. Reference validation stays offline; resolution runs when preparing planning comparisons. That snapshot required providers built for protocol 17 or earlier to rebuild against the SDK supplying those WIT bindings.

Rejecting an older provider is deliberate. A protocol bump changes what crosses the boundary, and accepting a provider from the other side of the change would lose information without any error. Protocol 16, for example, replaces the schema's identity boolean with a required closed mode and changes identity from an extra create-only hash input to the complete set of hash inputs. Version-15 providers could set the boolean on mutable attributes, so a version-16 host rejected them before schema decoding and asked for a rebuild. Protocol 12 similarly added a required `write_only` flag to nested struct fields. [docs/protocol.md](https://github.com/carina-rs/carina/blob/main/docs/protocol.md) lists what each version changed.

## When the protocol changes

When a Carina release raises the protocol version, a provider built for the old version stops loading in that release. To support it:

1. Update the SDK dependency's `rev` to a carina commit that speaks the new version.
2. If you also depend directly on other Carina crates, pin them to that same revision. The SDK supplies the matching WIT bindings and binding generator; your provider does not need a separate WIT checkout or generator dependency.
3. Rebuild and fix what no longer compiles. Protocol changes are usually visible as type changes in the SDK, so most of the work shows up as compile errors.
4. Release a new version of the provider, and say in its release notes which Carina release it needs.

Users then need both the new Carina and the new provider. A provider release cannot support two protocol versions at once, and a Carina release loads only the protocol range shown above, so keep the provider releases for each protocol available for users who have not upgraded Carina yet.

## The provider's own version

`info` also returns the provider's own version; the SDK examples use `env!("CARGO_PKG_VERSION")`, the crate version. Users choose a release by that version: with a `github.com/<owner>/<repo>` source, `version` is a semver constraint matched against your release tags, which may include or omit the `v` prefix (`v0.1.0` or `0.1.0`). `carina-providers.lock` records the exact release and checksum installed. `carina init --upgrade` re-resolves the constraint to the newest matching release.

Follow semver for your own version as usual. A protocol bump that forces users to upgrade Carina is a breaking change for them, even if your resources did not change.

## Next steps

[Docs Bundle](/providers/writing/docs-bundle/) describes how a provider will ship its documentation to the registry.
