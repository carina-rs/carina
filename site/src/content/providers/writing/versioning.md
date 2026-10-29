---
title: "Versioning and Compatibility"
description: "How the Carina provider protocol version decides which Carina releases can load a provider, and what to do when the protocol changes."
---

A provider has two version numbers. Its own release version is yours to choose. Its protocol version is fixed by the SDK it was built with, and it decides which Carina releases can load it.

## The protocol version

The host and the provider talk through a versioned protocol. `carina-provider-protocol` defines two numbers: `PROTOCOL_VERSION`, the version this build speaks, and `MIN_SUPPORTED_PROTOCOL_VERSION`, the oldest protocol version the host still loads from a provider. The SDK writes the version it was built with into the provider's `info` response; you never set it yourself.

Every time Carina starts a provider, for `validate`, `plan`, `apply` or any other command that uses it, it checks that version first and refuses to go further if it is out of range. `carina init` only downloads and records the provider without starting it, so a mismatch shows up at the first command after `init`:

```text
provider 'hello' was built against protocol version 12 but this host requires at least version 13; rebuild the provider against the current carina protocol
```

```text
provider 'hello' was built against protocol version 14 but this host supports up to version 13; upgrade carina to load this provider
```

Since v0.5.0, the first release that checks the version, every release has set the minimum equal to the protocol version it speaks, so a provider has to be built for exactly the protocol version of the Carina that loads it:

| Carina release | Protocol version | Oldest provider protocol it loads |
|----------------|------------------|-----------------------------------|
| v0.7.1 | 13 | 13 |
| v0.7.0 | 12 | 12 |
| v0.6.0 | 10 | 10 |
| v0.5.0 | 10 | 10 |

Rejecting an older provider is deliberate. A protocol bump changes what crosses the boundary, and accepting a provider from the other side of the change would lose information without any error. Protocol 12, for example, added a required `write_only` flag to nested struct fields in schemas. An older Carina would have ignored that flag from a newer provider, treated every nested field as not write-only, and planned updates that never converge. The other direction would have been just as bad, so the flag is required: a provider built for an older protocol sends schemas without it, and a newer Carina could not even decode them. [docs/protocol.md](https://github.com/carina-rs/carina/blob/main/docs/protocol.md) lists what each version changed.

## When the protocol changes

When a Carina release raises the protocol version, a provider built for the old version stops loading in that release. To support it:

1. Update the SDK dependency's `rev` to a carina commit that speaks the new version.
2. Check out the `carina-plugin-wit` submodule at the commit that carina pins at that revision, as in [Your First Provider](/providers/writing/getting-started/#project-layout).
3. Rebuild and fix what no longer compiles. Protocol changes are usually visible as type changes in the SDK, so most of the work shows up as compile errors.
4. Release a new version of the provider, and say in its release notes which Carina release it needs.

Users then need both the new Carina and the new provider. A provider release cannot support two protocol versions at once, and a Carina release loads only the protocol range shown above, so keep the provider releases for each protocol available for users who have not upgraded Carina yet.

## The provider's own version

`info` also returns the provider's own version; the SDK examples use `env!("CARGO_PKG_VERSION")`, the crate version. Users choose a release by that version: with a `github.com/<owner>/<repo>` source, `version` is a semver constraint matched against your release tags, which may include or omit the `v` prefix (`v0.1.0` or `0.1.0`). `carina-providers.lock` records the exact release and checksum installed. `carina init --upgrade` re-resolves the constraint to the newest matching release.

Follow semver for your own version as usual. A protocol bump that forces users to upgrade Carina is a breaking change for them, even if your resources did not change.

## Next steps

[Docs Bundle](/providers/writing/docs-bundle/) describes how a provider will ship its documentation to the registry.
