---
title: apply
---

Apply changes to reach the desired infrastructure state. Carina generates a plan, displays it for review, and then executes the effects after confirmation.

## Usage

```bash
carina apply [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). It must be either a directory containing one or more `.crn` files, or a saved plan file written by `carina plan --out` (any path that is not a directory is treated as a saved plan).

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

### `--parallelism <N>`

Maximum number of provider operations run concurrently. Defaults to `8`.

```bash
carina apply --parallelism=2
```

### `--accept-legacy-name-overrides`

Accept a state file that still uses the pre-Phase-5 name override format. Without this flag, `carina apply` refuses to run against a state file that has any `name_overrides` entry missing the recorded original DSL value, so the operator reviews the drift first. Run `carina plan` to inspect, then re-run `apply` with this flag.

## Applying a Saved Plan

You can apply a previously saved plan file (created with `carina plan --out`):

```bash
carina plan --out plan.json
carina apply plan.json
```

When applying a saved plan, Carina checks the state lineage and serial number against the current state to detect drift since the plan was created.

A saved plan also records the providers it was computed with: for each provider, the SHA-256 of its WebAssembly component with its locked source and version (or `file` for a `file://` source), and a fingerprint of the schema of every resource type the plan touches. `carina apply <plan>` loads the providers named in the plan and compares both before it reads any resource or runs any effect. If a provider was re-installed or rebuilt since the plan was made (for example after `carina init --upgrade`, with a different lock file, or after rebuilding a `file://` component), or a resource type's schema changed, apply stops with `Provider artifact mismatch` or `Provider schema mismatch` and shows the `carina plan --out` command to re-run.

Saved plans are tied to the plan-file format of the Carina version that wrote them. A plan written by another version is rejected with `Unsupported plan file version`; create it again with `carina plan --out`.

When a saved plan contains secret placeholders, Carina also re-reads and
evaluates the configuration from the source directory recorded in the plan.
New saved secret hashes are PHC strings that embed their random salt and
Argon2id parameters; an unchanged secret may reuse the exact digest and salt
already in state. Upgraded pre-v12 values use the self-contained
`argon2-legacy` form until a plaintext write replaces them with PHC. Carina
verifies the re-evaluated plaintext against the digest, including for resource
attributes, provider configuration, and deferred templates. The provider then
receives the rehydrated plaintext, while state stores the exact digest carried
by the plan placeholder. If a secret changed or can no longer be found, apply stops
before any planned provider mutation or state write and asks you to run
`carina plan --out` again. A secret that came through a module call is matched
to that call by its position in the source, not by its name. Apply therefore also
stops if such a call has moved since the plan was made, for example when a line
was added above it or its file was renamed. The error names the call's file and
line. Saved-plan verification may perform additional
provider reads, including reads for resources scheduled for deletion. A saved
plan without secret placeholders does not revalidate unrelated source
configuration.

Before applying either a live configuration or a saved plan, Carina also
checks the current project backend against `carina-backend.lock`. If the
backend changed, apply refuses and points at
`carina init --migrate-state`; this prevents silently writing state to
the new backend before migration is explicit. Saved-plan apply re-loads
the project's current `.crn` files from the plan's recorded source path
before running the same gate.
Carina also rejects a saved plan whose backend configuration differs
from the project's current `backend.crn`; re-run `carina plan` to
produce a fresh saved plan.

## Confirmation Flow

When `--auto-approve` is not set, Carina follows this flow:

1. Refreshes state from the cloud provider for all managed resources and reads all data sources
2. Computes and displays the execution plan
3. If no changes are needed, prints "No changes needed." and exits
4. Prompts: "Do you want to perform these actions?"
5. Waits for the user to type `yes` to confirm (any other input cancels)
6. Executes the effects concurrently (up to `--parallelism` at a time, default 8) with progress output (see [Progress output](#progress-output)), saving state to the backend as effects complete
7. Saves the final state to the backend
8. Prints a summary of results

When `carina apply` creates a plan directly from a configuration directory, data sources are always read. Likewise, `carina plan --refresh=false` skips only managed-resource refresh before writing a saved plan; it still reads every data source.

## Progress Output

When stderr is a terminal, Carina shows a spinner for each running effect. Otherwise (for example in CI), it prints plain lines without colors or cursor movement. In plain mode, an effect that is still running after 30 seconds without printing anything gets a line such as `… Still running: Create aws.acm.Certificate cert [1m 30s elapsed]`, repeated every 30 seconds, so a long create does not look stalled. An effect that reports its own wait progress gets this line only after 60 seconds of silence. The same applies to `carina destroy` and `carina state refresh`.

## Error Handling

- If an effect fails, Carina refreshes the resource state from the provider to capture any partial changes
- If a create fails after the provider reported the new resource's identifier, the identifier is still recorded in state. When Carina could read the resource back, the note says `resource created; recorded in state, the next apply completes it`. When it could not, the note says the provider reported the identifier but the resource could not be observed, with the reason; the next `carina plan` checks it again
- State is written incrementally while effects run, so results of completed effects survive even if the process is killed before the final save
- The state file is always saved after execution, even if some effects failed, to prevent state drift
- Failed, partial, skipped and interrupted effects are reported in the summary (e.g., "3 succeeded, 1 failed, 1 skipped, 1 outcome unknown"). An effect that was still running when apply stopped is shown as `? <effect> - interrupted (outcome unknown)`; run `carina plan` to see what it left behind
- Exit codes: `0` on success; `1` on an error or when any effect failed, was skipped or was interrupted; `2` when the only problem is creates that completed partially; `130` when cancelled with Ctrl+C

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
