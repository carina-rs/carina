# Typed module handle outputs and resolved dependency addresses

Tracking: carina-rs/carina#3803

Base: `origin/main` at `7f37f563`

This design makes resource handles a local, typed capability across module
boundaries. It also fixes the related wait-ordering bug in the same change by
removing string dependency operands after expansion.

## Problem

Carina already distinguishes a bare resource binding from the resource's
attribute values while parsing, but that distinction is lost at two module
boundaries.

First, `attributes {}` can store only `Value`. A module therefore cannot expose
a resource handle even though a module argument can receive one. Second,
dependency operands become binding-name strings. Expansion can rewrite those
strings incorrectly without the type system noticing.

The practical P3 shape is:

1. a producer module creates a Route 53 validation record;
2. the producer exposes that record as a module attribute;
3. the root passes `producer.validation_record` to another module;
4. the consumer uses the received handle in `wait ... depends_on`;
5. the wait must start only after the producer's real resource effect.

Today this shape cannot preserve the handle end to end.

## Current model

- `EvalValue::StructuralHandle` carries `StructuralReference`; its canonical identity is a binding `String`
  (`carina-core/src/eval_value.rs:27`, `:150`, `:187`). `EvalValue::into_value` correctly rejects a bare handle
  (`:526`).
- Module calls use direct-handle mode and lower handles to structural arguments
  (`carina-core/src/parser/blocks/module_call.rs:47`).
- `AttributeParameter.value` is only `Option<Value>` (`carina-core/src/parser/ast.rs:872`), so
  `attributes { r = record }` wrongly reaches the value-boundary rejection.
- Composition outputs are only `Forwarded(AccessPath) | Derived(Value)`
  (`carina-core/src/resource/composition.rs:55`); expansion at
  `carina-core/src/module_resolver/expander.rs:551` has no handle branch.
- Resource directive arguments are substituted near `expander.rs:853`, while waits near `:951` blindly prefix
  dependencies; resource identity/binding prefixing is separate near `:1424`.
- The wait grammar is identifier-only (`carina-core/src/parser/carina.pest:34`), and `WaitBinding.depends_on` is
  `Vec<BindingName>` (`carina-core/src/parser/ast.rs:2523`); neither distinguishes authored/prefixed/resolved IDs.
- Directives, implicit `dependency_bindings`, and wait effects still use strings
  (`carina-core/src/resource/mod.rs:5996`, `carina-core/src/differ/plan.rs:743`). The graph reconstructs
  `ResourceIdentity` from text and silently drops a missing lookup
  (`carina-core/src/effect/deps.rs:379`, `:480`, `:506`).
- `TypeExpr::Ref` accepts strings/enums (`carina-core/src/validation/typecheck.rs:439`), allowing an attribute
  value where a handle was declared.
- `BindingIndex` already lifts bare resources as handles; see
  `ref_type_lifts_bare_resource_binding_as_a_handle` (`carina-core/src/binding_index.rs:3081`). A neighboring test
  currently asserts handle/String compatibility (`:3034`).
- Exports retain declared/inferred `StaticType`, but attributes retain only an annotation
  (`carina-core/src/static_type.rs:197`, `carina-core/src/module_resolver/signature.rs:11`).
- Compositions are plan-scoped, never state-scoped (`carina-core/src/resource/composition.rs:1142`), but their
  value-only form is saved in plan files (`:1231`).

## Live ordering bug

For a module-internal wait such as:

```crn
arguments {
  x: aws.route53.RecordSet
}

let ready = wait certificate {
  until      = certificate.status == aws.acm.Certificate.Status.issued
  depends_on = [x]
}
```

expansion records `"<instance>.x"`. No such resource exists: `x` is a parameter whose caller supplied another,
already qualified resource. Planning succeeds because a missing string lookup means no edge, leaving the wait
silently unordered. The prefixing discrepancy is a symptom; the root cause is text surviving past address
resolution. Fixing only the wait branch would preserve the invalid state elsewhere.

## Decisions

1. A resource-type annotation, represented by `TypeExpr::Ref`, always means a
   resource handle. `vpc: aws.ec2.Vpc = main_vpc.vpc_id` is an error because
   the RHS is a value. Identity values use identity types such as
   `aws.ec2.Vpc.Id`.
2. There is no backward-compatibility mode for the former string acceptance.
   In-repository fixtures, tests, and documentation migrate in the same change.
3. Module `attributes` may expose a module-local resource (`r = record`) or a
   received handle argument (`r = x`). `child.r` is that resource handle,
   qualified for the concrete child instance.
4. `wait ... depends_on` accepts handle expressions, including `[r, child.r]`,
   rather than only identifier-shaped text. Resource
   `directives.depends_on` uses the same operand parser and resolver.
5. An `exports` or upstream-state export whose declared or inferred type is a
   resource handle is a declaration error. Handles do not cross state
   boundaries; export an ID, ARN, or another value instead.
6. An unannotated module attribute carries the RHS type. Both
   `attributes { r = record }` and `attributes { r = x }`, where `x` is a
   handle argument, infer the resource handle type without an annotation.
7. An optional attribute annotation checks the inferred RHS type; it does not
   coerce values into handles or handles into values.

## Rejected alternatives

Module-level `depends_on = [child]` was considered and rejected. It is too
coarse: it waits for every resource in the module, reducing apply parallelism
and creating false cycles. It would also make dependents plan as “(known after
apply)” after any unrelated change inside the module, even when the specific
resource they depend on is unchanged. A module-level form may be added later
only as sugar over resource-level handles.

## Invariants

- Authored binding text exists only in parser/pre-expansion types and diagnostic provenance.
- An expanded handle uses a system-assigned, resolved, instance-qualified address, never provider `name` or any
  user data.
- A bare handle cannot inhabit `Value`.
- Every wait/resource ordering operand reaching planning is a resolved address; `depends_on` has no string route
  to the scheduler.
- Module handles are not provider/state/export values, identity-hash inputs, or saved-composition values.
- `carina validate`, planning, and LSP diagnostics apply the same boundary rules.

## Phase-separated handle types

Keep authored and expanded handles as different types. Rename the current
string-backed structural reference so its phase is visible:

```rust
pub struct UnexpandedResourceHandle {
    reference: UnexpandedStructuralReference,
    resource_type: ResourceTypePath,
    kind: StructuralHandleKind,
}

pub enum UnexpandedStructuralReference {
    LocalBinding(BindingName),
    ModuleArgument(BindingName),
    ModuleAttribute { call: BindingName, attribute: String },
    Alias { alias: BindingName, target: Box<Self> },
}
```

This replaces `StructuralHandle`/`StructuralReference` in parser-facing `EvalValue` and structural operands. It
has no serde or `Value` conversion; it may retain spans for diagnostics.

After expansion, substitution, routing, and anonymous identity assignment, use the existing
`ResolvedResourceId`. It proves identity resolution and carries provider, resource type, provider instance, and
system `ResourceIdentity` (`carina-core/src/resource/mod.rs:615`). `ResourceAddress` can still contain a pending
`ResourceId`; `ResourceIdentity` omits provider/type/routing axes. Thus `ResolvedResourceId` is canonical and is
projected only by typed APIs that explicitly need less.

The stage placement is fixed:

| Types | Stage on current main | Stage after #4094 |
| --- | --- | --- |
| `UnexpandedResourceHandle`, `BoundaryExpression::Handle`, `UnexpandedDependencies` | parser/pre-expansion, beside values that may contain module-argument placeholders | beside `Value<ParserPlaceholder>` |
| `LiveCompositionAttributeValue::Handle`, `ResolvedDependencies`, wait `explicit_dependencies` | post-expansion only, beside fully expanded `Value` | beside the default `Value<NoPlaceholder>` (`Value`) |

No `ResolvedResourceId`-bearing handle/dependency type exists on the parser side, and no unexpanded handle or
dependency type crosses the expansion boundary.

Resolution is one fallible transition:

```rust
fn resolve_handle(
    handle: UnexpandedResourceHandle,
    bindings: &ExpandedBindingIndex,
) -> Result<ResolvedResourceId, HandleResolutionError>;
```

It runs after instance prefixing and final ID assignment. Missing/non-resource targets and type mismatches are
diagnostics here, never absent graph edges later.

## Module attribute representation

Parsing `attributes` and `exports` must preserve whether the RHS is a handle:

```rust
pub enum BoundaryExpression {
    Value(Value),
    Handle(UnexpandedResourceHandle),
}
```

This design targets current main, where `Value` is not generic. When #4094 lands after #3803, it mechanically
parameterizes the parser-side value arm as `Value<ParserPlaceholder>`; it does not change the handle arm or module
attribute inference. `attributes` admits both variants. `exports` recognizes both for a boundary-specific error,
but validated exports contain only `Value`.

Split the current shared composition enum into live and saved forms:

```rust
enum LiveCompositionAttributeValue {
    Forwarded(AccessPath),
    Derived(Value),
    Handle(ResolvedResourceId),
}

enum SavedCompositionAttributeValue {
    Forwarded(AccessPath),
    Derived(Value),
}
```

The live enum has no serde; the saved enum cannot represent a handle. An explicit live-to-saved phase consumes
structural uses and projects only value attributes into `SavedComposition`. This is a typed lowering, not serde
skip/filter logic, and proves consumers became resolved references or dependencies.

In structural context, `child.r` yields the `Handle` entry's `ResolvedResourceId`. `child.r.attr` validates the
attribute against that resource schema and creates an ordinary deferred value reference directly to the resource;
the handle never becomes `Value`. More path segments use normal access rules.

Forwarding is transitive: for `r = x`, the child stores the caller resource ID, not `"child.x"`; a parent exposing
`r = child.r` stores the same ID. No textual alias chain survives.

## Typed dependencies

Both explicit dependency syntaxes parse a list of handle expressions:

```rust
struct UnexpandedDependencies(Vec<UnexpandedResourceHandle>);
struct ResolvedDependencies(BTreeSet<ResolvedResourceId>);
```

The parser accepts `r`, aliases, and `child.r`; it rejects literals, `r.id`, value outputs, providers, wrong-kind
data sources, and other non-handles. Expansion substitutes structurally, then the expanded binding index produces
`ResolvedResourceId` after prefixing/final identity assignment and before the differ.

`Resource<S, D>` and `DataSource<S, D>` carry the dependency phase in `D`. Parser-phase values use
`UnexpandedDependencyFields`; the graph transition consumes those fields and produces resolved wrappers containing
`ResolvedDependencies`. There is no optional parallel field and no post-resolution accessor for authored dependency
spellings. Differ, waits, effects, satisfier hints, graph/sort, and plan image accept only the resolved phase.

Wait lowering therefore constructs:

```rust
Effect::Wait {
    explicit_dependencies: BTreeSet<ResolvedResourceId>,
    // ...
}
```

Directives and implicit snapshots use the same type. Flatten composition/value references to concrete resource IDs
before scheduling; resolve provider hints through the same index. `EffectInstanceKey` carries
`ResolvedResourceId` plus generation. `ScheduleEdge` may explicitly project to typed `ResourceIdentity`, never
reconstruct it from text; delete/deposed paths do likewise.

Remove `lookup_by_string_ref`, string explicit dependencies, and silent missing-target returns from this path. A
missing plan address is an error unless a documented delete/orphan rule proves the edge historical and safe.
Saved deferred loops carry a non-`Value` dependency-address template; child materialization must produce
`ResolvedDependencies` before effects exist and must never re-create a live handle.

## Type checking and inference

Resolve `TypeExpr::Ref(path)` to `ResolvedParameterType::Handle(path)` before generic value checking. Remove
Ref/String and Ref/enum acceptance at `validation/typecheck.rs:439`; generic value checking never decides handles.

For a module argument:

- handle actual + matching Ref formal succeeds;
- handle actual + value formal fails;
- value actual + Ref formal fails, even with a related scalar identity;
- handle actual + different Ref formal reports both resource types.

Add each module attribute's effective static type with declared/inferred provenance. A local resource, handle
argument, or handle-valued child attribute produces:

```rust
StaticType::Known(ConsumerResolvedType::new(TypeExpr::Ref(resource_type)))
```

Annotations check that type; values retain current inference/unchecked reasons, but known handles never degrade to
unknown. `BindingIndex` treats a bare Ref-typed module output as a handle and later segments as resource attributes.
Keep the bare-resource lifting test, add handle-argument/`child.r` cases, and delete handle/String compatibility.

Run export inference before boundary validation and reject declared or inferred Ref. Upstream loading stays
value-only.

## Diagnostics

Use the same messages in validate, plan, and LSP, at the declaration/argument span:

- `module argument 'vpc': expected resource handle aws.ec2.Vpc; got String`
- `attribute 'r': expected resource handle aws.route53.RecordSet; got resource handle aws.ec2.Vpc`
- `wait.depends_on: expected a managed resource handle; got aws.route53.RecordSet.Id`
- `export 'vpc': resource handle aws.ec2.Vpc cannot cross a state boundary; export an identity value such as aws.ec2.Vpc.Id instead`
- `export 'vpc': inferred resource handle aws.ec2.Vpc cannot cross a state boundary; export an identity value such as aws.ec2.Vpc.Id instead`

Print a named identity type instead of `String` when known; never suggest an annotation workaround.

`InferredExportParam` retains the parser's declaration source. Export boundary and reference validation return
typed `ExportBoundaryError` values carrying that provenance. LSP filters by source file, anchors the diagnostic at
the parser span start with the export-name width, and reports these boundary violations as errors; it never scans
source text to reconstruct a position.

## Persistence and identity boundaries

The structural separation is:

- `UnexpandedResourceHandle` has no serde and cannot enter `Value`.
- `LiveCompositionAttributeValue` has no serde or conversion to provider JSON/WIT, hashing, state, or saved output.
- Existing `EvalValue::into_value` rejection remains the value-boundary guard.
- Provider payload builders continue to accept only fully resolved `Value`.
- Identity hashing accepts resource/provider values, never handles or dependency collections.
- State may store typed dependency metadata for destroy order, but no handle value/output/export.
- Saved plans may store resolved scheduler/reference IDs, but no structural handle or Handle output.

Persist current/deposed dependency metadata as `PersistedDestroyDependencies`, an opaque set of
`ResolvedResourceId`, not binding strings. State writers can receive this type only from the expanded-graph
projection described below. This changes the state wire shape to version 21 and updates state writeback, refresh,
import, moved, orphan-destroy, and migration paths.

Saved-plan shape also changes. Keep its version number in the single `PlanFile::CURRENT_VERSION` constant; tests
and encoders reference that constant instead of repeating the integer. Storing the migration classification bumps
that constant once, while the state format remains v21. Old saved plans remain unsupported.

## Legacy dependency migration

The dependency snapshot written by main before this change was kind-blind. It recorded binding names from managed
resources, `upstream_state`, data sources, compositions, waits, and historically unprefixed module arguments in
the same string collection. Those names are not row edges and cannot be migrated correctly by parsing each string
as a resource address.

The v21 writer therefore accepts only `PersistedDestroyDependencies`. The expanded typed graph constructs it by
projecting every dependency source to managed-resource rows. Managed resources are leaves. Data sources project
the managed resources used by their inputs; compositions project their underlying targets; waits project their
target and explicit dependencies. The projection is transitive, so a downstream distribution that reads a wait
on a certificate retains the certificate edge and never persists the synthetic `__wait` ID. Loop collection
bindings project every materialized managed member. An exhaustive typed result records that upstream/external
values and literal arguments have no local destroy edge. Passing `value_dependencies().clone()` directly to a
state writer does not type-check.

Loading pre-v21 state follows the existing keyed chain through v19 → v20 and then stops in an unwritable in-memory
typestate. The v20 → v21 transition requires either the live expanded graph or the complete
`LegacyDependencyClassification` saved in a plan. It carries every v20 field through, including token-less
create-outcome-unknown rows. Source-less commands such as `state list`, `state show`, and `state lookup` may inspect
the pending typestate, display
`dependencies not yet migrated (legacy: ...)`, and never write it.

Configuration-aware commands classify each legacy name against state rows first and then the current expanded
graph. Graph construction consumes `ConfiguredGraphInput`, which owns the expanded configuration file once;
classification and live plan/apply, destroy, refresh, and offline checks retain that same graph. Its target maps
contain configuration resources only. Orphan rows participate only as state-row candidates during classification,
so a configured resource may reuse an orphan's binding after changing resource type. Configuration-only membership
is captured in `ConfiguredResources`; it determines strictness. Every row uses the same scope rule, including
configured, orphan, and deposed rows: when the resolved move pairs map it to a destination (`classified_id`), derive
its enclosing instance scopes from that destination identity. Otherwise use its persisted binding address (or
identity when no binding was recorded). Resolution probes those scopes from inside out before root, preserving
quoted map-key segments; at each spelling, state rows take precedence over the current graph. Removing a module
retains its historical scope. An explicit move adopts the destination scope, including root for a move out of a
module, even when migration is persisted before the move executes. For example, moving `vpc` to `net.vpc` makes
its legacy dependency `igw` resolve to `net.igw` before root `igw`.

V20 merged dependency strings from references, dependency snapshots, and directives without storing a per-edge
scope. Old releases stored both module-local and prefixed spellings, so the strings alone cannot establish a
source-versus-destination rule for a moved row. This design chooses the destination because classification already
treats the row as `classified_id`; its unqualified names use that same module instance. Source row IDs remain the
classification keys and fingerprints until the move executes. A configured row fails with a typed
`LegacyDependencyMigrationError` when a name is missing or ambiguous. A saved plan persists the complete
classification, including its source fingerprints and warnings, so `apply --plan` validates it against the loaded
state and cannot fall back to the old kind-blind strings. Move scope is an opaque `ResolvedMovePairs` value with no
`Default` or empty constructor, built only from an `AddressedMoves` resolver witness or the plan's complete
`SavedMoveEffects` witness. Address-preserving refresh also obtains its empty scope from the resolver; migration APIs
cannot be called with an untyped empty slice.

An orphan or deposed row is different: deletion must remain possible after its former module or dependency has
left the configuration. Classification retains every known managed edge and returns a typed
`LegacyDestroyOrderingWarning` for each unresolved name. Classification before terminal wait validation yields
`UnvalidatedLegacyMigration`, which owns the graph, classification, and still-unwritable legacy state. Its only
transition reports every current and deposed warning through one CLI formatter on stderr, then strictly validates
that same graph before releasing the writable v21 state or saved classification. A failed wait therefore prints
the warnings and leaves v20 state unchanged, byte for byte. Saved plans contain only classifications that crossed
strict validation; replay owns that saved managed-row projection through the same pending migration type,
reports its warnings, and validates completeness and source fingerprints before any migration write. Raw
classification, migration-input construction, and state writeback are private to the state crate. Scheduling uses
the known edges. Migration resolution errors retain their typed graph or persisted-address causes.
The warning is operation-scoped rather than state
data: v21 persists the known managed IDs only, never the unresolved name. Thus an unresolved name can leave memory
only after it has been surfaced, while the v21 format remains free of dependency strings.

Persisted destroy-dependency address mapping runs the same managed-row validation as decoding; neither mapping
nor decoding can introduce provider-less or synthetic wait addresses.

`carina state migrate --check DIR --state-url COPY` exercises the same transition without credentials, provider
initialization, CRUD/data-source calls, upstream-state reads, backend access, locks, network access, cache writes,
or state writes. It loads only provider metadata from exact installed artifacts; repeatable
`--provider-schema KIND=/absolute/provider.wasm` overrides allow checking copied states against explicitly selected
artifacts. The schema-only host admits the released protocol-v12 metadata function signatures and upgrades their
older schema JSON in memory, while the ordinary runtime loader continues to require the current protocol. The
command reports configured-row errors and orphan/deposed warnings and explicitly states that no state was written.
Destroy with refresh and state refresh materialize deferred loops through the same read/expansion builder before
using the migration-input builder shared with plan. A loop-owned row is therefore configured and strict on every
read-capable command. Destroy with `--refresh=false` uses the offline input. If an unmaterialized deferred loop
could own a state row, offline classification returns the typed `RequiresReads` outcome instead of guessing;
destroy tells the operator to rerun with `--refresh=true` and leaves pending legacy state unchanged.

## Consumers to update

- Parser/grammar: preserve handle RHSs in `attributes`/`exports`; parse `depends_on` as structural expressions with spans.
- `eval_value`: introduce unexpanded handle/reference types, retain value rejection, and support module-output access.
- Composition IR: add live `Handle`, split saved form, add typed consumption, and update visitors exhaustively.
- Module resolver: update loader/signature/resolver/expander/typecheck, substitution, identity, and access resolution.
- `BindingIndex`: index inferred handle outputs and resolve `child.r.attr` through the target schema.
- Validation/typecheck: make Ref handle-only, reject handle exports, check dependency kind, and share LSP diagnostics.
- Dependency graph: type directives, implicit snapshots, waits, effects, state/recovery, hints, sorting, and plan image.
- Plan display: match Handle explicitly; render its resource address/type, never a quoted/provider value, and use real IDs for edges.
- LSP: complete attributes after `child.r.`, preserve the bare handle, accept it in dependencies, and match CLI diagnostics.
- Docs: update `site/src/content/reference/dsl/{modules,types-and-values,expressions,directives}.md` and
  `site/src/content/guides/{using-modules,state-management}.md` for local handles and value-only exports/upstream state.

## Repository migration

The current resource-typed value annotations found by grep are:

- `carina-cli/tests/fixtures/module_info/main.crn:16` — `web_sg.id`
- `carina-cli/tests/fixtures/module_info/main.crn:17` — `web_instance.id`
- `carina-cli/tests/fixtures/module_info/no_args/main.crn:2` — `my_bucket.arn`
- `carina-cli/tests/fixtures/module_info/no_deps/main.crn:8` — `logs_bucket.id`
- `carina-cli/tests/fixtures/module_info/no_deps/main.crn:9` — `event_queue.id`

Replace each with the RHS identity type or omit the annotation for inference. The ARN is
`awscc.s3.Bucket.Arn`; EC2 IDs use registered `aws.ec2.*.Id` identities. Regenerate snapshots, repeat the grep,
and fix every test/doc match; there is no allowlist.

## Test plan

Write failing fixtures first. Every module scenario is a multi-file directory; an inline single-file test cannot
replace an end-to-end criterion.

1. P3: producer creates a validation record in one file and exposes unannotated
   `validation_record = validation_record` in another. Root forwards `producer.validation_record`; consumer declares
   the Ref argument in one file and uses `depends_on = [validation_record]` in another.
2. Assert the wait depends on the producer's instance-qualified `ResolvedResourceId`, never
   `<consumer>.validation_record`; it orders after the producer and fails if that effect is absent.
3. Separately test unannotated `r = record` and `r = x`: both signatures are
   `StaticType::Known(TypeExpr::Ref(...))` and can be forwarded.
4. Test annotated success/mismatched handles; the error names both types at the attribute declaration.
5. Pass String and a typed ID to Ref; both say `expected resource handle`. Remove tests accepting them.
6. Reject annotated and inferred handle exports through validate, module validation, lint, plan, and upstream load.
7. Regress the module-internal wait/handle-argument bug; no prefixed parameter spelling exists in the plan.
8. Test valid/unknown/mismatched `child.r.attr`, plus bare `child.r` rejected in provider value position.
9. Test `directives.depends_on = [r, child.r]`, wrong-kind targets, and value operands through the same resolver.
10. Save/reload P3: resolved scheduler IDs round-trip; saved compositions contain no handle output.
11. Mock-apply and inspect provider requests, state JSON, and plan JSON: handles occur in no payload/value/export;
    addresses occur only in graph metadata; handle outputs do not alter anonymous identity hashes.
12. Test state migration/destroy order for legacy, module, loop, deposed, and missing targets. Missing configured
    targets fail explicitly; unresolved orphan/deposed names retain known edges and appear in plan/apply/destroy warnings.
13. Match CLI/LSP directory diagnostics and ranges; complete resource attributes after `child.r.` for both forms.
14. Migrate fixtures/snapshots, Ref/String unit tests, and site examples; finish with the repository grep.

Run focused tests first:

```text
cargo nextest run -p carina-core
cargo nextest run -p carina-state
cargo nextest run -p carina-lsp
cargo nextest run -p carina-cli
```

Finish with timeout-wrapped `cargo nextest run --workspace` and `cargo test --workspace --doc` doctests.
Acceptance/cloud-mutating tests are not needed.

## Coordination with #4094

The #4094 owner confirmed on 2026-10-30 that [its design](./2026-10-30-4094-deferred-value-stages-design.md)
(design PR #4141) is a mechanical `Value<P = NoPlaceholder>` / `DeferredValue<P>` rewrite. It moves
`FnParam`/`FnLocal`/`ModuleArgument`/`EmptyInterpolation` to `ParserPlaceholder`, for-loop slots to `LoopSlot`, and
`UpstreamExport`/`UpstreamAllExports` plus `PendingReRead` to `ApplyTimeUnknown`. It does not change `DeferredValue::BindingRef`'s
contents, `StructuralHandle`, or module-attribute `StaticType` inference.

The agreed order is #3803 first on current main, then #4094 rebased on top. That rebase mechanically parameterizes
parser/pre-expansion value fields, including `BoundaryExpression::Value`, with `ParserPlaceholder`; post-expansion
handle/dependency types remain beside the default `Value<NoPlaceholder>`. #4094 must preserve #3803's structural
handle/address split rather than redesign it.

## Delivery

Land this document alone, then one atomic implementation PR with commits for red tests, phase types/parser,
resolver/graph, and persistence/UI/docs. String adapters or value-compatible Ref recreate the invalid state. There
is no follow-up phase for wait, directives, exports, LSP, or docs.

## Open risks

- Anonymous and deferred-loop resources receive final identities later than
  named resources. The handle-resolution boundary must be after that assignment
  without postponing dependency errors to scheduling.
- Saved deferred-loop templates may instantiate resources during apply. Their
  compiled template must carry typed address templates/dependency provenance,
  not a serializable live handle; saved-plan round-trip tests must prove this.
- Legacy state dependency strings can name bindings affected by `moved`, loop,
  or module expansion. Migration must use existing address/origin metadata and
  reject ambiguity instead of guessing from display text.
- Flattening a composition dependency to concrete resource IDs must preserve
  transitive edges and cycle diagnostics without retaining a textual alias.
- Full `ResolvedResourceId` scheduler keys may expose assumptions that identity
  strings are globally unique. That is desirable, but delete/deposed and
  provider-instance tests must cover the widened key.
- #4094 can create mechanical conflicts in value visitors and serde matches.
  Resolve them at the stated structural/value boundary rather than restoring a
  handle-shaped `DeferredValue`.
- Plan format reservations may advance before implementation. Select the next
  free version from current main and update header/decode tests together.
- Diagnostic spans cross files in merged module directories. CLI and LSP tests
  must prove the error remains attached to the authored declaration or operand.

Completion means the P3 fixture produces a real ordering edge, both unannotated
handle-output forms infer as known Ref types, Ref never accepts a value, exports
cannot carry handles, and no post-expansion dependency operand is a string.
