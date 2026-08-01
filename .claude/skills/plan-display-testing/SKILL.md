---
name: plan-display-testing
description: Fixture-based testing for Carina plan display code (display.rs, carina-tui). Use when modifying plan output rendering, adding plan display features, or updating insta snapshots.
---

# Plan Display Testing

When modifying plan display code (`display.rs`, `carina-tui`), use
fixture-based testing:

```bash
# Visual confirmation with fixture data (no AWS needed)
make plan-all-create      # All resources new (Create only)
make plan-mixed           # Mixed: Create + Update + Delete
make plan-delete          # Orphan resource deletion
make plan-compact         # Compact mode
make plan-mixed-tui       # TUI mode
make plan-fixtures        # Run all patterns

# Snapshot tests (automated, runs in CI)
cargo nextest run -p carina-cli plan_snapshot
```

Fixture files are in `carina-cli/tests/fixtures/plan_display/`. Each
directory contains a `.crn` file and optionally a `carina.state.json`
(state v3 with binding/dependency_bindings). When adding new plan display
features, add a fixture and snapshot test to cover the new behavior.

When plan output changes intentionally, update snapshots with
`cargo insta review` (interactive) or `cargo insta accept` (accept all).
If snapshots are not updated after a display change, CI fails on the
`Test` job.

**Review snapshot diffs before accepting them** — `cargo insta accept`
blindly blesses whatever the code currently produces, including a
regression.
