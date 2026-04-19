# Unify resource walk across `parsed.resources` and deferred for-bodies

## Goal

Eliminate the structural cause of "checker silently skips for-body resources"
bugs. `ParsedFile` splits parsed resources between two fields:

- `resources: Vec<Resource>` — resources with iterables resolved at parse time.
- `deferred_for_expressions[i].template_resource: Resource` — resources inside
  a `for` body whose iterable resolves later.

Most checkers walk only `resources` and miss the deferred bodies. Each checker
has to remember to also walk `deferred_for_expressions`, and they usually
don't. Concrete fallout:

- #2044 — type / enum checks on attributes inside `for` body are skipped
  entirely; only surface at provider-apply time.
- #1992 Phase 2 (PR #2045) — new upstream_state type-check has the same gap
  on day one.
- Future checkers inherit the same footgun by default.

One unified iterator closes this class of bugs.

## Chosen approach (Option A)

Add `ParsedFile::iter_all_resources()` that yields every resource from both
storage paths, tagged with a context enum so callers can react to deferred-
only quirks (loop-variable placeholders) if they need to.

```rust
pub enum ResourceContext<'a> {
    Direct,
    Deferred(&'a DeferredForExpression),
}

impl ParsedFile {
    pub fn iter_all_resources(
        &self,
    ) -> impl Iterator<Item = (ResourceContext<'_>, &Resource)> {
        self.resources
            .iter()
            .map(|r| (ResourceContext::Direct, r))
            .chain(
                self.deferred_for_expressions
                    .iter()
                    .map(|d| (ResourceContext::Deferred(d), &d.template_resource)),
            )
    }
}
```

Checkers become:

```rust
for (_ctx, resource) in parsed.iter_all_resources() {
    // existing per-attribute checks run uniformly
}
```

A checker that needs deferred-awareness (e.g., to skip loop-variable
placeholders when calling `visit_refs`) matches on `ctx`.

## Design decisions

### Why not a `ResourceVisitor` trait with `pub(crate)` gating (Option C)

- Existing checkers are free functions holding local `Vec<Error>` state.
  Converting every one into a visitor impl promotes locals to struct fields
  and adds ~15 lines of boilerplate per checker.
- `carina-cli` / `carina-lsp` have legitimate direct-`resources` consumers
  (plan display, module resolver, reconciliation, snapshot tests). A
  `pub(crate)` on `resources` forces those consumers to go through a new
  getter method, which re-opens the misuse surface.
- The iterator form with CI-level lint reaches the same correctness bar with
  a fraction of the churn.

### Why return `ResourceContext`

Deferred `template_resource.attributes` hold placeholder strings for loop
variables, e.g. `"__carina_deferred_upstream_value__"`. A checker that runs
`visit_refs` on those values must know the context to avoid false positives
(treating a placeholder as an unknown binding). Making that context part of
the iterator contract rather than per-checker knowledge keeps the awareness
from drifting.

### Leaving `deferred.attributes` untouched

`deferred.attributes` is a `Vec<(String, Value)>` snapshot of public
attributes, populated at parse time (`parser/mod.rs:1996-2001`). Two readers:

1. `check_upstream_state_field_references` — already also walks
   `template_resource.attributes`; its use of `deferred.attributes` is
   redundant.
2. `carina-cli::display::format_deferred_value` — plan display uses the
   flat `Vec<Value>` form to render deferred entries to stdout.

The display consumer is real and not vestigial. The unified iterator ignores
`deferred.attributes` and always visits through `template_resource`. A
follow-up can decide whether to consolidate the two storage forms; it is
out of scope for this design.

### CI lint instead of type-system gating

A `scripts/check-no-direct-resources-access.sh` script, run from
`.github/workflows/ci.yml`, greps for `parsed.resources.iter()` /
`\.resources\.iter()` / direct field access in application code
(`carina-cli/src`, `carina-lsp/src`, and `carina-core/src` outside
explicitly-allowed parser-internal modules).

Legitimate direct accesses are marked with `// allow: direct <reason>` on
the same line, and the grep explicitly ignores those. The list of allowed
reasons is enforced by the script (free-form strings reject). Examples:

- `parser/mod.rs` post-parse resolution (pre-deferred-expansion work)
- `deps.rs::sort_resources_by_dependencies` (topology over resolved refs only)
- plan-time reconciliation in `carina-cli/src/commands/{plan,apply,destroy}.rs`
- fixture tests inspecting parser output directly

### Scope: which checkers migrate now

In-scope for the main migration PR:

- `carina-core/src/upstream_exports.rs`:
  - `check_upstream_state_field_references` (already walks deferred via its
    own ad-hoc path; collapse to the iterator + remove the redundant
    `deferred.attributes` walk).
  - `check_upstream_state_field_types` (introduced by PR #2045).
- `carina-core/src/validation.rs`:
  - `check_unused_bindings`.
  - `validate_resource_ref_types` — called from
    `carina-cli::wiring::validate_resource_ref_types_with_ctx` which
    currently passes `&parsed.resources`. Either pass the full iterator, or
    have the CLI wiring flatten it before passing.
  - `validate_resources` (same pattern).
- `carina-lsp/src/diagnostics/`:
  - Attribute / enum checks that run over `parsed.resources` today
    (`diagnostics/mod.rs:179-189`, `diagnostics/checks.rs:803-1401`).

Out of scope — stays on direct `parsed.resources`:

- `parser/mod.rs` internal resolution passes (pre-expansion).
- `deps.rs::sort_resources_by_dependencies` (topological sort over
  resolved resources only).
- Module resolution in `module.rs` and `module_resolver/mod.rs`
  (TOPOLOGY; handles module expansion separately).
- Plan / apply / destroy / state reconciliation and backend checks in
  `carina-cli/src/commands/*.rs` — these operate on post-expansion
  resources.
- Fixture / snapshot tests.

Each stays as a direct access with a `// allow: direct <reason>` marker so
the lint passes and the reviewer sees the intent.

## File structure

- New: `ParsedFile::iter_all_resources` + `ResourceContext` enum in
  `carina-core/src/parser/mod.rs` (alongside `ParsedFile` definition).
- New: `scripts/check-no-direct-resources-access.sh` + CI job.
- Modified: each checker listed above.

## Edge cases

- A checker that calls `visit_refs` on a deferred template's attribute will
  see placeholder strings in `ResourceRef` positions (e.g.
  `ResourceRef { path: "__carina_deferred_upstream_value__" }`). The
  `ResourceContext::Deferred` tag lets the checker filter these out (or the
  helper `check_ref_against_type` can do it once, sharing the rule).
- A deferred body can reference the loop binding itself (`account_id`). The
  binding is not in `parsed.resources` / `parsed.variables`; checkers need
  to consult `deferred.binding` to know which identifier is loop-bound and
  skip it. This is the same lookup that existing deferred-aware code does.
- `deferred.template_resource.id.name` carries a `[?]` placeholder
  (`_for0[?]`). Error-message formatting should handle this rather than
  display the raw placeholder to users.

## Non-goals

- No change to `parser::parse` behavior or deferred-expansion algorithm.
- No change to `deferred.attributes` storage.
- No change to error-surface wiring (CLI / LSP paths); each checker's output
  surface stays exactly where it is today.
- No retroactive fix for #1992 Phase 3 (loop-variable type inference).

## Testing

- Unit test: `iter_all_resources` yields direct then deferred in order,
  with correct `ResourceContext` tags.
- Migration regression: each migrated checker gets one new test proving it
  now fires inside a for body (mirrors #2044's repro).
- CI lint: a probe file with `parsed.resources.iter()` without the allow
  marker must fail the script; the same line with the marker must pass.
