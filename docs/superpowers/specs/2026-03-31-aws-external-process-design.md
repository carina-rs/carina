# Phase 3: Convert aws Provider to External Process Binary

**Date**: 2026-03-31
**Status**: Draft
**Depends on**: Phase 2a (awscc external process) — merged in PR #1421

## Overview

Convert `carina-provider-aws` into a dual lib+binary crate, following the same pattern established in Phase 2a for awscc. The binary implements `CarinaProvider` from `carina-plugin-sdk` and runs as an external process.

## Goals

- `carina-provider-aws` works as an external process binary via `file://` or `github.com/` source
- Full plan/apply cycle works through the external process
- Existing in-process behavior is preserved

## Non-Goals

- Removing in-process provider support from carina-cli (Phase 4)
- Deprecating the aws provider in favor of awscc

## Architecture

Identical to Phase 2a (awscc). Add `main.rs` to existing crate:

```
carina-provider-aws/
  src/
    lib.rs          — existing library code (unchanged)
    main.rs         — NEW: CarinaProvider wrapper + carina_plugin_sdk::run()
```

### AwsProcessProvider

Wraps existing `AwsProvider` and `AwsNormalizer`:

- `info()` → name: "aws", display_name: "AWS provider"
- `schemas()` → convert existing `all_schemas()` via `core_to_proto_schema`
- `validate_config()` → validate region
- `initialize()` → create `AwsProvider::new(region)` via tokio `block_on()`
- `read/create/update/delete` → delegate to `AwsProvider` via `block_on()`
- `normalize_desired()` → delegate to `AwsNormalizer`
- `merge_default_tags()` → delegate to `AwsNormalizer`
- `normalize_state()` / `hydrate_read_state()` → default no-op (aws provider doesn't implement these)

### Differences from awscc

- aws uses individual SDK clients (s3, ec2, iam, sts, cloudwatchlogs) instead of CloudControl
- aws has no `hydrate_read_state` or `normalize_state` (simpler normalizer)
- aws `AwsProvider::new()` takes a region string and creates 5 SDK clients

## Test Strategy

- Smoke test: `echo JSON-RPC | cargo run -p carina-provider-aws --bin carina-provider-aws` returns correct provider_info
- Full workspace `cargo test` passes unchanged
