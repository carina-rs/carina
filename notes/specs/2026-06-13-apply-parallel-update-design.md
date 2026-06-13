# Parallel in-place update scheduling: Design

<!-- relates-to carina#3486 -->
<!-- constrained-by ../../carina-core/src/executor/parallel.rs -->
<!-- constrained-by ../../carina-core/src/effect.rs -->
<!-- constrained-by ../../carina-core/src/executor/basic.rs -->
<!-- constrained-by ../../carina-core/src/provider.rs -->

## Status

Draft design PR for carina#3486.

This document records the agreed design for making independent
in-place updates eligible for parallel execution. It deliberately does
not implement the scheduler, dependency-analysis, or CLI changes.
Implementation follows after the prerequisite comparison-logic fix
described in "Prerequisites".

## Problem

Today an apply with many independent `Update` effects can still execute
them serially because the dependency graph preserves every static
resource reference edge, even when the child effect does not read any
attribute the parent update will write. The carina#3486 case is a
large set of independent in-place tag changes: the resource graph is
structurally connected, but most updates do not need to wait for each
other's changed attributes.

Dropping edges blindly would be incorrect. If a child update resolves
its desired value from a parent attribute that the parent update is
about to change, running both effects from the same old binding
snapshot can build the child's provider patch from stale data. The
design therefore needs a precise enough safety predicate, a
conservative fallback when reads are not knowable, and a bounded
runtime parallelism model.

## Current execution model

The parallel executor builds a dependency map and schedules only
effects whose dependencies are complete. The scheduler itself only
needs `deps_of`: it checks ready effects, sorts newly-ready indices for
determinism, and dispatches work from the ready queue
(`carina-core/src/executor/parallel.rs:236-269`).

Each spawned effect receives a snapshot of bindings cloned before the
future is created (`binding_snapshot = input.bindings.clone()` in
`carina-core/src/executor/parallel.rs:296`). That means two effects
that run concurrently do not observe each other's post-update state
during resolution. The design must make that existing snapshot model
safe for any newly parallelized pair.

`Effect::Update` already records the top-level attributes whose values
changed in `changed_attributes: Vec<String>`
(`carina-core/src/effect.rs:150-156`). The plan-time diff computes that
set through `find_changed_attributes`
(`carina-core/src/differ/comparison.rs:509`), while apply-time
augmentation currently uses a raw comparison in
`carina-core/src/executor/basic.rs:578-597`.

## Safety invariant

An `Update -> Update` dependency edge may be removed only when the
parent update's write set and the child update's read set are disjoint:

```text
parent.writes ∩ child.reads = ∅
```

If the child reads no attribute the parent will change, then resolving
the child from the old `binding_snapshot` is equivalent for those reads:
the values it consumes are unchanged by the parent update. This relies
on the provider contract that `Provider::update` changes only the
requested patch and not unrelated attributes
(`carina-core/src/provider.rs:559-567`).

The root cause statement is therefore narrow: the static graph
currently treats every parent reference as a hard ordering edge, even
when the only attributes read by the child are outside the parent's
in-place write set. The fix is to relax only those edges whose
attribute-level read/write relation proves the ordering unnecessary.

## Design

### Edge relaxation rule

Use option C from the design discussion. For an edge where the parent
effect is `Update` and the child effect is `Update`, compute:

- `writes`: the parent's top-level changed attributes.
- `reads`: the attributes of the parent read by the child along the
  paths that produced this child-to-parent dependency edge.

If `reads` is known and `writes ∩ reads = ∅`, remove the edge from the
dependency map. The two effects then become parallel execution
candidates, still subject to the global concurrency cap.

If the read set is unknown, or if any changed attribute is read, keep
the edge exactly as today.

### Writes set source and prerequisite

`writes` comes directly from
`Effect::Update.changed_attributes: Vec<String>`
(`carina-core/src/effect.rs:150-156`). The set uses top-level
attribute names such as `tags` or `cidr_block`. No new field is added
to `Effect`.

This is safe only if the attributes used to decide the plan's
`changed_attributes` are the same attributes that apply will patch.
That is not guaranteed today. `find_changed_attributes`
(`carina-core/src/differ/comparison.rs:509`) uses type-aware
comparison, projection, and saved-attribute merge behavior; the
apply-time `effective_changed` augmentation in
`carina-core/src/executor/basic.rs:578-597` uses raw comparison. A raw
difference such as `Int(1)` versus `Float(1.0)` can therefore be absent
from plan-time `changed_attributes` but still enter the provider patch
at apply time.

PR1 must fix that first by moving the augmentation path onto a shared
`should_patch_attr`-style helper based on the same type-aware equality
as `find_changed_attributes`, including the same secret-unwrapping
semantics. This design PR assumes that bug fix has merged.

### Reads set representation

Reads are collected per child-to-parent edge. Each path is classified
conservatively:

- `Value::visit_resource_refs` with `AccessPath::attribute()` produces
  `Known(attribute)`.
- A bare binding through `Value::visit_binding_refs` /
  `Value::BindingRef` produces `Unknown`.
- `dependency_bindings` from state produce `Unknown`.
- Explicit `depends_on` from `directives.depends_on` produces
  `Unknown`.
- Composition expansion through `compositions_by_binding` produces
  `Unknown`.

Known read paths are top-level attribute names. Unknown does not mean
"reads everything" as data; it means the analysis cannot prove which
attribute is read, so the dependency edge must not be relaxed.

If multiple paths contribute to the same child-to-parent edge, they are
joined. `Known(a) ∪ Known(b)` stays known with the union of attributes.
`Known(_) ∪ Unknown` becomes `Unknown`.

### Scope of relaxation

Only parent `Update` plus child `Update` edges are candidates for
relaxation. Edges involving `Create`, `Replace`, `Delete`, `Read`,
`Wait`, `Import`, `Remove`, or `Move` keep the static graph behavior.

This intentionally excludes broader "batch all updates" behavior.
Create/replace/delete semantics can change identity, presence, and
state shape in ways the top-level write/read predicate does not model.

### State resolution

The executor keeps the existing binding snapshot behavior from
`carina-core/src/executor/parallel.rs:296`: bindings are cloned when an
effect future is spawned, and the future resolves resource values from
that snapshot.

When the C predicate holds, a child update can safely resolve from the
old snapshot because every parent attribute it reads is outside the
parent's write set. Patch-external attributes are assumed stable under
the `Provider::update` contract (`carina-core/src/provider.rs:559-567`).

### Concurrency cap

Add `--parallelism N` to both `carina apply` and `carina destroy`.
Default: `8`.

The cap means "maximum concurrent provider operations" for both
commands. It is implemented at ready-queue dispatch time: the scheduler
dispatches at most `N` ready effects at once. It is not a semaphore
inside already-spawned tasks, because spawning unbounded futures and
having them wait internally would hide queued provider work from the
scheduler.

`--parallelism 1` means fully serial execution in ready order. Newly
ready effects keep the existing deterministic order by sorting before
enqueueing. Environment variables, backend configuration, and provider
hints for this cap are future work and are not part of carina#3486.

The cap applies to `destroy` too, but edge relaxation is apply-only and
only for `Update -> Update` edges.

## Type representation

Use newtypes so writes and reads cannot be swapped accidentally, and so
unknown reads stay distinct from an empty known set.

```rust
pub struct WritesSet(BTreeSet<String>);

pub enum ReadsSet {
    Known(BTreeSet<String>),
    Unknown,
}

impl ReadsSet {
    pub fn disjoint(&self, writes: &WritesSet) -> bool {
        match self {
            ReadsSet::Known(set) => set.is_disjoint(&writes.0),
            ReadsSet::Unknown => false,
        }
    }
}
```

`ReadsSet::Known(BTreeSet::new())` means the edge is known not to read
any top-level attribute from that parent through the recorded paths.
`ReadsSet::Unknown` means the analysis lacks proof and must keep the
edge. They must not collapse into the same state.

## Code structure changes

Split dependency analysis from dependency scheduling:

```rust
struct DependencyAnalysis {
    deps_of: HashMap<usize, HashSet<usize>>,
    reads_by_edge: HashMap<usize, HashMap<usize, ReadsSet>>, // child -> parent -> reads
}

fn build_dependency_analysis(...) -> DependencyAnalysis;

fn build_dependency_map(...) -> HashMap<usize, HashSet<usize>> {
    build_dependency_analysis(...).deps_of
}

fn relax_update_update_edges(effects: &[Effect], analysis: &mut DependencyAnalysis);
```

`build_dependency_analysis` records both the existing dependency map and
the per-edge read information. `relax_update_update_edges` mutates only
`analysis.deps_of`, using `reads_by_edge` and each parent update's
`WritesSet`. The scheduler receives the relaxed `deps_of` and remains
ignorant of read/write analysis details.

This keeps the scheduling surface small: `carina-core/src/executor/parallel.rs:236-269`
continues to reason only about dependency completion and ready queues.

## Rejected alternatives

### A: Parallelize all `~` effects when there are no refs

This condition is too strong and too brittle. One `Replace` or `Create`
mixed into the plan would force the whole plan back to serial behavior,
and the rule would be hard to maintain as effect kinds evolve.

### D: Remove only identity-reference edges

Identity references are a special case of option C: an identity read is
disjoint from a non-identity write. Keeping a separate predicate adds a
second correctness rule without buying extra safety. It also depends on
provider identity stability in the same way C depends on the
`Provider::update` patch contract.

### E: Optimistic concurrency control

Starting all updates in parallel, detecting conflicts, and retrying
would make apply recovery and state management significantly more
complex. It is disproportionate for the carina#3486 scenario, where a
static read/write predicate is enough.

### F: Schema-level reads/writes declarations

Provider-generated read/write declarations would be the most precise
long-term model, but they require substantial provider-side changes and
updates across provider repositories. Keep this as a future refinement,
not the first implementation.

### G: Batch all updates, keep other effects static

This is too coarse to be correct. A child update that reads a parent
attribute the parent writes would be parallelized anyway.

### H: DSL `parallel_safe = true`

This pushes the safety proof onto the user and gives the type system no
help. It is rejected for the same reason runtime-only conventions are
rejected elsewhere in the project.

### D+C hybrid

The hybrid would remove identity-reference edges statically and then
apply C to the rest. Since D is already contained by C, the hybrid adds
another branch with no additional behavior.

## CLI semantics

`carina apply --parallelism N` and `carina destroy --parallelism N`
share the same meaning: at most `N` provider operations may be in
flight at a time.

`N=1` is not a compatibility mode that restores an old implementation;
it is the same scheduler with a dispatch cap of one. Ready ordering
remains deterministic through the existing newly-ready sort.

The default is `8`. That value may need tuning once CloudControl and
SDK provider rate-limit behavior is measured, but config surfaces
beyond the CLI flag are out of scope for carina#3486.

## Test plan

### Unit tests

Add focused tests around `build_dependency_analysis` and
`relax_update_update_edges`:

- Parent writes `{"tags"}`, child reads `Known({"id"})`: edge is
  removed.
- Parent writes `{"tags"}`, child reads `Known({"tags"})`: edge
  remains.
- Parent writes `{"cidr_block"}`, child reads `Unknown` from a bare
  binding: edge remains.
- Parent writes `{"tags"}`, child reads `Known({"id"})` and also has
  `depends_on`: reads escalate to `Unknown`, edge remains.
- Parent `Create`, child `Update`: edge remains.
- Parent `Replace`, child `Update`: edge remains.

### End-to-end tests

Add `carina-cli/tests/` coverage using a mock provider with artificial
per-effect delay, for example 200 ms. A serial plan should take roughly
`N * 200 ms`; a fully parallel plan should take roughly one delay
window, bounded by the selected cap.

Required scenarios:

- The issue-shaped fixture: 12 independent in-place tag changes.
- `--parallelism 4` never allows more than four in-flight provider
  operations.
- `--parallelism 1` executes fully serially.
- Bare binding, explicit `depends_on`, and composition expansion do not
  become accidentally parallel.
- The effective-changed augmentation scenario does not become
  parallel-safe unless PR1's unified comparison says the attribute is
  part of `changed_attributes`.

### Provider contract tests

Mock a provider update that mutates outside `changed_attributes` and
verify the C predicate does not rely on that mutation being invisible
when the child read path has been classified as `Unknown`. This does not
prove real providers obey the contract; it pins the conservative
analysis behavior for paths where the executor cannot prove a known
attribute set.

## Non-goals

- Nested path precision such as separating `tags.Name` from
  `tags.Other`.
- Provider hint traits such as `volatile_on_update`.
- Environment, backend config, or provider-hint configuration for the
  parallelism cap.
- Solving carina#3108, the per-provider `Mutex<Store>` bottleneck. That
  is orthogonal and can be addressed independently.
- Making reads through composition expansion precise. They stay
  `Unknown` in this design.

## Open risks

- A real provider may violate the `Provider::update` contract and change
  patch-external attributes. `carina-provider-aws` and
  `carina-provider-awscc` need a separate audit; a violation could
  create a silent data race under this relaxation rule.
- The default cap of `8` may be too high for CloudControl rate limits.
  The value is a starting point pending measurement.
- Reusing `read_only` to mean "volatile on update" is tempting but
  incorrect. `read_only` means the user cannot set the property; it does
  not mean the provider may arbitrarily change it during any update.
  CloudFormation `readOnlyProperties` has the same meaning. Future
  precision should use a distinct `volatile_on_update` flag or provider
  hint.

## Prerequisites

PR1, in a separate issue, must unify the apply-time augmentation
comparison with `find_changed_attributes`. The raw comparison in
`carina-core/src/executor/basic.rs:578-597` should be replaced with a
shared helper equivalent to `should_patch_attr`, based on the same
type-aware equality and secret-unwrapping behavior used by
`find_changed_attributes` (`carina-core/src/differ/comparison.rs:509`).

This prerequisite is mandatory because the safety predicate uses
`Effect::Update.changed_attributes` as the complete write set. If apply
can patch an attribute not present in that set, `writes ∩ reads = ∅`
can produce a false proof.

## Implementation phases

1. **PR1, separate issue**: fix augmentation comparison by unifying it
   with `find_changed_attributes`. This is a bug fix and a prerequisite
   for carina#3486.
2. **PR2, this design PR**: add this document as
   `notes/specs/2026-06-13-apply-parallel-update-design.md`, referencing
   carina#3486.
3. **PR3, implementation PR**: after PR1 and PR2 merge, implement
   `DependencyAnalysis`, `WritesSet`, `ReadsSet`,
   `relax_update_update_edges`, the ready-queue concurrency cap, CLI
   flags for apply and destroy, and the unit and E2E tests. This PR
   closes carina#3486.
