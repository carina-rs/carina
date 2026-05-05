# Plan policy pretty-printing — design

<!-- derived-from ./2026-05-03-typeexpr-stage2-design.md#background -->

Issue: [#2409](https://github.com/carina-rs/carina/issues/2409) (Problem 1 only).
Problem 2 (resource origin / file provenance) is split out to a separate
Issue and not addressed here.

## Goal

Make `carina plan` output for IAM/S3 policy documents (and any
`Value::List` of `Value::Map`) readable. Today the entire `statement`
array renders as a single inline `[{...}, {...}]` line; reading even one
statement requires horizontal scrolling. After this change, list-of-map
attributes render with one entry per line under a `- ` prefix, and
long list-of-string attributes wrap onto multiple lines so no single
line exceeds 80 columns.

Update-time diff rendering of the same attributes is **out of scope** —
it has its own design surface (entry add/remove/modify coloring,
ordering of `+`/`-`/`~` markers in nested structures) and will be
tracked separately.

## Chosen approach

Add a new public function
`carina_core::value::format_value_pretty(value, indent_cols)` that
returns a multi-line, indentation-aware representation of a `Value`.
Existing `format_value` / `format_value_with_key` are unchanged — every
caller outside the Create-time plan display continues to get the
single-line representation.

Plan display (`carina-cli/src/display`) opts in to the new function
**only on the Create path** of `render_detail_row`. Update-time diff
rendering keeps its current single-line behavior.

### Why this shape

- **Blast radius is bounded.** `format_value` has callers in plan, export,
  diff display, error messages, snapshot tests. Modifying it in place
  would force every snapshot to be reviewed. A new function lets us
  expand only the surface the issue actually targets.
- **`carina-core::value` is the right home.** `format_value_pretty`
  recurses into `Value::Map`/`Value::List` and must understand
  `Value::ResourceRef`, `Value::Interpolation`, `Value::Secret`,
  `Value::Unknown`, and DSL enum string resolution. All of that logic
  lives in `value.rs` today; duplicating it inside `display/` is a
  maintenance liability (re-implementing 8 `Value` variants is the
  wrong reuse boundary).
- **Create-only is the smallest correct slice.** Issue #2409's
  reproduction is a `carina plan` of a fresh stack — entirely Create.
  Update-time pretty-printing has unresolved spec questions (how do
  `+`/`-` markers compose with `- ` list-of-map prefixes? where do
  removed entries appear?), and the issue body's "Desired" example
  doesn't cover them.

## Key design decisions

### D1. List-of-map → vertical, always

When a `Value::List` contains only `Value::Map` items
(`is_list_of_maps(...)` returns `true`), render one entry per line under
the parent key, each prefixed with `- `:

```
statement:
  - sid: "ManageDeployRoles"
    effect: "Allow"
    action: ["iam:CreateRole", "iam:DeleteRole"]
    resource: "arn:..."
  - sid: "ReadOIDCProvider"
    ...
```

No threshold — list-of-map is *always* expanded. Justification: a
list-of-map serialized inline is essentially never readable, regardless
of length.

### D2. List-of-string → vertical only when the line would exceed 80 columns

For `Value::List` whose items are not all maps, measure the would-be
single-line length (`current_indent + key + ": " + "[" + items_joined +
"]"`). If that length ≤ 80, render inline (`["a", "b", "c"]`).
Otherwise expand:

```
action: [
  "iam:CreateRole",
  "iam:DeleteRole",
  ...
]
```

Threshold is a **fixed compile-time constant** (`PRETTY_LINE_LIMIT =
80`), not terminal-width-derived. Reasons:

- Snapshot tests must be deterministic regardless of the developer's
  terminal.
- Plan output is read in CI logs, PR comments, and pipes — terminal
  width isn't authoritative.
- 80 is the conventional readability ceiling for code-adjacent text.

### D3. List-of-string format is bracketed (`[...]`), not YAML dashes

The expanded list-of-string uses bracket-wrapped, comma-separated lines:

```
action: [
  "iam:CreateRole",
  "iam:DeleteRole",
]
```

Not the YAML `- "iam:CreateRole"` form. This visually distinguishes
**list-of-map** (uses `-` prefix) from **list-of-string** (uses
brackets), so a reader can tell at a glance whether each child is a map
or a scalar. It also matches Issue #2409's "Desired" example verbatim.

### D4. Nested `Value::Map` inside list-of-map entries → vertical recursively

If a list-of-map entry contains another `Value::Map` value (e.g. a
condition block inside an IAM statement), that nested map renders
vertically too — same indent semantics, no special case:

```
- sid: "ConditionalAccess"
  condition:
    StringEquals:
      "aws:PrincipalOrgID": "o-..."
```

### D5. `format_value_pretty` is recursive, indent-driven

Signature (subject to refinement):

```rust
pub fn format_value_pretty(value: &Value, indent_cols: usize) -> String
```

- `indent_cols` is the column at which the *value* starts (i.e. parent
  has already written `<indent>key: ` and now hands off rendering of the
  value).
- Returns a `String` that may contain newlines. The caller is
  responsible for appending it after `key: ` — `format_value_pretty` does
  NOT prefix its first line with indentation (continuation lines do
  contain the appropriate indentation).
- Internal scalar formatting (DSL enum resolution, secret masking,
  string quoting, ResourceRef / Interpolation / FunctionCall / Unknown
  rendering) shares helpers with `format_value_with_key` to avoid
  drift.

### D6. Apply only on Create path

`render_detail_row` in `carina-cli/src/display/mod.rs` chooses between
`format_value` and `format_value_pretty` based on the `Effect` it is
rendering for. Currently:

- `Effect::Create { .. }` → use `format_value_pretty`
- `Effect::Update { .. }` / `Effect::Delete { .. }` / others →
  unchanged (single-line `format_value` and existing diff helpers)

This keeps Update diff display, list-diff coloring, and
`format_replace_changed_attrs` untouched.

## File structure / architecture

| File | Change |
| ---- | ------ |
| `carina-core/src/value.rs` | Add `format_value_pretty(value, indent_cols)`. Add internal helpers shared with `format_value_with_key` for scalar formatting. Add unit tests covering all `Value` variants, list-of-map, list-of-string above/below threshold, nested maps, secret masking, DSL enum resolution. |
| `carina-cli/src/display/mod.rs` | In `render_detail_row` (or its Create-path callers), branch on the `Effect` variant and call `format_value_pretty` for Create. Single change site; no API change to `format_value`. |
| `carina-cli/tests/fixtures/plan_display/policy_pretty/` | New fixture: `.crn` with an IAM policy resource (or any `Struct(StructList)` attribute) producing a list-of-map. Expected snapshot demonstrates D1. |
| `carina-cli/tests/fixtures/plan_display/pretty_long_string_list/` | New fixture: `.crn` whose Create plan has a list-of-string attribute longer than 80 columns. Snapshot demonstrates D2/D3 expanded form. |
| `carina-cli/tests/fixtures/plan_display/pretty_short_string_list/` | New fixture: `.crn` whose Create plan has a list-of-string attribute well under 80 columns. Snapshot asserts inline form (regression guard for D2 threshold). |
| `Makefile` | Add `plan-policy-pretty`, `plan-pretty-long-string-list`, `plan-pretty-short-string-list` targets per project rule "Add Makefile target for new fixtures". |
| `carina-cli/src/plan_snapshot_tests.rs` | (Auto-discovers fixtures via existing harness; no manual entry expected. Verify when implementing.) |

## Edge cases and constraints

### Empty containers

- `Value::List(vec![])` → `[]` (always inline; no threshold check).
- `Value::Map(empty)` → `{}` (always inline).

### Single-element list

- `Value::List([single_map])` → still vertical (D1 rule). One-line `- sid: ...` is acceptable; rule consistency wins over micro-compaction.
- `Value::List([single_short_string])` → inline if under threshold (D2).

### `Value::Unknown` / `Value::Secret`

- Render with their existing single-token form (`(known after apply: ...)`, `(secret)`). Never expand.
- Inside a list-of-map entry, they appear as the value of a key on a single line:
  ```
  - sid: "X"
    role_arn: (known after apply: <ref>)
  ```

### DSL enum strings

- `format_value_pretty` must call `is_dsl_enum_format` / `convert_enum_value` on `Value::String` exactly like `format_value_with_key` does. The unit tests cover this (e.g., `aws.s3.Bucket.VersioningStatus.enabled`).

### Secret leakage

- The existing `SECRET_PREFIX` short-circuit and `Value::Secret` masking behavior must be preserved verbatim; the unit tests assert this on a list-of-map entry that contains a secret value.

### Snapshot stability

- Threshold is fixed at 80; no terminal-width detection.
- `BTreeMap`-ordered or alphabetical ordering of map keys is preserved (the existing `format_value_with_key` sorts keys; `format_value_pretty` matches).
- TUI mode (`carina-tui`) is not in scope for this design — the change targets terminal text plan output. If TUI calls `format_value` it stays single-line; we can revisit in a follow-up if needed.

### Width of the threshold's "current_indent"

For D2's threshold check, "current indent" is the column at which the
key starts (so that `<indent>key: [items]` is what we measure). The
caller in `display/mod.rs` already knows this — it has a fixed indent
schedule for resource attributes.

### Out-of-scope (will not be addressed in this PR)

- Update-time diff pretty-printing for list-of-map / list-of-string. Tracked separately.
- TUI plan view changes.
- Resource origin / file provenance (Issue #2409 Problem 2). Tracked separately.
- Pretty-printing in `carina state show`, export display, error messages — those callers continue to use `format_value`.

## Risks

- **`format_value` and `format_value_pretty` drift.** Two formatters can
  diverge in scalar rendering (e.g., a future `Value` variant added with
  a special render in only one place). Mitigation: extract a private
  `format_scalar(value)` used by both. Unit tests cover every variant
  for both functions.
- **Fixture brittleness.** Adding a fixture with an IAM policy requires
  the right schema to be available without provider boot-up. Plan
  fixtures use the mock provider; we must verify the mock provider
  schema can express a list-of-map attribute. If it can't, fall back to
  a synthetic `Struct(StructList)` attribute on the mock schema. This
  is the only place the design depends on a runtime assumption to
  validate at implementation time.
- **80-column threshold off-by-one.** A list whose inline form is
  exactly 80 columns: render inline (rule is "exceeds 80", `> 80`).
  Document the boundary in the unit test.

## Acceptance

The Issue #2409 reproduction (`carina-rs/infra` branch
`issue-24-registry-dev-bootstrap`, `registry/dev/bootstrap/`) renders
the IAM policy `statement` array vertically with one entry per line,
and long `action` lists wrap. No horizontal scrolling needed to read a
single statement.
