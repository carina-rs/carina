# `canonicalize_with_type` Must Recurse Through `Union`: Design

<!-- constrained-by ./2026-05-16-schema-aware-detail-rows-design.md -->

## Goal

Make `value::canonicalize_with_type` recurse through
`AttributeType::Union` members so that a `Union[String, list(String)]`
(`string_or_list_of_strings`) field nested **inside a Union-typed
container** is canonicalized to `ConcreteValue::StringList` on both the
desired and the actual side *before the differ* — restoring the
existing #2481/#2511/#2513 canonicalization invariant for that nesting,
instead of letting the non-canonical `String` vs `List([String])` mix
reach `type_aware_equal` and render a never-converging phantom diff.

Trigger: carina#3080. `aws.s3.BucketPolicy`'s
`policy.statement[].principal.service` is schema-typed
`Union[String, List<String>]`, but its enclosing `principal` is
`Union[Struct{ service: Union[String, List<String>], … }, String]`
(`string_or_principal_struct()`). `carina plan` shows:

```
~ service: ["cloudfront.amazonaws.com"] → "cloudfront.amazonaws.com"
```

— state holds the aws-read singleton list, desired holds the user's
bare scalar, every plan, never converging.

## The real root cause (verified)

carina-core **already** canonicalizes `Union[String, list(String)]` to
`ConcreteValue::StringList` on both sides upstream of the differ
(`value::canonicalize_resources_with_schemas` #2511 /
`canonicalize_states_with_schemas` #2513, wired into the real
plan/apply pipeline). `differ/comparison.rs:28-47` documents this as an
**enforced invariant** and *explicitly prohibits* a comparator-level
`"x" == ["x"]` special-case ("Special-case equality at the comparator
hides the divergence … exactly the phantom-diff regression that #2481
set out to eliminate"). So the bug is **not** in the differ — it is
that one side escaped canonicalization.

`canonicalize_with_type` (`value.rs:1202-1244`) recurses:

```
List   → recurse inner
Map    → recurse value-type
Struct → recurse each field's type
Secret → recurse inner
(v, _) => v          // ← catch-all: STOP
```

There is **no `Union` arm**. `principal`'s declared type is
`Union[Struct{…}, String]`. When the canonicalizer reaches `principal`
it matches the `(v, _) => v` catch-all and **stops**, never descending
into the `Struct` member, so `principal.service`'s
`Union[String, List<String>]` is never canonicalized. Its desired side
stays `String("…")`, its actual side stays the aws-read
`List([String("…")])`; the non-canonical mix reaches the differ; the
invariant's own warning fires. (`json_to_principal`'s singleton-list
wrap in the aws provider is **not** the bug — it is a legitimate
provider shape that the canonicalizer is supposed to fold; the
canonicalizer simply never reaches it.)

## Chosen design

Add a `Union` arm to `canonicalize_with_type` that recurses into the
member whose **declared shape best matches the value's shape**, then
lets the existing arms do their job. **Member selection reuses the
existing, already-tested `union_member_score` (`schema/mod.rs:1465`,
#2219)** rather than introducing a second, parallel shape predicate:

```rust
// Union: the value conforms to one member shape (IAM/Cargo unions are
// shape-disjoint — Struct vs String, String vs List). Pick the
// structurally-closest member with the SAME ranking validate_union
// already uses, then re-dispatch so nested string_or_list_of_strings
// fields are still canonicalized. Reusing union_member_score keeps the
// canonicalizer's member choice and the validator's error-attribution
// member choice provably identical (one ranking, one source of truth).
(val, AttributeType::Union(members)) => {
    match select_union_member(members, &val) {  // wraps union_member_score
        Some(m) => canonicalize_with_type(val, m),  // re-dispatch
        None => val,                                 // identity (safe)
    }
}
```

**Why reuse `union_member_score` instead of a new
`value_shape_matches`.** The first cut of this design proposed a fresh
`value_shape_matches(&Value, &AttributeType) -> bool` predicate. The
codebase already owns the exact judgement that predicate would
re-implement: `union_member_score(member, ConcreteValueRef)` ranks a
Union member's structural distance from a runtime value
(Map↔Struct=100, same-constructor=80, List↔List with inner peek,
`Custom` defers to `base`, nested `Union` recurses) and `validate_union`
uses it to pick which member a value "is". A second predicate with the
same job is a **drift hazard**: the canonicalizer could fold a value
into member *X* while the validator attributes it to member *Y*, and
the two would silently disagree — the same class of split-source bug
(`feedback_state_enum_phantom_diff_is_core_not_provider`,
`feedback_unit_test_path_is_not_apply_path`) this project has been
burned by. `select_union_member` is a thin total wrapper over the
existing scorer: project `val` through the existing `as_concrete()`
(the same projection `validate_union` uses), score every member, return
the strict-max member (declaration order breaks ties, exactly as
`validate_union`), or `None` when no member shares any structure. No
new shape table is invented.

For carina#3080: `principal` value is a `Map` → matches the `Struct`
member → re-dispatch → existing `Struct` arm recurses fields →
`service` is `Union[String, List<String>]` → its value (`String` or
`List`) matches the union, re-dispatch → `is_string_or_list_of_strings`
fires → `StringList`. Both sides converge to `StringList(["…"])`
*before the differ*; the invariant is upheld, not bypassed.

### Why not the comparator special-case (rejected)

That is what `comparison.rs:42-47` explicitly forbids: it would leave
state files recording the non-canonical shape, so the diff keeps
firing on the next run (the #2481 regression). The first cut of this
design proposed exactly that; fact-checking against the in-code
invariant rejected it. Canonicalization-layer recursion is the
codebase's *sanctioned* mechanism for this exact class.

### Why not patch the aws read path (rejected)

`json_to_principal`'s singleton-list wrap is a valid provider shape;
`Union[String, list(String)]` *means* "either spelling is acceptable",
and the canonicalizer's job is to fold both. A per-provider carve-out
is the awscc#255/#3073 anti-pattern; the canonicalizer is the correct,
provider-agnostic layer (it already handles List/Map/Struct nesting —
Union is the one missing nesting kind).

## Alternatives considered

| Approach | Verdict |
|---|---|
| **A: Union arm reusing `union_member_score` for selection (chosen)** | Fixes the root, upholds the #2481 invariant, provider-agnostic, mirrors the existing List/Map/Struct recursion, and reuses the validator's existing member-ranking so canonicalizer and validator can't drift. |
| A′: Union arm with a *new* `value_shape_matches` predicate | **Rejected** (was the first cut of A) — duplicates `union_member_score`'s judgement; two predicates with the same job drift silently (canonicalizer folds into member X, validator attributes Y). Single-source the ranking instead. |
| B: Comparator `"x"==["x"]` special-case in `type_aware_equal` Union arm | **Rejected** — explicitly prohibited by `comparison.rs:28-47`; masks the phantom, state stays non-canonical, re-fires next run. |
| C: aws read path emits scalar when AWS returned scalar | **Rejected** — per-provider carve-out; `Union[String,list]` legitimately accepts either shape; doesn't fix awscc or future providers. |
| D: Canonicalize *all* Union members unconditionally then pick | **Rejected** — wasteful and ambiguous (which canonicalized form wins?); shape-directed re-dispatch is precise. |

## Blast radius

- **One function changed + one thin wrapper:** `canonicalize_with_type`
  (`value.rs`) gains a `Union` arm; `select_union_member` is a small
  total wrapper over the **already-existing** `union_member_score`
  (`schema/mod.rs`, #2219) — no new shape-matching logic is authored.
  No signature change → `canonicalize_resources_with_schemas` /
  `canonicalize_states_with_schemas` and their ~5 pipeline call sites
  (`wiring/mod.rs`, `commands/apply/mod.rs`, `fixture_plan.rs`) are
  unchanged.
- **No comparator/renderer/provider/state-format change.** The differ
  and the `comparison.rs` invariant are untouched (the fix makes the
  invariant *hold* for this nesting).
- **Behavioral surface:** any attribute whose schema nests a
  canonicalizable type (`string_or_list_of_strings`, or List/Map/Struct
  thereof) **inside a `Union`**. In the AWS providers that is IAM
  policy `principal`/`not_principal` (`Union[Struct, String]`) and any
  future Union-wrapped struct. Previously-non-canonicalized nested
  fields now canonicalize, so a scalar/singleton pair that rendered a
  phantom now correctly shows no change. A *genuine* difference still
  shows (both sides canonicalize to `StringList`, then compare by
  value).
- **Risk of over-canonicalization:** member selection must not pick a
  member that would *wrongly* coerce (e.g. a `Map` value must not
  select a `String` member). This is bounded by reusing
  `union_member_score`, which already scores `(String, Map)` and other
  cross-constructor pairs as `0` (the `_ => 0` arm) — a `Map` value can
  never out-score its way into a `String` member. The `None` (all
  members score 0) arm is identity (safe fallthrough, today's
  behavior). No new coercion surface is introduced because no new
  predicate is authored.

## Type safety

This section is load-bearing, not a footnote: the project's standing
guidance is to prove invariants in the type system and single-source
shared judgements rather than re-deriving them with parallel runtime
predicates.

1. **One ranking, one source of truth (no drift by construction).**
   The canonicalizer's "which member is this value?" decision and the
   validator's "which member's error do I surface?" decision are now
   the *same function call* (`union_member_score`). It is structurally
   impossible for `canonicalize_with_type` to fold a value into member
   *X* while `validate_union` believes it is member *Y* — there is one
   ranking, not two that must be kept in sync by review. The rejected
   A′ (`value_shape_matches`) reintroduced exactly the split-source
   shape this project has repeatedly been burned by
   (`feedback_state_enum_phantom_diff_is_core_not_provider`); A closes
   that off at the type level by not creating the second predicate.

2. **Total over `AttributeType`, no `unreachable!`/`panic!`.**
   `select_union_member` returns `Option<&AttributeType>`; the `None`
   case is a real, handled value (identity re-dispatch), not a
   `debug_assert!`/`unreachable!` escape
   (`feedback_type_safety_over_runtime_checks`). Every `AttributeType`
   member variant is already exhaustively handled by
   `union_member_score`'s `match` (compiler-enforced exhaustiveness);
   adding a future `AttributeType` variant forces an update there, and
   the canonicalizer inherits it for free.

3. **`None` is observable, not silent.** The non-canonicalizing path
   (no member shares structure with the value) is the *same* condition
   `validate_union` already treats as `TypeError::TypeMismatch`. The
   implementation must add a `debug_assert!`-free invariant test (Test
   plan item 5) asserting that for the carina#3080 schema the value
   *does* select a member — so a regression where the value stops
   matching surfaces as a failing canonicalization-invariant test, not
   a silently-skipped fold that reappears as a phantom diff months
   later. (`feedback_unit_test_path_is_not_apply_path`: the test must
   exercise the real `canonicalize_*_with_schemas` entry, not call the
   `Union` arm directly.)

4. **Termination is type-structural, not runtime-guarded.** Re-dispatch
   recursion (`Union → member → possibly inner Union`) terminates
   because each step strips one `AttributeType` constructor — the
   recursion is well-founded on the strictly-decreasing type structure,
   the same argument the existing List/Map/Struct arms rely on. No
   depth counter or visited-set runtime guard is needed; if a future
   change could introduce a cyclic `AttributeType`, that is a type-level
   defect to fix at the schema, not to paper over here.

## Test plan

1. **Unit (`value.rs` tests):** `canonicalize_with_type` on
   `Union[Struct{ service: Union[String,List<String>] }, String]` with
   a `Map{ service: String("x") }` → `Map{ service: StringList(["x"]) }`;
   with `service: List([String("x")])` → same; the `String`-member
   value (a bare `String`) under the same Union passes through
   unchanged; a `Union` with no shape-matching member → identity.
   Mirror the existing `canonicalize_recurses_into_struct_fields`
   test (`value.rs:2967`) for the Union nesting.
2. **Differ parity:** with the schema registered,
   `type_aware_equal`/`find_changed_attributes` on the carina#3080
   `principal.service` shape (scalar vs singleton) → **equal** (because
   both sides are now `StringList` before the differ — *not* because of
   any comparator change).
3. **Renderer:** an `Effect::Update` whose only diff is
   `principal.service` scalar-vs-singleton → zero rows; genuine sibling
   change still renders; no phantom sub-row.
4. **Repro / non-regression of the invariant:** add a test asserting
   that after `canonicalize_*_with_schemas` the carina#3080 value is
   `StringList` on both sides (the invariant holds), so the
   `comparison.rs:28-47` "non-canonical reaching the differ is a bug"
   contract is satisfied rather than worked around. Must call the real
   `canonicalize_*_with_schemas` pipeline entry, not the `Union` arm
   directly (`feedback_unit_test_path_is_not_apply_path`).
5. **Member-selection invariant (type-safety guard):** assert
   `select_union_member` picks the `Struct` member for the carina#3080
   `principal` `Map` value and the list/string member for `service`,
   and the *negative*: a `Map` value never selects the `String` member
   of `Union[Struct, String]` regardless of declaration order. This
   pins the property that makes `None` truly unreachable for the real
   schema, so a future schema/scorer change that breaks selection fails
   loudly here instead of silently skipping the fold.

## PR sequence (design-before-implementation)

1. **This design PR** (`notes/specs/…` only) — merges first.
2. **Implementation PR** — the `Union` arm + `select_union_member`
   (thin wrapper over the existing `union_member_score`) + tests,
   `Closes #3080`.
3. carina-provider-aws / -awscc: routine carina-core pin bump (the
   pin-staleness guard added in carina-provider-aws#332 /
   carina-provider-awscc#256 enforces the minimum rev).

## Risks / open questions (resolve in implementation)

- **Selection precision is inherited, not re-derived.** Because
  selection reuses `union_member_score`, the "shape-disjoint, prefer
  most specific, identity on no match" properties are the scorer's
  existing, tested behavior (Map↔Struct=100 > same-constructor=80 >
  unrelated=0). The implementation does **not** author a new
  Value→AttributeType table; it adds the negative test (Test plan
  item 5) against the scorer's behavior for the carina#3080 schema.
- **Member ordering.** `string_or_principal_struct` deliberately puts
  `Struct` before `String` (serializer ordering, see its comment).
  `union_member_score` already breaks ties by declaration order via
  `validate_union`'s strict `>`; `select_union_member` MUST use the
  same strict-`>` tie-break so canonicalizer and validator pick the
  identical member. Confirm a `Map` value scores 0 against the
  `String` member regardless of order (it does — `(String, Map)` hits
  `_ => 0`).
- **`Custom` members.** `union_member_score` already defers `Custom` to
  its declared `base` (`(AT::Custom { base, .. }, v) => …`), so
  `Custom`-wrapped members are handled consistently *without* a
  separate `peel_custom` pass in the new arm — another duplication
  avoided by reuse. Re-dispatch still calls `canonicalize_with_type`,
  which peels `Custom` at entry as today.
- **Nested `Union[…, Union[…]]`.** `union_member_score` already
  recurses over inner Union members (`(AT::Union(inner), v) => …`); the
  re-dispatch into `canonicalize_with_type` then re-enters the Union
  arm — termination is type-structural (each step strips one
  constructor; see Type safety §4), no runtime cycle guard needed.
- **Interaction with the differ invariant.** This change *restores*
  the #2481 invariant for Union nesting; it must not be paired with any
  comparator change. The implementation PR must explicitly NOT touch
  `type_aware_equal`'s Union arm (the rejected approach B).
