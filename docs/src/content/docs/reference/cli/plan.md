---
title: plan
---

Show an execution plan without applying changes. The plan compares the desired state defined in `.crn` files against the current infrastructure state and displays what actions Carina would take.

## Usage

```bash
carina plan [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). It can be a `.crn` file or a directory containing `.crn` files.

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

```bash
carina plan --out plan.json
carina apply plan.json
```

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

## Examples

Plan from the current directory:

```bash
carina plan
```

Plan a specific file with compact output:

```bash
carina plan --detail=none infra.crn
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
