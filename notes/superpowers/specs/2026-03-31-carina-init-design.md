# Phase 2b: `carina init` + Provider Auto-Resolution

**Date**: 2026-03-31
**Status**: Draft
**Depends on**: Phase 2a (awscc external process binary) — merged in PR #1421

## Overview

Add `carina init` command and automatic provider resolution so that providers declared with `source` in `.crn` files are automatically downloaded, cached, and loaded. When a provider binary is missing at plan/apply time, the resolution runs automatically (same as explicit `carina init`).

## Goals

- `carina init` downloads provider binaries from GitHub Releases into `.carina/providers/`
- `carina plan` / `carina apply` auto-resolve missing providers (same logic as init)
- `carina.lock` records SHA256 hashes for binary integrity verification
- Platform-specific binary selection (target triple detection)

## Non-Goals

- Repository separation for `carina-provider-awscc` (future work)
- Removing in-process provider support (Phase 3/4)
- Provider version constraints or ranges (exact version only for now)
- Provider registry or index (GitHub Releases only)

## Provider Resolution Flow

```
Parse .crn provider blocks
  │
  ├─ No source → in-process provider (unchanged, existing behavior)
  │
  ├─ source = "file://..." → local binary (Phase 2a, unchanged)
  │
  └─ source = "github.com/{owner}/{repo}" →
      ├─ Binary exists in .carina/providers/?
      │   ├─ YES + carina.lock exists → verify SHA256 → use if match, error if mismatch
      │   ├─ YES + no carina.lock → use as-is (no verification)
      │   └─ NO → download from GitHub Releases → place in .carina/providers/ → update carina.lock
      └─ Spawn binary via ProcessProviderFactory
```

## Directory Layout

```
project/
  example.crn
  carina.lock
  .carina/
    providers/
      github.com/
        carina-rs/
          carina-provider-awscc/
            0.1.0/
              carina-provider-awscc    (binary)
```

The `.carina/` directory should be added to `.gitignore`. `carina.lock` should be committed to Git.

## Download URL Convention

```
https://github.com/{owner}/{repo}/releases/download/v{version}/{repo}-v{version}-{target}.tar.gz
```

Where `{target}` is the Rust target triple:
- `aarch64-apple-darwin` (macOS ARM)
- `x86_64-apple-darwin` (macOS Intel)
- `x86_64-unknown-linux-gnu` (Linux x86_64)
- `aarch64-unknown-linux-gnu` (Linux ARM)

The tar.gz contains the binary at the top level (no subdirectory).

## Lock File Format

`carina.lock` uses TOML:

```toml
[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
version = "0.1.0"
sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
```

Behavior:
- Generated/updated during provider resolution (init or auto-resolve)
- SHA256 is computed from the downloaded binary after extraction
- On subsequent runs, binary is verified against the recorded hash
- If hash mismatches, error with message to re-run `carina init`
- Committed to Git for reproducibility across team members

## `carina init` Command

```
carina init [path]
```

- `path` defaults to current directory
- Scans for `.crn` files to find provider declarations
- For each provider with `source = "github.com/..."`:
  - Detects current platform target triple
  - Downloads `{repo}-v{version}-{target}.tar.gz` from GitHub Releases
  - Extracts binary to `.carina/providers/{source}/{version}/`
  - Computes SHA256 and writes to `carina.lock`
- Prints progress for each provider

## CLI Changes (plan/apply)

Before provider loading in `wiring.rs`:
1. For each provider config with `source = "github.com/..."`:
   - Check `.carina/providers/` for the binary
   - If missing: run resolution (download + lock)
   - If present + lock exists: verify SHA256
2. Construct `file://` path from `.carina/providers/` location
3. Pass to existing `load_process_provider()` (Phase 2a code)

This means `wiring.rs` treats resolved GitHub sources as `file://` sources after resolution.

## Architecture

### New Module: `carina-cli/src/provider_resolver.rs`

Single module responsible for:
- Parsing `source`/`version` from provider configs
- Target triple detection (`std::env::consts::ARCH` + `std::env::consts::OS`)
- Download URL construction
- HTTP download (using `reqwest` or `ureq`)
- tar.gz extraction
- SHA256 computation
- Lock file read/write
- Cache path resolution

### New Command: `carina-cli/src/commands/init.rs`

Thin command handler that calls `provider_resolver::resolve_all()`.

### Modified: `carina-cli/src/wiring.rs`

Before provider loading, call `provider_resolver::ensure_resolved()` for each provider with a GitHub source. This returns the resolved binary path.

## HTTP Client Choice

Use `ureq` (synchronous, minimal dependencies) rather than `reqwest` (async, heavy). The download happens once during init, synchronous is fine, and `ureq` keeps the dependency tree small.

## Error Handling

- Network errors: clear message with URL that failed
- 404 (version/platform not found): suggest checking available releases
- SHA256 mismatch: error with "binary may have been tampered with, re-run `carina init`"
- Missing `version` in provider block: error "version is required when source is specified"
- Unsupported platform: error listing supported targets

## Test Strategy

- Unit tests: URL construction, target triple detection, lock file serialization/deserialization, cache path resolution
- Integration test with `file://` source (no real HTTP download)
- Lock file round-trip test (write → read → verify)
- Real GitHub download is NOT tested in CI (requires network + published release)
