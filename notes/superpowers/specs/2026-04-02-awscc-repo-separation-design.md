# Separate carina-provider-awscc into Standalone Repository

**Date**: 2026-04-02
**Status**: Draft
**Issue**: #1450

## Overview

Move `carina-provider-awscc` from the carina monorepo into `carina-rs/carina-provider-awscc` as a standalone repository with its own CI and release workflow.

## Goals

- `carina-provider-awscc` has its own repo, CI, and GitHub Releases
- `carina init` can download provider binaries from GitHub Releases
- Independent versioning and release cycle
- `carina` monorepo no longer contains awscc provider code

## Approach

Clean start — copy current code to new repo as initial commit. History remains in carina monorepo.

## New Repository Structure

```
carina-rs/carina-provider-awscc/
  Cargo.toml
  src/
    lib.rs
    main.rs
    bin/codegen.rs
    provider/
    schemas/
  cfn-schema-cache/
  acceptance-tests/
  scripts/
  .github/workflows/
    ci.yml
    release.yml
  CLAUDE.md
```

## Dependencies

Git dependencies pointing to carina monorepo:

```toml
[dependencies]
carina-core = { git = "https://github.com/carina-rs/carina" }
carina-aws-types = { git = "https://github.com/carina-rs/carina" }
carina-plugin-host = { git = "https://github.com/carina-rs/carina" }
carina-plugin-sdk = { git = "https://github.com/carina-rs/carina" }
carina-provider-protocol = { git = "https://github.com/carina-rs/carina" }
```

## CI Workflow (ci.yml)

Triggers: push to main, pull requests

Jobs:
- `cargo build`
- `cargo test`
- `cargo clippy`
- `cargo fmt --check`

Needs: cfn-schema-cache download or cache in CI

## Release Workflow (release.yml)

Triggers: tag push (`v*`)

Builds 4 platform binaries:
- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`

Asset naming: `carina-provider-awscc-v{version}-{target}.tar.gz`

Each tar.gz contains the binary at the top level.

## Carina Monorepo Changes

1. Delete `carina-provider-awscc/` directory
2. Remove from `Cargo.toml` workspace members
3. Move awscc-specific codegen (`carina-codegen-aws/` awscc parts) to new repo
4. Move `acceptance-tests/` awscc tests to new repo
5. Update any references in scripts, docs

## Version

Initial release: `v0.1.0`. Independent versioning thereafter.

## Acceptance Criteria

- `carina init` with `source = "github.com/carina-rs/carina-provider-awscc"` and `version = "0.1.0"` downloads and installs the binary
- `carina plan` / `carina apply` work with the downloaded binary
- CI passes in new repo
- All awscc acceptance tests pass in new repo
