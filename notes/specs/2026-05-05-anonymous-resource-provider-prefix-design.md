# Anonymous resource name: provider prefix — design

Issue: [#2419](https://github.com/carina-rs/carina/issues/2419) (provider prefix portion).

## Goal

Make anonymous resource names in `carina plan` output (and state files) include the provider identifier as a prefix, so that operators can tell at a glance which provider produced each anonymous resource.

Today:

```
+ awscc.iam.RolePolicy iam_role_policy_b94fde85
```

After this change:

```
+ awscc.iam.RolePolicy awscc_iam_role_policy_b94fde85
```

The `awscc` provider prefix is added in front of the snake_case resource type and the SimHash digest. This is the smaller of two changes contemplated by Issue #2419; the second change (verifying / fixing module-instance prefixing) is tracked separately.

## Chosen approach

Modify the single name-assembly site in `carina-core/src/identifier/mod.rs` (currently around line 467–473):

```rust
// Before:
let identifier = format!("{}_{}", type_snake, hash_str);

// After:
let provider_snake = pascal_to_snake_components(&resource.id.provider);
let identifier = format!("{}_{}_{}", provider_snake, type_snake, hash_str);
```

`pascal_to_snake_components` here is a no-op for already-snake provider names (`awscc`, `aws`, `my_alias`); it exists only to keep the call site uniform with the existing `type_snake` step.

A second site requires a small change so the rename of state entries stays consistent:

- The module expander (`module_resolver/expander.rs:155`) prefixes `instance_prefix.<inner_name>`. Since `<inner_name>` is the assembled identifier returned by `compute_anonymous_identifiers`, the new provider prefix flows through automatically. A module-internal anonymous resource named `iam_role_policy_b94fde85` becomes `bootstrap.awscc_iam_role_policy_b94fde85` after expansion.
- `reconcile_anonymous_identifiers` (`identifier/mod.rs:570-592`) — the SimHash-distance match path that today copies the *old state's name* back into the new resource's id (line 587-591) — is updated to **keep the freshly-computed new-format identifier** instead of overwriting it with the legacy state name. The state file is then re-keyed to the new identifier (the existing rename detection / state-rewrite plumbing handles this). Resource identity (which physical resource it represents) is preserved via the SimHash distance check; only the display name is normalized to the new format. This avoids the destroy+recreate that would happen if we let the old name stand and treated it as a non-match.

### Why this shape

- **Single mutation site.** The whole behavior change lives in one `format!`. Easy to review, easy to revert.
- **No reconciliation special-case.** The hash is independent of the surrounding name, so backward-compat shims would have to be string-pattern matching (`"^<type_snake>_<hex>$"`). That couples future changes to a regex and adds a permanent compatibility branch. Per project policy ("No backward compatibility — and don't mention it"), we don't add it.
- **Provider prefix is taken from `Resource.id.provider`, not from a config-name resolution step.** The DSL identifier the user typed (`awscc.iam.RolePolicy`) already populates `Resource.id.provider = "awscc"`. Using the config-name (e.g. an alias) would require touching wiring code and isn't aligned with what users see.

## Key design decisions

### D1. Apply only to anonymous resources

Named bindings (`let bootstrap = github { ... }`) keep their user-supplied name verbatim. Provider prefix appears only when `compute_anonymous_identifiers` assembles an identifier — i.e. the `Resource.id.name` was `Pending` at compute time.

### D2. Prefix derives from `Resource.id.provider` directly

No alias resolution, no config lookup. Whatever the parser stored as the provider segment of the resource type (`awscc.iam.RolePolicy` → `awscc`) becomes the prefix.

For provider names that contain dots (none today, but the type system allows it), apply the same `pascal_to_snake` + dot-split logic the type segment already uses, joined by `_`. For the current provider universe (`aws`, `awscc`, `mock`, plus user aliases), this is a no-op.

### D3. Reconciliation normalizes old state names to the new format

`reconcile_anonymous_identifiers` is changed at one point: when the SimHash-distance match succeeds against a state entry, the resource's `id.name` is **kept as the freshly-computed new-format identifier** (`awscc_iam_role_policy_<hash>`) instead of being overwritten with the state's legacy name (`iam_role_policy_<hash>`). The state file's rename plumbing then carries the entry forward under the new key.

This is not a backward-compatibility shim — there is no branch on "is this old format?" The behavior change is uniform: SimHash-distance matches always yield the new-format identifier. Old-format state entries naturally become orphaned strings under the new key, no special parsing required.

Why not the strict alternative (let the SimHash distance match fail and treat old-format entries as non-existent):

- That destroys and recreates every existing anonymous resource on the next `apply` — a real, user-visible side-effect on infra that has nothing to do with the operator's actual intent (which was just renaming a display label).
- "No backward compatibility" applies to *code* (don't keep old read paths alive); it doesn't justify destroying production resources to avoid a 3-line normalization.

A note in the PR body and CHANGELOG documents the user-visible effect: state files written by older Carina versions will be silently re-keyed on the next plan/apply cycle.

### D4. Snapshot churn is expected

Every `carina-cli/src/snapshots/carina_cli__plan_snapshot_tests__snapshot_*.snap` that contains an anonymous resource will need its expected name updated. `cargo insta review` walks them. Per memory rule "Review snapshots before accepting", each pending snapshot is inspected, not blanket-accepted.

## File structure / architecture

| File | Change |
| ---- | ------ |
| `carina-core/src/identifier/mod.rs` | Update the name format string at the assembly site (`format!("{}_{}_{}", ...)`). Add unit tests covering the prefix presence, snake-case conversion, and zero-collision guarantee for same-type-different-provider resources. |
| `carina-cli/tests/fixtures/plan_display/provider_prefix/main.crn` | New fixture: a single anonymous resource whose plan output proves the prefix appears. |
| `carina-cli/src/plan_snapshot_tests.rs` | Add `snapshot_provider_prefix` test function. |
| `carina-cli/src/snapshots/*.snap` | Updated snapshots for every existing fixture that produces anonymous resources. |
| `Makefile` | Add `plan-provider-prefix` target and include in `plan-fixtures` aggregator. |

## Edge cases and constraints

### Anonymous resources with no provider

If `Resource.id.provider` is empty (which `compute_anonymous_identifiers` already early-returns for at line 392–394), no identifier is computed, so no prefix question arises. Existing behavior preserved.

### Module-internal anonymous resources

Order: module expand → anonymous-identifier compute. Module expand prefixes `<binding>.<inner_name>`; at that point `<inner_name>` is `Pending`. After compute, `<inner_name>` becomes `awscc_iam_role_policy_<hash>`, so the final name is `bootstrap.awscc_iam_role_policy_<hash>`. No additional code changes; the prefix flows through automatically.

### Provider name with non-ASCII / dotted chars

Current provider names are `[a-z][a-z0-9_]*`. The DSL grammar (`carina.pest`) doesn't allow dots in provider declarations. If a future provider name contains uppercase letters, apply the same `pascal_to_snake` step used for the type segment. No special handling for non-ASCII because the parser rejects it.

### State file reconciliation

After this change, an `apply` against an existing state file with old-format names goes through the SimHash-distance match path: each old anonymous entry matches its desired counterpart on identity-attribute SimHash distance, and the entry is re-keyed under the new-format name. No destroy+create churn. The user sees a regular plan / apply with no anonymous-resource diffs — only the state file's keys are silently normalized.

If the SimHash distance has *also* exceeded the threshold (i.e. the resource itself was independently edited beyond the rename detector's reach), the resource appears as a Create + an orphan Delete, which is the same behavior the system has today for any out-of-band rename.

### TUI plan view

TUI uses the same `Resource.id.name` via `format_effect_tree`. Display will pick up the new prefix automatically. No code change needed.

## Risks

- **Snapshot churn is large.** Hard to spot a regression among many cosmetic name changes. Mitigation: implement the change in 2 commits — (1) the format change with `cargo insta review` performed once, locking in the new baseline; (2) the new dedicated `provider_prefix` fixture and any subsequent feature work. Reviewers diff each `.snap` only against the `_<type>_<hash>` → `_<provider>_<type>_<hash>` substitution pattern.
- **A user with hand-crafted state files breaks.** Acceptable per project policy. Mention in PR body.
- **Module-prefix issue may not actually be present** (Issue B). After this lands, re-run the original Issue #2409 reproduction. If `bootstrap.awscc_iam_role_policy_<hash>` is what we see, Issue B may turn out to be a false alarm and can be closed without code change. If we still see prefix-less output, Issue B has a real bug to chase.

## Acceptance

The Issue #2409 reproduction's plan output shows `awscc_iam_role_policy_<hash>` (or `bootstrap.awscc_iam_role_policy_<hash>` for module-internal resources) instead of `iam_role_policy_<hash>`. The provider segment is visible at a glance.
