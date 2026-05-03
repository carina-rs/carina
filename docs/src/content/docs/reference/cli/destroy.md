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

Refresh state from the cloud provider before computing the destroy plan. Defaults to `true`.

### `--force`

Destroy even resources that have `prevent_destroy = true` in their lifecycle block.

### `--reconfigure`

Accept a changed backend configuration and overwrite the local backend lock. Use when the backend block in your configuration has changed since the last apply.

## Examples

Destroy everything in the current directory after confirmation:

```bash
carina destroy
```

Destroy without prompting (e.g. in a CI teardown step):

```bash
carina destroy --auto-approve
```

Destroy a specific directory and bypass `prevent_destroy`:

```bash
carina destroy --force path/to/config
```
