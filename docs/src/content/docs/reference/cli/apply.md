---
title: apply
---

Apply changes to reach the desired infrastructure state. Carina generates a plan, displays it for review, and then executes the effects after confirmation.

## Usage

```bash
carina apply [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). It must be either a directory containing one or more `.crn` files, or a saved plan JSON file (any path ending in `.json` is treated as a saved plan).

## Flags

### `--auto-approve`

Skip the interactive confirmation prompt and apply changes immediately. Use with caution.

```bash
carina apply --auto-approve
```

### `--lock <BOOL>`

Enable or disable state locking during apply. Defaults to `true`.

When locking is enabled, Carina acquires a lock on the state backend before applying changes. This prevents concurrent modifications from corrupting state.

```bash
carina apply --lock=false
```

## Applying a Saved Plan

You can apply a previously saved plan file (created with `carina plan --out`):

```bash
carina plan --out plan.json
carina apply plan.json
```

When applying a saved plan, Carina checks the state lineage and serial number against the current state to detect drift since the plan was created.

## Confirmation Flow

When `--auto-approve` is not set, Carina follows this flow:

1. Refreshes state from the cloud provider for all managed resources
2. Computes and displays the execution plan
3. If no changes are needed, prints "No changes needed." and exits
4. Prompts: "Do you want to perform these actions?"
5. Waits for the user to type `yes` to confirm (any other input cancels)
6. Executes the effects concurrently (up to 5 at a time) with progress spinners
7. Saves the updated state to the backend
8. Prints a summary of results

## Error Handling

- If an effect fails, Carina refreshes the resource state from the provider to capture any partial changes
- The state file is always saved after execution, even if some effects failed, to prevent state drift
- Failed and skipped effects are reported in the summary (e.g., "3 succeeded, 1 failed, 1 skipped")
- Exit code `1` indicates an error occurred

### State Locking Errors

If the state is already locked by another process, Carina displays the lock holder and lock ID, and suggests using `carina force-unlock` if the lock is stale.

## Bootstrap

When an S3 backend is configured but the bucket does not yet exist, `carina apply` automatically bootstraps the state bucket before applying other changes. The bucket resource can be defined in the `.crn` configuration, or Carina will auto-create it if `auto_create` is enabled (the default).

## Examples

Apply from the current directory:

```bash
carina apply
```

Apply with auto-approval (for CI):

```bash
carina apply --auto-approve
```

Apply a saved plan:

```bash
carina apply plan.json
```
