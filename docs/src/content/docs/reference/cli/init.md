---
title: init
---

Resolve and download the provider plugins declared in `providers.crn`. Carina caches WASM plugin binaries locally and writes a lock file so subsequent runs use the same versions.

## Usage

```bash
carina init [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory containing one or more `.crn` files.

## Flags

### `--upgrade`

Re-resolve all provider versions from the constraints in `providers.crn`, ignoring any pinned versions in the existing lock file. Use when you want to pull in newer provider releases that satisfy your constraints.

### `--locked`

Require the lock file to match `providers.crn` exactly. Errors if any declared provider is missing from the lock file. Intended for CI, similar to `cargo --locked`.

`--upgrade` and `--locked` are mutually exclusive.

## Examples

Download providers for the current directory:

```bash
carina init
```

Upgrade all providers to the latest versions allowed by the constraints:

```bash
carina init --upgrade
```

Verify the lock file is up to date in CI:

```bash
carina init --locked
```
