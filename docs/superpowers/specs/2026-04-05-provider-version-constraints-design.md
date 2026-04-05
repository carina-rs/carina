# Provider Version Constraints Design

**Date:** 2026-04-05
**Status:** Approved

## Overview

Add version constraint support to Carina's provider system. Users can specify semver constraints (e.g., `~0.5.0`, `^1.2.0`) in the DSL, and Carina resolves them against GitHub Releases to find the best matching version.

## Goals

1. **Version constraint resolution** — Resolve `~0.5.0` style constraints against available GitHub releases to download the best matching provider version
2. **Lock file integrity** — Verify that locked versions satisfy DSL constraints; error if they don't

## Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Constraint syntax | Cargo/semver style (`~`, `^`, `>=`, etc.) | Rust ecosystem; `semver` crate handles parsing and matching |
| Version source | GitHub Releases API | Consistent with existing download mechanism |
| Version unspecified | Resolve to latest release | `source` implies remote intent; lock file ensures reproducibility |
| Lock update command | `carina init --upgrade` | Terraform-style; no new commands |
| Backward compat | Not required | Experimental project; providers must be rebuilt |

## DSL Syntax

```
provider aws {
    source  = "github.com/carina-rs/carina-provider-aws"
    version = "~0.5.0"
    region  = aws.Region.ap_northeast_1
}
```

`version` value is a string literal parsed by `semver::VersionReq::parse()`. Invalid constraint syntax results in a parse error.

## Data Model

### `VersionConstraint` (new type in `carina-core`)

```rust
pub struct VersionConstraint {
    pub raw: String,              // Original string ("~0.5.0")
    pub req: semver::VersionReq,  // Parsed constraint
}
```

### `ProviderConfig` change

```rust
pub struct ProviderConfig {
    pub name: String,
    pub attributes: HashMap<String, Value>,
    pub default_tags: HashMap<String, Value>,
    pub source: Option<String>,
    pub version: Option<VersionConstraint>,  // Was: Option<String>
}
```

### `ProviderInfo` change (carina-provider-protocol)

```rust
pub struct ProviderInfo {
    pub name: String,
    pub display_name: String,
    pub capabilities: Vec<String>,
    pub version: String,  // New: semver string, e.g., "0.5.2"
}
```

No `#[serde(default)]` — `version` is required. Old providers must be rebuilt.

### Lock file (`carina.lock`) change

```json
{
  "providers": [
    {
      "name": "aws",
      "source": "github.com/carina-rs/carina-provider-aws",
      "version": "0.5.2",
      "constraint": "~0.5.0",
      "sha256": "abc123..."
    }
  ]
}
```

New `constraint` field records which constraint resolved to this version.

## Version Resolution Flow

### `carina init`

1. Parse `.crn` files to get `ProviderConfig` list
2. For each provider with `source`:
   - If `carina.lock` exists and locked version satisfies constraint → use locked version
   - Otherwise → query GitHub Releases API (`GET /repos/{owner}/{repo}/releases`)
   - Parse `tag_name` (strip `v` prefix) as `semver::Version`
   - Filter by `VersionReq::matches()`
   - Select highest matching version
   - Download → SHA256 verify → cache → write lock entry
3. If `version` is unspecified but `source` exists → fetch latest release, treat constraint as `*`

### `carina init --upgrade`

Ignore existing lock entries. Re-resolve all providers from constraints against GitHub Releases API.

### `carina plan` / `carina apply`

Before loading providers:
1. Read `carina.lock`
2. For each provider, check locked version satisfies DSL constraint
3. If not, error with message:
   ```
   Error: Provider 'aws' locked at version 0.5.0, but constraint '~0.6.0' requires a different version.
   Run `carina init --upgrade` to resolve.
   ```

### Provider load-time validation

After WASM provider is loaded, call `info()` and verify:

```rust
let info = provider.info();
let actual = semver::Version::parse(&info.version)?;
if !constraint.req.matches(&actual) {
    return Err(format!(
        "Provider '{}' version {} does not satisfy constraint '{}'",
        info.name, actual, constraint.raw
    ));
}
```

## Crate Changes

| Crate | Changes |
|-------|---------|
| **carina-core** | Add `VersionConstraint` type, change `ProviderConfig.version` type, add `semver` dependency |
| **carina-provider-protocol** | Add `version: String` to `ProviderInfo` |
| **carina-plugin-sdk** | None (inherits `ProviderInfo` change) |
| **carina-plugin-host** | Add load-time version validation, add `semver` dependency |
| **carina-cli** | Add `--upgrade` flag to `init`, GitHub Releases API version resolution, lock file `constraint` field, pre-plan/apply lock validation |
| **carina-provider-mock** | Return `version` in `ProviderInfo` |

## Out of Scope

- Changes to `carina-provider-aws` and `carina-provider-awscc` (separate repos; only need to add `version` to `ProviderInfo`)
- Provider registry/index beyond GitHub Releases
- Transitive provider dependency resolution
- LSP completion for version constraint syntax
