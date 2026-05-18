# Loop-variable field access resolution: Design

<!-- derived-from #root-cause -->
<!-- constrained-by ../../CLAUDE.md -->
<!-- constrained-by ./2026-05-18-same-config-read-iterable-resolution-design.md -->

## Goal

Make field access on a **struct/map-valued `for`-loop variable** —
`for (_, opt) in <iterable> { ... opt.resource_record.name ... }` —
resolve to the bound element's nested value, instead of (a) erroring
outright for a one-level access or (b) materializing as an unresolved
`ResourceRef` for a two-level access.

This is the **design document only**. Implementation follows in a
separate PR after this design merges, per the repo's split-PR policy
(`CLAUDE.md` → "Design PR must merge before implementation PR"). It is
carina#3136, discovered while implementing carina#3132 PR-1 (#3137,
merged): that PR makes a same-config read iterable *materialize*
concrete resources at `carina plan`, but their attributes that read
the loop variable via a field path stay unresolved because of the gap
documented here. **carina#3132 PR-3 (the real-infra
`registry/dev/registry` acceptance, `Closes #3132`) cannot pass until
this is fixed** — the real `usecases/registry/acm.crn` is exactly
`for (_, opt) in cert.domain_validation_options { name =
opt.resource_record.name; resource_records = [opt.resource_record.value] }`.

## Root cause

<!-- constrained-by ../../CLAUDE.md -->

The bug is **not** specific to deferred-for. It is a general parser
gap in `parse_primary_eval`'s `variable_ref` handling
(`carina-core/src/parser/expressions/primary.rs`). Two distinct sites
fail to consult the in-scope bound variable when the reference carries
a field path:

### Site A — two-level (or deeper) field access: unresolved `ResourceRef`

`primary.rs:489-500` (the `else` arm of the `variable_ref` field/index
walk): once `field_names` is non-empty it builds an
`AccessPath::with_fields_and_subscripts(binding_name, attribute_name,
field_names, subscripts)` and **unconditionally** emits
`Value::Deferred(DeferredValue::ResourceRef { path })`. It never calls
`ctx.get_variable(first_ident)`. Contrast `primary.rs:481` (the
*no-field* arm) which **does** consult `get_variable` and returns the
bound value — that asymmetry is the entire defect for this site.

So `for (_, o) in [{ rr = { name = "n1" } }] { ... name = o.rr.name }`
parses to `name = ResourceRef { binding: "o", attribute: "rr",
segments: [Field { name: "name" }] }` and stays that way. There is no
downstream pass that resolves it: `resolve_ref_value`
(`carina-core/src/resolver.rs:199`) resolves a `ResourceRef` against
`ResolvedBindings`, which is keyed by **resource `binding` name**; a
loop variable `o` is never a `ResolvedBindings` entry, so the ref
survives to the differ as a phantom.

### Site B — one-level field access on a bound non-resource variable: hard error

`primary.rs:148-159`: a two-part dotted id (`o.k`) where
`ctx.get_variable("o").is_some()` **and** `!ctx.is_resource_binding("o")`
returns
`ParseError::InvalidExpression { "'o' is not a resource, cannot access
attribute 'k'" }`. This guard is correct for a *scalar* variable
(`let x = "s"; x.foo` is meaningless) but wrong for a **struct/map-valued
loop variable**, where `o.k` is legitimate map-key access into the
bound element.

### Verified behavior matrix

Established empirically against `52c32c4c` (PR-1 merged) with throwaway
parser probes — *not* assumed:

| DSL                                                                   | Result today                                                                 |
| --------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| `for (_, o) in [{k="v"}] { … name = o }`                              | ✅ resolves (`name = "{...}"` via the no-field `get_variable` arm)            |
| `for (_, o) in [{k="v"}] { … name = o.k }`                            | ❌ **parse error** "'o' is not a resource…" (Site B)                          |
| `for (_, o) in [{rr={name="n"}}] { … name = o.rr.name }`             | ⚠️ parses, `name` = **unresolved `ResourceRef{binding:o,…}`** (Site A)        |
| `for o in [{k="v"}] { … name = o.k }`                                 | ❌ same as Site B (binding kind — Simple/Indexed/Map — is irrelevant)          |
| same shapes inside a *deferred* `for … in cert.<read>`                | identical: the template body is parsed by the same `parse_for_body` path     |

The deferred and non-deferred paths share `parse_for_body`
(`for_expr.rs:501`) and therefore the same `primary.rs` defect — which
is why carina#3132 PR-1 could materialize the loop yet leave the body
refs unresolved, and why this must be fixed in the parser, once, for
both.

## Non-goals

- Changing `for`-binding *iterable-shape* semantics (Map-binding over a
  list is still a shape mismatch; that is a separate, intended rule).
- Changing `ResourceRef` resolution for genuine **resource** bindings
  (`vpc.id`, `cert.domain_validation_options`) — those must keep
  flowing through `resolve_ref_value`/`ResolvedBindings` unchanged.
- The carina#3132 pipeline-ordering work (done in #3137). This design
  is strictly the loop-variable field-navigation axis.
- Deep list-indexing of arbitrary structs after a field
  (`a.b[0].c`) — `primary.rs:454-460` already rejects "field access
  after index access"; this design does not relax that.

## Chosen approach: resolve loop-var field paths at parse time, where the binding is in scope

A `for`-loop already binds the value variable to the concrete element
`Value` in the iteration `ParseContext` (`for_expr.rs:187/201/219` for
the resolved path; `for_expr.rs:246` binds the `ForValue` placeholder
for the deferred-template path). The element value is therefore
**in scope at the exact point `primary.rs` decides what to emit**. The
fix is to navigate that in-scope value along the parsed field path
*there*, the same way the no-field arm already returns
`ctx.get_variable(first_ident)`.

### Mechanism

1. **A single shared navigator.** Introduce
   `fn navigate_value_path(root: &Value, attribute: &str, fields:
   &[String], subscripts: &[Subscript]) -> Option<Value>` in
   `carina-core/src/resource` (next to `AccessPath`). It walks a
   `ConcreteValue::Map`/`List` by key/index following exactly the
   `attribute → fields → subscripts` order `AccessPath` encodes, and
   returns `None` when any hop is missing or the shape is wrong (not a
   map where a field is expected, etc.). This is pure and unit-testable
   in isolation and is the *single* place path-walking semantics live.

2. **Site A (`primary.rs:489`): consult the bound variable before
   emitting `ResourceRef`.** When `ctx.get_variable(first_ident)` is
   `Some(EvalValue)` whose value is a *concrete* (or
   placeholder-bearing) tree — i.e. it is a bound **variable**, not a
   resource binding — attempt `navigate_value_path` over it:
   - **Concrete tree, navigation succeeds** → emit the navigated
     `Value` (the loop body gets the real nested scalar).
   - **Tree still contains the deferred `ForValue` placeholder at the
     navigated position** (deferred-for template parse: the element
     value is not yet known) → emit a placeholder that *carries the
     path*, so `expand_deferred_for_expressions`'s `substitute_*` can
     later replace the loop var with the real element and the path
     re-navigates. Concretely: keep emitting an
     `AccessPath`-bearing node but tag it as a **loop-variable**
     reference (see step 4), not a resource `ResourceRef`, so
     `resolve_ref_value` never tries (and fails) to find binding `o`
     in `ResolvedBindings`.
   - **Not a bound variable at all** (genuine resource binding, e.g.
     `cert.domain_validation_options`) → unchanged: emit
     `ResourceRef { path }` exactly as today. This is the invariant
     that keeps real resource refs working.

3. **Site B (`primary.rs:148`): scope the guard to scalars.** The
   "'X' is not a resource, cannot access attribute" error must fire
   only when the bound value is a **scalar** (`String`/`Int`/`Bool`/…),
   where field access is genuinely meaningless. When the bound value
   is a `Map` (or `List` with a following index) — or a deferred
   placeholder standing in for one — fall through to the navigator in
   step 2 instead of erroring. The error message and behavior for the
   real misuse (`let s = "x"; s.foo`) are unchanged.

4. **The still-placeholder case is a new `UnknownReason`, not a new
   top-level `DeferredValue`.** The defect exists because a loop-var
   field access is *represented as* a resource `ResourceRef`, which a
   later resolver can only resolve against resource bindings. The fix
   needs a representation that says "loop-variable field path, resolved
   at for-expansion, never serialized" — which is **exactly** the
   contract of `UnknownReason`, the parse-internal placeholder family
   that already holds `ForValue` / `ForKey` / `ForIndex` and the
   `AccessPath`-carrying `UpstreamRef { path }`. Add
   `UnknownReason::ForValuePath { path: AccessPath }` (name
   provisional) — the `ForValue` sibling that additionally carries the
   navigation path — emitted only by step 2's still-placeholder case.

   This is deliberately **not** a new `DeferredValue::LoopVarRef`
   variant. `DeferredValue` is mid-migration under RFC #2972 (the
   `ConcreteValueRef`/`DeferredValueRef` borrowing split, eventually a
   physical `Value { Concrete, Deferred }` split); a new top-level
   variant there forces the `DeferredValueRef<'a>` mirror, ~35
   `match DeferredValue` sites across carina-core (plus carina-cli /
   lsp / the triplicated aws-types — see
   [[project_aws_types_triplicated_copies]]), and a serde decision.
   `UnknownReason` lives under the already `#[serde(skip)]`
   `DeferredValue::Unknown(..)` arm: no state-format change, no RFC
   #2972 surface touched, and the blast radius is the handful of sites
   that already exhaustively match `UnknownReason`.

   `substitute_placeholder` (`carina-core/src/parser/ast.rs:993`)
   gains one arm next to its existing `ForValue` arm: replace the node
   by `navigate_value_path(substituted_element, …path)` (the *same*
   navigator from step 1 — single source of truth). Its
   "explicit-arm, no wildcard" `UnknownReason` match
   (ast.rs:1013-1015) **already forces a compile error** when a new
   `UnknownReason` variant is added, so the substitution cannot be
   silently skipped — the typed-completeness lever is inherited, not
   re-invented. `resolve_ref_value` is **not** taught about loop vars;
   it never sees the new variant because for-expansion resolves it
   first.

### Why this shape (long-term, type-safe)

- **One navigator, one semantics.** Path-walking lives in exactly one
  pure function used by both the parse-time resolved case and the
  deferred `substitute_*` case. There is no second, drifting
  implementation of "follow `attribute.fields[subscripts]` into a
  `Value`" (the carina#3132 root-cause-over-per-site rule applied here:
  the bug is *one* missing navigation, fixed once).
- **Loop-var vs resource-ref is made unrepresentable-to-confuse, at
  minimum blast radius.** The new `UnknownReason::ForValuePath` variant
  means a loop-variable reference can never again be mistaken for a
  resource binding by `resolve_ref_value` (which only matches
  `ResourceRef`); the bug class — `ResourceRef{binding:loop_var}`
  reaching a resolver that only knows resource bindings — becomes a
  compile-time-distinct shape, mirroring how carina#3132's
  `IterableBindings` made "iterable resolved against the wrong map"
  unrepresentable. Choosing `UnknownReason` over a new top-level
  `DeferredValue` variant keeps that guarantee while *not* perturbing
  the RFC #2972 split or the state serialization format.
- **The differ's never-equal invariant is inherited, not re-asserted.**
  `Value`'s hand-rolled `PartialEq` (`resource/mod.rs:751`) makes
  `Value::Deferred(DeferredValue::Unknown(_))` **never equal to
  anything** — so an unresolved loop-var-path placeholder is correctly
  never "the same value" as another, and the differ cannot silently
  suppress a real diff. A new top-level `DeferredValue::LoopVarRef`
  would have to *manually* opt into this invariant (and a future
  maintainer could get it wrong); an `UnknownReason` variant gets it
  for free because the `Unknown(_)` arm already covers it. This is the
  decisive type-safety reason for the placement.
- **Resource refs strictly untouched.** Step 2's "not a bound variable
  ⇒ unchanged `ResourceRef`" branch is the guard that keeps
  `cert.domain_validation_options` (a real resource binding) flowing
  exactly as before — the change is additive on the *variable* path
  only.
- **Deferred and non-deferred converge.** Both paths parse the body
  through `parse_for_body`; fixing `primary.rs` once fixes both, so
  plan and apply cannot diverge on this axis
  ([[feedback_unit_test_path_is_not_apply_path]]).

## Risks / open questions

- **Placeholder-bearing navigation in the deferred-template parse.** At
  template-parse time the element is the `ForValue` placeholder, so
  `navigate_value_path` cannot produce a concrete value yet — step 2
  must emit the path-carrying `UnknownReason::ForValuePath` and rely on
  `substitute_placeholder` re-navigating post-expansion. The
  implementation PR must prove (fixture) that a deferred
  `for (_, opt) in cert.<read> { name = opt.resource_record.name }`
  yields a concrete `name` after carina#3132 PR-1's post-refresh
  expansion runs `substitute_*`. This is the carina#3132 PR-3
  acceptance and the highest-risk integration point.
- **`Indexed` binding index variable.** `for (i, o) in …` binds `i` to
  `ForIndex`. `i` has no field access in practice, but the design must
  not regress `i` resolution (still the no-field `get_variable` arm).
- **Subscripts after fields on a loop var** (`o.items[0]`). The
  navigator must honor `AccessPath.subscripts`; the existing
  "field after index" rejection (`primary.rs:454`) is unchanged and
  the navigator only needs the already-allowed
  `attribute → fields → trailing subscripts` order.
- **LSP / scope check.** `check_identifier_scope` must not flag the new
  `UnknownReason::ForValuePath` as an unknown binding (a loop var is
  intentionally not a top-level binding). Since it lives under
  `DeferredValue::Unknown(..)`, scope-check paths that already skip
  `Unknown(_)` cover it for free; the implementation PR verifies this
  the same way carina#3132 audited
  `resolve_refs_*`.
- **Not a phantom/renderer change.** Parser-level value shaping only;
  no differ/detail-row code is touched. Stated to scope reviewers away
  from the phantom-diff class.

## Implementation PR breakdown (post-merge, single PR)

A single implementation PR (the change is one cohesive parser fix, not
a multi-stage pipeline like carina#3132):

1. Add `navigate_value_path` (pure, unit-tested in isolation:
   map/list/missing-hop/scalar-reject).
2. Site A + Site B parser changes + the
   `UnknownReason::ForValuePath` variant + the `substitute_placeholder`
   arm.
3. Acceptance: parser tests for the full behavior matrix above flipping
   from ERR/unresolved to resolved; a carina-cli wiring test that the
   deferred `for (_, opt) in cert.<read>` path yields concrete body
   attributes post-expansion (extends the carina#3132 PR-1
   `chained_loop_var_field_access_is_a_known_limitation` pin — that
   test's assertion flips from "still a `ResourceRef`" to "concrete
   String"); real-infra `registry/dev/registry` smoke is carina#3132
   PR-3's job and is user-driven ([[feedback_no_real_infra_aws_commands]]).

`Closes #3136` on that implementation PR. carina#3132 PR-3 then
unblocks (it `Closes #3132` and removes the `moved.crn` workaround).
