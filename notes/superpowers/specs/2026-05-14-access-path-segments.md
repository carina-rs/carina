# AccessPath unified segment representation

<!-- derived-from ../../../../docs/reference/dsl/syntax.md -->

## Background

`AccessPath` is the structured representation the parser emits for
every `<binding>.<attribute>…` reference in a `.crn` file. It is the
spine of resolve, plan, state, and display: state files round-trip
through it, the differ uses it as a dependency edge identifier, and
plan display renders it as a dotted source string.

Today the struct is:

```rust
pub struct AccessPath {
    binding: String,
    attribute: String,
    field_path: Vec<String>,    // pre-subscript field chain
    subscripts: Vec<Subscript>, // trailing index/key subscripts
}
```

This is shaped by the grammar's pre-#3025 surface, where
`subscripted_id = { namespaced_id ~ index_access+ }` accepts only
trailing subscripts after the namespaced head. The grammar refuses
any `.field` continuation after the first `[idx]`.

## Problem (carina#3025)

A natural read pattern — `cert.domain_validation_options[0].resource_record_name`
— is rejected at parse time. Users with a list-of-structs and a known
single element have to fall back to a length-1 `for` loop. This blocks
the carina-rs/infra T6c work (ACM DNS validation CNAME wired into the
same hosted zone) where no SANs are configured, so
`domain_validation_options` has exactly one entry.

The `AccessPath` shape participates in the bug: even if we relaxed the
grammar to accept `cert.foo[0].bar`, there is no slot on the struct to
hold `bar` — `field_path` is *pre*-subscript only, and `subscripts` is
trailing only. Generalising to `cert.a[0].b[1].c` (arbitrary mix of
field and index access in any depth) needs a single ordered sequence
of access steps, not two separate vectors.

## Decision

Replace `field_path` + `subscripts` with a single ordered
`segments: Vec<PathSegment>` where:

```rust
pub enum PathSegment {
    Field(String),
    Subscript(Subscript),
}
```

`binding` and `attribute` stay as named struct fields — the
"binding + attribute is mandatory" invariant is load-bearing for
state-file lookups and dependency-edge identity, and dropping it
would force every consumer to handle a "what if the path has zero
segments" branch that doesn't exist in valid DSL. The first
`Field` after the attribute lives in `segments[0]`, not folded into
`attribute`.

### Why this shape and not a smaller fix

A `trailing_field_path: Vec<String>` alongside the existing two
vectors covers exactly the carina#3025 reproduction, but stops at
two depth steps (`field[idx]field`). It cannot represent
`a[0][1].b`, `a.b[0].c[1]`, or any deeper interleaving. The user's
T6c shape happens to be the simplest pattern; the next natural
pattern (e.g. reading a single field out of a single element of a
list-of-list) would re-open the same bug. Modelling at the right
granularity — an ordered sequence — closes the class.

### Why not `Vec<PathSegment>` *including* attribute

Folding `attribute` into `segments[0]` would simplify the type
(binding + segments only) but force every consumer that asserts
"this path has an attribute" to write a guarded `match
segments.first()` instead of `path.attribute()`. State-file lookups
and dependency-edge identity rely on `(binding, attribute)` as a
2-tuple key in several places; flattening would require widening
those keys to `(binding, Vec<PathSegment>)` everywhere. Keep the
two as named fields, keep the post-attribute walk as segments.

## Migration plan

This is a structural change with ~117 caller sites (including tests).
We do it in one PR rather than four because:

- The serde format changes; a partial migration would leave state files
  written in mixed shapes.
- The accessors `field_path()` / `subscripts()` no longer have a
  meaningful single-value answer once a path mixes both, so they have
  to be removed in lockstep with the new accessors.

No backward-compat shim. The project policy is "no backward
compatibility" (memory: `feedback_no_backward_compat`); the
serde representation switches cleanly to `segments` in the same PR.

### Steps within the implementation PR

1. Introduce `PathSegment` enum and rewrite `AccessPath` to carry
   `segments: Vec<PathSegment>`.
2. Replace `with_fields` / `with_fields_and_subscripts` with
   `with_segments` (and keep `new` for the bare-attribute case).
3. Add new accessors:
   - `segments() -> &[PathSegment]` — the ordered walk.
   - `field_segments() -> impl Iterator<Item = &str>` — equivalent to
     the pre-fix `field_path()` for paths with no subscripts.
   - `subscript_segments() -> impl Iterator<Item = &Subscript>` — for
     paths with no post-subscript field continuation.
4. Remove `field_path()` / `subscripts()`.
5. Walk every caller and replace. Where the caller's intent was
   "evaluate the access chain end-to-end" (the common case in
   resolver / state lookup / plan-time evaluation), switch to a
   `for segment in path.segments() { ... }` loop.
6. Update `to_dot_string` to walk segments in order — covers the
   carina#3025 surface for free.
7. Grammar: `subscripted_id = { namespaced_id ~ (index_access |
   field_access)+ }`.
8. Parser AST builder: collect segments by iterating the pair's
   inners in source order.

### Tests

- carina#3025 reproduction: `cert.domain_validation_options[0].resource_record_name`
  parses and reaches the resolver as a path whose segments are
  `[Field("domain_validation_options"), Subscript(Int 0),
  Field("resource_record_name")]`.
- Existing tests for `binding.field[0]`, `binding.field.subfield[0]`,
  `binding[0]` (folded-into-binding) keep passing — those collapse
  into segments shapes the same callers walk identically.
- State round-trip: serialise an AccessPath with mixed segments,
  reload, assert structural equality.

### Out of scope

- Multi-step subscript with intervening field, e.g. `a.b[0].c[1].d`,
  is supported by the segment representation but currently
  untested. Add coverage when a real use case appears.
- LSP completion at chained-access positions (`cert.foo[0].|`) —
  separate UX issue.

## Acceptance

`carina plan` on a fixture containing the carina#3025 reproduction
succeeds, the resolved value is the same as the equivalent
length-1 `for`-loop form, and the state file round-trips with the
new `segments` representation.
