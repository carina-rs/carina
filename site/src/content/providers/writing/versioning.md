---
title: "Versioning and Compatibility"
description: "How the Carina provider protocol version decides which Carina releases can load a provider, and what to do when the protocol changes."
---

A provider has two version numbers. Its own release version is yours to choose. Its protocol version is fixed by the SDK it was built with, and it decides which Carina releases can load it.

## The protocol version

The host and the provider talk through a versioned protocol. `carina-provider-protocol` defines two numbers: `PROTOCOL_VERSION`, the version this build speaks, and `MIN_SUPPORTED_PROTOCOL_VERSION`, the oldest protocol version the host still loads from a provider. The SDK writes the version it was built with into the provider's `info` response; you never set it yourself.

Every time Carina starts a provider, for `validate`, `plan`, `apply` or any other command that uses it, it first rejects incompatible WIT packages before linking, then checks the declared protocol before binding provider exports. It refuses to go further if either is incompatible. `carina init` only downloads and records the provider without starting it, so a mismatch shows up at the first command after `init`:

```text
provider 'hello' was built against protocol version 27 but this host requires at least version 28; rebuild the provider against the current carina protocol
```

```text
provider 'hello' was built against protocol version 29 but this host supports up to version 28; upgrade carina to load this provider
```

Since v0.5.0, the first release that checks the version, every release has set the minimum equal to the protocol version it speaks, so a provider has to be built for exactly the protocol version of the Carina that loads it:

| Carina release | Protocol version | Oldest provider protocol it loads |
|----------------|------------------|-----------------------------------|
| v0.14.0 | 28 | 28 |
| Refinement-layer snapshot (unreleased) | 27 | 27 |
| Conditional input-rule snapshot (unreleased) | 26 | 26 |
| Development snapshot after v0.13.1 (unreleased) | 25 | 25 |
| v0.13.1 | 24 | 24 |
| v0.13.0 | 24 | 24 |
| v0.12.1 | 23 | 23 |
| v0.12.0 | 23 | 23 |
| Development snapshot after v0.11.0 (unreleased) | 22 | 22 |
| v0.11.0 | 21 | 21 |
| Development snapshot after v0.10.0 (unreleased) | 20 | 20 |
| v0.10.0 | 19 | 19 |
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

Carina v0.14.0 speaks protocol 28 and requires at least protocol 28 (WIT 0.3.0). Protocol 28 requires an exclusive typed error retryability class that survives transport; message, cause and code never infer retry policy. Protocol 27 requires separate identity and member constraint records for refined String and Int, including explicit empty records and rejection of unknown layer fields; within-provider identity definitions must agree, and legacy Custom bases cannot carry member constraints. Protocol 26 requires typed input-rule conditions and Struct-local rule arrays. Protocol 25 introduced typed diagnostics, discarded guest stdout/stderr, and required credential registration before adoption, including each AWS refresh through `AwsSdkParts`. `CARINA_WASI_HTTP_TRACE` is host-only, and legacy WIT packages are rejected before linking. Released Carina v0.13.0 and v0.13.1 use protocol 24; v0.12.0 and v0.12.1 use protocol 23, v0.11.0 uses protocol 21 and v0.10.0 uses protocol 19. Protocol 24 requires [explicit pattern match modes and reasons containing visible text for substring exceptions](/providers/writing/schemas/#attribute-types). Protocol 23 requires every resource schema to carry an `input_rules` array, `[]` when the resource declares no rules. Each rule names a scope (`create_only` or `every_write`), a typed target, a stable diagnostic code and a predicate, and Carina checks it on the inputs of the writes in that scope, never on read-back or state values. Protocol 22 added an optional entry-count bound to map attribute types, the same `length` shape that list types use. Providers built for protocol 27 or earlier must rebuild against the current SDK, including for metadata-only state migration checks.

For protocol 28 AWS configuration, use `AwsSessionMaterial`, `AwsSessionSource`, `AwsSessionFuture` and `AwsSdkParts` (SDK feature `aws`). Sources return `AwsSessionMaterial::new(credentials)` with the complete SDK value; registration retains expiry, account ID, provider name and feature/custom properties. Registration and acknowledged snapshots remain internal; snapshots cannot provide credentials independently or prepare unsigned requests. The SDK installs the refreshing provider and HTTP client together and registers every returned generation; later AWS config changes remain under caller control.

Protocol 26 introduced required `InputRule.condition` and Struct `input_rules` arrays to the protocol-25 diagnostics contract, retaining WIT 0.2.0. CreateOnly input-rule violations are validate/LSP warnings with `(rejected if this resource is created)` wording and plan errors only at create-capable sites; EveryWrite violations remain errors. Carina v0.14.0 requires the matching protocol-28 SDK; v0.13.1 remains protocol 24/24. Schema fingerprint encoding stays v13.

The protocol 21 schema envelope requires `assignability_edges`, even when empty, and SDK providers must implement `CarinaProvider::assignability_edges`. Both endpoints must be declared by that provider as scalar String, Int, or Float identities with the same scalar kind. Enum, Reference, and core built-in identities cannot be endpoints; self edges, duplicates, and cycles are rejected. Each provider's transitive closure is retained separately, so paths never combine different providers' declarations.

Assignment first checks structural identity. When identities differ, a provider attribute uses its sink owner's edges. Type annotations and module boundaries require a path declared by every owner of the sink identity. The existing length and range checks still apply, and sink validators still run on values during validation, planning, and apply.

Protocols 14 through 27 came before protocol 28, and their changes remain part of it:

| Protocol | Change |
|----------------------|--------|
| 27 | Separate String/Int identity and member constraint records. |
| 26 | Required conditional input rules and Struct-local rule arrays. |
| 25 | WIT 0.2.0 typed diagnostics and credential registration. |
| 24 | Required explicit pattern match modes and justified substring exceptions. |
| 23 | Required scoped desired-input rules on resource schemas. |
| 22 | Optional entry-count bound on map attribute types. |
| 21 | Required provider-wide identity assignability declarations in the schema envelope. |
| 20 | Create-outcome names describe cloud knowledge: `not-created`, `identifier-reported`, `no-resource`, and `unchanged`; identifier-bearing records are `create-identified` and `resume-identified`. |
| 19 | Desired normalization uses structural trees with per-leaf, exactly-once deferred handles; CRUD encoding is unchanged. |
| 18 | Typed reference schema leaves and batched plan-time reference resolution. |
| 17 | Required list element policy and canonical-value uniqueness validation. |
| 16 | Required identity mode field; declared identity hashing and nested create-only diffing. |
| 15 | Create-progress variants, poll finality, and separate read-back budgets. |
| 14 | Host-resolved AWS credentials through the typed AWS world. |

Protocol 17 introduced the required list element policy. The host checks unique lists during validation, planning, and LSP diagnostics, using canonical element values. Protocol 19 added per-leaf desired normalization. Protocol 20 renamed create-outcome verdicts without changing their handling.

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
