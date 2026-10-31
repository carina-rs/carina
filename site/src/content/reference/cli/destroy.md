---
title: destroy
---

Destroy all resources defined in the configuration. Carina computes a destroy plan and, after confirmation, deletes every managed resource recorded in state for the given directory.

## Usage

```bash
carina destroy [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory containing one or more `.crn` files.

## Flags

### `--auto-approve`

Skip the interactive confirmation prompt and proceed with the destroy immediately.

### `--lock <BOOL>`

Enable or disable state locking during the destroy. Defaults to `true`. Disable only when you know no other process is touching the same state.

### `--refresh <BOOL>`

Refresh managed-resource state from the cloud provider before computing the destroy plan. Defaults to `true`. When set to `false`, Carina uses cached managed-resource state. Destroy does not evaluate data sources because they do not contribute managed objects to the destroy set.

### `--force`

Destroy even resources protected by an explicit `prevent_destroy = true` or by
their provider schema's destruction-protection default.

### `--parallelism <N>`

Maximum number of provider operations run concurrently. Defaults to `8`.

## Progress and Errors

Every provider must load before destroy starts. Progress is shown the same way as for `carina apply`: spinners on a terminal, and plain lines with a periodic `… Still running` line otherwise (see [Progress output](/reference/cli/apply/#progress-output)).

## Backend Drift

Before destroying resources, Carina checks the current project backend
against `carina-backend.lock`. If the backend changed, destroy refuses
and points at `carina init --migrate-state`; this prevents silently
reading or writing state from the new backend before migration is
explicit.

## Examples

Destroy everything in the current directory after confirmation:

```bash
carina destroy
```

Destroy without prompting (e.g. in a CI teardown step):

```bash
carina destroy --auto-approve
```

Destroy a specific directory and bypass destruction protection:

```bash
carina destroy --force path/to/config
```
