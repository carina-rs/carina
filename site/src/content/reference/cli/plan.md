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

### `--image <FILE>`

Write the plan as a dependency-graph image, in addition to the normal text,
`--json`, or `--tui` output. The format is chosen by the file extension:
`.svg` or `.png` (case-insensitive). Any other extension is rejected before
planning starts.

The image is meant for reviewing a plan where the text output is hard to scan,
such as a pull request:

- Each changed resource is a card colored by its effect (create, update,
  replace, delete, and so on). Update cards list the names of the changed
  attributes; replace cards list the attributes that force the replacement.
- Cards are laid out left to right in apply order. Cards in the same column
  have no ordering dependency on each other and can run in parallel. Solid
  arrows mean "runs before".
- Resources downstream of a replace or delete are highlighted with a halo,
  so the blast radius of a destructive change is visible at a glance.
- Unchanged resources appear only when they connect two changed resources.
  Their edges are dashed and mean "references", not ordering. A module call
  with no changes is collapsed into a single card.
- Resources inside a module call are grouped in a box labelled with the call.
- The header shows per-effect counts, with replace and destroy counts in bold,
  and the state lineage and serial the plan was computed against.

The image never contains attribute values, so it can be shared without
exposing secrets.

The image is written last, after everything else in the command has
succeeded, so a failed `plan` never leaves a fresh image behind. The image is
rendered before the `--out` file is saved: if rendering fails, the command
exits with code `1` and writes neither file. Only an I/O error while writing
the image file itself can happen after `--out` has been saved. PNG output is
limited to 64 megapixels; for very large plans, use `.svg` instead.

`--image` does not change exit codes: with `--detailed-exitcode`, the exit
code is the same as without `--image`.

PNG output uses an embedded font, so it renders identically on every machine,
including CI runners without any fonts installed.

```bash
carina plan --image plan.png
```

In GitHub Actions, keep exit code `2` (changes present) from failing the job,
fail it on a real error (`1`), and post the image to the pull request with
`gh pr comment --attach` (GitHub CLI 2.99.0 or later):

```yaml
- id: plan
  run: |
    set +e
    carina plan --detailed-exitcode --image plan.png
    code=$?
    echo "exitcode=$code" >> "$GITHUB_OUTPUT"
    [ "$code" -ne 1 ]
- if: steps.plan.outputs.exitcode == '2'
  env:
    GH_TOKEN: ${{ secrets.PLAN_COMMENT_TOKEN }}
  run: gh pr comment "${{ github.event.pull_request.number }}" --attach 'plan.png#carina plan'
```

The workflow's own `GITHUB_TOKEN` cannot upload attachments: the upload
endpoint answers `404` to it even with `contents`, `issues`, and
`pull-requests` write permissions. Store a classic personal access token with
the `repo` scope, from a user with write access to the repository, as a secret
(`PLAN_COMMENT_TOKEN` above).

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
