# Plan policy pretty-printing — implementation plan

<!-- derived-from #2026-05-05-plan-policy-pretty-print-design -->

Issue: [#2409](https://github.com/carina-rs/carina/issues/2409) (Problem 1).
Design doc: [`2026-05-05-plan-policy-pretty-print-design.md`](./2026-05-05-plan-policy-pretty-print-design.md).

## Architecture summary

Two-layer change:

1. **`carina-core::value`**: add `format_value_pretty(value: &Value, indent_cols: usize) -> String`. Pure function, returns multi-line string. Internal scalar helper shared with existing `format_value_with_key` to avoid drift.
2. **`carina-core::detail_rows`** + **`carina-cli/src/display/mod.rs`**: introduce a new `DetailRow::PrettyAttribute { key: String, value: Value }` variant (carries the original `Value`, NOT a pre-stringified `String`). `build_create_rows` emits this variant for list-of-map attributes (and any other attribute we later opt into pretty-printing). The renderer in `display/mod.rs::render_detail_row` knows the actual `attr_prefix` (which is dynamic — depends on tree depth and whether the resource is a child via `│  ` continuation lines), so it can compute `indent_cols = attr_prefix.len() + key.len() + 2` and call `format_value_pretty(value, indent_cols)` at render time. This is necessary because `attr_prefix.len()` is **not known at `build_detail_rows` time** — only the renderer has it (verified: `display/mod.rs:630-639` builds `attr_prefix` dynamically from `base_indent`, `attr_base`, and `continuation`).

   `DetailRow::ListOfMaps` (rendered as `+ {key1: val1, ...}` inline) becomes dead code — `build_list_of_maps_row` is removed and `DetailRow::ListOfMaps` is no longer constructed. We keep the variant in the enum to avoid an enum-API ripple in TUI / consumers; Task 6 verifies no constructors remain.

This keeps the Update / diff paths untouched (they call `format_value_with_key`, which is unchanged), and the renderer gains exactly one new branch (`DetailRow::PrettyAttribute`).

## File map

| File | Type | Purpose |
| ---- | ---- | ------- |
| `carina-core/src/value.rs` | modify | Add `PRETTY_LINE_LIMIT`, `format_value_pretty`, internal `format_scalar` helper. Add unit tests. |
| `carina-core/src/detail_rows.rs` | modify | Add `DetailRow::PrettyAttribute { key: String, value: Value }`. In `build_create_rows`, emit this variant for list-of-map attributes. Remove dead `build_list_of_maps_row`. |
| `carina-cli/src/display/mod.rs` | modify | Add a `DetailRow::PrettyAttribute` arm to `render_detail_row` that calls `format_value_pretty(value, attr_prefix.len() + key.len() + 2)`. |
| `carina-tui/src/app/mod.rs` | inspect | Verify TUI's `DetailRow` consumer handles the new variant (likely identical render to `Attribute`, or a passthrough). |
| `carina-cli/tests/fixtures/plan_display/policy_pretty/main.crn` | create | Fixture: Create plan with list-of-map attribute (e.g. `statement = [{...}, {...}]`). |
| `carina-cli/tests/fixtures/plan_display/pretty_long_string_list/main.crn` | create | Fixture: Create plan with a list-of-string attribute > 80 columns. |
| `carina-cli/tests/fixtures/plan_display/pretty_short_string_list/main.crn` | create | Fixture: Create plan with a list-of-string attribute well under 80 columns. |
| `carina-cli/src/plan_snapshot_tests.rs` | modify | Add three `#[test] fn snapshot_*` functions for the new fixtures. |
| `Makefile` | modify | Add `plan-policy-pretty`, `plan-pretty-long-string-list`, `plan-pretty-short-string-list` targets and include them in `plan-fixtures`. |

## Task list

### Task 1: Add `PRETTY_LINE_LIMIT` constant and `format_value_pretty` skeleton with scalar fallthrough

**Goal**: Establish the new function with the same behavior as `format_value_with_key` for all scalar / non-collection variants. List-of-map and list expansion come in later tasks. This lets us introduce the function and its tests without touching collection logic yet.

**Files**: `carina-core/src/value.rs`

**Test** (add to existing tests module in `value.rs`):

```rust
#[test]
fn format_value_pretty_string_matches_format_value() {
    let v = Value::String("hello".to_string());
    assert_eq!(format_value_pretty(&v, 0), format_value(&v));
}

#[test]
fn format_value_pretty_int_matches_format_value() {
    let v = Value::Int(42);
    assert_eq!(format_value_pretty(&v, 0), "42");
}

#[test]
fn format_value_pretty_bool_matches_format_value() {
    let v = Value::Bool(true);
    assert_eq!(format_value_pretty(&v, 0), "true");
}

#[test]
fn format_value_pretty_dsl_enum_resolves() {
    // Same enum-resolution behavior as format_value_with_key
    let v = Value::String("aws.s3.Bucket.VersioningStatus.enabled".to_string());
    let pretty = format_value_pretty(&v, 0);
    let single = format_value(&v);
    assert_eq!(pretty, single);
}

#[test]
fn format_value_pretty_secret_masked() {
    let v = Value::Secret(Box::new(Value::String("my-password".to_string())));
    assert_eq!(format_value_pretty(&v, 0), "(secret)");
}

#[test]
fn format_value_pretty_unknown_renders_like_format_value() {
    use crate::value::UnknownReason;
    let v = Value::Unknown(UnknownReason::ForKey);
    assert_eq!(format_value_pretty(&v, 0), format_value(&v));
}
```

**Implementation**:

```rust
/// Maximum line width for pretty-printed plan output before list-of-string
/// attributes expand vertically. Fixed (not terminal-derived) so snapshot
/// tests are deterministic and CI/PR-comment readers see identical output.
pub const PRETTY_LINE_LIMIT: usize = 80;

/// Format a `Value` for human-readable, multi-line plan output.
///
/// `indent_cols` is the column at which the *value* starts (i.e. the caller
/// has already written `<indent>key: ` and is about to append this string).
/// The first line of the returned string is NOT prefixed with whitespace;
/// continuation lines (when expansion happens) carry the appropriate
/// indentation.
///
/// For all scalar variants and `Value::Map`, behavior matches
/// `format_value_with_key(value, None)`. The function expands only
/// `Value::List` containing all `Value::Map` items (always vertical) and
/// `Value::List` of scalars whose inline form would exceed `PRETTY_LINE_LIMIT`.
pub fn format_value_pretty(value: &Value, indent_cols: usize) -> String {
    match value {
        // Collection variants: routed to dedicated helpers in later tasks.
        Value::List(_) | Value::Map(_) => format_value_with_key(value, None),
        // Scalars: identical to single-line form.
        _ => format_value_with_key(value, None),
    }
}
```

(Subsequent tasks specialize the `Value::List` and `Value::Map` arms.)

**Verification**:

```bash
cargo nextest run -p carina-core format_value_pretty
```

All 6 new tests pass; existing tests untouched.

---

### Task 2: List-of-map vertical rendering

**Goal**: When `value` is `Value::List` of all `Value::Map`, render each entry on its own line under `- ` prefix, with map keys sorted alphabetically and values formatted via single-line `format_value_with_key`.

**Files**: `carina-core/src/value.rs`

**Test**:

```rust
#[test]
fn format_value_pretty_list_of_maps_vertical() {
    let mut s1 = IndexMap::new();
    s1.insert("sid".to_string(), Value::String("First".to_string()));
    s1.insert("effect".to_string(), Value::String("Allow".to_string()));
    let mut s2 = IndexMap::new();
    s2.insert("sid".to_string(), Value::String("Second".to_string()));
    s2.insert("effect".to_string(), Value::String("Deny".to_string()));
    let v = Value::List(vec![Value::Map(s1), Value::Map(s2)]);

    // indent_cols=6 means the parent already wrote "      key: " (6 spaces of indent)
    // so continuation lines (the `- ` and following keys) need 6 spaces of indent too.
    let out = format_value_pretty(&v, 6);
    let expected = "\n      - effect: \"Allow\"\n        sid: \"First\"\n      - effect: \"Deny\"\n        sid: \"Second\"";
    assert_eq!(out, expected);
}

#[test]
fn format_value_pretty_list_of_maps_single_entry() {
    let mut m = IndexMap::new();
    m.insert("k".to_string(), Value::String("v".to_string()));
    let v = Value::List(vec![Value::Map(m)]);
    let out = format_value_pretty(&v, 4);
    assert_eq!(out, "\n    - k: \"v\"");
}

#[test]
fn format_value_pretty_empty_list_inline() {
    // Empty list never expands — inline `[]`
    let v = Value::List(vec![]);
    assert_eq!(format_value_pretty(&v, 0), "[]");
}
```

**Implementation**: Replace the `Value::List(_)` arm:

```rust
Value::List(items) => {
    if items.is_empty() {
        return "[]".to_string();
    }
    if is_list_of_maps(value) {
        return format_list_of_maps_vertical(items, indent_cols);
    }
    // List-of-scalars: still single-line for now; threshold handled in Task 3.
    format_value_with_key(value, None)
}
```

Add helper:

```rust
fn format_list_of_maps_vertical(items: &[Value], indent_cols: usize) -> String {
    let parent_indent = " ".repeat(indent_cols);
    let entry_indent = " ".repeat(indent_cols + 2);
    let mut out = String::new();
    for item in items {
        if let Value::Map(map) = item {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let mut first = true;
            for k in keys {
                let val_str = format_value_with_key(&map[k], Some(k));
                if first {
                    out.push('\n');
                    out.push_str(&parent_indent);
                    out.push_str("- ");
                    first = false;
                } else {
                    out.push('\n');
                    out.push_str(&entry_indent);
                }
                out.push_str(k);
                out.push_str(": ");
                out.push_str(&val_str);
            }
        }
    }
    out
}
```

**Verification**:

```bash
cargo nextest run -p carina-core format_value_pretty
```

3 new tests pass plus Task 1's 6.

---

### Task 3: List-of-scalars vertical rendering above 80 columns

**Goal**: When `Value::List` is not all maps, expand vertically iff the inline form would make the total line (`indent_cols + len(inline_repr)`) exceed `PRETTY_LINE_LIMIT`. Boundary: `>` strict (exactly 80 stays inline).

**Files**: `carina-core/src/value.rs`

**Test**:

```rust
#[test]
fn format_value_pretty_list_of_strings_under_80_inline() {
    let v = Value::List(vec![
        Value::String("a".to_string()),
        Value::String("b".to_string()),
    ]);
    assert_eq!(format_value_pretty(&v, 0), "[\"a\", \"b\"]");
}

#[test]
fn format_value_pretty_list_of_strings_over_80_vertical() {
    // 5 strings of 20 chars each → inline ~110 chars
    let items: Vec<Value> = (0..5)
        .map(|i| Value::String(format!("iam:LongActionName{}", i)))
        .collect();
    let v = Value::List(items);
    let out = format_value_pretty(&v, 4);
    assert!(out.starts_with("[\n"), "expected bracket-newline start, got: {out}");
    assert!(out.contains("\n      \"iam:LongActionName0\","), "missing first item line: {out}");
    assert!(out.ends_with("\n    ]"), "expected closing bracket on its own indented line: {out}");
}

#[test]
fn format_value_pretty_list_of_strings_threshold_boundary() {
    // Construct a list whose inline form is exactly 80 chars at indent 0 → stays inline.
    // "[\"aaaa\", \"bbbb\"]" is 16 chars; pad to exactly 80.
    let item = "x".repeat(76);   // "x"*76 → wrapped as "..." → "[\"xxxx...xxxx\"]" = 76 + 4 = 80
    let v = Value::List(vec![Value::String(item.clone())]);
    let inline = format_value_with_key(&v, None);
    assert_eq!(inline.len(), 80, "fixture sanity: {} chars", inline.len());
    assert_eq!(format_value_pretty(&v, 0), inline);
}

#[test]
fn format_value_pretty_list_of_strings_indent_pushes_over_threshold() {
    // Inline form is 75 chars; at indent_cols=10 the total would be 85 → expand.
    let inline_target = 75;
    let item = "x".repeat(inline_target - 4);  // "[\"xxxx...\"]" = inline_target chars
    let v = Value::List(vec![Value::String(item)]);
    let inline = format_value_with_key(&v, None);
    assert_eq!(inline.len(), inline_target);
    let out = format_value_pretty(&v, 10);
    assert!(out.starts_with("[\n"), "indent should have pushed over threshold: {out}");
}
```

**Implementation**: Replace the list-of-scalars branch:

```rust
Value::List(items) => {
    if items.is_empty() {
        return "[]".to_string();
    }
    if is_list_of_maps(value) {
        return format_list_of_maps_vertical(items, indent_cols);
    }
    let inline = format_value_with_key(value, None);
    if indent_cols + inline.len() <= PRETTY_LINE_LIMIT {
        return inline;
    }
    format_list_of_scalars_vertical(items, indent_cols)
}
```

Add helper:

```rust
fn format_list_of_scalars_vertical(items: &[Value], indent_cols: usize) -> String {
    let item_indent = " ".repeat(indent_cols + 2);
    let close_indent = " ".repeat(indent_cols);
    let mut out = String::from("[\n");
    for (i, item) in items.iter().enumerate() {
        out.push_str(&item_indent);
        out.push_str(&format_value_with_key(item, None));
        if i + 1 < items.len() {
            out.push(',');
        } else {
            out.push(',');  // Trailing comma for diff stability
        }
        out.push('\n');
    }
    out.push_str(&close_indent);
    out.push(']');
    out
}
```

**Verification**:

```bash
cargo nextest run -p carina-core format_value_pretty
```

4 new tests pass (total 13 tests touching `format_value_pretty`).

---

### Task 4: Maps inside list-of-map entries — recurse for `Value::Map` nested values

**Goal**: If a list-of-map entry contains another `Value::Map` value (e.g. `condition: { StringEquals: {...} }` inside an IAM statement), the inner map renders vertically too. Currently `format_list_of_maps_vertical` calls single-line `format_value_with_key` for entry values. Make it recurse into `format_value_pretty` so nested maps and threshold-exceeding scalar lists also expand correctly.

**Files**: `carina-core/src/value.rs`

**Test**:

```rust
#[test]
fn format_value_pretty_list_of_maps_with_nested_map() {
    let mut inner = IndexMap::new();
    inner.insert("StringEquals".to_string(), Value::Map({
        let mut m = IndexMap::new();
        m.insert("aws:Tag".to_string(), Value::String("prod".to_string()));
        m
    }));
    let mut entry = IndexMap::new();
    entry.insert("sid".to_string(), Value::String("X".to_string()));
    entry.insert("condition".to_string(), Value::Map(inner));
    let v = Value::List(vec![Value::Map(entry)]);
    let out = format_value_pretty(&v, 4);
    // Top entry under "    - "; nested keys must be reachable
    assert!(out.contains("    - condition:"), "expected nested key line, got: {out}");
    assert!(out.contains("sid: \"X\""), "expected sid line, got: {out}");
}

#[test]
fn format_value_pretty_list_of_maps_with_long_string_list_inside() {
    // statement-style map with a long action list that should expand vertically.
    let actions: Vec<Value> = (0..6)
        .map(|i| Value::String(format!("iam:Action{:03}", i)))
        .collect();
    let mut entry = IndexMap::new();
    entry.insert("sid".to_string(), Value::String("X".to_string()));
    entry.insert("action".to_string(), Value::List(actions));
    let v = Value::List(vec![Value::Map(entry)]);
    let out = format_value_pretty(&v, 4);
    assert!(out.contains("action: ["), "expected expanded action list bracket: {out}");
    assert!(out.contains("\"iam:Action000\","), "expected first action on its own line: {out}");
}
```

**Implementation**: Update `format_list_of_maps_vertical` and add a `Value::Map` arm to `format_value_pretty`:

```rust
fn format_list_of_maps_vertical(items: &[Value], indent_cols: usize) -> String {
    let parent_indent = " ".repeat(indent_cols);
    let entry_indent_cols = indent_cols + 2;
    let entry_indent = " ".repeat(entry_indent_cols);
    let mut out = String::new();
    for item in items {
        if let Value::Map(map) = item {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let mut first = true;
            for k in keys {
                // Value sits at: entry_indent_cols + len(k) + 2 (": ")
                let val_indent = entry_indent_cols + k.len() + 2;
                let val_str = format_value_pretty(&map[k], val_indent);
                if first {
                    out.push('\n');
                    out.push_str(&parent_indent);
                    out.push_str("- ");
                    first = false;
                } else {
                    out.push('\n');
                    out.push_str(&entry_indent);
                }
                out.push_str(k);
                out.push_str(": ");
                out.push_str(&val_str);
            }
        }
    }
    out
}
```

And the top-level `Value::Map` arm:

```rust
Value::Map(map) => {
    if map.is_empty() {
        return "{}".to_string();
    }
    // Reuse single-line form when total line fits.
    let inline = format_value_with_key(value, None);
    if indent_cols + inline.len() <= PRETTY_LINE_LIMIT {
        return inline;
    }
    format_map_vertical(map, indent_cols)
}
```

Add `format_map_vertical`:

```rust
fn format_map_vertical(map: &indexmap::IndexMap<String, Value>, indent_cols: usize) -> String {
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort();
    let mut out = String::new();
    let key_indent = " ".repeat(indent_cols);
    let mut first = true;
    for k in keys {
        let val_indent = indent_cols + k.len() + 2;
        let val_str = format_value_pretty(&map[k], val_indent);
        if first {
            // First line: caller already at value position; emit on new line under parent
            out.push('\n');
            first = false;
        } else {
            out.push('\n');
        }
        out.push_str(&key_indent);
        out.push_str(k);
        out.push_str(": ");
        out.push_str(&val_str);
    }
    out
}
```

(Top-level Map expansion only triggers when the inline form alone exceeds 80 cols — uncommon for plan attributes, but supports policy_document's `version: ..., statement: [...]` outer map.)

**Verification**:

```bash
cargo nextest run -p carina-core format_value_pretty
```

All 15 tests pass.

---

### Task 5: Add `DetailRow::PrettyAttribute` variant and emit it for list-of-map in Create

**Goal**: Introduce a new variant carrying the raw `Value` (not pre-stringified). `build_create_rows` emits this variant for list-of-map attributes; the renderer (Task 5b) computes `indent_cols` from the *runtime* `attr_prefix` and calls `format_value_pretty` then.

**Files**: `carina-core/src/detail_rows.rs`

**Test** (`carina-core/src/detail_rows.rs`, new module):

```rust
#[cfg(test)]
mod create_pretty_tests {
    use super::*;
    use crate::effect::Effect;
    use crate::resource::Resource;
    use crate::value::Value;
    use indexmap::IndexMap;

    #[test]
    fn create_row_list_of_maps_emits_pretty_attribute() {
        let mut r = Resource::new("awscc.iam.RolePolicy", "test");
        let mut s1 = IndexMap::new();
        s1.insert("sid".to_string(), Value::String("S1".to_string()));
        s1.insert("effect".to_string(), Value::String("Allow".to_string()));
        r.attributes.insert(
            "statement".to_string(),
            Value::List(vec![Value::Map(s1)]),
        );

        let effect = Effect::Create(r);
        let rows = build_detail_rows(&effect, None, DetailLevel::Default, None);

        let pretty = rows.iter().find_map(|row| match row {
            DetailRow::PrettyAttribute { key, value } if key == "statement" => Some(value),
            _ => None,
        });
        assert!(pretty.is_some(), "expected PrettyAttribute row for statement, got: {rows:?}");
        let value = pretty.unwrap();
        assert!(matches!(value, Value::List(_)), "PrettyAttribute should carry raw Value::List");

        assert!(
            !rows.iter().any(|row| matches!(row, DetailRow::ListOfMaps { .. })),
            "ListOfMaps row should no longer be emitted for Create"
        );
    }

    #[test]
    fn create_row_scalar_unchanged_uses_plain_attribute() {
        let mut r = Resource::new("awscc.iam.Role", "test");
        r.attributes.insert("role_name".to_string(), Value::String("foo".to_string()));
        let effect = Effect::Create(r);
        let rows = build_detail_rows(&effect, None, DetailLevel::Default, None);
        assert!(
            rows.iter().any(|row| matches!(row, DetailRow::Attribute { key, .. } if key == "role_name")),
            "scalar attribute should still emit DetailRow::Attribute"
        );
    }
}
```

**Implementation**:

1. Add the variant to `DetailRow` enum in `detail_rows.rs`:

```rust
/// A pretty-printed attribute whose value is rendered with `format_value_pretty`
/// at render time (so the renderer can supply the actual indent column).
/// Currently used for list-of-map attributes on Create.
PrettyAttribute {
    key: String,
    value: Value,
},
```

2. In `build_create_rows` (around line 302), replace:

```rust
if is_list_of_maps(value) {
    rows.push(build_list_of_maps_row(key, value));
} else if let Value::Map(map) = value {
```

with:

```rust
if is_list_of_maps(value) {
    rows.push(DetailRow::PrettyAttribute {
        key: key.to_string(),
        value: value.clone(),
    });
} else if let Value::Map(map) = value {
```

3. Delete `fn build_list_of_maps_row` (now unused).

**Verification**:

```bash
cargo check -p carina-core    # ensure new variant didn't break callers
cargo nextest run -p carina-core create_pretty_tests
```

Both new tests pass. `cargo check` will likely show non-exhaustive-match warnings/errors in `display/mod.rs` and `carina-tui` — those are addressed in Task 5b (display) and Task 6 (TUI inspection).

---

### Task 5b: Render `DetailRow::PrettyAttribute` in display/mod.rs

**Goal**: Add a renderer branch that uses the runtime `attr_prefix` to compute the correct indent column, then calls `format_value_pretty`.

**Files**: `carina-cli/src/display/mod.rs`

**Test**: Indirect — covered by Task 7's `snapshot_policy_pretty` end-to-end snapshot. No separate unit test needed (the renderer is mostly string concatenation; a snapshot is more meaningful).

**Implementation**: Add the new arm to `render_detail_row` (next to `DetailRow::Attribute`):

```rust
DetailRow::PrettyAttribute { key, value } => {
    // attr_prefix is the column-0 → key indentation. The value starts at
    // column attr_prefix.len() + key.len() + ": ".len() = + 2.
    let indent_cols = attr_prefix.chars().count() + key.chars().count() + 2;
    let pretty = carina_core::value::format_value_pretty(value, indent_cols);
    let cv = match effect {
        Effect::Delete { .. } => pretty.red().strikethrough().to_string(),
        _ => colored_value(&pretty, false),
    };
    writeln!(out, "{}{}: {}", attr_prefix, key, cv).unwrap();
}
```

Add the import at the top of `display/mod.rs`:

```rust
use carina_core::value::{format_value, format_value_pretty, format_value_with_key, is_list_of_maps, map_similarity};
```

**Verification**:

```bash
cargo check -p carina-cli
cargo nextest run -p carina-cli create_pretty_tests || true
cargo nextest run -p carina-cli   # workspace tests must remain green
```

`cargo check` clean (the new variant is now matched). Workspace tests still green.

---

### Task 6: Handle `DetailRow::PrettyAttribute` in TUI and remove dead code

**Goal**: The `DetailRow` enum is also matched by `carina-tui`. Adding a new variant requires updating any non-exhaustive match. Inspect first, then add a render path that mirrors `DetailRow::Attribute` (TUI is not the issue's target — same single-line form is acceptable for now). Also remove the now-dead `build_list_of_maps_row`.

**Files**: `carina-tui/src/app/mod.rs` (modify if `DetailRow` is matched), `carina-core/src/detail_rows.rs`

**Test**: No new test. Workspace tests must continue passing.

**Implementation**:

1. Locate TUI's `DetailRow` consumer:

```bash
grep -nE "DetailRow::|match .*detail_row|use carina_core::detail_rows" carina-tui/src/app/mod.rs
```

2. If TUI matches `DetailRow` exhaustively, add a `DetailRow::PrettyAttribute { key, value } => { ... }` arm. The simplest correct implementation calls `format_value(value)` for now — TUI rendering can be improved in a follow-up. If TUI uses a wildcard `_`, no change needed.

3. Confirm `build_list_of_maps_row` is not referenced anywhere:

```bash
grep -rn "build_list_of_maps_row" carina-core/ carina-cli/ carina-tui/
```

Should return no matches after Task 5's deletion.

4. Confirm `DetailRow::ListOfMaps` has no remaining constructors:

```bash
grep -rnE "DetailRow::ListOfMaps\s*\{" carina-core/ carina-cli/ carina-tui/
```

Should return no matches except the renderer's `match` arms (which we keep — the variant stays in the enum to avoid breaking the API).

**Verification**:

```bash
cargo nextest run -p carina-core
cargo nextest run -p carina-cli
cargo nextest run -p carina-tui
cargo clippy --workspace --all-targets -- -D warnings
```

Build clean, all tests pass.

---

### Task 7: Snapshot test fixture — `policy_pretty/`

**Goal**: End-to-end visual verification of D1 (list-of-map vertical).

**Files**:
- `carina-cli/tests/fixtures/plan_display/policy_pretty/main.crn` (create)
- `carina-cli/src/plan_snapshot_tests.rs` (modify)

**Test** (add to `plan_snapshot_tests.rs`):

```rust
#[test]
fn snapshot_policy_pretty() {
    let fp = build_plan_from_fixture_name("policy_pretty");
    let plan = fp.plan;
    let _states = fp.states;
    let _registry = fp.schemas;

    let mut output = String::new();
    let _ = format_plan(
        &plan,
        DetailLevel::Default,
        Some(&_registry),
        None,
        &mut output,
    );
    insta::assert_snapshot!(strip_ansi(&output));
}
```

**Implementation** (`policy_pretty/main.crn`):

```
# Single anonymous resource with a list-of-map attribute.
# Mock provider has empty schema, so any attribute name works.

provider mock {}

mock.iam.RolePolicy {
  policy_name = 'carina-bootstrap-inline'
  role_name   = 'carina-bootstrap'
  statement = [
    { sid = 'ManageDeployRoles', effect = 'Allow', action = 'iam:CreateRole', resource = 'arn:aws:iam::*:role/carina-*-deploy' },
    { sid = 'ReadOIDCProvider',  effect = 'Allow', action = 'iam:GetOpenIDConnectProvider', resource = 'arn:aws:iam::*:oidc-provider/token.actions.githubusercontent.com' },
  ]
}
```

(Note: mock provider syntax check — verify the `provider mock {}` block parses with no factory; if not, fall back to `provider awscc { region = awscc.Region.ap_northeast_1 }` and use `awscc.iam.RolePolicy { ... }`.)

**Verification**:

```bash
cargo nextest run -p carina-cli snapshot_policy_pretty
# First run fails (no .snap exists); review and accept:
cargo insta review
# Confirm the snapshot shows:
#   + ... iam_role_policy_<hash>
#       statement:
#         - action: "iam:CreateRole"
#           effect: "Allow"
#           resource: "..."
#           sid: "ManageDeployRoles"
#         - action: "iam:GetOpenIDConnectProvider"
#           ...
```

Per memory rule "Review snapshots before accepting": confirm content matches the design D1 spec before `cargo insta accept`.

---

### Task 8: Snapshot fixture — `pretty_long_string_list/`

**Goal**: D2 (>80 col list-of-string expands) and D3 (bracketed format).

**Files**:
- `carina-cli/tests/fixtures/plan_display/pretty_long_string_list/main.crn` (create)
- `carina-cli/src/plan_snapshot_tests.rs` (modify)

**Test**:

```rust
#[test]
fn snapshot_pretty_long_string_list() {
    let fp = build_plan_from_fixture_name("pretty_long_string_list");
    let plan = fp.plan;
    let _registry = fp.schemas;
    let mut output = String::new();
    let _ = format_plan(&plan, DetailLevel::Default, Some(&_registry), None, &mut output);
    insta::assert_snapshot!(strip_ansi(&output));
}
```

**Implementation** (`pretty_long_string_list/main.crn`):

```
# A list-of-string attribute long enough to exceed 80 columns inline,
# expected to expand with bracketed multi-line form.

provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.iam.Role {
  role_name = 'carina-long-list-test'
  managed_policy_arns = [
    'arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess',
    'arn:aws:iam::aws:policy/AmazonEC2ReadOnlyAccess',
    'arn:aws:iam::aws:policy/IAMReadOnlyAccess',
  ]
}
```

**Verification**:

```bash
cargo nextest run -p carina-cli snapshot_pretty_long_string_list
cargo insta review
# Expected:
#   managed_policy_arns: [
#     "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess",
#     "arn:aws:iam::aws:policy/AmazonEC2ReadOnlyAccess",
#     "arn:aws:iam::aws:policy/IAMReadOnlyAccess",
#   ]
```

---

### Task 9: Snapshot fixture — `pretty_short_string_list/`

**Goal**: Regression guard for D2 boundary — lists under 80 cols stay inline.

**Files**:
- `carina-cli/tests/fixtures/plan_display/pretty_short_string_list/main.crn` (create)
- `carina-cli/src/plan_snapshot_tests.rs` (modify)

**Test**:

```rust
#[test]
fn snapshot_pretty_short_string_list() {
    let fp = build_plan_from_fixture_name("pretty_short_string_list");
    let plan = fp.plan;
    let _registry = fp.schemas;
    let mut output = String::new();
    let _ = format_plan(&plan, DetailLevel::Default, Some(&_registry), None, &mut output);
    insta::assert_snapshot!(strip_ansi(&output));
}
```

**Implementation** (`pretty_short_string_list/main.crn`):

```
# A list-of-string attribute short enough to stay inline (under 80 cols).

provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.iam.Role {
  role_name = 'short-list-test'
  short_tags = ['a', 'b', 'c']
}
```

**Verification**:

```bash
cargo nextest run -p carina-cli snapshot_pretty_short_string_list
cargo insta review
# Expected:
#   short_tags: ["a", "b", "c"]
# (single line, no expansion)
```

---

### Task 10: Makefile targets and `plan-fixtures` aggregator

**Goal**: Make manual visual inspection easy and CI's `Check Plan Fixtures` pass.

**Files**: `Makefile`

**Implementation**:

```make
plan-policy-pretty:
	$(PLAN_FIXTURE) policy_pretty
plan-pretty-long-string-list:
	$(PLAN_FIXTURE) pretty_long_string_list
plan-pretty-short-string-list:
	$(PLAN_FIXTURE) pretty_short_string_list
```

Add to the `plan-fixtures` aggregator and to `.PHONY`:

```make
plan-fixtures:
	...
	@echo "=== policy_pretty ==="
	@$(MAKE) plan-policy-pretty
	@echo ""
	@echo "=== pretty_long_string_list ==="
	@$(MAKE) plan-pretty-long-string-list
	@echo ""
	@echo "=== pretty_short_string_list ==="
	@$(MAKE) plan-pretty-short-string-list
	@echo ""
```

Add to `.PHONY`:

```
.PHONY: ... plan-policy-pretty plan-pretty-long-string-list plan-pretty-short-string-list
```

**Verification**:

```bash
make plan-policy-pretty
make plan-pretty-long-string-list
make plan-pretty-short-string-list
make plan-fixtures
```

All targets run; output looks correct visually.

---

### Task 11: Final verify sweep

**Goal**: Ensure workspace-level health after the change. Per CLAUDE.md verify protocol order:

```bash
# 1. Crate-scoped tests
cargo nextest run $(scripts/touched-crates.sh)

# 2. Doctests (nextest skips them; cheap to run)
cargo test --workspace --doc

# 3. Lints
cargo clippy --workspace --all-targets -- -D warnings

# 4. Repo invariants
bash scripts/check-tmlanguage-parity.sh    # other check-*.sh as present
ls scripts/check-*.sh | xargs -n1 bash
```

All green. No SKIP_CLIPPY=1 (per memory rule "Never skip clippy").

---

## Out of scope (deferred follow-up)

- Update-time diff pretty-printing for list-of-map and list-of-string attributes. Will be filed as a separate Issue once this PR lands.
- Resource origin / file provenance (Issue #2409 Problem 2). Filed as a separate Issue during Step 10 of brainstorming.
- TUI plan view changes.

## Risks tracked

| Risk | Mitigation |
| ---- | ---------- |
| `format_value` and `format_value_pretty` drift on a future `Value` variant | Both delegate to a common path for scalars; unit tests cover every variant. |
| Mock provider doesn't accept `provider mock {}` in fixture | Fall back to `awscc` provider in fixtures (Task 7 note). |
| Indent calculation mismatch in `build_create_rows` (`CREATE_ATTR_PREFIX_COLS = 4`) | Verify via Task 7 snapshot — visual misalignment is immediately obvious. If wrong, adjust the constant in one place. |
| 80-col boundary off-by-one | Task 3 boundary test exercises exactly 80 chars and 75 chars-with-indent-pushing-over. |
| Snapshot test discovery | Existing harness uses explicit `#[test] fn snapshot_*` per fixture (verified at plan time); each new fixture needs its own test function (Tasks 7–9). |
