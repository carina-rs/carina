# Phase 4: Remove Direct Provider Dependencies from carina-cli

**Date**: 2026-03-31
**Status**: Draft
**Depends on**: Phase 3 (aws external process) — merged in PR #1423

## Overview

Remove `carina-provider-aws` and `carina-provider-awscc` as direct dependencies of `carina-cli`. All providers (except mock) are loaded exclusively as external processes. The `source` and `version` attributes become required in provider blocks for aws/awscc.

## Goals

- `carina-cli` no longer depends on `carina-provider-aws` or `carina-provider-awscc`
- Significantly reduced binary size and build time for `carina-cli`
- All provider loading goes through `provider_resolver` + `ProcessProviderFactory`
- Clear error message when `source`/`version` is missing

## Non-Goals

- Removing `carina-provider-aws` / `carina-provider-awscc` crates from the workspace (they remain as standalone binaries)
- Changing the mock provider (stays in-process for testing)

## Breaking Change

Provider blocks for aws/awscc now require `source` and `version`:

```crn
# Before (no longer works)
provider awscc {
  region = awscc.Region.ap_northeast_1
}

# After (required)
provider awscc {
  source = "github.com/carina-rs/carina-provider-awscc"
  version = "0.1.0"
  region = awscc.Region.ap_northeast_1
}
```

## Changes

### carina-cli/Cargo.toml

Remove these dependencies:
- `carina-provider-aws`
- `carina-provider-awscc`
- `aws-config` (only used for provider factory registration)
- `aws-sdk-kms` (only used for state encryption, check if still needed)
- `aws-sdk-s3` (check if still needed for state backend)

Note: `aws-config`, `aws-sdk-kms`, and `aws-sdk-s3` may still be needed for the state backend (S3 backend for state storage, KMS for state encryption). Only remove them if they are exclusively used by provider factories.

### carina-cli/src/wiring.rs

1. Remove `use carina_provider_aws::AwsProviderFactory;` and `use carina_provider_awscc::AwsccProviderFactory;`
2. Remove `WiringContext` factory registration — the `factories` field and `new()` method that creates the vector of provider factories
3. Remove `provider_mod::find_factory()` call path in `get_provider_with_ctx` — the "hardcoded factory lookup" branch
4. For provider configs without `source`: error with message "Provider '{name}' requires 'source' and 'version' attributes. Example: source = \"github.com/carina-rs/carina-provider-{name}\""
5. Keep `MockProvider` fallback when no providers are configured (empty router)

### carina-cli/src/commands/

Update any commands that directly reference `WiringContext::new()` factories. The `WiringContext` may be simplified or removed if its only purpose was holding factories.

### LSP Impact

The LSP (`carina-lsp`) currently gets schemas from the in-process provider factories. After this change, the LSP will need a different mechanism to get schemas for completion and diagnostics.

Options:
- LSP spawns provider binaries itself to get schemas (via `ProcessProviderFactory`)
- LSP caches schemas from a previous `carina init` or `carina plan` run
- LSP works without provider schemas (reduced completion, no attribute validation)

For Phase 4, the simplest approach: LSP continues to work with whatever schemas it can get. If a provider binary is available in `.carina/providers/`, the LSP can load schemas from it. If not, LSP provides reduced functionality (no provider-specific completions). This is acceptable since the LSP already degrades gracefully when schemas are unavailable.

### Example Files

Update all `.crn` files in the repository (examples, tests, fixtures) to include `source` and `version` in provider blocks. Files that use `provider awscc { ... }` or `provider aws { ... }` without `source` must be updated.

Exception: fixture files that test mock provider behavior (no `source` needed for mock).

## Test Strategy

- All existing tests must pass after updating fixture `.crn` files
- Verify `carina-cli` builds without aws/awscc provider crates
- Verify clear error message when `source` is missing
- Verify `carina plan` works with `source` + `version` via `file://` (local binary)
