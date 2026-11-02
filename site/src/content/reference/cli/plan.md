---
title: plan
---

Show an execution plan without applying changes. The plan compares the desired state defined in `.crn` files against the current infrastructure state and displays what actions Carina would take.

## Usage

```bash
carina plan [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory containing one or more `.crn` files — single-file paths are rejected.

KMS references may require `kms:DescribeKey` during planning, independently of
`--check-iam`. See [KMS reference planning permissions](/guides/aws-credentials/#kms-reference-planning-permissions), including key-policy authorization and first-use initialization of AWS-managed keys.

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

Save the plan to a JSON file for later use with `carina apply`. The saved plan includes the effects, resource definitions, provider configurations, state metadata for drift detection, and the identity (SHA-256, source and version) and schema fingerprints of the providers it was computed with. Re-run `carina plan --out` after re-installing or upgrading a provider: `carina apply` rejects a saved plan whose providers no longer match (see [Applying a Saved Plan](/reference/cli/apply/#applying-a-saved-plan)).

Saved-plan format 39 supersedes format 38, retains its lifecycle directive
values and per-element loop expressions, and records complete resolved
dependency addresses and the legacy dependency migration classification used
by planning. Applying it replays that classification against locked state
before writing state v21. Older saved plans are rejected; regenerate them with
`carina plan --out`.

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

Refresh managed-resource state from the cloud provider before planning. Defaults to `true`.

When set to `false`, Carina uses cached state for managed resources and prints a warning. Data sources are queries rather than managed objects, so Carina always reads them regardless of this flag. The plan may not reflect the actual managed infrastructure.

```bash
carina plan --refresh=false
```

### `--json`

Output the plan as structured JSON instead of human-readable text. The output uses the same format as `--out` (PlanFile), including effects, resource definitions, and current states. Secrets are redacted.

The top-level `persisted_directive_changes` array records directive-only state
updates. It includes a desired resource only when that resource has no
mutating execution-plan effect; directives for resources being created,
updated, replaced, moved, or otherwise mutated are written by the normal
apply path and are not duplicated here. `resource_id` is the full routed
identity used for matching. Field names are limited to `force_delete`,
`create_before_destroy`, `prevent_destroy`, `depends_on`, and
`provider_instance`. Boolean fields use JSON booleans, `depends_on` uses an
array, `provider_instance` uses a string or `null`, and `prevent_destroy` uses
its tagged tri-state object:

```json
{
  "persisted_directive_changes": [
    {
      "resource_id": {
        "provider": "mock",
        "resource_type": "test.default_prevent_destroy",
        "identity": "account"
      },
      "fields": [
        {
          "name": "prevent_destroy",
          "from": { "kind": "unspecified" },
          "to": { "kind": "explicit", "value": false }
        }
      ]
    }
  ]
}
```

The array has been required since saved-plan format 35, even when it is empty.
Applying a saved plan recomputes it against locked state and refuses the plan
if the typed IDs or values differ.

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
- Dashed edges mean "references" and impose no ordering. Unchanged resources
  appear only when they connect two changed resources. A module call with no
  changes is collapsed into a single card.
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

### `--image-inline`

Display the dependency graph directly in an image-capable terminal after the
normal text plan. Carina renders the graph as PNG and uses the Kitty graphics
protocol in Kitty and Ghostty, or the iTerm2 inline-image protocol in iTerm2
and WezTerm. In Kitty-compatible terminals, an image wider than the terminal
is scaled down to the available columns.

Any other terminal, including tmux, and output that is not a terminal are
rejected before planning starts; use `--image <FILE>` there. `--image-inline`
cannot be combined with `--json`, which keeps stdout as JSON, or `--tui`,
which owns the terminal. It can be combined with `--image <FILE>` to display
the graph and save it in one run.

```bash
carina plan --image-inline
```

### `--image-theme <THEME>`

Select the palette used by `--image` and `--image-inline`. The accepted values
are `light`, `dark`, and `auto`.

When this option is omitted, SVG and PNG files use the light palette, while
inline terminal images use `auto`. An explicit `light` or `dark` value applies
to every requested image output. For an SVG file, `auto` embeds both palettes
and switches them with `@media (prefers-color-scheme: dark)`. For inline
output, `auto` queries the terminal background and falls back to light if the
terminal does not reply. PNG colors are fixed at render time, so combining a
PNG file with explicit `--image-theme auto` is rejected before planning starts.

For a GitHub page or pull-request description, render separate light and dark
PNGs and select between them with `<picture>`:

```bash
carina plan --image-theme light --image plan-light.png
carina plan --image-theme dark --image plan-dark.png
```

```html
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="plan-dark.png">
  <img src="plan-light.png" alt="carina plan">
</picture>
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

When a destroy and create have matching configured create-only values, `plan`
also warns on stderr about a possible state move; see
[Moving resources in state](/guides/state-management/#moving-resources-in-state).

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
- **Release** (`!`) -- an omitted key is no longer authored by the configuration,
  but the remote value is left unchanged because the API does not reset omitted keys

A summary line shows the total count of each effect type. Releases have their own
`to release` count; a plan containing only releases is not reported as "No changes."

## Keyed partial-update list releases

Some provider list attributes are maps in list form: every element has a key,
and an update changes only the keys sent to the API. Omitting an element does
not delete or reset its remote value. When a provider declares this behavior,
Carina shows an authored key removed from configuration as a release instead of
as a misleading deletion:

```text
! target_group_attributes["stickiness.enabled"]: left as 'true' (AWS does not revert omitted keys; write the default value to revert)
```

Applying a release makes no provider API call for that key. It only stops
treating the element as authored in Carina state, so later plans hide it in the
same way as a service-added default. To reset the remote setting, keep the
element in configuration and write the API's default value explicitly.
