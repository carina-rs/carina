# LSP ExplicitFields Audit (#2906)

<!-- derived-from ../specs/2026-05-10-explicit-fields-design.md -->

Audit conducted as part of the ExplicitFields series (refs awscc#206)
to confirm that the LSP carries no references to the now-removed
`desired_keys` field or the old `Vec<String>` parameter shape that
preceded the `ExplicitFields` recursive tree.

## Methodology

```bash
grep -rn "desired_keys\|prev_desired_keys" carina-lsp/
grep -rn "differ\|find_changed_attributes\|create_plan\|build_explicit\|ExplicitFields" carina-lsp/
```

## Findings

1. **Zero references to `desired_keys` / `prev_desired_keys`** in
   `carina-lsp/`. The flat top-level user-authored key list never
   crossed the LSP boundary.

2. **Zero references to differ entry points** (`differ::diff`,
   `find_changed_attributes`, `create_plan`). The LSP does not
   compute plans — its responsibilities are completion, diagnostics,
   semantic tokens, and code actions, all driven by parsing and
   schema lookups rather than the differ.

3. **`ExplicitFields` is not used in the LSP**, and intentionally so:
   the projection logic is a runtime concern (apply / plan), not a
   diagnostics one. The LSP works at the source level, before any
   actual-state side exists, so there is nothing to project against.

## Conclusion

No code change required for Task 12.

The LSP is structurally insulated from the ExplicitFields rework: it
does not consume state files, run the differ, or render plan rows. The
recursive authoring tree introduced in #2916–#2924 lives entirely in
the planning and rendering paths.

## Out of scope

- Hypothetically, the LSP could surface "this field is a likely
  server-side default — consider not authoring it" hints by reading
  saved-state `explicit` trees, but no such feature is in scope. If
  it ever becomes one, threading `prev_explicit` through diagnostics
  is mechanical.
