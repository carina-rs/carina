# Schema-Bound Type-Ref Projection Design (awscc#290 hazard-1)

<!-- derived-from ./2026-05-21-resource-kind-typestate-design.md -->

## Status

Design proposal. Implements the type-level half of the awscc#290 /
carina#3340 / carina#3349 line of work. The opaque-`AttributeType` +
`Shape` reshape (carina#3349) already landed; this doc closes the
remaining hole it left.

## Background

`carina-core/src/schema/mod.rs` exposes:

- `AttributeType::resolve_refs(&self, defs: &BTreeMap<String, AttributeType>) -> ResolvedAttrType` (~1450)
- `AttributeType::shape(&self, defs: &BTreeMap<String, AttributeType>) -> Shape` (~1490, calls `resolve_refs`)
- `empty_defs() -> &'static BTreeMap<String, AttributeType>` (~1425)

`resolve_refs` peels `AttrTypeKind::Ref(name)` by `defs.get(name)` and
**panics** (`mod.rs:1461`) when the name is absent. `empty_defs()`
returns a `&'static` empty map.

`Ref` is produced **only** by the cyclic-CFN codegen, populated inside a
`ResourceSchema.defs` / `Schema.defs` graph (carina#3340;
`convert.rs` `ProtoAttributeType::Ref => CoreAttributeType::ref_`).
There is no other producer.

### The residual hazard

The `defs` argument is a **loose `&BTreeMap` decoupled from the
`&AttributeType`** it must resolve. Nothing in the type system stops a
caller passing the wrong/empty map for a Ref-bearing schema-derived
type:

```rust
ref_bearing_type.shape(empty_defs())   // compiles, panics at runtime
```

This is not theoretical. awscc#286 / awscc#288 were **live instances**:
`carina-provider-awscc/.../provider/normalizer.rs` documents it directly
(`:91-92`, `:1197-1199`) — peeling a `Ref("CaptchaConfig")` field
against `empty_defs()` panics in `AttributeType::shape`, poisons the
WASM instance, and silently strips `default_tags`. The runtime fix
(awscc PR #289) threaded the real defs at the offending call sites, but
the **type system still permits** the mistake at any sibling/future
call site. carina#3349 made `Ref` unmatchable from outside `schema`
(the opaque struct + `Shape` with no `Ref` variant); it did **not**
couple `defs` to the type. This doc does.

> The carina#3349 -> awscc#290 lesson (already recorded in CLAUDE.md
> "Root-cause fixes only"): a runtime resolver threaded at N consumer
> sites that passes review is **not** root-cause if the type still
> permits the bug. The fix is to make the broken state unrepresentable.

## Goal / acceptance bar

Make `ref_bearing_type.shape(empty_defs())` (or any wrong-defs call for
a schema-derived type) **impossible to write**. The panic must become
either a **compile error** (schema-coupled path) or a **typed
`Option`/`Result`** the caller must handle (bare path) — eliminated
entirely, not relabeled.

## Call-site classification (non-test production callers)

Every `Ref` lives inside a `ResourceSchema.defs` graph, so a bare
`AttributeType` is safe to project without defs **iff** it is provably
outside any defs graph. Sites fall into three classes.

### Bucket A — schema is in scope (fixed for free by threading)

The caller already has `&Schema` / `&ResourceSchema` / `&s.defs`
reachable and passes `empty_defs()` only because the old API did not
thread it, or threads `defs` already.

- carina-core: `detail_rows.rs:604` (`build_update_rows`),
  `detail_rows.rs:767` (`build_replace_rows`),
  `detail_rows.rs:348/1213`, `diff_helpers.rs:60`,
  `differ/comparison.rs:74/443/556`, `upstream_exports.rs:613`,
  `schema/mod.rs:4103/4112`, `validation/mod.rs` (several),
  `validation/inference.rs:505/557/587`,
  `validation/deferred_populate.rs:276`, `value.rs:1395`.
  The `schema.map(|s| &s.defs).unwrap_or(empty_defs())` ternaries
  (diff_helpers / detail_rows / comparison) collapse to a clean
  `match schema { Some(s) => s.shape_of(t), None => t.shape_ref_free() }`.
- carina-provider-awscc: `provider/normalizer.rs:119`,
  `provider/normalizer.rs:794-884`, `provider/conversion.rs:33/228`,
  `provider/operations.rs:299` — already thread `&config.schema.defs`.

### Bucket B1 — structurally Ref-free (safe no-defs path)

Operates on a bare `AttributeType` that **cannot** contain a `Ref` by
construction.

- carina-lsp `completion/top_level.rs:453` and
  `completion/values.rs:1270`: types come from
  `type_expr_to_attribute_type` / `parse_exports_type_text`. `TypeExpr`
  (the DSL surface grammar) has **no `Ref` production** — confirmed by
  the in-code comments at `top_level.rs:448-450` and
  `values.rs:1983-1986`.
- carina-core `utils.rs:698` (`resolve_enum_value_recursive`) and
  `utils.rs:934` (`lift_string_enum_leaves`): the public "no schema in
  scope" wrappers; Ref-free **by contract**, not yet by type — this is
  part of the hole.
- carina-lsp `completion/values.rs:551/565/1987`,
  `completion/mod.rs:801`: scalar/Map-key types, no Ref.

### Bucket B2 — latent panics (the real hazard)

Operates on a bare `AttributeType` that **flows from a schema** and can
contain a `Ref`, but the caller passes `empty_defs()` and "knows" (by
fragile convention) there is none.

- **carina-provider-awscc `provider/normalizer.rs:649/680/712/979`** —
  pass `empty_defs()` into the same `resolve_struct_enum_values`
  recursion that `:119` correctly threads `&config.schema.defs` into.
  These are **the awscc#286 panic pattern still latent** at sibling
  call sites.
- carina-provider-aws `normalizer.rs:171/309` — recurse over Struct
  fields; safe only because aws schemas are hand-written-flat today,
  fragile under future codegen.
- carina-lsp `completion/mod.rs:787` (`resolve_map_key_type`) — type
  came from a real schema via `resolve_type_for_path` but the defs map
  was dropped; a Map-of-Ref would panic.

carina-plugin-host's five `wasm_convert.rs` `empty_defs()` sites are all
`#[cfg(test)]`; the production conversion path uses `raw_shape()` (the
Ref-preserving projection) and is unaffected.

## Considered designs

### (a) Sealed `Defs<'a>` newtype with a safe `Defs::empty()`

Replace `&BTreeMap<...>` with a sealed `Defs<'a>` constructible only via
`schema.defs()` (real) or `Defs::empty()` (sanctioned empty).

- **Rejected.** `t.shape(Defs::empty())` still compiles for a Ref type —
  `Defs::empty()` is the same foot-gun with a nicer name. The escape
  hatch reopens the hole. ~100+ mechanical rewrap sites for no
  type-level guarantee. Does not meet the bar.

### (b) Typestate: schema-bound projection + Ref-free bare path (RECOMMENDED)

Two coordinated additions:

1. **Schema-bound projection.** Add
   `Schema::shape_of(&self, &AttributeType) -> Shape` and
   `ResourceSchema::shape_of(&self, &AttributeType) -> Shape` (plus
   `resolve_of`). These borrow `self.defs` internally; the caller never
   names a defs map. **A Ref type can be projected only through an
   object that owns the defs that can resolve it.**

2. **Ref-free bare path.** Add
   `AttributeType::shape_ref_free(&self) -> Option<Shape>` (or
   `Result<Shape, RefEncountered>`) that peels nothing and returns
   `None` / `Err` the instant it meets a `Ref` — **no defs, no panic**.
   This is the sanctioned entry point for bucket B1 and for any
   genuinely-no-schema caller. It cannot panic and forces the caller to
   handle the Ref case.

The old `shape(defs)` / `resolve_refs(defs)` / `empty_defs()` become
`#[deprecated]` thin shims (panic behavior intact) during the
deprecation window, then are deleted.

- **(i) Unwritable?** Yes. To project a Ref you must go through
  `schema.shape_of(t)` (real defs guaranteed). To project without a
  schema you use `shape_ref_free()`, which returns `Option`/`Result`
  and physically cannot panic. `shape(empty_defs())`-for-a-Ref-type is
  deleted from the public API.
- **(ii) Escape hatch reopens hole?** No. `shape_ref_free()`'s escape is
  a typed `None`/`Err`, not a panic. The B2 sites are **forced at
  compile time** to either obtain the schema (becoming bucket A) or
  accept `Option` semantics — converting the latent awscc#286 panics
  into handled control flow.
- **(iii) Radius:** comparable site count to (a) (~100+), but the edits
  are semantically meaningful, not mechanical rewraps. Bucket-A sites
  already have the schema in scope. Bucket-B1 sites adopt
  `shape_ref_free()` with a `None` arm matching their existing
  "fall silent / return None" contract.
- **Verdict:** meets the bar. Recommended.

### (c) Split type `RefFreeAttributeType` vs `AttributeType`

A distinct `RefFreeAttributeType` statically guaranteed Ref-free;
no-defs `shape()` lives only on it.

- **Rejected.** Strongest guarantee but largest radius by far — every
  constructor, every `StructField::field_type` / `List::inner` /
  `Map::key/value`, and the hundreds of generated provider schema files
  (`carina-aws-types/src/lib.rs`, aws/awscc `schemas/generated/**`) plus
  the codegen crates would need to pick a variant or convert. A
  long-lived two-type schism. Disproportionate.

## Recommendation

**Adopt (b).** It is the only candidate that turns the panic into a
compile error (schema-coupled path) plus a typed `Option`/`Result`
(bare path), without the whole-ecosystem schism of (c).

### Breaking-change handling across the git pin

Both provider repos pin carina-core by exact git rev
(`carina-provider-aws/.../Cargo.toml`, `carina-provider-awscc/.../Cargo.toml`)
and call `AttributeType::shape(defs)` / `empty_defs()` directly in
production. Removing/renaming them is a **breaking API change** ->
requires the coordinated cross-repo provider-PR pattern (the same shape
as carina#3241 / carina#2807).

But carina-core PR1 can land **additively**: add the new API, keep the
old as `#[deprecated]` shims (old behavior, panic intact) so the pinned
providers still compile against the new rev, and migrate every in-repo
caller in the same PR. Providers are unaffected until someone bumps the
rev; the shims guarantee a green bump.

### PR chain

1. **carina-core PR1** (in-repo, additive + internal migration). Add
   `Schema::shape_of` / `ResourceSchema::shape_of` / `resolve_of` and
   `AttributeType::shape_ref_free`. Deprecate `shape` / `resolve_refs` /
   `empty_defs`. Migrate carina-core + carina-lsp + carina-plugin-host
   (test-only there). Fixes the in-repo latent sites (LSP
   `mod.rs:787`, `values.rs:1987`). Lands independently — no provider
   coordination.
2. **carina-provider-aws PR2.** Bump carina-core rev to PR1; migrate
   `normalizer.rs:171/309`, `iam/role.rs`, `schemas/types.rs`,
   `carina-aws-types/src/lib.rs`. Audit the B2 recursion sites onto
   `shape_ref_free` so a future Ref cannot panic.
3. **carina-provider-awscc PR3** (highest value). Bump rev; migrate.
   The latent awscc#286-class sites
   (`provider/normalizer.rs:649/680/712/979`) are **forced by the
   compiler** to thread `config.schema.defs` (via `shape_of`) — this is
   the structural fix awscc#290 hazard-1 asks for. Bulk
   conversion/operations sites adopt `shape_ref_free`.
4. **carina-core PR4** (optional, later). Once both providers are bumped
   past PR2/PR3, delete the deprecated shims, sealing the hole
   permanently.

PR2 and PR3 are independent of each other; both depend on PR1. PR4
depends on both.

### Does (b) eliminate the panic entirely?

Yes, at the acceptance bar, once the chain completes (PR3 for the
providers in practice, PR4 to remove the shims):

- The schema-coupled projection (`schema.shape_of(t)`) owns defs that
  can resolve any Ref `t` carries, so the "wrong/empty defs"
  precondition is **unconstructible** — compile-time.
- The bare projection (`shape_ref_free()`) **cannot panic** — it returns
  `None` / `Err(RefEncountered)` instead of dereferencing a missing
  defs entry.
- After PR4 removes `shape(defs)` / `empty_defs()`, no public API
  through which `ref_type.shape(empty_defs())` can be written remains.

Bounding caveat: the internal `pub(crate)` threaded `shape(defs)` (used
by the schema-walk recursion behind `shape_of`) can still be miscalled
**inside** carina-core, but it is no longer public, in-repo callers
always thread `self.defs`, and the 256-hop guard (`mod.rs:1457`) remains
a backstop. The externally reachable, type-permitted panic — the actual
awscc#290 hazard-1 — is eliminated.

## TDD note

PR1 must include a compile-fail test (e.g. `trybuild`) asserting that
`empty`-style projection of a Ref-capable type does not type-check
through the new public API, plus a unit test that `shape_ref_free()`
returns `None`/`Err` (not panic) on a `Ref`. Per CLAUDE.md "TDD for Bug
Fixes" and "make the broken state unrepresentable".

## Critical files

- `carina-core/src/schema/mod.rs` — `empty_defs` (1425), `resolve_refs`
  (1450), `shape` (1490), `Schema` (872), `ResourceSchema` (3552); add
  `shape_of` / `resolve_of` / `shape_ref_free`.
- `carina-core/src/utils.rs` — 698, 934 (bucket-B1 wrappers).
- `carina-lsp/src/completion/values.rs` — 551, 565, 1270, 1987;
  `carina-lsp/src/completion/mod.rs` — 787 (latent).
- `carina-provider-awscc/.../provider/normalizer.rs` — 119 vs
  649/680/712/979 (the A vs latent-B split the reshape forces).
- `carina-provider-aws/.../normalizer.rs` — 171; the rev pins in both
  providers' `Cargo.toml`.
