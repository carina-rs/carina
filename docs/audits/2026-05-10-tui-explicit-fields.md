# TUI ExplicitFields Audit (#2903)

<!-- derived-from ../specs/2026-05-10-explicit-fields-design.md -->

Audit conducted as part of the ExplicitFields series (refs awscc#206)
to confirm that the TUI does not bypass the projection introduced in
`build_detail_rows` (PR #2921 / Task 8) by reading `from.attributes`
or other actual-state values directly.

## Methodology

```bash
grep -rn "from\.attributes\|state\.attributes\|\.attributes\[" carina-tui/src/
grep -rn "DetailRow\|build_detail_rows" carina-tui/src/
grep -rn "Resource\|State\|Effect" carina-tui/src/app/mod.rs
```

## Findings

1. **`carina-tui/src/ui/` (detail.rs, diff.rs, tree.rs, value_view.rs,
   help.rs, search.rs, style.rs, mod.rs)**: zero direct accesses to
   `from.attributes` or any field on `State` / `Resource`. All
   rendering consumes `DetailRow` values produced upstream.

2. **`carina-tui/src/app/mod.rs::effect_to_node`** is the single
   bridge from `Effect` to TUI rows. It calls
   `build_detail_rows(effect, schemas, DetailLevel::Full, None, None)`
   — passing `prev_explicit: None`, which matches the contract
   established in PR #2921 (Task 8).

3. **`build_tree_structure`** and **dependency graph builders** in
   `carina-tui/src/app/mod.rs` walk `Resource.attributes` to discover
   `ResourceRef` values for navigation. This is not a state-display
   path — it's structural metadata for the tree view — so projection
   does not apply.

## Conclusion

No code change required for Task 9.

The TUI inherits the projection contract through `DetailRow`. When
`prev_explicit` becomes available end-to-end (a follow-up PR will
thread it through both CLI `format_plan` and TUI `App::new`), the
TUI will benefit automatically without further changes to the `ui/`
modules.

## Out of scope

- Threading `prev_explicit` from `App::new` into `effect_to_node` —
  symmetric to the same change needed in `carina-cli` `format_plan`.
  Both are tracked separately; both are mechanical once the
  caller-side state file is in scope.
