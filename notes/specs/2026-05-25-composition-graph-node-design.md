# Composition + GraphNode: Design

<!-- derived-from ./2026-05-21-resource-kind-typestate-design.md -->

## Goal

Complete the `#3169` typestate split by reshaping the IR around three
explicit ideas:

1. **Rename `ManagedResource` → `Resource`** now that the ambiguous
   "Resource = three-way umbrella" role has been retired by the
   `#3181` series.
2. **Rename `VirtualResource` → `Composition`** to reflect that the
   node is not a "virtual stand-in for a resource" but the
   composition-call site that expands into leaf effects.
3. **Introduce `GraphNode`** as the explicit umbrella enum, replacing
   today's "three parallel `Vec`s threaded through every function
   signature" pattern with a single typed graph node.

On top of that, encode three further invariants in the type system:

- **`Signature` struct on `Composition` only** carrying the
  `arguments` (resolved call-site inputs) and `attributes` (module
  outputs) that uniquely make a composition function-shaped.
  `Resource` / `DataSource` keep their existing `attributes` field
  unchanged — they have no `arguments` concept, since the DSL's
  `argument`/`attribute` keywords are module-only.
- **`PersistentId` / `EphemeralId`** newtypes so "this id may not be
  looked up in state" is a compile-time fact, not a convention.
- **`CompositionAttribute = Forwarded | Derived`** so the post-apply
  resolver can distinguish single-leaf aliasing from multi-leaf
  computation without inspecting `Value` variants at runtime.
- **`ExpansionTrace`** as a plan-scoped sidecar carrying
  leaf-to-composition lineage for display, kept out of the persisted
  state.

## Non-goals

- Changing on-disk state file schema. State already excludes
  `VirtualResource`; this design does not alter that boundary.
- Touching DSL syntax. `let x = read aws...`, `let x = module_call
  { ... }`, anonymous `<provider>.<type> { ... }` all stay byte-
  identical at the parser surface.
- Reworking the provider plugin protocol or WIT boundary. The
  protocol's `Resource` type is independent of `carina-core`'s
  `Resource` — they have always been separate types connected by
  `convert::proto_to_core_*`, and this design does not change that.
- Replacing `ResourceLike` immediately. The trait stays through PRs
  E–G and is removed in the final PR once `attributes` accessor call
  sites have been migrated to direct field access (`r.attributes` for
  resources / data sources, `c.signature.attributes` for compositions).

## Background

### Where the `#3169` series landed

The typestate split (design PR #3172, implementation chain through
#3181's A–E PRs ending #3200) produced this shape:

```rust
// carina-core/src/resource/
pub struct ManagedResource { ... }     // CRUD path
pub struct DataSource     { ... }      // read-only
pub struct VirtualResource { ... }     // module-call expansion proxy

pub trait ResourceLike { ... }          // shared accessors
```

Three siblings, runtime `kind` discriminant gone, `compile_fail`
doctests pinning the "VirtualResource may not enter the pre-apply
differ" invariant (`carina-core/src/differ/plan.rs:147-171`). State
does not know about `VirtualResource` at all (`carina-state` has
zero references to the type).

### What still leaks

Three things the `#3181` series intentionally deferred:

1. **No umbrella type.** Every function that operates on "all nodes"
   threads three slices in parallel:

   ```rust
   pub fn create_plan(
       managed: &[ManagedResource],
       data_sources: &[DataSource],
       ...
   ) -> Plan { ... }
   ```

   Callers must remember the threading order. Adding a fourth node
   class would touch every signature.

2. **Composition's call boundary is implicit.** A `Composition` is
   the only node that is genuinely function-shaped: it takes
   resolved call-site arguments (`ModuleCall.arguments`) and exposes
   module outputs (`AttributeParameter` resolved values, stored in
   `Composition.attributes`). Today those two sides are not joined
   on the composition itself — the arguments live on the
   `ModuleCall`, which gets dropped at expansion time, leaving no
   in-IR record of what was passed in. Reasoning about a composition
   post-expansion requires consulting a separately-tracked call
   site, which the type system does not enforce.

   `Resource` and `DataSource` do not have this problem: their DSL
   syntax (`<provider>.<type> "<name>" { key = value }`) uses a
   single `attributes` namespace for user-written input, and there
   is no `arguments` concept in DSL or runtime. The `attributes`
   field on those structs is fine as-is.

3. **`ResourceId` is single-flavour.** Both leaf nodes (which key
   into state) and virtual nodes (which never persist) use the same
   `ResourceId` type. The "lookup this id in state" call sites have
   no static signal that a virtual id will always miss.

### Why the names need to move

`ManagedResource` was the right name *while* `Resource` still meant
"three-way umbrella" — it disambiguated. With the umbrella role
vacated by `#3181` and reassignable to a new `GraphNode` enum,
`Resource` becomes free to denote the CRUD variant specifically, and
the awkward `Managed*` prefix can drop.

`VirtualResource` is more misleading than helpful. The node is not
"a virtual version of a resource"; it is the composition-call site
that disappears at expand time. Naming it `Composition` makes the
role-by-name explicit and aligns with Carina's effects-as-values
framing: a composition is a function from arguments to a bundle of
leaf effects.

## Design

### 1. Umbrella enum

```rust
pub enum GraphNode {
    Resource(Resource),
    DataSource(DataSource),
    Composition(Composition),
}
```

Function signatures that previously threaded three slices collapse
to `&[GraphNode]`. Variant-specific paths (the pre-apply differ, the
provider CRUD dispatch) take the leaf-only subtype defined below.

### 2. Leaf subtype for post-expansion paths

```rust
pub enum LeafNode {
    Resource(Resource),
    DataSource(DataSource),
}
```

The expand phase has signature:

```rust
fn expand(graph: Vec<GraphNode>) -> (Vec<LeafNode>, ExpansionTrace);
```

By construction the post-expansion view cannot carry a `Composition`.
This is the type-level version of today's `compile_fail` doctest on
`create_plan` — instead of pinning the invariant by an artificial
test, the differ's signature simply takes `&[LeafNode]` and the
compiler enforces the boundary at every call site.

### 3. `Signature` on `Composition` only

```rust
pub struct Signature {
    /// Resolved call-site arguments. Populated from
    /// `ModuleCall.arguments` at expansion time, so the call boundary
    /// is recorded on the expanded node itself instead of being lost
    /// when the `ModuleCall` is dropped.
    pub arguments:  IndexMap<String, Value>,
    /// Resolved module-output values. Today's `Composition.attributes`
    /// content lives here, unchanged in semantics.
    pub attributes: IndexMap<String, Value>,
}

pub struct Composition {
    pub id: ResourceId,
    pub signature: Signature,
    pub binding: Option<String>,
    pub module_name: String,
    pub instance: String,
    pub dependency_bindings: BTreeSet<String>,
    pub quoted_string_attrs: HashSet<String>,
}
```

`Signature` is **not** shared with `Resource` or `DataSource`. Those
structs keep their existing `attributes: IndexMap<String, Value>`
field — the DSL gives them a single user-written input namespace with
no `arguments`/`attributes` split, so a `Signature` would be a
pretextual abstraction with one populated half.

`ResourceLike::attributes()` continues to work for all three siblings
by reading either the bare `attributes` field (resources, data
sources) or `signature.attributes` (composition). The trait stays
through the series and is removed in the final PR, once every
accessor goes through field access directly.

The value of `Signature` is twofold:

1. **Boundary preservation.** Today's expander writes
   `ModuleCall.arguments` into nothing post-expansion; the call
   record is lost. With `Composition.signature.arguments` the
   inputs that produced this composition are inspectable on the
   expanded node, enabling later debug / display / diff features
   without re-running the parser.
2. **`CompositionAttribute` lift target.** PR G replaces
   `signature.attributes`' value type from `Value` to
   `CompositionAttribute`. Pinning the field path on `signature`
   keeps that diff localized to one struct.

### 4. ID typestate

```rust
pub struct PersistentId(ResourceId);   // Resource, DataSource
pub struct EphemeralId(ResourceId);    // Composition
```

Helpers that touch state (`StateBackend::load`,
`current_states[id]`, etc.) take `&PersistentId`. The `Composition`
variant exposes `EphemeralId` and has no `From<EphemeralId> for
PersistentId` conversion. A code path that tries to look up a
composition's id in state stops compiling.

The wrapping is intentionally cheap (newtype around the existing
`ResourceId`); serialization round-trips through the inner type so
on-disk state is unchanged.

### 5. Composition attribute classification

PR G changes the value type of `Signature.attributes` so that the
expander can record *how* each module output was constructed, not
just its current resolved value:

```rust
pub enum CompositionAttribute {
    /// Single-hop alias to another node's attribute.
    Forwarded(NodeId, AttrPath),
    /// Multi-source expression, evaluated after post-apply state
    /// is available.
    Derived(Expr),
}

pub struct Signature {
    pub arguments:  IndexMap<String, Value>,
    pub attributes: IndexMap<String, CompositionAttribute>,
}
```

- `Forwarded(node, path)` — the attribute is a single-hop alias to
  `node.path`. Display can fold the alias; dependency analysis adds
  one edge.
- `Derived(expr)` — the attribute is a multi-source expression. The
  resolver evaluates the expression after post-apply state is
  available.

Today's `Composition.attributes: IndexMap<String, Value>` collapses
both cases into one `Value`-shaped slot and re-classifies them at
runtime. Splitting them at the type level removes the runtime
classification and matches how the resolver actually consumes them.

### 6. Expansion trace

```rust
pub struct ExpansionTrace {
    pub leaf_to_call_sites: BTreeMap<PersistentId, Vec<EphemeralId>>,
}
```

Built during `expand`. Consumed by the display layer to fold leaf
rows under their originating composition:

```
+ Composition "cluster"
    + aws.eks.Cluster      cluster/inner
    + aws.iam.Role         cluster/inner-role
+ aws.s3.Bucket            logs
```

The trace is never persisted. The next plan rebuilds it from DSL.
Composition renames or restructures incur zero state migration; only
the rendered tree changes.

## Task decomposition

Each PR is independently mergeable and lands the rename or
type-introduction it advertises. Sequencing keeps blast radius
contained.

| PR | Scope |
| --- | --- |
| **A** | Rename `ManagedResource` → `Resource`. Mechanical sweep; the `Resource` name is currently unused after `#3181`. |
| **B** | Rename `VirtualResource` → `Composition` (struct + module + use sites). Mechanical sweep. |
| **C** | Introduce `GraphNode` enum. Collapse three-slice signatures (`create_plan`, resolver entry points, parser output) to `&[GraphNode]`. Largest of the series. |
| **D** | Introduce `LeafNode` subtype and re-shape `expand` to produce `Vec<LeafNode>`. Replace the `compile_fail` doctest with a real type-level guarantee at the differ entry. |
| **E** | Introduce `Signature` struct on **`Composition` only**, with `arguments` + `attributes` fields. Populate `arguments` from `ModuleCall.arguments` at expansion time; move `Composition.attributes` content into `signature.attributes`. `Resource` and `DataSource` are not touched. `ResourceLike::attributes()` reads `signature.attributes` for the `Composition` arm. |
| **F** | Introduce `PersistentId` / `EphemeralId` newtypes; thread them through state and resolver boundaries. |
| **G** | Introduce `CompositionAttribute = Forwarded \| Derived`; migrate `Signature.attributes`'s value type from `Value` to `CompositionAttribute`; update resolver to dispatch on the variant. |
| **H** | Introduce `ExpansionTrace`; add display folding by composition; remove ad-hoc `module_name`/`instance` field reads where they were a proxy for trace lookup. |

Suggested merge order: A → B → C → D → E → F → G → H. Each step
leaves the tree green; F/G/H can be parallelised after E lands.
PR H also retires `ResourceLike` once all `attributes` accessor call
sites have been migrated to direct field access.

## Acceptance

- All `#3169`-era `compile_fail` doctests either survive or are
  superseded by stronger type-level guarantees (signature-level
  separation of `GraphNode` vs `LeafNode`, of `PersistentId` vs
  `EphemeralId`).
- `carina-state` still contains zero references to `Composition`.
- `cargo nextest run --workspace --all-features` green.
- `cargo test --workspace --doc` green (includes the surviving
  `compile_fail` cases).
- `bash scripts/check-*.sh` green.
- Real-infra smoke: `carina plan` against
  `carina-rs/infra/envs/registry/dev/infra-deploy/` produces a plan
  whose composition folding renders cleanly.
