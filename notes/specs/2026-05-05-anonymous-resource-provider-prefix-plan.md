# Anonymous resource provider prefix — implementation plan

<!-- derived-from #2026-05-05-anonymous-resource-provider-prefix-design -->

Issue: [#2419](https://github.com/carina-rs/carina/issues/2419) (provider prefix portion).
Design: [`2026-05-05-anonymous-resource-provider-prefix-design.md`](./2026-05-05-anonymous-resource-provider-prefix-design.md).

## Architecture summary

Two-layer change:

1. **`carina-core/src/identifier/mod.rs`** — change the assembled identifier format from `{type_snake}_{hash}` to `{provider_snake}_{type_snake}_{hash}` at the single name-assembly site (around line 474). Provider segment derives from `Resource.id.provider` via the same `pascal_to_snake` helper used for the type segment.

2. **State reconciliation** — when `reconcile_anonymous_identifiers` finds a SimHash-distance match against an old-format state entry (`iam_role_policy_<hash>`), keep the freshly-computed new-format identifier (`awscc_iam_role_policy_<hash>`) and **emit a rename pair** so the wiring layer can re-key the state entry. Mirrors the existing `apply_anonymous_to_named_renames` plumbing.

Snapshot churn is expected — every plan-display fixture that contains an anonymous resource gets its `.snap` updated. A new dedicated fixture `provider_prefix/` makes the prefix presence intent explicit.

## File map

| File | Type | Purpose |
| ---- | ---- | ------- |
| `carina-core/src/identifier/mod.rs` | modify | Update name format. Update `reconcile_anonymous_identifiers` to return rename pairs. Add unit tests. |
| `carina-cli/src/wiring/mod.rs` | modify | Apply the new rename pairs to `current_states` / `prev_desired_keys` / `saved_attrs`, mirroring `apply_anonymous_to_named_renames`. |
| `carina-cli/tests/fixtures/plan_display/provider_prefix/main.crn` | create | Fixture: a single anonymous resource. Snapshot proves the new prefix appears. |
| `carina-cli/src/plan_snapshot_tests.rs` | modify | Add `snapshot_provider_prefix` test function. |
| `carina-cli/src/snapshots/*.snap` | modify (auto, via `cargo insta review`) | Updated names for every fixture that produces anonymous resources. |
| `Makefile` | modify | Add `plan-provider-prefix` target; include in `plan-fixtures` aggregator. |

## Task list

**PR-merge boundary note**: Tasks 1–5 must land as a single PR (or as a series of fast-merging PRs). Splitting Task 1 from Task 2 leaves the workspace red on identifier-format assertion failures, and splitting Task 3 from Task 4 leaves the reconciliation rename plumbing partial. The Issue list below uses task labels for tracking, but the implementing PRs should bundle these tightly-coupled tasks together.

### Task 1: Add `provider_snake` to identifier assembly

**Goal**: Change the format string in `compute_anonymous_identifiers` so identifiers gain a provider prefix. This is the smallest correct change — no reconciliation tweaks yet, so existing state-comparison tests will fail in a controlled way that Task 2 fixes.

**Files**: `carina-core/src/identifier/mod.rs`

**Test** (add to existing tests module in `identifier/mod.rs`):

```rust
#[test]
fn anonymous_identifier_includes_provider_prefix() {
    use crate::resource::{Resource, ResourceId, ResourceName};
    use crate::schema::SchemaRegistry;

    let mut r = Resource::new("iam.RolePolicy", "");
    r.id = ResourceId {
        provider: "awscc".to_string(),
        resource_type: "iam.RolePolicy".to_string(),
        name: ResourceName::Pending,
    };
    r.attributes.insert(
        "policy_name".to_string(),
        crate::value::Value::String("foo".to_string()),
    );

    let mut resources = vec![r];
    let registry = SchemaRegistry::new();
    compute_anonymous_identifiers(&mut resources, &[], &registry, &|_| vec![]).unwrap();

    let name = resources[0].id.name_str();
    assert!(
        name.starts_with("awscc_"),
        "identifier should begin with the provider prefix, got: {name}"
    );
    assert!(
        name.contains("iam_role_policy_"),
        "identifier should still contain the snake-case type, got: {name}"
    );
}

#[test]
fn anonymous_identifier_provider_prefix_for_aws_provider() {
    use crate::resource::{Resource, ResourceId, ResourceName};
    use crate::schema::SchemaRegistry;

    let mut r = Resource::new("s3.Bucket", "");
    r.id = ResourceId {
        provider: "aws".to_string(),
        resource_type: "s3.Bucket".to_string(),
        name: ResourceName::Pending,
    };

    let mut resources = vec![r];
    let registry = SchemaRegistry::new();
    compute_anonymous_identifiers(&mut resources, &[], &registry, &|_| vec![]).unwrap();
    assert!(resources[0].id.name_str().starts_with("aws_s3_bucket_"));
}
```

**Implementation**: At `identifier/mod.rs:467-474`:

```rust
let provider_snake = resource
    .id
    .provider
    .split('.')
    .map(crate::parser::pascal_to_snake)
    .collect::<Vec<_>>()
    .join("_");
let type_snake = resource
    .id
    .resource_type
    .split('.')
    .map(crate::parser::pascal_to_snake)
    .collect::<Vec<_>>()
    .join("_");
let identifier = format!("{}_{}_{}", provider_snake, type_snake, hash_str);
```

**Verification**:

```bash
cargo nextest run -p carina-core anonymous_identifier_includes_provider_prefix
cargo nextest run -p carina-core anonymous_identifier_provider_prefix_for_aws_provider
```

Both new tests pass. Existing `compute_anonymous_identifiers` tests (`carina-core/src/identifier/mod.rs` and `carina-cli/src/tests.rs`) will start failing with name-mismatch errors — Task 2 / Task 3 update them to the new format.

---

### Task 2: Update existing identifier-format tests to the new format

**Goal**: Refresh every existing test that asserts an anonymous identifier matches a specific string. These tests are correct in intent (verify identifier shape) but their expected values are now stale.

**Files**:
- `carina-core/src/identifier/mod.rs` (test module)
- `carina-cli/src/tests.rs`

**Test**: No new tests — only updates to existing assertions. Run the full test scope to see which assertions fail:

```bash
cargo nextest run -p carina-core 2>&1 | grep -A2 "FAILED" | head -50
cargo nextest run -p carina-cli 2>&1 | grep -A2 "FAILED" | head -50
```

For each failing test, update the expected name from `<type>_<hash>` to `<provider>_<type>_<hash>`. The hash digest itself does NOT change (SimHash is over identity attributes, not over the assembled name).

**Implementation**: Mechanical string substitution per test. Examples:

- `"ec2_vpc_a3f2b1c8"` → `"awscc_ec2_vpc_a3f2b1c8"`
- `"s3_bucket_cafef00d"` → `"aws_s3_bucket_cafef00d"`

The `carina-cli/src/tests.rs:591-947` block has many `compute_anonymous_identifiers` invocations — methodically update each assertion.

**Verification**:

```bash
cargo nextest run -p carina-core
cargo nextest run -p carina-cli identifier
cargo nextest run -p carina-cli compute_anonymous
```

All pass.

---

### Task 3: Update `reconcile_anonymous_identifiers` SimHash-match path to keep the new identifier and emit a rename pair

**Goal**: When the SimHash-distance match path (`identifier/mod.rs:570-592`) finds an old-format state entry as the closest match, keep the freshly-computed new-format identifier in `resource.id.name` (instead of overwriting with the state's old name) and return a rename pair so the wiring layer can re-key state file entries.

**Files**: `carina-core/src/identifier/mod.rs`

**Test**:

```rust
#[test]
fn reconcile_simhash_match_keeps_new_format_identifier_and_emits_rename() {
    use crate::resource::{Resource, ResourceId, ResourceName};
    use crate::schema::SchemaRegistry;

    // Build a resource that has been freshly assigned the new-format name.
    let mut r = Resource::new("iam.RolePolicy", "awscc_iam_role_policy_b94fde85");
    r.id = ResourceId::with_provider("awscc", "iam.RolePolicy", "awscc_iam_role_policy_b94fde85");

    // Build a state entry under the old-format name with a SimHash within reconciliation distance.
    // For this test, hash equality (distance 0) is sufficient.
    let state_info = AnonymousIdStateInfo {
        name: "iam_role_policy_b94fde85".to_string(),
        create_only_values: BTreeMap::new(),
    };

    let mut resources = vec![r];
    let registry = SchemaRegistry::new();
    let renames = reconcile_anonymous_identifiers(
        &mut resources,
        &registry,
        &|_, _| vec![state_info.clone()],
        &[],
        &|_| vec![],
    );

    assert_eq!(
        resources[0].id.name_str(),
        "awscc_iam_role_policy_b94fde85",
        "resource id should retain the new-format identifier",
    );
    assert_eq!(
        renames,
        vec![("iam_role_policy_b94fde85".to_string(), "awscc_iam_role_policy_b94fde85".to_string())],
        "should emit a rename pair from the old state name to the new identifier",
    );
}
```

**Implementation**: Two changes to `reconcile_anonymous_identifiers`:

1. Change the function signature to return `Vec<(String, String)>` (old_name, new_name pairs).
2. Replace lines 586-592 (the SimHash-match write-back) with rename emission:

```rust
if let Some((state_name, _)) = best_match {
    // The state entry uses an older identifier format. Keep our
    // freshly-computed new-format name on the resource (already
    // stored in resource.id) and record the rename so wiring can
    // re-key the state entry.
    renames.push((state_name.to_string(), resource.id.name_str().to_string()));
}
continue;
```

(The function should accumulate `renames: Vec<(String, String)>` from the start of the resource loop.)

3. Audit existing callers of `reconcile_anonymous_identifiers` for the new return value. Most callers ignore the return; only `wiring/mod.rs` consumes it (Task 4).

**Verification**:

```bash
cargo nextest run -p carina-core reconcile_simhash_match_keeps_new_format
cargo nextest run -p carina-core reconcile
```

Both pass. Existing reconciliation tests may fail if they assumed name overwrite — update them to check for rename emission instead.

---

### Task 4: Apply the rename pairs in the wiring layer

**Goal**: Take the `Vec<(old_name, new_name)>` from Task 3 and re-key state entries (`current_states`, `prev_desired_keys`, `saved_attrs`) so the differ sees the resource under its new identifier.

**Files**: `carina-cli/src/wiring/mod.rs`

**Test** (in `wiring` test module):

```rust
#[test]
fn apply_provider_prefix_renames_re_keys_current_states() {
    use carina_core::resource::{ResourceId, State};
    use std::collections::HashMap;

    let old_id = ResourceId::with_provider(
        "awscc",
        "iam.RolePolicy",
        "iam_role_policy_b94fde85",
    );
    let new_id = ResourceId::with_provider(
        "awscc",
        "iam.RolePolicy",
        "awscc_iam_role_policy_b94fde85",
    );

    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    current_states.insert(
        old_id.clone(),
        State {
            id: old_id.clone(),
            identifier: Some("real-aws-id".to_string()),
            attributes: HashMap::new(),
            exists: true,
            dependency_bindings: Default::default(),
        },
    );
    let mut prev_desired_keys: HashMap<ResourceId, Vec<String>> = HashMap::new();
    let mut saved_attrs: HashMap<ResourceId, HashMap<String, carina_core::value::Value>> = HashMap::new();

    let renames = vec![(
        "iam_role_policy_b94fde85".to_string(),
        "awscc_iam_role_policy_b94fde85".to_string(),
    )];

    apply_provider_prefix_renames(
        &renames,
        &mut current_states,
        &mut prev_desired_keys,
        &mut saved_attrs,
    );

    assert!(current_states.contains_key(&new_id), "state should be re-keyed under new identifier");
    assert!(!current_states.contains_key(&old_id), "state should no longer be under old identifier");
}
```

**Implementation**: Add to `carina-cli/src/wiring/mod.rs`:

```rust
/// Re-key state entries when `reconcile_anonymous_identifiers` produced rename
/// pairs (anonymous → anonymous due to identifier-format upgrade).
///
/// For each `(old_name, new_name)` pair, scan each map for a `ResourceId`
/// whose `.name` segment equals `old_name`. Remove that entry, and reinsert
/// it under a `ResourceId` whose `.name` is `new_name` (preserving provider
/// and resource_type).
pub fn apply_provider_prefix_renames(
    renames: &[(String, String)],
    current_states: &mut HashMap<ResourceId, State>,
    prev_desired_keys: &mut HashMap<ResourceId, Vec<String>>,
    saved_attrs: &mut HashMap<ResourceId, HashMap<String, Value>>,
) {
    for (old_name, new_name) in renames {
        rekey_map(current_states, old_name, new_name, |state, new_id| {
            State { id: new_id, ..state }
        });
        rekey_map(prev_desired_keys, old_name, new_name, |v, _| v);
        rekey_map(saved_attrs, old_name, new_name, |v, _| v);
    }
}

fn rekey_map<V, F>(
    map: &mut HashMap<ResourceId, V>,
    old_name: &str,
    new_name: &str,
    transform: F,
)
where
    F: Fn(V, ResourceId) -> V,
{
    let old_keys: Vec<ResourceId> = map
        .keys()
        .filter(|k| k.name_str() == old_name)
        .cloned()
        .collect();
    for old_key in old_keys {
        if let Some(value) = map.remove(&old_key) {
            let new_key = ResourceId::with_provider(
                &old_key.provider,
                &old_key.resource_type,
                new_name,
            );
            let transformed = transform(value, new_key.clone());
            map.insert(new_key, transformed);
        }
    }
}
```

Then update the wiring call site in `carina-cli/src/commands/mod.rs:335` so it captures the rename pairs returned by Task 3's modified `reconcile_anonymous_identifiers_with_ctx` and invokes `apply_provider_prefix_renames` immediately afterward, threading `current_states`, `prev_desired_keys`, and `saved_attrs` through. Note: `compute_anonymous_identifiers_with_ctx` (line 383 of `wiring/mod.rs`) is the wrapper that calls `reconcile_anonymous_identifiers`; its return type also expands to include renames. Locate the corresponding call sites in `carina-cli/src/commands/plan.rs` and `apply.rs` and apply the same pattern.

**Verification**:

```bash
cargo nextest run -p carina-cli provider_prefix_rename
cargo nextest run -p carina-cli wiring
```

Pass. Then run the full carina-cli suite to confirm no regression in unrelated reconciliation tests:

```bash
cargo nextest run -p carina-cli
```

---

### Task 5: Update existing plan-display snapshots to the new format

**Goal**: Every existing snapshot file that contains an anonymous identifier needs its expected name updated. This is mechanical: `cargo insta review` walks each diff and accepts only correct ones.

**Files**: `carina-cli/src/snapshots/*.snap` (multiple)

**Test**: Snapshot tests themselves. Run:

```bash
cargo nextest run -p carina-cli plan_snapshot
```

Expect failures. For each failing snapshot:

1. `cargo insta review` opens an interactive review.
2. For each pending snapshot, verify the only change is the addition of `<provider>_` in front of every anonymous resource name. **Reject** any snapshot where other content has changed (would indicate a regression beyond name format).
3. Accept the snapshot.

Per memory rule "Review snapshots before accepting": **do not** `cargo insta accept` blindly. Each snapshot's diff must be inspected.

**Implementation**: No code change — snapshot acceptance only.

**Verification**:

```bash
cargo nextest run -p carina-cli plan_snapshot
```

All snapshots pass without pending changes.

---

### Task 6: Add `provider_prefix/` fixture and snapshot test

**Goal**: A dedicated fixture whose snapshot makes the new-prefix intent explicit. Future regressions in identifier format are caught by name on this snapshot, not buried in shared fixtures.

**Files**:
- `carina-cli/tests/fixtures/plan_display/provider_prefix/main.crn` (create)
- `carina-cli/src/plan_snapshot_tests.rs` (modify)

**Test** (add to `plan_snapshot_tests.rs`):

```rust
#[test]
fn snapshot_provider_prefix() {
    let fp = build_plan_from_fixture_name("provider_prefix");
    let plan = fp.plan;
    let registry = fp.schemas;
    let mut output = String::new();
    let _ = format_plan(
        &plan,
        DetailLevel::Default,
        Some(&registry),
        None,
        &mut output,
    );
    insta::assert_snapshot!(strip_ansi(&output));
}
```

**Implementation** (`provider_prefix/main.crn`):

```
# Single anonymous resource — Create plan should display
# `awscc_<type>_<hash>` form to confirm provider prefix is applied.

provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.iam.RolePolicy {
  policy_name = 'test-inline'
  role_name   = 'test-role'
  policy_document = {
    version   = '2012-10-17'
    statement = []
  }
}
```

**Verification**:

```bash
cargo nextest run -p carina-cli snapshot_provider_prefix
cargo insta review
# Expected: header line shows
#   + awscc.iam.RolePolicy awscc_iam_role_policy_<8hex>
```

Per memory rule "Review snapshots before accepting": confirm the visible identifier on the header line begins with `awscc_iam_role_policy_` before accepting.

---

### Task 7: Makefile target

**Goal**: Add `plan-provider-prefix` target and register in `plan-fixtures`.

**Files**: `Makefile`

**Implementation**:

```make
plan-provider-prefix:
	$(PLAN_FIXTURE) provider_prefix
```

Add to `plan-fixtures`:

```make
	@echo "=== provider_prefix ==="
	@$(MAKE) plan-provider-prefix
	@echo ""
```

Add `plan-provider-prefix` to the `.PHONY` list.

**Verification**:

```bash
make plan-provider-prefix
make plan-fixtures
```

Both run; the target's output shows `+ awscc.iam.RolePolicy awscc_iam_role_policy_<hash>`.

---

### Task 8: Final verify sweep

**Goal**: Workspace-level health after the change.

```bash
cargo nextest run --workspace        # carina-core touched → workspace-wide
cargo test --workspace --doc
cargo clippy --workspace --all-targets -- -D warnings
ls scripts/check-*.sh | xargs -n1 bash
```

All green. No `dead_code` warnings, no clippy warnings. Per memory rule "Local cargo green ≠ CI green", run the repo's check scripts too.

---

## Out of scope (deferred follow-up)

- **Module-instance prefix verification** (Issue #2419's second part). Once provider prefix lands, re-run the original Issue #2409 reproduction. If the output shows `bootstrap.awscc_iam_role_policy_<hash>`, the module-prefix concern was already handled by `expander.rs:155` and the follow-up issue can be closed without code change. If not, a real bug exists and a separate PR investigates.
- TUI changes (TUI consumes the same `Resource.id.name` and picks up the new prefix automatically).
- State-file format upgrades beyond name re-keying.

## Risks tracked

| Risk | Mitigation |
| ---- | ---------- |
| Snapshot churn obscures unrelated regressions | Task 5's review step is mechanical-but-careful — every `.snap` diff is verified to be ONLY the prefix addition, not other content. |
| Reconciliation rename loses state for resources whose SimHash drifted independently | Existing behavior: those resources already manifest as Create + orphan Delete today. Task 3's change does not regress this. |
| `Resource.id.provider` is unexpectedly empty | Existing early-return at `identifier/mod.rs:392-394` skips the resource entirely, so no empty-prefix identifier is ever produced. |
| Provider name with dots or PascalCase | `pascal_to_snake` handles them. Only ASCII alphanumeric + `_` provider names are valid in the DSL grammar today. |
