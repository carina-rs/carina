---
title: plan
---

Show an execution plan without applying changes. The plan compares the desired state defined in `.crn` files against the current infrastructure state and displays what actions Carina would take.

## Usage

```bash
carina plan [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory containing one or more `.crn` files — single-file paths are rejected.

## Flags

### `--detail <LEVEL>`

Controls how much detail is shown in plan output.

| Value | Description |
|-------|-------------|
| `full` (default) | Show all attributes: user-specified, defaults, read-only, and unchanged (dimmed) |
| `explicit` | Show only attributes explicitly specified in the `.crn` file |
| `none` | Show resource names only (no attributes) |

### `--tui`

Display the plan in an interactive TUI (terminal user interface) mode. Allows navigating resources and viewing attribute details in a structured layout.

### `--out <FILE>`

Save the plan to a JSON file for later use with `carina apply`. The saved plan includes the effects, resource definitions, provider configurations, and state metadata for drift detection.

Saved plans never contain secret plaintext. Carina stores each secret as a
typed placeholder containing only an Argon2id hash, including secrets nested
inside lists or maps and values derived from `decrypt()` or other expressions.
New placeholders use a self-contained PHC string with a randomly generated
salt and the Argon2id version and cost parameters used to create it. When an
unchanged secret already exists in state, the plan reuses that exact digest
and therefore keeps its salt. A pre-v12 digest may instead appear in the
self-contained `argon2-legacy` form until the next write that has the
plaintext available. Equal newly written secrets receive unrelated hashes,
and a resource move does not change how a stored digest is verified. State
writeback stores the same digest carried by the plan placeholder. The
placeholder is not a provider value and cannot be applied directly. Backend
attributes cannot be secret-derived because backend implementations accept only
ordinary configuration values; validation rejects `secret()` and `decrypt()`
results there.

```bash
carina plan --out plan.json
carina apply plan.json
```

If the configured backend differs from `carina-backend.lock`, `plan`
prints a backend-drift warning, reads state from the locked old backend,
and includes a migration-pending note before the plan summary. A saved
plan produced in this state records the locked backend that supplied the
state; run `carina init --migrate-state` before applying.

### `--detailed-exitcode`

Change the exit code behavior:

| Exit code | Meaning |
|-----------|---------|
| `0` | No changes needed |
| `1` | Error occurred |
| `2` | Changes are present (only with `--detailed-exitcode`) |

This is useful in CI pipelines to detect whether changes exist without parsing output.

### `--refresh <BOOL>`

Refresh state from the cloud provider before planning. Defaults to `true`.

When set to `false`, Carina uses cached state and prints a warning. The plan may not reflect actual infrastructure.

```bash
carina plan --refresh=false
```

### `--json`

Output the plan as structured JSON instead of human-readable text. The output uses the same format as `--out` (PlanFile), including effects, resource definitions, and current states. Secrets are redacted.

```bash
carina plan --json
```

### `--check-iam`

Before showing the plan, check whether the AWS caller identity (the role that will run `apply`) has the IAM actions the providers declare for the planned effects. Carina uses `iam:SimulatePrincipalPolicy`, and falls back to reading the role's policies (`iam:GetRolePolicy`, `iam:ListAttachedRolePolicies`) when simulation is denied. Missing actions are reported per effect as warnings; the plan does not fail. If the caller identity or its policies cannot be read, the check is skipped with a warning.

### `--strict-iam`

With `--check-iam`, exit with code `1` instead of warning when permissions are missing. Requires `--check-iam`.

```bash
carina plan --check-iam --strict-iam
```

## State Locking

`plan` does not acquire the state lock, so it never blocks or is blocked by other runs. Because a concurrent `apply` or `destroy` can change state while the plan is computed, `plan` re-reads state just before display. If the state's serial or lineage changed (or the state was removed), it prints a warning to stderr that the plan may be stale and still shows the plan. `--out` still writes the plan file; applying a stale saved plan is caught by the lineage/serial check in `carina apply`.

## Examples

Plan from the current directory:

```bash
carina plan
```

Plan a specific directory with compact output:

```bash
carina plan --detail=none path/to/config
```

Save a plan for later apply:

```bash
carina plan --out plan.json
```

Use in CI to detect drift:

```bash
carina plan --detailed-exitcode
```

## Output Format

The plan output shows each resource with its effect type:

- **Create** (`+`) -- new resource to be created
- **Update** (`~`) -- existing resource with attribute changes
- **Delete** (`-`) -- resource to be removed
- **Replace** (`+/-`) -- resource that must be destroyed and recreated

A summary line shows the total count of each effect type.
