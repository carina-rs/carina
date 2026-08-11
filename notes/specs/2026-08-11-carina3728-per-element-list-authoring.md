# Issue #3728 Per-element List Authoring Records — Design

Issue: https://github.com/carina-rs/carina/issues/3728

Related fix: https://github.com/carina-rs/carina/pull/3727

## Goal

Let a user remove a field they previously authored inside one list
element without treating a provider-populated field on a different,
heterogeneous element as an authored field to remove.

Concretely, after applying two security-group rules where only the port
80 rule authored `description`, deleting that `description` from the DSL
must plan an update that unsets it on the port 80 rule. A provider-set
`description` on the port 443 rule must remain untouched.

## Motivation

carina#3726 / PR #3727 extended saved-state merging so removal of a
previously-authored nested field is detected at map value /
`ExplicitFields::Struct` positions. `should_patch_attr` builds the
effective desired value by calling `merge_with_saved`
(`carina-core/src/differ/comparison.rs:519` and
`carina-core/src/differ/comparison.rs:530`). At a matching map,
`merge_with_saved` now omits a saved key when the prior `Struct` says the
key was authored but the current desired map no longer contains it
(`carina-core/src/resource/mod.rs:1777`).

List positions deliberately stayed conservative. The current
`build_from_value` folds every desired list element into one
`ExplicitFields::List { element }` union
(`carina-core/src/explicit.rs:74` and
`carina-core/src/explicit.rs:82`). The list merge therefore cannot know
which saved element authored a union member. Both list recursion paths
pass `ExplicitFields::Unrecorded` instead of the union tree
(`carina-core/src/resource/mod.rs:1839` and
`carina-core/src/resource/mod.rs:1908`). This preserves provider values,
but it also preserves a saved field that the user really removed.

Applying the union tree to each paired element would be unsafe. Given
these prior desired elements:

```text
{ port: 80,  description: "web" }
{ port: 443 }
```

the union says both `port` and `description` were authored. If the
provider stored a default `description` on the port 443 element, using
that union for removal would falsely claim the default was user-authored
and could plan a destructive removal. The regression guard added in PR
#3727 pins exactly this conservative behavior
(`carina-core/src/resource/tests.rs:903`). The consequence is the bug in
#3728: removing `description` from the port 80 element still plans no
change, so the remote value cannot be unset.

The original ExplicitFields design anticipated this extension. Its
[D3 decision](./2026-05-10-explicit-fields-design.md#d3-list-of-struct-uses-union-semantics-not-per-element)
states that the enum may grow a per-element variant while the existing
union `List` remains a valid restriction.

## Design decision

<!-- derived-from ./2026-05-10-explicit-fields-design.md#d3-list-of-struct-uses-union-semantics-not-per-element -->

Add one internally-tagged serde variant to `ExplicitFields`, whose
current definition is at `carina-core/src/explicit.rs:34`:

```rust
pub enum ExplicitFields {
    Leaf,
    Struct {
        children: HashMap<String, ExplicitFields>,
    },
    List {
        element: Box<ExplicitFields>,
    },
    ListElements {
        elements: Vec<ExplicitFields>,
    },
    Unrecorded,
}
```

With the enum's existing `#[serde(tag = "kind", rename_all =
"kebab-case")]`, the wire form is:

```json
{
  "kind": "list-elements",
  "elements": [
    { "kind": "struct", "children": { "port": { "kind": "leaf" } } },
    { "kind": "unrecorded" }
  ]
}
```

The chosen name is `ListElements`. It follows the existing noun-first
`List` variant, reads naturally with its `elements` field, and describes
the stored shape without implying that element identity is stable
across runs. `PerElementList` was considered but rejected: it reads as a
different kind of list rather than an authoring record indexed by the
stored list, and "per-element" can suggest durable element identity that
this design intentionally does not introduce.

`ExplicitFields::List { element }` remains valid and keeps its current
union semantics. It represents legacy rows, explicit union folding, and
the conservative fallback at any list position where aligned
per-element records cannot be produced. It is not deprecated.

## Invariants after this change

1. **Stored-index alignment.** For a state row whose stored list value is
   `stored`, `ListElements.elements[j]` describes the authoring of
   `stored[j]`. The vector is indexed by the list value in that same row,
   not by the desired list and not by the current provider read.
2. **Atomic co-storage.** The authoring record and the list it describes
   live in the same `ResourceState`. They are constructed from the same
   provider `State` and upserted as one row by the same writeback. No code
   path may update aligned authoring independently from the stored value.
3. **No cross-run element identity.** Alignment is derived once while a
   row is written. Planning later pairs current desired elements to that
   row's saved elements using values. No index, synthetic key, or schema
   key is assumed stable between provider calls.
4. **Uncertainty loses authoring precision.** A pairing with no positive
   similarity evidence, an unmatched provider element, a malformed
   vector length, or a shape mismatch degrades to `Unrecorded` or legacy
   `List`. It must never invent an authored claim. A false negative merely
   retains today's merge behavior; a false positive can cause a
   destructive remote unset.
5. **Legacy restrictions remain conservative.** `List`, `Leaf`,
   `Unrecorded`, and shape-mismatched prior nodes continue to give list
   elements the legacy full merge. A v9 row cannot become more
   destructive merely because a v10 binary reads it.
6. **Pairing remains schema-blind.** Element matching depends only on
   `Value` similarity and `ExplicitFields`. Neither the merge nor the
   authoring builder takes `AttributeType`.
7. **Projection preserves stored indices and attribution is
   corroborated.** Plan-time value merging keeps the legacy pairing over
   `projected_saved`, while a second pairing over the raw saved list
   reconstructs the view used to assign stored-index authoring at
   writeback. A projected pair at index `j` may consult `elements[j]`
   only when the raw pairing independently selects the same `j`; a
   disagreement degrades to `Unrecorded`. Projection can strip
   provider-only map keys and change both similarity scores and canonical
   hash buckets, so projected pairing alone cannot safely attribute an
   authoring record. Projection must remain a one-to-one, length- and
   order-preserving map over list items for both legacy `List` and the new
   `ListElements` union arm. That invariant ensures raw index `j` and
   projected index `j` still denote the same stored element when the two
   pairings corroborate one another. Dropping or reordering an item is
   forbidden without revisiting this design.

The alignment invariant is the cornerstone. It avoids the problem D3
identified because it does not try to preserve an index across provider
reads. It only records a relationship between two fields committed in
one state row.

## Authoring construction and writeback

### Raw construction

`build_from_value` currently treats a `ConcreteValue::List` as one union
tree (`carina-core/src/explicit.rs:70`). It changes to emit:

```rust
ExplicitFields::ListElements {
    elements: items.iter().map(build_from_value).collect(),
}
```

This applies uniformly to every `ConcreteValue::List`, including empty
lists, scalar lists, lists of maps, and nested lists. Uniform emission is
chosen over "only lists of maps" for three reasons:

- An empty list carries no value-level evidence about its future element
  shape; deciding from schema would violate the schema-blind invariant.
- One representation makes recursive construction, nested lists, and
  exhaustive matching simpler and prevents the representation from
  changing merely because a list becomes empty.
- Scalar elements produce only `Leaf`, so the semantic cost is zero.

The storage cost is linear: a 50-element string list represented as
`ConcreteValue::List` stores 50 `Leaf` records instead of one union
`Leaf`. That overhead is accepted for a state-only correctness record;
it is bounded by the already-stored list cardinality and avoids a second
shape-dependent construction mode. `ConcreteValue::StringList` is a
separate packed value variant (`carina-core/src/resource/mod.rs:902` and
`carina-core/src/resource/mod.rs:906`) and remains `Leaf`, so its scalar
elements do not acquire records.

Raw construction describes the value it walks, so its list vector is in
that value's order. State writeback must not store a raw tree built only
from desired order; it performs the alignment below before assigning
`ResourceState.explicit`.

### The one-time pairing seam

`ResourceState::from_provider_state_for_resource_and_schema` is the
single row-construction seam where both inputs coexist
(`carina-state/src/state/mod.rs:1391`):

- `resource.attributes` contains the desired values
  (`carina-state/src/state/mod.rs:1419`); and
- `state.attributes` contains the provider-returned values whose JSON
  serialization populates the row (`carina-state/src/state/mod.rs:1406`).

The assignment of authoring records to stored indices happens exactly
once per row write at this seam. Later plan-time matching only consumes
that assignment; it never rewrites or persists it.

The current writeback builds `explicit` from the desired resource alone
at `carina-state/src/state/mod.rs:1473`. Replace that call with a
high-level core helper such as
`explicit::build_from_resource_for_stored_values(resource,
&state.attributes)`. Keep the low-level scorer and pairing helper inside
carina-core; carina-state should not grow a second matching algorithm.

The helper walks desired maps by key. When matching desired and stored
children are both concrete lists, it pairs their elements and constructs
a vector indexed by the stored list:

1. Allocate `elements = vec![Unrecorded; stored.len()]` and mark every
   stored index unused.
2. Visit desired elements in desired order.
3. Among unused stored elements, choose the element with the highest
   positive `similarity_score`. Equal positive scores keep the first
   stored candidate encountered.
4. Mark the chosen stored index `j` used and set `elements[j]` by
   recursively building authoring from `desired[i]` against `stored[j]`.
   Recursive construction repeats this alignment at every nested list.
5. Leave an unmatched stored element as `Unrecorded`. A desired element
   with no match has no stored slot and therefore creates no record.

`similarity_score` currently counts semantically equal key/value pairs
for maps, returns 1 for semantically equal non-maps, and returns 0
otherwise (`carina-core/src/resource/mod.rs:1922`). A score of 0 is not a
pair. The existing list merge is greedy and first-fit, using a quadratic
path for small lists and a hash-assisted path for lists at or above the
threshold (`carina-core/src/resource/mod.rs:1804`,
`carina-core/src/resource/mod.rs:1815`, and
`carina-core/src/resource/mod.rs:1853`). Extract their selection logic
into one shared pairing helper and have both merge and aligned authoring
consume its mapping. This keeps small-list, large-list, tie-breaking, and
future matcher changes identical at the two sites.

Positive ties retain the existing first-fit rule. Exact duplicate
desired values may pair interchangeably because `build_from_value` is a
pure function of the value shape, so identical values have identical
authoring trees. No-positive-score cases are the ambiguity this design
can identify without schema knowledge; they remain unpaired. If a future
matcher detects another form of uncertainty, its safe result is also
`Unrecorded`.

When a desired list has no stored concrete-list counterpart—because the
stored key is absent, either side has another shape, or the stored value
is unavailable—the helper collapses the desired per-element tree to the
legacy `List { element: union }` restriction. Map authoring above that
position remains precise. This fallback records no vector whose indices
could be mistaken for stored indices.

`from_provider_state_for_resource_and_schema` constructs both
`rs.attributes` and `rs.explicit` before returning the row. Apply
writeback then upserts that one row
(`carina-cli/src/commands/shared/state_writeback.rs:1214` and
`carina-cli/src/commands/shared/state_writeback.rs:1226`). Thus the list
and its authoring vector cannot be committed by separate apply
writebacks.

### Worked example

Suppose desired order is port 80 followed by port 443, but the provider
returns and stores the elements in the opposite order:

| Stored index | Stored value | Aligned authoring record |
| --- | --- | --- |
| 0 | `{ port: 443, description: "provider-default" }` | `Struct { port }` |
| 1 | `{ port: 80, description: "web" }` | `Struct { port, description }` |

The state vector follows stored order: index 0 does not claim
`description`; index 1 does. On the next plan, desired port 80 pairs to
saved index 1 and may remove `description`. Desired port 443 pairs to
saved index 0 and retains the provider default. Nothing needs to remember
that either element used a different index in the desired input or a
previous provider response.

## Plan-time consumption

`find_changed_attributes` projects current and saved state and threads
both the raw and projected saved views, together with the prior
attribute's authoring tree, through `SavedAttr`
(`carina-core/src/differ/comparison.rs:673` and
`carina-core/src/differ/comparison.rs:704`). `merge_with_saved` consumes
that bundle. Map recursion descends both views by the same key. A key in
the projected map is expected to exist in raw saved state because
projection only filters; if malformed input violates that expectation,
the projected child is used as both views, the conservative degenerate
case.

Change the concrete-list arm at `carina-core/src/resource/mod.rs:1753`
to pass `prior_explicit` into `merge_lists`. The quadratic and hashed
helpers compute two mappings through the same shared pairing function:

```text
pairs_projected = pair_list_elements(desired, projected_saved)
pairs_raw = pair_list_elements(desired, raw_saved)

value pair for desired[i] is projected index j
    => merge desired[i] with projected_saved[j]

prior is ListElements
and elements.len() == projected_saved.len() == raw_saved.len()
and pairs_projected[i] == pairs_raw[i] == Some(j)
    => recurse with elements[j]
otherwise
    => recurse with Unrecorded
```

This replaces the two hard-coded `Unrecorded` arguments at
`carina-core/src/resource/mod.rs:1839` and
`carina-core/src/resource/mod.rs:1908`. The result order remains desired
order, exactly as today.

The split is deliberate. Value pairing remains on `projected_saved`,
byte-for-byte preserving legacy merge fidelity and its ability to fill
server fields into effective desired values. Authoring records were
assigned by raw pairing at writeback, however, and projection can raise
similarity scores or change hash-bucket priority. When raw and projected
pairings select different stored indices, neither record is safe to
attribute to that desired element. Corroboration therefore retains the
legitimate-removal path when both views agree and applies invariant 4's
`Unrecorded` fallback when they disagree.

The length equality guard is required even though v10 writers uphold the
alignment invariant. The record vector, raw saved list, and projected
saved list must all have equal length. If a state file is manually edited
or partially corrupted, trusting the prefix of a short or long vector
could assign authoring to the wrong element. A mismatch therefore
degrades the whole list node to legacy merge, not merely the missing
indices. The next successful writeback reconstructs a correctly-sized
vector.

When the prior node is `List { element }`, `Leaf`, `Unrecorded`,
`Struct`, or any other shape mismatch, list merging is byte-for-byte the
current logical behavior: every paired recursion receives
`Unrecorded`. In particular, the legacy `List.element` union is never
used for an element-level removal decision.

Map recursion must treat `ListElements` like `List` when it appears at a
map position: it supplies no map children, so the fallback is the
legacy full map merge. At the resource root,
`find_changed_attributes` likewise treats an unexpected
`ListElements` like the current unexpected `List` and uses
`Unrecorded` for an attribute (`carina-core/src/differ/comparison.rs:705`).

No `AttributeType` parameter is added. Desired-to-saved identity remains
the same value-derived heuristic the merge already uses.

## Projection remains union-based

`project` currently applies one `List.element` tree to every current
list element (`carina-core/src/explicit.rs:149` and
`carina-core/src/explicit.rs:168`). It must not index a current provider
list with `ListElements.elements`: current provider output is not the
stored row, and its order is not guaranteed to match.

For `ListElements`, derive a union of all recorded elements by folding
them with `explicit::merge`, then apply that one union tree to every item
using the current `List` projection behavior. An empty vector folds to
`Leaf`, which passes current elements through conservatively. The same
rule applies when projecting both current and saved maps in
`find_changed_attributes` and when projecting display inputs in
`detail_rows` (`carina-core/src/detail_rows.rs:783`).

Projection and removal now have deliberately different responsibilities:

- Projection hides fields that were never authored anywhere in the
  prior list. Union projection is order-immune and conservative with
  respect to provider reordering.
- Saved merge decides whether a field was authored on one particular
  saved element. It already has the desired-to-saved pairing needed for
  that decision.

The comparison remains coherent. If a field was authored on one prior
element, union projection keeps that field in `projected_current` for all
elements. Per-element merge then drops it from effective desired only
for the paired element whose record contains the field. The differ sees
the removal exactly there. On a heterogeneous sibling whose record does
not contain the field, merge carries the saved/provider value into
effective desired, so no false removal appears.

`project_attributes` should add `ListElements` to the same conservative
top-level pass-through arm as `List`; a full resource authoring tree is
normally a `Struct` (`carina-core/src/explicit.rs:188`).

## ExplicitFields union algebra

`explicit::merge` is the union operation at
`carina-core/src/explicit.rs:97`. Its only production entry today is the
list fold passed as a function at `carina-core/src/explicit.rs:87`; calls
inside `merge` recursively union matching struct children and list
elements. The remaining direct callers are unit tests. No current caller
needs `merge` to preserve per-element cardinality.

Keep that property explicit: whenever `ListElements` participates in
`merge`, first reduce it to a legacy union list. Define
`U(E) = E.into_iter().fold(Leaf, merge)`. The new cases are:

| Operands | Result |
| --- | --- |
| `ListElements(E)` and `ListElements(F)` | `List { element: merge(U(E), U(F)) }` |
| `ListElements(E)` and `List { element: x }` | `List { element: merge(U(E), x) }` |
| `ListElements(E)` and `Leaf` | `List { element: U(E) }` |
| `ListElements(E)` and `Unrecorded` | `List { element: U(E) }` |
| `ListElements(E)` and `Struct { .. }` | `List { element: U(E) }`; this is a malformed mixed-shape union, and projecting a map through the resulting list tree still takes the conservative shape-mismatch pass-through |

The rules are symmetric in operand order. When `ListElements` is a root
operand, `merge` never returns `ListElements` at the root; it is
intentionally a lossy cross-element union seam. A nested `ListElements`
can survive as an unmatched child of an asymmetrically merged `Struct`
(for example, merging `Struct { a: ListElements }` with
`Struct { b: Leaf }`). That behavior is safe because merge results are
transient and are never persisted as row authoring, and projection
reduces any nested vector to its union when it later consumes it. A unit
test pins this survival so changing the asymmetric union behavior is a
conscious decision. All existing non-`ListElements` cases retain their
behavior. After this change, production uses of `merge` are union
projection, conservative fallback construction, and recursive union
work; raw list construction no longer folds away element records.

## State schema v10

`StateFile::CURRENT_VERSION` bumps 9 → 10 at
`carina-state/src/state/mod.rs:86`. Add this line to its version history:

```rust
/// v10: Added `ExplicitFields::ListElements`, aligned by index with the
///      stored list value in the same resource row.
```

There is no v9 → v10 migration function:

- The existing `List` variant remains in the enum, so v9 rows deserialize
  unchanged and retain today's conservative list merge.
- The generic older-version read path already deserializes the row and
  updates the in-memory version (`carina-state/src/state/mod.rs:883`).
- The next apply writeback with desired and provider values available
  rebuilds the row as aligned `ListElements`. Legacy rows therefore
  self-heal without guessing during migration.

The version bump is still required for forward-compatibility safety. A
v9 binary does not know the `list-elements` kind. Once a v10 binary has
written one, the older binary must reject the file based on its future
version instead of attempting to deserialize or silently weaken the
record. The future-version guard is at
`carina-state/src/state/mod.rs:892`.

The carina#3280 repair path inside
`from_provider_state_for_resource_and_schema` has no desired attributes
with which to realign authoring after a fresh provider read. Its
idempotent-preserve branches therefore recursively demote every
`ListElements` in the prior tree to legacy `List { element: U(E) }`
instead of cloning aligned vectors. This applies both to an unexpected
top-level `ListElements` and to vectors nested inside a preserved,
populated root `Struct` (or legacy `List`). A provider reorder can then
lose only per-element precision, never leave a stale same-length vector
pointing at different stored elements. The next writeback with real
desired values available reconstructs aligned `ListElements`.

The `Unrecorded` self-heal rebuild at
`carina-state/src/state/mod.rs:1503` may continue calling raw
`build_from_value` on `state.attributes`: that input is the stored value
itself, so any emitted `ListElements` is already in stored order and
satisfies the invariant without another pairing pass.

`StateFile::build_explicit` remains a straight clone of each row's tree
(`carina-state/src/state/mod.rs:404`). `is_empty_explicit` remains true
only for `Leaf` (`carina-state/src/state/mod.rs:1695`); in particular,
`ListElements { elements: [] }` is meaningful and must serialize.
`ResourceState.explicit` keeps the existing serde default and
skip predicate (`carina-state/src/state/mod.rs:1045`).

## Edge cases

- **Empty desired list.** If the provider also stores an empty list, the
  row records `ListElements { elements: [] }`. If the provider stores
  elements, the vector has the stored length and every entry is
  `Unrecorded`. Planning an empty desired list still exposes provider
  elements as a list-level difference; it makes no field-level authored
  claim about them.
- **Provider returns more elements than desired.** Desired elements take
  positive-score matches. Every unmatched stored/provider-added element
  receives `Unrecorded`, so its nested provider fields use legacy merge.
- **Provider returns fewer elements than desired.** The vector has only
  stored slots. An unmatched desired element creates no phantom record;
  a later plan treats it as a desired element with no saved match.
- **Desired element pairs to no stored element.** A maximum score of 0
  means no pair. The desired element has no state-row authoring slot, and
  all unmatched stored elements remain `Unrecorded`.
- **Duplicate elements.** Greedy first-fit may swap equal duplicates,
  but identical values generate identical authoring trees, so the
  stored vector is equivalent. Non-identical positive-score ties remain
  a heuristic limitation of the matcher.
- **Nested lists.** After an outer desired/stored pair is selected,
  construction recursively aligns inner desired and stored lists. Each
  nested `ListElements` vector is indexed by the nested stored list at
  the corresponding row position. A failure at an inner level degrades
  only that inner position.
- **`ConcreteValue::StringList`.** It remains `Leaf`. Its elements are
  scalar and cannot contain a removable nested field; per-element and
  union authoring are equivalent for this purpose.
- **Deferred or unknown list value at writeback.** A whole
  `Value::Deferred`, including `DeferredValue::Unknown`, continues to
  fall through `build_from_value` as `Leaf`; only a concrete `List` can
  produce list authoring. The concrete/deferred split is defined at
  `carina-core/src/resource/mod.rs:871` and
  `carina-core/src/resource/mod.rs:913`.
- **Reorder between applies.** Every writeback derives alignment again
  from desired and the new provider-returned order, so the persisted
  vector self-corrects. Between writes, plan-time desired-to-saved
  similarity pairing already absorbs reorder exactly as list merge does
  today.
- **Malformed vector length.** Plan-time merge treats the whole node as
  `Unrecorded`; it never trusts a partial index alignment. Projection can
  still fold the vector as an order-independent union. The next
  writeback repairs the length.

## Known limitations and non-goals

### Pairing remains heuristic

The matcher is greedy, accepts any positive score, has no confidence
threshold, and resolves positive ties by first fit. The new authoring
alignment therefore has the same identity fidelity as today's saved
list merge—not a stronger identity model. The safe fallback covers
zero-score and structurally invalid cases, but a weak positive match can
still be wrong.

A future Option B may add schema-declared list-element keys derived from
provider metadata such as CloudFormation `uniqueItems` /
`primaryIdentifier`. That is out of scope because the metadata is not
present in today's list type. Adding it would cross the internal
`AttrTypeKind::List` (`carina-core/src/schema/mod.rs:514`), the public
`Shape::List` and transport `RawShape::List`
(`carina-core/src/schema/mod.rs:603` and
`carina-core/src/schema/mod.rs:777`), the provider protocol's list type
(`carina-provider-protocol/src/types.rs:581`), host conversions such as
`carina-plugin-host/src/wasm_convert.rs:943`, and provider schema
codegen. It would also make merge schema-aware. None of that plumbing is
required for #3728.

### Display pairing remains independent

The display layer has its own two-phase list-of-maps pairing in
`compute_list_of_maps_diff_parts`
(`carina-core/src/detail_rows.rs:1422`): schema-aware exact matches first
(`carina-core/src/detail_rows.rs:1465`), then unmatched elements paired
with `value::map_similarity` (`carina-core/src/detail_rows.rs:1492` and
`carina-core/src/value.rs:1435`). It does not consume the merge's pairing
mapping.

In a rare heuristic tie or schema-normalization case, plan correctness
may identify one modified pair while display partitions the same values
as a different modified pair or as add/remove. Unifying those matchers
is a separate display design and is out of scope. This change must not
make `detail_rows` index current provider output with the stored
`ListElements` vector.

### No DSL-level explicit null/unset

An explicit DSL unset spelling such as `description = null` was
considered and rejected. The value model has no null variant
(`ConcreteValue` is defined at `carina-core/src/resource/mod.rs:879`), so
adding one would affect parsing, validation, inference, equality,
formatting, plan/state serialization, the WIT/provider wire format, and
unset behavior in both providers. It would also create two spellings for
the same struct-field operation: once prior authoring is known, omitting
a previously-authored field already means unset. That blast radius is
not justified for a provenance problem that can be solved in state and
merge.

### Other boundaries

- No persistent list-element IDs are added to state.
- No provider or WIT contract change is part of the chosen design.
- No parser, formatter, LSP, or DSL syntax change is required.
- No `Cargo.toml` change is required.
- Projection remains union-based; per-element removal authority exists
  only in saved merge.

## File touch map for implementation

### carina-core

- `carina-core/src/explicit.rs` — add `ListElements`; make
  `build_from_value` emit it for every concrete list; add aligned and
  conservative construction helpers; define the lossy `merge` cases;
  project it through a union; make raw `build_from_resource`
  crate-private with its persistence warning; update top-level
  pass-through and tests.
- `carina-core/src/resource/mod.rs` — extract one shared list pairing
  function from the quadratic/hash-assisted merge paths; thread both
  saved views and prior authoring through `merge_lists`; select
  `elements[saved_index]` only under the length invariant and when raw
  and projected pairing corroborate that index.
- `carina-core/src/resource/tests.rs` — replace the #3727 conservative
  guard's new-variant counterpart with precise paired behavior while
  retaining a separate legacy-`List` regression test.
- `carina-core/src/differ/comparison.rs` — add `ListElements` to the
  exhaustive unexpected-root fallback and carry both raw and projected
  saved views through `SavedAttr`.
- `carina-core/src/differ/plan_tests.rs` and/or
  `carina-core/src/differ/comparison_tests.rs` — add the end-to-end
  list-of-structs removal case.

### carina-state

- `carina-state/src/state/mod.rs` — call the aligned builder at the row
  writeback seam, bump v9 → v10 with a version-history line, and
  recursively demote `ListElements` in the carina#3280 idempotency
  preserve branches. Do not add a migration function; do not change
  `build_explicit` or `is_empty_explicit`.
- `carina-state/src/state/tests.rs` — cover stored-order pairing,
  provider-added/unmatched elements, nested alignment, repair
  idempotency, v9 legacy read, and v10 serde.

### carina-cli

- `carina-cli/src/commands/shared/state_writeback.rs` needs no new data
  source: it already passes desired `Resource` and provider `State` into
  the state-row constructor. Its apply-path tests should assert that the
  row's stored list and explicit vector are written together.
- Plan snapshot coverage is needed only if the new correctness case has
  a stable user-visible fixture; no fixture should be mechanically
  rewritten merely to replace valid legacy `List` records.

No other file is part of the design change, and the design document
itself introduces no code or manifest changes.

## Testing plan

1. **Serde pin.** Round-trip `ListElements` and assert the exact kind
   string is `"list-elements"`; retain the existing `List` round-trip.
2. **Raw builder.** Assert `build_from_value` emits `ListElements` for an
   empty list, a scalar list, a list of heterogeneous maps, and a nested
   list. Assert `StringList` and a whole deferred/unknown value remain
   `Leaf`.
3. **Union algebra and projection.** Pin every new root `merge`
   combination above, prove no root result is `ListElements`, pin the
   intentional nested-survival case for an unmatched `Struct` child,
   and show projection uses the same union for every current element
   regardless of current order.
4. **Writeback pairing.** Cover aligned same-order and reordered values;
   provider-returned extra elements; provider-returned missing elements;
   score-0 ambiguity degrading to `Unrecorded`; duplicates; nested lists;
   and the malformed-length defensive fallback.
5. **Paired saved merge.** Starting from two heterogeneous rules, remove
   an authored field from only one desired element. Assert the field is
   dropped only from effective desired for that paired element, while a
   same-named provider default on its sibling remains. Run corroborated
   attribution through both the small quadratic path and a
   large-list/hash-assisted path, including a raw/projected disagreement
   that must degrade to `Unrecorded`.
6. **Diff-level reproduction.** Add a list-of-structs plan/differ test
   that is impossible to satisfy today: the previously-authored
   `description` is absent from the new desired element, current still
   has it, and the plan reports an update/removal for exactly that
   element.
7. **Legacy v9 behavior.** Deserialize a v9 row containing
   `List { element }`, verify the tree survives the in-memory v10 lift,
   round-trip it, and assert merge remains conservative for every list
   element. A subsequent writeback should replace it with aligned
   `ListElements`.
8. **Reorder between applies.** Write one row, simulate a later provider
   read returning the same elements in a different order, write again,
   and assert the vector realigns. Plan between the two writes and prove
   desired-to-saved pairing still selects the correct element.
9. **Atomic row assertion.** Through the apply writeback seam, inspect
   the resulting `ResourceState` and assert `attributes[list][j]` and
   `explicit.ListElements.elements[j]` describe the same element before
   serialization and after state round-trip.
10. **Projection index stability.** Project a three-element list with
    distinct sentinel identities through both a legacy `List` node and a
    `ListElements` node whose elements fold to a union. Assert that each
    result still has length three and that the sentinel sequence at
    indices 0, 1, and 2 is unchanged, pinning projection as a per-item,
    order-preserving map.

All tests are local core/state/CLI tests. No AWS acceptance test or real
cloud mutation is required for the implementation.

## Phasing

The conceptual phases are:

1. **Core semantics:** the new variant, raw/aligned construction, shared
   pairing, plan-time consumption, lossy union merge, and union
   projection in carina-core.
2. **Persistence:** state writeback pairing, state v10, repair
   idempotency, and carina-state/carina-cli seam tests.

Whether these land as one PR or two is deliberately left to the
implementation-planning step after the actual radius is measured. A
single PR is the simplest consistent boundary.

If split, the only safe order is **consumers first, emitter second**.
The consumer PR may add `ListElements`, serde support, projection,
`merge`, and plan-time fallback/consumption while no production builder
emits it. The emitter PR then switches construction and state writeback
and bumps v10 atomically. Emitter-first is unsafe because a state file
could contain a kind that planning does not understand.

There is one concrete coupling to respect: final state writeback calls
`build_from_resource_for_stored_values` at the row-construction seam,
while the `Unrecorded` self-heal uses `build_from_value` on stored-order
provider values. Therefore changing `build_from_value` to return
`ListElements` is itself an emission switch even if the edit is
physically in carina-core. A mergeable consumer-first PR must either
leave that builder behavior legacy until the emitter PR or explicitly
keep production writeback on a legacy builder. The final builder switch,
aligned row construction, all consumer arms, and the v10 bump must never
be separated. Dormant consumer support is safe; emitted records without
complete consumers or without the version bump are not.

## Summary

<!-- derived-from #design-decision -->
<!-- derived-from #invariants-after-this-change -->
<!-- derived-from #authoring-construction-and-writeback -->
<!-- derived-from #plan-time-consumption -->
<!-- derived-from #projection-remains-union-based -->
<!-- derived-from #state-schema-v10 -->

`ListElements` records authoring in the stored row's list order, paired
once from desired and provider-returned values during writeback.
Planning keeps projected-list pairing for value merging and consults a
stored-index record only when an independent raw-list pairing selects
the same index. Any missing or conflicting evidence falls back to
`Unrecorded` or legacy `List`, making imprecision non-destructive.
Projection stays an order-independent union, v9 rows remain valid and
conservative, and v10 protects older binaries from the new serde kind.
