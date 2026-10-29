---
title: init
---

Resolve and download the provider plugins declared in `providers.crn`. Carina caches WASM plugin binaries locally and writes a lock file so subsequent runs use the same versions.

`init` also owns the backend lock (`carina-backend.lock`). It records which backend the directory was last initialized against, and is the single command that migrates state when the configured backend changes.

## Usage

```bash
carina init [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory containing one or more `.crn` files.

## Provider Verification

Every installed or cached provider artifact is checked against the SHA-256 recorded in `carina-providers.lock`, and `validate`, `plan` and `apply` check the installed copy again before loading it. If a cached copy does not match, `init` stops without changing the lock file, removes the mismatched copy, and asks you to run `carina init` again to download a verified one. If a freshly downloaded artifact does not match the pin, the origin changed the artifact after it was pinned; confirm the change is legitimate before removing that provider's entry from the lock file and running `carina init` again. A cached artifact that has no lock entry is never trusted; `init` downloads it from the origin instead.

For official `carina-rs` providers, `init` also records the verified artifact for the current user under `~/.carina/trust/` (override with `CARINA_TRUST_RECORD_DIR`). These providers fail to load until the user running Carina has run `carina init`. See [Installation](/getting-started/installation/) for details.

Concurrent `carina init` runs in the same project are serialized: a second run waits and prints `Waiting for provider install lock at <path> (held by another carina process)...`.

## Flags

### `--upgrade`

Re-resolve all provider versions from the constraints in `providers.crn`, ignoring any pinned versions in the existing lock file. Use when you want to pull in newer provider releases that satisfy your constraints.

### `--locked`

Require the lock file to match `providers.crn` exactly. Errors if any declared provider is missing from the lock file. Intended for CI, similar to `cargo --locked`.

It also errors on `github.com/...` lock entries written by older Carina versions (a `version` entry without its release tag, or a `revision` entry without its artifact ID); run `carina init` once without `--locked` to rewrite them.

`--upgrade` and `--locked` are mutually exclusive.

### `--migrate-state`

Migrate the state file when the configured backend differs from the one
recorded in `carina-backend.lock`. This covers backend changes such as
moving from local state to a remote backend, or changing the
`backend s3 { key = ... }` value during a directory refactor.

Without this flag, a backend change is a warning, not a hard error:
`carina init` still resolves provider plugins and exits successfully, but
it leaves `carina-backend.lock` unchanged and points you at
`--migrate-state`. Project-backed readers (`plan`, `state list`, `state show`,
`state lookup`, `export`, and `upstream_state` consumers) also warn and read
state from the old backend recorded in the lock. This keeps inspection and
PR-time plans accurate across backend-key changes. Explicit `--state-url`
reads bypass project backend selection. `apply`, `destroy`, and `state
refresh` refuse until state migration is explicit.

With the flag, `init` reads the state from the locked (old) backend,
writes it to the configured backend, and verifies the copy. It then
rewrites `carina-backend.lock` to the new address — this is the commit
point. The old source is only touched *after* the lock is rewritten, so
an interrupted migration is always recoverable: a crash before the
commit leaves the lock and the untouched source both describing the old
backend (a re-run retries the whole migration), and a crash after the
commit leaves the lock describing the new, verified state (a re-run is a
no-op).

- **local source** (local → remote, or a local → local path change such
  as a directory rename): the old local state file is deleted after a
  verified copy.
- **remote source** (remote → remote): the old object is **kept** as a
  recoverable backup; remove it manually once you have confirmed the new
  backend.

### `--force`

When migrating, overwrite a target backend that already contains a
*different* state (different lineage, or any resources). Without
`--force`, a populated target aborts the migration loudly so a
mistargeted backend cannot silently clobber live state. Requires
`--migrate-state`; passing `--force` alone is a usage error.

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

Migrate state after changing the backend (e.g. a directory refactor that
changes the `backend s3 { key }`):

```bash
carina init --migrate-state
```

Migrate and replace a stale state already present at the new backend:

```bash
carina init --migrate-state --force
```
