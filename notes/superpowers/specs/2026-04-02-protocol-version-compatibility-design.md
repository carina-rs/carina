# Protocol Version Compatibility Design

## Problem

Carina's plugin protocol (JSON-RPC over stdin/stdout between carina-cli and external provider plugins) has no version negotiation mechanism. As the protocol evolves during this experimental phase, there is no way to detect or communicate incompatibilities between Carina and provider plugins.

Additionally, the crate versioning strategy needs to account for the possibility of splitting `carina-provider-protocol` and `carina-plugin-sdk` into separate repositories in the future.

## Approach

Combine a simple protocol version number (for breaking changes) with capability information (for optional features). Keep crate versioning simple during the monorepo phase, with a clear path to separation.

## Design

### 1. Protocol Version Handshake

Add a `PROTOCOL_VERSION` constant to `carina-provider-protocol`:

```rust
// carina-provider-protocol/src/lib.rs
pub const PROTOCOL_VERSION: u32 = 1;
```

Include the version in the `ready` notification that plugins send on startup:

```jsonc
// ready notification (plugin -> host)
{
  "jsonrpc": "2.0",
  "method": "ready",
  "params": { "protocol_version": 1 }
}
```

Host-side handling in `carina-plugin-host/src/process.rs`:

- On receiving `ready`, extract `protocol_version` from params
- If version matches: proceed as normal
- If version does not match: terminate the plugin process and return an error:
  - Plugin older than host: `"Plugin <name> uses protocol version <N>, but Carina requires version <M>. Please update the plugin."`
  - Plugin newer than host: `"Plugin <name> uses protocol version <N>, but this version of Carina only supports version <M>. Please update Carina."`

### 2. Version Increment Rules

Increment `PROTOCOL_VERSION` when:

- Provider trait method signatures change (add/remove/modify parameters or return types)
- Existing fields in protocol types change type or are removed
- Required methods are added or removed

Do NOT increment when:

- Adding optional fields with `#[serde(default)]` to existing types
- Adding new capabilities (see below)
- Adding optional fields to response types

### 3. Capability Information

Extend `ProviderInfo` in `carina-provider-protocol/src/types.rs`:

```rust
pub struct ProviderInfo {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}
```

Defined capabilities (initial set, based on currently optional methods):

| Capability | Method | Behavior when absent |
|---|---|---|
| `normalize_desired` | `normalize_desired` | Skip (no-op) |
| `normalize_state` | `normalize_state` | Skip (no-op) |
| `hydrate_read_state` | `hydrate_read_state` | Skip (no-op) |
| `merge_default_tags` | `merge_default_tags` | Skip (no-op) |

Required methods (`read`, `create`, `update`, `delete`, `schemas`, `validate_config`) are not listed as capabilities. Their presence is guaranteed by protocol version match.

Host-side handling in `carina-plugin-host`:

- `ProcessProviderNormalizer` checks capabilities before calling each optional method
- If capability is absent, skip the JSON-RPC call and return default (no-op) behavior

SDK-side (`carina-plugin-sdk`):

- Provider implementors explicitly declare supported capabilities via `fn capabilities(&self) -> Vec<String>`
- SDK includes the result in the `provider_info` response

### 4. Crate Versioning Strategy

**Current phase (monorepo):**

- All crates share `workspace.package.version`
- `PROTOCOL_VERSION` is independent of crate version (e.g., crate `0.3.0` may have protocol version `2`)
- API changes across crates are fixed simultaneously in the same commit/PR

**When separation is needed:**

Priority order for stabilization and extraction:

1. `carina-provider-protocol` - the contract between host and plugin
2. `carina-plugin-sdk` - the developer-facing SDK that depends on protocol

Versioning rules after separation:

- `carina-provider-protocol`: semver. Major version = protocol version (protocol v2 -> crate 2.x.x)
- `carina-plugin-sdk`: follows protocol crate version. SDK 2.x corresponds to protocol 2.x
- Remaining monorepo crates continue with unified workspace version

**Dependency graph (public vs internal):**

```
External plugin
  +-- carina-plugin-sdk (public)
       +-- carina-provider-protocol (public)

carina-cli (internal)
  +-- carina-plugin-host (internal)
  |    +-- carina-core (internal)
  |    +-- carina-provider-protocol (public)
  +-- carina-core (internal)
  +-- carina-state (internal)
```

External plugins depend only on `carina-plugin-sdk` and `carina-provider-protocol`. They never depend on `carina-core` directly.

### 5. Protocol Version Upgrade Workflow

When making a breaking protocol change:

1. Increment `PROTOCOL_VERSION`
2. Modify types/methods in `carina-provider-protocol`
3. Update all in-repo plugins (`carina-provider-mock`, etc.) simultaneously
4. Document the change in CHANGELOG with migration notes
5. If external plugins exist, provide a migration guide

### 6. Backward-Compatible Extension Examples

These changes do NOT require a protocol version bump:

- Adding `#[serde(default)]` optional field to `ProviderInfo` or any response type
- Defining a new capability string (existing plugins simply don't declare it)
- Adding a new optional method gated behind a capability
