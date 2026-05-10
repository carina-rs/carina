# Selective Refresh Design

## Summary

Introduce a `--selective-refresh` mode that skips `provider.read()` for resources whose DSL definition has not changed since the last apply. This reduces API calls during `plan` and speeds up the feedback loop for large configurations.

## Motivation

Currently, `carina plan` refreshes every resource's state via the cloud provider API, even when most resource definitions haven't changed. In large configurations with many resources, this is slow. By comparing a hash of each resource's definition against the previously recorded hash in state, we can skip refresh for unchanged resources.

This optimization is opt-in and assumes the infrastructure is not modified outside of Carina (no drift). Users who want full drift detection continue using the default `--refresh` mode.

## Design

### Definition Hash

For each resource, compute a SHA-256 hash from:

- `provider`, `resource_type`, `name`
- All user-specified attributes (keys sorted lexicographically, values normalized)
- Lifecycle configuration
- Definition hashes of all dependency resources (resources referenced via `ResourceRef`)

**Excluded from hash:**
- Comments, whitespace, formatting
- State-derived values (identifier, read-only attributes returned by the provider)

The hash is computed in topological order (using the existing `sort_resources_by_dependencies`), so dependency hashes are always available when computing a resource's hash.

### State Changes

Add a `definition_hash: String` field to `ResourceState`:

```rust
pub struct ResourceState {
    // ... existing fields ...

    /// Hash of the resource's DSL definition (including dependency hashes).
    /// Used for selective refresh to skip unchanged resources.
    pub definition_hash: String,
}
```

- State version bumped to v5.
- Migration sets `definition_hash` to empty string for existing resources (forces refresh on first run).
- The hash is written to state on successful apply.

### CLI Interface

Three refresh modes:

| Flag | Behavior |
|------|----------|
| `--refresh` (default) | Refresh all resources via provider API |
| `--selective-refresh` | Refresh only resources whose definition hash changed |
| `--refresh=false` | Use cached state for all resources, no API calls |

### Selective Refresh Flow

Within `create_plan_from_parsed`, when `--selective-refresh` is active:

1. Compute definition hashes for all resources in topological order.
2. For each resource, compare computed hash with `definition_hash` in state.
3. **Hash matches** -> skip refresh, use cached state from state file.
4. **Hash differs or missing** -> call `provider.read()` to refresh.

### Dependency Propagation

Definition hashes include dependency hashes, so changes propagate automatically:

```
VPC (hash: aaa) <- Subnet (hash includes VPC's hash)
```

If VPC's definition changes, VPC's hash changes, which changes Subnet's hash, triggering refresh for both.

Resources without `ResourceRef` relationships are independent. Changing an S3 Bucket does not trigger refresh of an unrelated VPC.

### Edge Cases

| Case | Behavior |
|------|----------|
| New resource (not in state) | No `definition_hash` in state -> always refresh |
| Orphan resource (in state, removed from .crn) | No definition to hash -> always refresh |
| Module resources | Module argument changes propagate through resource definitions, changing hashes naturally |
| `state {}` blocks (import/removed/moved) | Target resources always refresh, bypass hash comparison |

## Non-Goals

- Physical state file splitting (not needed for this optimization)
- Automatic drift detection (users opt into selective refresh knowing drift won't be caught)
- TTL-based or git-based change detection (definition hash is self-contained)
