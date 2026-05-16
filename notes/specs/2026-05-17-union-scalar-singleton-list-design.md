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
member whose **declared shape matches the value's shape**, then lets
the existing arms do their job:

```rust
// Union: the value conforms to exactly one member shape (Cargo/IAM
// unions are shape-disjoint — Struct vs String, String vs List).
// Recurse into the member that matches the value's concrete shape so
// nested string_or_list_of_strings fields are still canonicalized.
(val, AttributeType::Union(members)) => {
    let chosen = members.iter().find(|m| value_shape_matches(&val, peel_custom(m)));
    match chosen {
        Some(m) => canonicalize_with_type(val, m),  // re-dispatch on the member
        None => val,
    }
}
```

`value_shape_matches(&Value, &AttributeType) -> bool` — a cheap
structural predicate (no recursion): `Map` ↔ `Struct`/`Map`; `List` ↔
`List`; `String`/`Int`/`Float`/`Bool`/`EnumIdentifier`/`StringList` ↔
the corresponding scalar/`String`/`StringEnum`; nested `Union` →
recurse the predicate over its members. It only picks a member to
re-dispatch into; the actual canonicalization is still the existing
`is_string_or_list_of_strings` / Struct / List / Map arms.

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
| **A: Union arm in `canonicalize_with_type` (chosen)** | Fixes the root, upholds the #2481 invariant, provider-agnostic, mirrors the existing List/Map/Struct recursion. |
| B: Comparator `"x"==["x"]` special-case in `type_aware_equal` Union arm | **Rejected** — explicitly prohibited by `comparison.rs:28-47`; masks the phantom, state stays non-canonical, re-fires next run. |
| C: aws read path emits scalar when AWS returned scalar | **Rejected** — per-provider carve-out; `Union[String,list]` legitimately accepts either shape; doesn't fix awscc or future providers. |
| D: Canonicalize *all* Union members unconditionally then pick | **Rejected** — wasteful and ambiguous (which canonicalized form wins?); shape-directed re-dispatch is precise. |

## Blast radius

- **One function changed:** `canonicalize_with_type` (`value.rs`) gains
  a `Union` arm + a private `value_shape_matches` helper. No signature
  change → `canonicalize_resources_with_schemas` /
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
- **Risk of over-canonicalization:** `value_shape_matches` must not
  pick a member that would *wrongly* coerce (e.g. a `Map` value must
  not match a `String` member). Disjoint-by-shape selection + the
  existing arms' own type guards bound this; the `None` (no match) arm
  is identity (safe fallthrough, today's behavior).

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
   contract is satisfied rather than worked around.

## PR sequence (design-before-implementation)

1. **This design PR** (`notes/specs/…` only) — merges first.
2. **Implementation PR** — the `Union` arm + `value_shape_matches` +
   tests, `Closes #3080`.
3. carina-provider-aws / -awscc: routine carina-core pin bump (the
   pin-staleness guard added in carina-provider-aws#332 /
   carina-provider-awscc#256 enforces the minimum rev).

## Risks / open questions (resolve in implementation)

- **`value_shape_matches` precision.** Must be shape-disjoint and
  conservative: prefer the most specific member; on ambiguity or no
  match, identity (never guess-coerce). Enumerate the
  Value→AttributeType shape table explicitly; unit-test the negative
  (Map must not select a String member).
- **Member ordering.** `string_or_principal_struct` deliberately puts
  `Struct` before `String` (serializer ordering, see its comment).
  `value_shape_matches` is shape-directed so order-independent, but
  confirm a `Map` value never matches the `String` member regardless
  of order.
- **`peel_custom` on members.** `canonicalize_with_type` already peels
  `Custom`; apply `peel_custom` to each Union member before the shape
  test so `Custom`-wrapped members are handled consistently.
- **Nested `Union[…, Union[…]]`.** The predicate recurses over inner
  Union members; the re-dispatch into `canonicalize_with_type` then
  re-enters the Union arm — confirm termination (members are strictly
  smaller types; no cycle).
- **Interaction with the differ invariant.** This change *restores*
  the #2481 invariant for Union nesting; it must not be paired with any
  comparator change. The implementation PR must explicitly NOT touch
  `type_aware_equal`'s Union arm (the rejected approach B).
