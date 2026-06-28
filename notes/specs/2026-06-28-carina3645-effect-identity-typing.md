# Effect Identity Typing

This spec closes the remaining identity-axis gap in scheduler-bound
`Effect` payloads: every `Effect` variant must carry identity in a type
that makes "identity is present" a compile-time fact.

Refs #3645. Builds on the [identity-axis spec](./2026-06-28-carina3632-identity-axis.md).

## 1. Summary

The resolver boundary is the point after which every resource identity is
present. Downstream consumers already moved most scheduler-bound effects to
`ResolvedResourceId`; this spec finishes that migration for the remaining
payloads:

- `Effect::Create(Resource)`
- `Effect::Read { resource: DataSource }`
- `Effect::Wait { binding: String }`
- `Resource` payloads inside `Update`, `Replace`, and `CascadingUpdate`

After this change, a resource or data source without identity cannot be stored
inside an `Effect`.

## 2. New resolved payload types

Add two newtypes in `carina-core/src/resource/mod.rs`, next to
`ResolvedResourceId`.

```rust
pub struct ResolvedResource(Resource);
pub struct ResolvedDataSource(DataSource);
```

Both follow the same pattern as `ResolvedResourceId(ResourceId)`:

| Type | Invariant | Constructor | Borrowing |
|---|---|---|---|
| `ResolvedResource` | `resource.id.identity.is_some()` | `new(Resource)` asserts; `try_new(Resource) -> Option<Self>` | `Deref<Target = Resource>`, `as_inner()`, `into_inner()` |
| `ResolvedDataSource` | `data_source.id.identity.is_some()` | `new(DataSource)` asserts; `try_new(DataSource) -> Option<Self>` | `Deref<Target = DataSource>`, `as_inner()`, `into_inner()` |

Serde uses the inner type's shape. `ResolvedResource` serializes as a
`Resource`, and `ResolvedDataSource` serializes as a `DataSource`; deserialization
validates the identity invariant with `try_from`/`into` or equivalent custom
serde. Missing identity is a deserialize error, not a late scheduler error.

## 3. Effect shape changes

| Variant | Before | After |
|---|---|---|
| `Read` | `Read { resource: DataSource }` | `Read { resource: ResolvedDataSource }` |
| `Create` | `Create(Resource)` | `Create(ResolvedResource)` |
| `Update` | `Update { id: ResolvedResourceId, from: Box<State>, to: Resource, changed_attributes: Vec<String> }` | `Update { from: Box<State>, to: ResolvedResource, changed_attributes: Vec<String> }` |
| `Replace` | `Replace { id: ResolvedResourceId, from: Box<State>, to: Resource, ... }` | `Replace { from: Box<State>, to: ResolvedResource, ... }` |
| `Delete` | `Delete { id: ResolvedResourceId, ... }` | Unchanged |
| `Import` | `Import { id: ResolvedResourceId, ... }` | Unchanged |
| `Remove` | `Remove { id: ResolvedResourceId }` | Unchanged |
| `Move` | `Move { from: ResolvedResourceId, to: ResolvedResourceId }` | Unchanged |
| `Wait` | `Wait { binding: String, target_id: ResolvedResourceId, ... }` | `Wait { identity: ResourceIdentity, target_id: ResolvedResourceId, ... }` |
| `DeferredCreate` | `DeferredCreate { id: ResolvedResourceId, ... }` | Unchanged |
| `DeferredReplace` | `DeferredReplace { deletes: NonEmptyDeletes, id: ResolvedResourceId, ... }` | Unchanged |

`Update.id` and `Replace.id` disappear because the same identity is now
guaranteed inside `to.id`. Helpers that expose an effect id derive it from the
payload:

- `Effect::resource_id()` returns `&resource.id` for `Read`, `Create`,
  `Update`, and `Replace`.
- `Effect::as_resource_ref()` continues to return a resource/data-source view,
  borrowing through the resolved wrappers.
- Display, cleanup, dependency, and scheduling helpers keep exhaustive matches;
  only field access changes.

## 4. BasicEffect

`BasicEffect` must reflect the new post-resolver payloads:

```rust
pub enum BasicEffect<'a> {
    Create {
        effect: &'a Effect,
        resource: &'a ResolvedResource,
    },
    Update {
        effect: &'a Effect,
        from: &'a State,
        to: &'a ResolvedResource,
        changed_attributes: &'a [String],
    },
    Delete {
        effect: &'a Effect,
        id: &'a ResourceId,
        identifier: &'a str,
        directives: &'a Directives,
    },
}
```

`BasicEffect::Update` drops its separate `id` borrow. The basic executor should
read `&to.id` when it needs the id; the identity guarantee comes from the
`ResolvedResource` wrapper, not from a duplicated field on `Effect::Update`.

## 5. CascadingUpdate

`CascadingUpdate` carries a dependent resource that will be updated during
create-before-destroy replacement. Its `id` field is the same resource identity
as `to.id`, so it follows the same rule as `Effect::Update`.

| Before | After |
|---|---|
| `CascadingUpdate { id: ResolvedResourceId, from: Box<State>, to: Resource }` | `CascadingUpdate { from: Box<State>, to: ResolvedResource }` |

Consumers derive the affected id from `to.id`.

## 6. Wait identity

`Effect::Wait` currently stores the wait's own binding as a raw `String`.
Replace it with `ResourceIdentity`:

```rust
Wait {
    identity: ResourceIdentity,
    target_id: ResolvedResourceId,
    until: WaitPredicate,
    until_surface: String,
    timeout: Duration,
    interval: Duration,
    explicit_dependencies: HashSet<String>,
}
```

`identity` is the wait's own identity from `let <identity> = wait <target> { ... }`.
It is not the target resource identity; `target_id` continues to hold that.

Helpers update mechanically:

- `binding_name()` returns `Some(identity.to_string())`.
- `blocking_bindings()` still starts with `target_id.identity_str()` and then
  adds explicit wait dependencies.
- Scheduler and display code convert `ResourceIdentity` to string only at the
  boundary where existing maps or output text require strings.

## 7. Serde and saved-plan compatibility

`ResolvedResource` and `ResolvedDataSource` are transparent at the JSON shape
level: plan JSON stores the same resource/data-source object as before, but
deserialization now rejects missing identity.

`ResourceIdentity` already serializes as a non-empty string, so
`Wait.identity` is still a JSON string. The field name changes from `binding`
to `identity`.

Saved-plan format changes are intentionally minimal:

- `Update.id` disappears.
- `Replace.id` disappears.
- `CascadingUpdate.id` disappears.
- `Read.resource` and `Create` keep their existing inner JSON shape.
- `Wait.binding` is renamed to `Wait.identity`.

Saved plans are internal plan files, not a stable public API. No compatibility
shim is required for the removed or renamed fields unless a test fixture needs
temporary transition support.

## 8. Consumer changes

Implementation should update consumers at the boundary where effects are built
or inspected:

- Differ: wrap resolved `Resource` and `DataSource` values before constructing
  effects; stop passing duplicate ids into `Update`, `Replace`, and
  `CascadingUpdate`; construct wait identity as `ResourceIdentity`.
- Executor: match the new shapes; pass `&Resource`/`&DataSource` to provider
  code through `Deref` or `as_inner()`; read update ids from `to.id`.
- Display: derive ids and labels from resolved payloads; render wait identity
  from `ResourceIdentity`.
- Scheduler/dependency analysis: keep existing ordering semantics, but consume
  the new wait identity field and resolved payload wrappers.
- Tests: update effect fixtures, serde snapshots, plan snapshots, compile-fail
  examples, and add invariant tests for constructing/deserializing wrappers
  without identity.

## 9. Out of scope

This spec does not include:

- Parser, resolver, or module-expander type changes. Those layers still work
  with `Resource` and `DataSource`.
- Any #3625 work.
- State-file format changes beyond internal saved-plan files.
- Provider WIT changes.
- A broader scheduler key redesign; variants that already carry
  `ResolvedResourceId` remain unchanged.

## 10. Task decomposition

Suggested implementation order:

1. Add `ResolvedResource` and `ResolvedDataSource`, including serde and invariant
   tests.
2. Change `Effect`, `CascadingUpdate`, and `BasicEffect` shapes, then update
   core helpers such as `resource_id()`, `as_basic()`, `as_resource_ref()`, and
   `binding_name()`.
3. Migrate differ/executor/display/scheduler call sites and update tests and
   snapshots.
4. Clean up transitional helper code and old fixture JSON that still contains
   redundant `id` fields.
