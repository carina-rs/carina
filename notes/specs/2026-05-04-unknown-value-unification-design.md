# Unknown Value Unification: a Single `Value::Unknown` Variant

<!-- derived-from ../../README.md -->
<!-- constrained-by ./2026-04-14-exports-design.md -->

## Goal

Replace the three parallel "value not known at plan time" mechanisms in
carina with a single typed `Value::Unknown` variant. Eliminate
stringly-typed sentinels that travel through the `Value` tree, give the
type system control of the plan/apply boundary, and prevent the next
person who adds an "unknown value" source from creating a fourth
parallel mechanism.

Resolves #2371. Sets up the rendering convergence in #2370 and removes
the marker class of leak surfaced by #2369.

## Background

Three mechanisms exist today, each correct in isolation, none aware of
each other:

| # | Marker | Where created | Where displayed |
| - | ------ | ------------- | --------------- |
| 1 | `(known after apply)` | hard-coded literal injected at display time (no `Value` representation) | `carina-cli/src/display/mod.rs:99,1072,1110,1177,1683,1746` |
| 2 | `(known after upstream apply)` (+ `_KEY` / `_INDEX` siblings) | parser stamps `Value::String("(known after upstream apply...)")` for unresolved for-expression iterables (`carina-core/src/parser/expressions/for_expr.rs`) | `format_deferred_value` (`carina-cli/src/display/mod.rs:55`) |
| 3 | `(known after upstream apply: <ref>)` (NUL-prefixed sentinel) | resolver stamps `Value::String("\0upstream_unresolved:<ref>")` for plain attributes whose upstream is unresolved (`carina-core/src/resolver.rs::stamp_unresolved_upstream`, added in #2367) | `format_value_with_key` decode arm (`carina-core/src/value.rs:194`) |

#1 lives only in display code and never enters the `Value` tree, so it
is **out of scope** for this work. Mechanisms #2 and #3 both inject
`Value::String` sentinels that consumers must recognize via string
matching, and that have to be defended against at every serialization
boundary (#2367's `check_no_unresolved_upstream_in_plan`, plus the gaps
filed as #2369).

The asymmetry between #2 (existed) and #3 (didn't) is what produced
issue #2366 in the first place. Adding #3 closed that asymmetry, but
the architecture that allowed it to exist — two parallel `Value::String`
encoding schemes with no shared abstraction — is unchanged.

## Constraints

The following are non-negotiable; the design enforces them through the
type system rather than through code review or documentation:

1. **`apply` never sees an unknown value.** The plan path is the only
   producer; any `apply` consumer that encounters one is a bug, not a
   normal control-flow case.
2. **State files never carry unknown values.** Persisting one would
   leave a stale "still unknown" marker that the next plan would have
   to filter out.
3. **Provider plugins (WASM) never receive unknown values.** A
   provider call with an unknown attribute is undefined behavior at the
   protocol level.
4. **Mechanism #1 (`(known after apply)`) is out of scope.** It is a
   display-only string with no `Value` representation; the unification
   does not need to touch it.
5. **No backward compatibility.** Existing `state.json` and `plan.json`
   formats may change. (The current sentinel-bearing values are
   plan-display only and never persisted under #2367's guards, so this
   is a no-op for state; `plan.json` already errors when a marker is
   present.)
6. **`Value` enum signature change is permitted.** Provider crates
   (`carina-provider-aws`, `carina-provider-awscc`) will need
   matching updates.

Constraints 1–3 are what justify the type-system approach: with
`Value::Unknown` as a first-class variant, every `match value` site is
forced by the compiler to handle (or explicitly reject) the unknown
case. Today the same guarantee is "I hope every consumer remembers to
check the sentinel string."

## Design

### Value enum addition

```rust
pub enum Value {
    String(String),
    Int(i64),
    // ... existing variants ...
    Unknown(UnknownReason),
}

/// The reason a value is not known at plan time. Each variant carries
/// only the data its own consumer needs — no shared "context" field
/// invites drift.
pub enum UnknownReason {
    /// The value is a reference into an `upstream_state` binding that
    /// did not resolve at plan time (state file missing, or the
    /// referenced export was absent). Rendered as
    /// `(known after upstream apply: <binding>.<attribute_path>)`.
    UpstreamRef {
        binding: String,
        attribute_path: String,
    },
    /// Map-binding key in a deferred for-expression
    /// (`for (k, _) in iterable`). Substituted with the actual key
    /// when the iterable is later resolved.
    ForKey,
    /// Indexed-binding index in a deferred for-expression
    /// (`for (i, _) in iterable`). Substituted with the actual index.
    ForIndex,
    /// Loop-variable value in a deferred for-expression
    /// (`for v in iterable`). Substituted with the actual element.
    ForValue,
}
```

Future unknowns (e.g. `data` source not yet read, conditional branch
not taken) add new `UnknownReason` variants. The compiler then forces
every `match reason { ... }` site to handle them.

### Display

A single helper in `carina-core/src/value.rs` renders any
`UnknownReason`:

```rust
pub fn render_unknown(reason: &UnknownReason) -> String {
    match reason {
        UnknownReason::UpstreamRef { binding, attribute_path } => {
            format!("(known after upstream apply: {}.{})", binding, attribute_path)
        }
        UnknownReason::ForKey => "(known after upstream apply: key)".to_string(),
        UnknownReason::ForIndex => "(known after upstream apply: index)".to_string(),
        UnknownReason::ForValue => "(known after upstream apply)".to_string(),
    }
}
```

`format_value_with_key` matches `Value::Unknown(reason)` and returns
`render_unknown(reason).dimmed()` so the wording **and** the dimmed
style are uniform across mechanisms #2 and #3 (resolves #2370). The
old `format_deferred_value` collapses into the main formatter.

### Producer-side migration

- **#3 → `Value::Unknown(UpstreamRef{...})`.** `resolver.rs::stamp_unresolved_upstream`
  no longer goes through `encode_unresolved_upstream_marker`; it
  builds `Value::Unknown(UpstreamRef { binding: path.binding().to_string(), attribute_path: path.attribute_path_dot_string() })`
  directly. The `UPSTREAM_UNRESOLVED_MARKER_PREFIX` constant and the
  `encode_/decode_unresolved_upstream_marker` helpers are removed.
- **#2 → `Value::Unknown(ForKey/ForIndex/ForValue)`.** The for-expression
  parser stamps the appropriate variant. Iterable-resolution code
  (`replace_placeholder` family in `parser/ast.rs` and
  `parser/expressions/for_expr.rs`) switches from string match to
  `match Value::Unknown(reason) => ...` — substitute key/index/value
  by reason variant rather than by sentinel string.

### Consumer-side enforcement

Every `match value { ... }` site is updated:

- **Display paths** (`format_value_with_key`, `format_deferred_value`,
  TUI value rendering): handle `Value::Unknown` via `render_unknown`.
- **Serialization paths** (`value_to_json`, `dsl_value_to_json`,
  state writeback, provider WASM marshalling): return `Err` when
  `Value::Unknown` appears. These are the constraint-1/2/3 enforcement
  points; today they are protected only by the guard in
  `check_no_unresolved_upstream_in_plan`. Pushing the check to the
  type-aware boundary makes the guard redundant for the marker class.
- **Resolver / differ / planner**: pass through unchanged where the
  variant is opaque; reject (or ignore) where appropriate.

### Removed surface

After full migration:

- `parser::ast::DEFERRED_UPSTREAM_PLACEHOLDER`,
  `DEFERRED_UPSTREAM_KEY_PLACEHOLDER`,
  `DEFERRED_UPSTREAM_INDEX_PLACEHOLDER`
- `parser::ast::UPSTREAM_UNRESOLVED_MARKER_PREFIX` and the
  `encode_/decode_unresolved_upstream_marker` helpers
- `format_deferred_value` in `carina-cli/src/display/mod.rs`
- `check_no_unresolved_upstream_in_plan` in `carina-cli/src/commands/plan.rs`
  (the corresponding constraint moves to the serialization layer where
  the type system enforces it)

## Migration plan (staged)

Each stage leaves the workspace green and the test suite passing. No
"big bang" — `Value` enum changes ripple to many `match` sites and a
single PR would be unreviewable.

### Stage 1 — Add the variant

Add `Value::Unknown(UnknownReason)` and the `UnknownReason` enum to
`carina-core`. Update every existing `match value { ... }` site with
an explicit `Value::Unknown(_) => unreachable!("not yet produced")`
arm so the compiler accepts the change. No producer creates the
variant yet; no consumer relies on seeing it. Tests for the helper
constructors and `render_unknown` go in `carina-core/src/value.rs`.

This stage is mechanical and primarily about establishing the type
shape.

### Stage 2 — Migrate mechanism #3

Replace `stamp_unresolved_upstream`'s `Value::String(encoded)` output
with `Value::Unknown(UnknownReason::UpstreamRef {...})`. Remove
`UPSTREAM_UNRESOLVED_MARKER_PREFIX` and the `encode_/decode_*` helpers.
Update `format_value_with_key` to render `Value::Unknown` via
`render_unknown`. Update the two `format_value_with_key`-adjacent
formatters that previously checked the sentinel.

`unimplemented!()` arms added in stage 1 for display and persistence are
replaced with real handling (display: `render_unknown`; persistence:
the existing `unimplemented!()` stays — it is promoted to `Err` in
stage 4, not stage 2).

`check_no_unresolved_upstream_in_plan` is downsized — it now exists
only as a defense check that calls into the persistence-layer
`unimplemented!()` arms; the recursive Value walk is no longer needed
because the type system guarantees the encoding cannot leak from a
`String`.

Existing tests in `carina-cli/src/plan_snapshot_tests.rs`
(`plan_snapshot_upstream_state_unresolved`,
`plan_snapshot_upstream_state_empty_exports`) should continue to pass
unchanged — same display output, different internal representation.

#### Stage 2 also includes the WASM provider boundary (lesson from #2375)

The first stage-2 attempt (issue #2375, abandoned without merging)
exposed a gap in the original RFC: the WASM provider boundary
(`carina-plugin-host/src/wasm_convert.rs`) is hit during plan-time
**`PlanPreprocessor::prepare`**, which calls
`ProviderNormalizer::normalize_desired` to canonicalize attribute
shapes. That call chain reaches `core_to_wit_value` — which, after
stage 2's producer migration, sees `Value::Unknown` for unresolved
upstream refs even on the plan path.

The original RFC's assumption "providers never see `Value::Unknown`"
held only for `apply` (where a strict resolver runs). It does **not**
hold for plan-time normalization. Three options were considered:

1. **Provider-side handling**: pass `Unknown` through `core_to_wit_value`
   as a placeholder string and let the provider's normalize hook
   round-trip it. The first attempt at #2375 took this route and it
   broke the plan display: the provider returns the placeholder as-is
   in the resource it sends back, so the user sees
   `vpc_id: "Unknown(UpstreamRef { … debug-format … })"` instead of
   `(known after upstream apply: …)`. The display layer cannot recover
   the typed `Value::Unknown` from a string the provider has touched.
2. **Skip Unknown-bearing resources at the normalize boundary**: omit
   `Value::Unknown` attributes from the WIT payload, run the provider's
   normalizer on the remaining attributes, then re-attach the `Unknown`
   values to the result. This keeps plan-time display correct without
   exposing the type to the provider plugin.
3. **Defer the problem to stage 4**: leave the stage-1 `unimplemented!()`
   arm panicking and treat all `plan` runs against unresolved upstreams
   as broken until stage 4 lands. Unacceptable — `carina plan` against
   unresolved upstreams must work as it did under #2367.

Stage 2 takes **option (2)**: the producer migration is paired with a
`PlanPreprocessor` change that strips `Value::Unknown` from each
resource's attribute map before `normalize_desired`, runs the
normalizer on the remaining concrete attributes, then re-merges the
`Unknown` values into the normalized result. The WASM boundary's
`Value::Unknown(_) => unimplemented!(...)` arm stays in place — it is
the type-level guarantee that this preprocessing actually happens.

This is a mechanical refactor in `carina-cli/src/wiring/mod.rs`; the
provider plugin contract does not change. A regression test that runs
the existing `upstream_state_unresolved` fixture through
`PlanPreprocessor::prepare` (or through the real plan path with a
mock provider) and asserts `Unknown` survives the round trip must be
added in this stage.

### Stage 3 — Migrate mechanism #2 (3 siblings)

Replace `Value::String(DEFERRED_UPSTREAM_*_PLACEHOLDER)` with the
matching `Value::Unknown(UnknownReason::ForKey/ForIndex/ForValue)`.
Rewrite `replace_placeholder` to match on `Value::Unknown(reason)`.
`format_deferred_value` is deleted — `format_value_with_key` covers
its cases via `Value::Unknown`.

Display style and wording converge on the same canonical form
(resolves #2370). The for-expression placeholder for the loop
variable was previously `(known after upstream apply)` (no colon,
no ref) — the new rendering is the same string for the `ForValue`
variant, so the for-expression snapshot is unchanged.

The `KEY` / `INDEX` siblings render unchanged (their literal strings
were `(known after upstream apply: key)` / `... : index)` and
`render_unknown` produces the same).

### Stage 4 — Type-enforced constraints

Make the unreachable!()-replaced-by-Err pattern from stage 2 the
type-level invariant for **all** serialization boundaries. Each
location that took a `Value` and produced a non-display output (state
JSON, plan JSON, provider WASM) returns `Err(...)` on
`Value::Unknown`.

This subsumes most of #2369:

- `value_to_json` for `ResourceRef` falling back to `"${...}"` raw
  string is replaced with explicit `Err` for unresolved refs (the
  caller should have substituted) and the marker class is now caught
  by `Value::Unknown` exhaustively.
- `dsl_value_to_json` silent `_ => None` drop becomes either an
  explicit warn or an `Err` depending on whether the dropped variant
  is `Value::Unknown` (impossible in resolved exports — `Err`) or a
  legitimate non-serializable variant.

After stage 4, removing any `Value::Unknown(_) => Err(...)` arm on a
serialization path is a compile error.

## Edge cases

- **Cascade ref hints in `Effect::Replace::cascade_ref_hints`**: these
  are `(String, String)` already (attribute name → original ref dot
  string), not `Value`. They are display-only strings, not affected by
  the `Value::Unknown` change.
- **State file diff display**: `Value::Unknown` should never appear in
  state-derived `Value`s (constraint 2). The state-comparison display
  paths get a defensive `Value::Unknown(_) => unreachable!()` so the
  invariant is asserted.
- **for-expression replay during apply**: when the iterable becomes
  available at apply time, the for-expression replays — substituting
  `Value::Unknown(ForKey)` with the actual key, etc. The substitution
  logic moves from string-match to enum-match. Order of substitution
  is unchanged.
- **Mixing #2 and #3 in one .crn**: a config that uses both
  `for x in network.accounts` and `vpc_id = network.upstream_value`
  produces `Value::Unknown` of different reason variants in the same
  resource tree. Display handles each via the same `render_unknown`,
  so the user sees consistent dimmed output. Warnings are emitted
  once per upstream binding regardless of which mechanism triggered
  the unresolved state (#2370 follow-up; this design enables it
  without dictating the warning-dedup logic).

## Risks and mitigations

- **Risk**: `Value` enum additions break provider crates
  (`carina-provider-aws`, `carina-provider-awscc`) on the next
  dependency bump. **Mitigation**: stage 1's `unreachable!()` /
  ignore arms are mechanical to add. Coordinate the dependency bump
  in those repos as a follow-up; until then the provider crates pin
  to the pre-stage-1 commit.
- **Risk**: stage 4's `Err`-on-`Value::Unknown` changes serialization
  function signatures from infallible to fallible. **Mitigation**:
  many of these already return `Result`. For the ones that don't,
  the change is local to the serialization layer; consumers either
  unwrap (provider call sites that have already been gated) or
  propagate the error.
- **Risk**: a future contributor adds a new `Value` variant that
  needs unknown-aware semantics and forgets the `Unknown` arm.
  **Mitigation**: the `unreachable!()` / `Err` arms added at every
  match site are visible in code review as "this is the unknown
  policy here." A grep for `Value::Unknown` lists every consumer
  position.

## Out of scope

- Mechanism #1 (`(known after apply)` for self-resource computed
  attrs). It does not exist in the `Value` tree and the unification
  does not need it. A future "all unknowns are typed" cleanup could
  consider promoting it, but it is independent.
- The plan-strictness questions in #2368 (other plan paths that should
  warn rather than error). Independent axis.
- Warning deduplication logic itself (consolidating per-binding
  warnings emitted by the loader and by the for-expr expander). The
  design enables it but does not specify it; that is a #2370 detail.

## Acceptance

- A short RFC-style discussion (this document) merged into
  `docs/specs/`.
- A follow-up PR per stage. Stage boundaries are real branch points;
  do not attempt a single PR.
- Each stage's PR includes regression coverage for its specific
  migration, plus the unreachable/err arms added at every match site.

After stage 4: `grep -r 'UPSTREAM_UNRESOLVED' carina-core carina-cli`
returns nothing, `grep -r 'DEFERRED_UPSTREAM_' carina-core carina-cli`
returns nothing, and adding a new `UnknownReason` variant is a one-line
change in `carina-core/src/value.rs` plus the compiler-flagged consumer
updates.
