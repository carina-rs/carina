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

- **`Signature` struct** for the shared `arguments`/`attributes`
  surface that all three variants expose.
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
- Replacing `ResourceLike` immediately. The trait coexists with
  `Signature` field-sharing during the migration and is removed in
  the final PR of the series, once every accessor goes through
  `signature.*` directly.

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

2. **`Signature` duplication.** `ManagedResource`, `DataSource`, and
   `VirtualResource` each carry their own `attributes` map plus
   arguments. `ResourceLike` papers over the duplication at the
   accessor level, but the underlying fields remain triplicated.

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

### 3. Shared `Signature`

```rust
pub struct Signature {
    pub arguments:  BTreeMap<String, Value>,
    pub attributes: IndexMap<String, AttributeType>,
}
```

Each variant carries `signature: Signature` as a field. `ResourceLike`
accessors that today return slices into per-struct fields can be
re-pointed at `signature.arguments` / `signature.attributes` without
breaking existing callers. The trait stays during migration; whether
to retire it after all field accesses go through `signature` is a
follow-up question, not a prerequisite.

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

```rust
pub enum CompositionAttribute {
    Forwarded(NodeId, AttrPath),
    Derived(Expr),
}

pub struct Composition {
    pub signature: Signature,
    pub call_site: EphemeralId,
    pub attributes: IndexMap<String, CompositionAttribute>,
    pub body: CompositionBody,
}
```

- `Forwarded(node, path)` — the attribute is a single-hop alias to
  `node.path`. Display can fold the alias; dependency analysis adds
  one edge.
- `Derived(expr)` — the attribute is a multi-source expression. The
  resolver evaluates the expression after post-apply state is
  available.

Today's `VirtualResource.attributes: IndexMap<String, Value>` collapses
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
| **E** | Introduce `Signature` struct and re-point `Resource` / `DataSource` / `Composition` field accesses through it. `ResourceLike` impls delegate to `signature`. |
| **F** | Introduce `PersistentId` / `EphemeralId` newtypes; thread them through state and resolver boundaries. |
| **G** | Introduce `CompositionAttribute = Forwarded \| Derived`; migrate `Composition.attributes` to the new shape; update resolver to dispatch on the variant. |
| **H** | Introduce `ExpansionTrace`; add display folding by composition; remove ad-hoc `module_name`/`instance` field reads where they were a proxy for trace lookup. |

Suggested merge order: A → B → C → D → E → F → G → H. Each step
leaves the tree green; F/G/H can be parallelised after E lands.
PR H also retires `ResourceLike` once all field accesses go through
`signature.*`.

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
