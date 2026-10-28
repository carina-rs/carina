# Stateless Mode by Provider-Declared Natural Key

Tracking: carina-rs/carina#4043

## Problem

Carina currently joins desired resources to remote objects through persisted
state. `Provider::read` requires a saved, non-empty `AppliedIdentifier`; when
there is no state row, refresh treats the resource as absent instead of asking
the provider to discover it (`carina-core/src/provider.rs:3540-3569`,
`carina-cli/src/wiring/mod.rs:3518-3568`,
`carina-cli/src/wiring/mod.rs:4895-4939`).

That is necessary for resources such as an EC2 VPC, whose useful identifier is
assigned by AWS. It may be unnecessary for a resource addressed by a
provider-enforced unique key, such as GitHub organization plus variable name.

The viable design is not "ignore state and match names." It is a separate
execution model in which the provider schema proves that every managed type
can be found, compared, recovered, and mutated from resolved desired input.

This document recommends a narrow GitHub-first implementation, but not around
the planned types: `OrganizationSecret` is write-only and
`OrganizationVariable` would stay additive under this design. They are the
weakest showcase for stateless planning. A useful slice requires the provider
to grow Repository, IssueLabel, Ruleset, and BranchProtection first.

A provider-wide AWS stateless mode is not worth doing. Some types have durable
keys, but useful stacks include server-assigned IDs and both providers rely on
saved identifiers. Types may opt in later after type-specific API proof.

## Decisions

1. `backend none {}` produces separate stateless types, not a fake backend.
2. Every type needs a provider-declared, schema-extracted natural key.
3. Parent-scoped types declare a key projection that produces typed ordering
   edges; a new repository proves scoped children absent, while branch
   protection is deferred until its branch exists.
4. State-backed Carina turns a previously authored attribute that is later
   dropped into a removal; stateless Carina stops managing it, while an
   explicit empty value clears it (see [Field ownership, refresh, and
   whole-object APIs](#field-ownership-refresh-and-whole-object-apis)).
5. Collections are additive by default. Typed, opt-in authoritative scopes are
   limited initially to labels, rulesets, and branch protection.
6. `absent` explicitly deletes one keyed subresource. Repository deletion is
   gated off; removing a repository block only stops management.
7. Write-only fields, exports, state operations, saved/deposed operations,
   managed output references, and unrecoverable creates are rejected.
8. Shared validation serves CLI and LSP and constructs distinct typestates.

This applies `CLAUDE.md:11-80` (root cause, newtypes, typestate) and
`CLAUDE.md:83-135`: user-supplied attributes never become identity keys for
scheduler, state, dependency, or diff maps.

## Current Boundaries

### Carina identity is not remote identity

`ResourceIdentity` is opaque; `ResourceId` combines it with provider, type, and
resource name, while `ResolvedResourceId` types resolution
(`carina-core/src/resource/mod.rs:203-286`, `carina-core/src/resource/mod.rs:388-570`,
`carina-core/src/resource/mod.rs:628-731`).

`AttributeSchema::identity()` contributes only to anonymous-resource identity;
anonymous hashes combine those fields with create-only inputs
(`carina-core/src/schema/mod.rs:6378-6384`,
`carina-core/src/schema/mod.rs:6548-6551`,
`carina-core/src/identifier/mod.rs:118-171`). It promises no API lookup.

`UniqueNameSpec` is also replacement behavior, not discovery
(`carina-core/src/schema/mod.rs:6686-6695`). The existing axis is documented at
`notes/specs/2026-06-28-carina3632-identity-axis.md:5-69`.

### Providers cannot discover managed objects today

Managed `read` accepts `AppliedIdentifier`; data-source `read` accepts resolved
inputs (`carina-core/src/provider.rs:3540-3614`). Managed discovery additionally
needs not-found, canonical-key, and mutation-identifier results.

WIT and SDK have no find/list/import operation (`carina-plugin-wit/wit/provider.wit:39-73`,
`carina-plugin-sdk/src/lib.rs:699-759`), and `ResourceSchema` has no capability
(`carina-core/src/schema/mod.rs:6698-6773`,
`carina-provider-protocol/src/types.rs:1100-1129`).

### State is more than an identifier map

`StateFile` carries lineage, serial, resources, and exports; rows also carry
current/deposed instances, explicit-input provenance, bindings, dependencies,
directives, prefixes, and overrides (`carina-state/src/state/mod.rs:484-502`,
`carina-state/src/state/mod.rs:2101-2166`).

State v5 added exports; v15 made current instance a tagged union so an applied
identifier and in-flight create cannot coexist; v17 added top-level
`exports_version`; and v18 added nested write-only provenance
(`carina-state/src/state/mod.rs:741-778`). Stateless mode needs an answer for
each responsibility, not merely no file.

## Prior Art: gh-infra

[gh-infra's state-management design](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/internals/state-management.md)
is working prior art for stateless GitHub IaC. GitHub is its source of truth;
every plan fetches live API state and diffs it against YAML. It has no state
file or lock, and a repository removed from YAML is no longer managed rather
than deleted (documented as "Deleted resources: Not detected").

Its [design principles](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/introduction/principles.md)
make partial ownership central: an omitted field is neither compared nor sent,
while an explicit empty value such as `topics: []` clears it. Collections are
additive by default. `reconcile: authoritative` is opt-in, only applies to
labels, rulesets, and branch protection, and only runs when that collection is
present in the manifest. Secrets, variables, and milestones stay additive.

| Concern | gh-infra behavior | Consequence for Carina |
| --- | --- | --- |
| Identity | Repository full name ([state management](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/internals/state-management.md)); label names case-folded by gh-infra ([labels](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/labels.md)); ruleset name unique within its repository ([rulesets](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/rulesets.md)) | Confirms provider lookup may resolve a natural key to an ephemeral numeric ID |
| Related IDs | Team/app slugs are resolved through the API at run time ([rulesets](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/rulesets.md)) | `Found` may return a run-local `AppliedIdentifier`; persistence is unnecessary when lookup is repeatable |
| Whole-object APIs | GitHub replaces a whole ruleset, so gh-infra reads current and overlays declared fields ([rulesets](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/rulesets.md)); the same partial-ownership rule preserves unmodeled settings ([principles](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/introduction/principles.md)) | Stateless mutation needs a typed field mask and an authorized whole-object overlay model |
| Unreadable settings | Report the repository and leave it out of the plan; never compare unreadable as absent ([principles](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/introduction/principles.md)) | Observation must type unreadable separately from missing |
| Apply verification | Compare GitHub's returned object with what was sent; silently dropped settings fail ([principles](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/introduction/principles.md)) | Carina must verify the managed projection after mutation |
| Secrets | Detect only missing secrets; changed plaintext has no diff; `--force-secrets` resends all ([secrets and variables](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/secrets-variables.md)) | Carina v1 deliberately rejects them; a future always-reassert type may opt in |
| Concurrency | No lock; last-write-wins, with CI concurrency groups or a merge queue ([state management](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/internals/state-management.md)) | Carina may use the same model, but declares it in capability types and plan output |
| Unsolved | Removed repositories are not detected and there are no exports ([state management](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/internals/state-management.md)); label aliases are future work ([labels](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/labels.md)) | These remain explicitly unsupported in Carina stateless mode |

Carina should copy the live-read, partial-field, additive-default, and
post-apply-verification semantics. It should deliberately differ in how the
contract is enforced: gh-infra documents resource-specific behavior, whereas
Carina must reject unsupported types and features through provider schema
capabilities, `ManagedFieldMask`, and stateless typestates.

## 1. Identity Without State

### Eligibility

A type is eligible only when its provider declares a uniqueness scope, total
extractor from resolved desired input, deterministic component normalization,
direct or list-backed lookup with distinct not-found, usable mutation
semantics, and interrupted-operation recovery.

Carina must not infer eligibility from an attribute named `name`, a tag,
`create_only`, `AttributeSchema::identity()`, `UniqueNameSpec`, or a successful
import. A provider-enforced API tuple is different from a human label because
the API guarantees at most one target and accepts the tuple for lookup.

### Types and ownership

`NaturalKey` privately owns provider instance, resource type, and non-empty
normalized components. It has no public constructor, string conversion, or
equality with `ResourceName`/raw `Value`. Only a schema-created extractor may
produce it from `ProviderReadyResource`, whose type already proves resolved,
validated provider input (`carina-core/src/executor/provider_ready.rs:1-66`).

`StatelessResource` contains resolved ID, natural key, and provider-ready
desired data. Internal maps remain keyed by resolved ID; natural key is only an
API address/check. Duplicate keys are errors, never coalesced entries.

### Why this is not name-as-identity

The `CLAUDE.md` rule prevents four concrete failure modes; the proposed type
boundary excludes each one.

- A user label may be pending, `Unknown`, or empty and then collide in
  scheduler/state/diff maps. Only a schema-owned extractor from a fully
  resolved `ProviderReadyResource` can construct `NaturalKey`, whose
  components are non-empty, so none of those states is representable.
- Nobody guarantees that an arbitrary `name` is unique. `NaturalKey` exists
  only when the provider declares that its API enforces uniqueness in an
  explicit scope.
- Carina's identity is unchanged: scheduler, plan, and diff maps stay keyed by
  `ResolvedResourceId`. `NaturalKey` is only a remote address and the subject
  of duplicate-key equality; that check mirrors the provider's own uniqueness
  constraint instead of inventing an internal identity.
- `NaturalKey` has no `From<ResourceName>`, `From<Value>`, or cross-type
  `PartialEq`. A future consumer therefore cannot put a raw user label in the
  remote-key position accidentally.

### Resolution and composite keys

`Value` can be concrete or deferred; deferred variants include references,
secrets, redacted secrets, and `Unknown`
(`carina-core/src/resource/mod.rs:1925-1944`,
`carina-core/src/resource/mod.rs:2527-2595`).

Before plan construction every component must be concrete. Unknown, secret,
provider-populated, or not-yet-created managed references fail extraction
before lookup; resolved data-source/upstream values are allowed.

Ordered components support `(owner, repository, branch)` or
`(zone, name, type, set_identifier)` and may include resolved provider scope.
Changing scope selects another target, not an in-place rename.

Version one rejects keys depending on another managed resource. A provider may
still resolve a declared slug or natural-key component to a numeric API ID for
this run, as [gh-infra does for teams/apps](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/rulesets.md);
that ID is not Carina identity and is never persisted by Carina.

Literal-only keys are extracted during shared validation, so their duplicates
are CLI and LSP errors. Keys containing permitted data-source or upstream
values are extracted after resolution and checked for duplicates before the
first lookup; a collision is a plan-time error. Neither case is coalesced.

### Parent scopes and creation ordering

Rejecting managed-resource references in a key does not order a literal
IssueLabel key after a Repository with the same owner/repository. The schema
must express that relationship. `NaturalKeyCapability` therefore declares an
optional `ParentScopeCapability`: parent resource type plus a schema-owned
projection from the child key to `ParentKey<ParentType>`. IssueLabel, Ruleset,
and BranchProtection declare Repository as parent, projected by owner and repo.

Only that projection can construct `ParentKey`; it cannot be built from or
compared with `ResourceName` or raw `Value`, and it preserves provider instance,
parent type, and normalized scope components. Schema loading rejects an
unsupported parent, incompatible normalization, or a parent cycle. During plan
preparation the host compares `ParentKey<Repository>` only with declared
`NaturalKey<Repository>` values. An exact match produces a typed
`ParentKeyEdge` between the two `ResolvedResourceId` values; scheduler maps
remain keyed by those IDs.

The edge determines observation and creation:

- If the declared parent is found, the host looks up each child normally.
- If the declared parent is not found, its scoped children cannot exist. The
  host skips child lookups and authoritative list calls, plans IssueLabel and
  Ruleset declarations as `ProvenAbsentByParent` creates, treats child
  `absent` declarations as no-ops, renders “not looked up; parent will be
  created,” and orders creates after the parent.
- If no matching parent is declared, it must already exist. The host first
  looks up that parent; parent not-found is `MissingParent`, a typed plan error,
  never a child `NotFound` result.

BranchProtection also requires its branch to exist. Following gh-infra's
[design principles](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/introduction/principles.md) ("After apply, plan shows no changes"),
version one defers a desired branch-protection create when its repository will
be created in the same apply; an `absent` declaration is already satisfied. The
plan shows `Deferred: branch unavailable until repository creation`; the next
plan can create it once the branch exists. One apply therefore may not converge
for a brand-new repository. A missing branch under an already-existing
repository is a typed prerequisite error, not a deferred create.

### Normalization

Normalization belongs to the key/schema, never scattered lowercase calls.
Exact, documented case-folding, and DNS normalizers are deterministic and part
of the provider schema fingerprint.

GitHub's REST reference says only that the `owner` and `repo` parameters are
not case sensitive on the [repository](https://docs.github.com/en/rest/repos/repos)
and [label](https://docs.github.com/en/rest/issues/labels) endpoints; it makes
no equivalent statement for the label-name parameter. [gh-infra's label
documentation](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/labels.md)
case-folds label names, rejects manifest duplicates that differ only by case,
and treats a case-only difference as a rename. Carina may adopt that behavior
only as a provider assumption backed by an acceptance test; the precise Unicode
folding rule must be part of the schema fingerprint.

With that tested case-folded key, desired `Bug` and observed `bug` produce the
same `NaturalKey`; the plan updates the label's display name instead of creating
a second label.

Lookup returns a canonical observed key; aliases or redirects to another name
are not equality. GitHub documents repository redirects, but old-name REST
behavior was not fully verified, so providers compare returned `full_name`.

## 2. Deletion

State-backed Carina finds orphans because state retains resources removed from
configuration. Stateless mode has no inventory. Three models were considered.

### Field clearing is explicit

Following [gh-infra's principles](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/introduction/principles.md),
omission means "unmanaged": the field is neither compared nor sent. Clearing
requires a schema-valid empty or typed clear value; `null` clears only where
declared. This current-source ownership mask needs no authoring history and is
independent of object deletion.

### Explicit absence

Removing a block only stops management. Remote deletion uses a typed `absent`
declaration carrying the full IssueLabel key (repository plus name); plan looks
up that key and shows an explicit delete.

Today's `removed` means "remove from state without destroying"
(`carina-core/src/parser/ast.rs:1021-1050`). It produces state removal, not a
provider delete (`carina-core/src/effect.rs:560-572`,
`carina-cli/src/commands/shared/effect_execution.rs:145-174`,
`carina-cli/src/commands/shared/state_writeback.rs:1450-1471`). Overloading it
would give one keyword opposite meanings. Stateless validation rejects it;
`absent` expresses remote absence.

### Authoritative scopes

Opt-in authoritative scopes initially cover only labels, rulesets, and branch
protection. The collection must be present; absence means unmanaged. Variables,
secrets, and milestones stay additive. A typed scope requires complete paged
list, cross-page normalization/deduplication, and every deletion in the plan.

No delete is planned after a failed, forbidden, cyclic, or incomplete page.
Scope is never inferred from several resources sharing a parent: a wrong scope
can delete an entire collection.

### Ownership markers

Repository topics/descriptions and label/team descriptions are user data, not
neutral markers; many GitHub types have no slot and AWS tags are not universal.
Markers may filter deletion, never prove identity or ownership.

### Decision

The first useful slice combines additive default behavior, individual `absent`,
and opt-in authoritative label/ruleset/branch-protection scopes. Repository
removal only stops management; repository `absent` and `carina destroy` are
gated off. Markers may strengthen a scope but never replace it.

## 3. What State Carries and Stateless Mode Loses

| Responsibility | Current evidence | Stateless answer |
| --- | --- | --- |
| Remote identifier | `Provider::read` consumes saved ID (`carina-core/src/provider.rs:3540-3569`) | Capable types use `NaturalKey` lookup |
| Accepted desired values | `RowBasis` records accepted provenance (`carina-state/src/state/mod.rs:367-386`) | Current source produces `ManagedFieldMask` |
| Authored shape | Differ uses prior explicit input (`carina-core/src/differ/comparison.rs:838-969`) | Omitted now means unmanaged; explicit empty clears |
| Write-only values | v18 stores accepted hashes/provenance (`carina-state/src/state/mod.rs:741-778`) | Unsupported in v1 |
| Provider outputs | Current instance retains outputs (`carina-state/src/state/mod.rs:2039-2072`) | Live observation only; managed refs rejected |
| Exports | `StateFile.exports` persists upstream bindings (`carina-state/src/state/mod.rs:499-501`) | Cannot publish exports |
| Renames | `moved` rewrites state binding (`carina-core/src/effect.rs:566-572`) | Unsupported; new key plus old `absent` |
| In-flight create | Pending/in-flight types retain recovery (`carina-core/src/provider.rs:1743-1865`, `carina-core/src/provider.rs:2179-2350`) | Must recover by refinding key |
| Saved-plan freshness | Plan stores lineage/serial (`carina-cli/src/commands/plan.rs:127-215`) | Saved plans unsupported in v1 |
| Mutual exclusion | Backend owns lock (`carina-state/src/backend.rs:550-603`) | No Carina lock; explicit optimistic model |
| Orphan inventory | Differ enumerates unmatched state IDs (`carina-core/src/differ/mod.rs:262-325`) | No implicit orphan detection |
| Deposed instances | Resource state retains current/deposed (`carina-state/src/state/mod.rs:2101-2166`) | CBD/deposed recovery unsupported |

### Write-only values

Carina compares secrets through accepted-state hashes
(`carina-core/src/differ/comparison.rs:494-521`) and overlays sent/retained
write-only values during writeback (`carina-state/src/state/mod.rs:2980-3078`).

GitHub Actions secret reads expose `updated_at`, not plaintext. This design
assumes the timestamp records a write, not value equality; its exact update
behavior requires an acceptance test. Regardless, an out-of-band plaintext
change cannot be compared, so `updated_at` cannot replace the accepted-value
hash.

[gh-infra's secrets and variables documentation](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/secrets-variables.md)
plans only missing secrets and offers `--force-secrets` to re-send all
values. Carina deliberately starts stricter: version one rejects every
top-level or nested write-only desired field through schema validation
(`carina-core/src/schema/mod.rs:528-617`,
`carina-core/src/schema/mod.rs:6293-6506`). A future typed
`AlwaysReassertWriteOnly` capability could model "write every apply; equality
unknown" rather than adding an untyped force flag.

### Exports and upstream values

`carina export` reads persisted exports (`carina-cli/src/main.rs:154-171`,
`carina-cli/src/commands/export.rs:27-66`). Stateless projects cannot publish or
serve as upstream state, though they may consume concretely resolved stateful
upstream values. `--state-url` may inspect another file; [gh-infra also has no
exports](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/internals/state-management.md).

### Renames and state operations

The grammar's `import`, `removed`, and `moved` state operations are rejected
(`carina-core/src/parser/carina.pest:142-167`). A DSL-label change with the same
key is harmless; a key change is new management plus optional old-key `absent`.
Like [gh-infra](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/labels.md),
version one has no rename history or aliases.

### In-flight creates

`CurrentInstance` persists #3909's applied/create-in-flight distinction
(`carina-state/src/state/mod.rs:2039-2072`). Stateless cannot save an opaque
poll token, so capability `RefindByKey` must recover interruption without
duplicates; otherwise create is unsupported. GitHub types must test ambiguous
responses.

### Saved plans

Saved-plan apply locks/reloads state and checks lineage/serial
(`carina-cli/src/commands/apply/mod.rs:3200-3262`, `carina-cli/src/commands/apply/mod.rs:3298-3352`), represented by
`ReloadedPlanFile` (`carina-cli/src/commands/plan.rs:1359-1378`). Stateless has
no equivalent and rejects `plan --out`/saved apply; interactive apply re-reads.
A future format requires revision tokens or conditional requests.

### Lock and concurrency

State-backed apply locks (`carina-cli/src/commands/apply/mod.rs:1554-1571`);
`backend none` cannot. As in [gh-infra's state model](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/internals/state-management.md),
use last-write-wins plus CI concurrency
or a merge queue, with the next plan exposing divergence. Carina types remote
uniqueness, fresh lookup, conflict recovery, and available revision conditions.
Serializable users need state-backed mode; a lock-only backend is open.

### Provider-populated values

Provider-populated output is typed (`carina-core/src/schema/mod.rs:491-525`) and
may be displayed, but managed references to it are rejected because they need
live-output scheduling/cycle semantics. Shared validation enforces this;
already-resolved data-source/upstream values remain allowed.

### Field ownership, refresh, and whole-object APIs

State-backed Carina projects away fields never authored, but omission of a
previously authored top-level field emits a change
(`carina-core/src/differ/comparison.rs:838-969`) and `PatchOp::Remove`
(`carina-core/src/provider.rs:715-766`).

Nested maps are more selective: saved keys never authored are merged back,
while previously authored keys omitted now are removed
(`carina-core/src/resource/mod.rs:4920-5045`). State's explicit tree is what
lets state-backed Carina distinguish those cases.

Stateless has no previous tree. `ManagedFieldMask` is derived from the current
source authoring tree before schema defaults or provider normalization; defaults
never silently become managed. Only masked paths are compared/sent, omission is
unmanaged, and explicit clear values remain masked. Existing `ExplicitFields`
is the relevant representation precedent (`carina-core/src/explicit.rs:1-160`).

Existing `UpdateRequest` forbids writing unrepresented fields
(`carina-core/src/provider.rs:585-710`), fitting patch APIs. A typed
`WholeObjectOverlay` instead reads current, overlays masked paths, preserves
unmasked/unmodeled settings, sends the whole object, and verifies the result.

Every stateless plan is a live refresh, so `--refresh=false` is rejected.
Readable drift on managed paths is detected. An unreadable managed path is a
typed observation error and removes that resource from the plan, never a
missing/default value. Current `ReadOutcome` lacks partial-read information,
although mutation outcomes have `PartialReadDiagnostic`
(`carina-core/src/provider.rs:1574-1679`); the new lookup must close that gap.

## 4. Gating and Typestate

### Schema capability

Every resource schema carries exhaustive `StatelessCapability::Unsupported` or
`NaturalKey(NaturalKeyCapability)`. The latter contains key, recovery,
concurrency, optional parent scope/prerequisite, supported authoritative scopes,
and a mutation model of either `FieldPatch` or `WholeObjectOverlay`.

This is not inferred or optional. Core builders may initialize `Unsupported`,
but the protocol field is mandatory so an old or malformed provider cannot opt
in through a default. `ManagedFieldMask` also has private construction: only
schema-aware extraction from the current source can produce valid field paths.

Component paths are ordered and non-empty. Schema loading rejects paths into
provider-populated, write-only, secret, collection, or unstable values, plus
invalid parent projections, incompatible parent normalization, and parent
cycles.

### Validation

The stateless validation pipeline rejects:

- any managed type with `Unsupported` capability;
- duplicate literal-only natural keys in one provider instance (the shared CLI
  and LSP validation error);
- duplicate keys containing permitted data-source/upstream values (after
  resolution and before any provider lookup, as a plan-time error);
- ambiguous or cyclic `ParentKeyEdge` construction;
- top-level or nested write-only desired fields;
- exports, `import`, `removed`, and `moved`;
- saved-plan input/output and `--refresh=false`;
- create-before-destroy or other deposed-instance lifecycles;
- managed provider-populated references;
- fields or clear values unsupported by the declared mutation model;
- unrecoverable pending creates; and
- authoritative scopes absent from the type's capability.

Dynamic extraction rejects unresolved keys before observation. It resolves the
permitted data-source/upstream case, performs the second duplicate check,
derives `ParentKeyEdge` values, and observes parents before any child lookup.
`MissingParent`, missing branch prerequisites, and unreadable managed paths fail
before effect construction. Only those successes, plus explicitly typed
deferred children, can construct `StatelessPlan`.

CLI validation already loads schemas and calls shared checks
(`carina-cli/src/commands/validate.rs:266-320`). The new checker belongs there,
with LSP diagnostics calling the same implementation. That satisfies the
CLI/LSP parity rule in `CLAUDE.md:433-437`.

### Separate pipelines

`ValidatedProject` is an exhaustive enum of `ValidatedStateBackedProject` and
`ValidatedStatelessProject`. `StateBackedPlan` retains state snapshot, lineage,
serial, and state effects. `StatelessPlan` contains live observations and
`StatelessEffect`, but no state, exports, pending creates, or writeback; its
apply function cannot accept `StateBackend`.

The stateless builder consumes `ParentOrderedResource`, which always carries a
typed scope disposition: `Root`, `ParentFound`, `ProvenAbsentByParent`, or
deferred prerequisite. A child effect without that disposition is
unrepresentable.

The command layer performs one exhaustive dispatch on `ValidatedProject`.
There are no pervasive `if stateless` branches. `StatelessEffect` cannot
represent state removal/move, deposed replacement, or export publication, so
those paths are unreachable rather than guarded by warnings.

## 5. User Experience

### Selection

The grammar already accepts a backend identifier and body
(`carina-core/src/parser/carina.pest:137-140`). `none` accepts no attributes and
is exclusive with `local` or `s3`. Lifting produces
`ExecutionMode::Stateless`, not `BackendConfig::None`; current
`BackendConfig` variants are real storage (`carina-state/src/backend_config.rs:15-84`).

A separate `stateless = true` could contradict a real backend and is rejected.
An omitted backend keeps current resolution behavior. Switching an existing
state-backed project is not automatic: the CLI rejects known state, in-flight,
or deposed data until an explicit abandonment design exists.

### Non-final configuration sketch

This is design syntax, not accepted grammar. Only the general block shape and
`backend none {}` follow today's parser; provider type names, `stateless_scope`,
and `absent` remain to be specified. The `organization` provider attribute
matches the GitHub provider design
(`carina-rs/carina-provider-github:docs/design.md:61`).

```carina
backend none {}

provider github { organization = 'carina-rs' }

github.Repository {
  name        = 'carina'
  description = 'Strongly typed infrastructure management'
}

github.IssueLabel {
  repository = 'carina'
  name       = 'bug'
  color      = 'd73a4a'
}

github.IssueLabel {
  repository = 'carina'
  name       = 'documentation'
  color      = '0075ca'
}

stateless_scope github.IssueLabel {
  repository = 'carina'
  reconcile  = authoritative
}

absent github.IssueLabel {
  repository = 'carina'
  name       = 'obsolete'
}
```

The two ordinary labels are the authoritative scope's declared set. `absent`
is an explicit keyed deletion; omitting the repository block would only stop
managing it.

### Non-final plan sketch

Illustrative output for that configuration is:

```text
$ carina plan
Mode: stateless (backend none; no state file)
Lock: none (last write wins; serialize applies externally)

Live lookup
  FOUND      github.Repository[carina-rs/carina]
  FOUND      github.IssueLabel[carina-rs/carina#bug]
  NOT FOUND  github.IssueLabel[carina-rs/carina#documentation]
  FOUND      github.IssueLabel[carina-rs/carina#obsolete] (explicit absent)
  LIST       github.IssueLabel[carina-rs/carina]
             pages read=2, declared=2, remote=4

Changes
  ~ github.Repository[carina-rs/carina]
      description: 'Infrastructure management' -> 'Strongly typed infrastructure management'
      visibility: unmanaged (not compared or sent)
  ~ github.IssueLabel[carina-rs/carina#bug]
      color: 'b60205' -> 'd73a4a'
  + github.IssueLabel[carina-rs/carina#documentation]
  - github.IssueLabel[carina-rs/carina#obsolete] (explicit absent)
  - github.IssueLabel[carina-rs/carina#needs-triage] (authoritative scope)
  - github.IssueLabel[carina-rs/carina#stale] (authoritative scope)

Plan: 1 to create, 2 to update, 3 to delete; no state will be written
```

Those child lookups are valid because the repository is `FOUND`. If it were
`NOT FOUND`, label/ruleset rows would instead say `NOT LOOKED UP (parent will be
created)` and plan creates after the repository; branch protection would say
`DEFERRED (branch unavailable until repository creation)`.

Lookup failure is distinct from not-found: 403, unreadable managed settings,
rate limits, or transport errors never become create or false diffs. The
affected resource is reported and contributes no effects.

Apply resolves every key and capability before mutation, rechecks the relevant
key, and uses the found result's ephemeral `AppliedIdentifier`. It neither
opens state nor locks, exports, or saves pending work. It fails an effect when
the returned managed projection differs from what was sent.

### Non-final validation diagnostics

Errors name mode, source, rejected capability, and repair. For an AWS VPC:

```text
$ carina validate
error[stateless-unsupported-type]: aws.ec2.Vpc cannot be used with backend none
  --> network.crn:8:16
  = schema capability: StatelessCapability::Unsupported
  = use backend local or backend s3 for this resource
```

For the GitHub provider's planned secret type:

```text
$ carina validate
error[stateless-write-only]: github.actions.OrganizationSecret has a write-only desired value
  --> secrets.crn:12:3
  = attribute `plaintext_value` cannot be read back, so stateless equality is unavailable
  = use a state-backed backend; OrganizationSecret is not supported in stateless v1
```

### Commands

`carina state`, project export, import, saved-plan apply, and `destroy` return
mode-specific errors. `destroy` directs users to explicit `absent` declarations.
`carina export --state-url` may still inspect an unrelated state file.

## Provider Assessment

### GitHub: mostly future types

The GitHub provider is only a scaffold.
`carina-rs/carina-provider-github:README.md:10-24` describes bootstrapping,
`carina-rs/carina-provider-github:docs/design.md:6-14` says implementation is
future work, and
`carina-rs/carina-provider-github:carina-provider-github/src/lib.rs:1-4` is
scaffold only.

The planned types are `OrganizationSecret` and `OrganizationVariable`
(`carina-rs/carina-provider-github:docs/design.md:41-55`); repositories, teams,
and branch protection are initial non-goals
(`carina-rs/carina-provider-github:docs/design.md:16-39`). Secrets require state
hashing (`carina-rs/carina-provider-github:docs/design.md:79-86`,
`carina-rs/carina-provider-github:docs/design.md:178-182`). Thus the planned
secret is gated off; the variable would stay additive under this design and is
a weak proof. The provider resource set must expand before stateless mode is
worth shipping.

| Proposed type | Candidate key | Classification |
| --- | --- | --- |
| Organization variable | organization + name | Readable; additive-only under this design; supplementary, not the showcase |
| Organization secret | organization + name | Direct key, but write-only; reject v1 |
| Repository | owner + repository | Required slice; field overlay, but rename/redirect hazard |
| Classic branch protection | owner + repository + branch | Required slice; whole-object overlay |
| Repository ruleset | owner + repository + ruleset name | Required slice; gh-infra documents repository uniqueness, but the provider must test it |
| Issue label | owner + repository + case-folded name | Required slice if the provider acceptance test proves gh-infra's case-folding behavior |
| Team | organization + team slug | Conditional; slug-change-on-rename is an unverified assumption |
| Team membership | organization + team slug + username | Candidate; IdP-synchronized write restrictions are an unverified assumption |
| Repository collaborator | owner + repository + username | Poor first type; read may reflect indirect effective access |
| Repository variable | owner + repository + name | Good later candidate; readable/direct |
| Environment variable | owner + repository + environment + name | Good later if environment normalization is explicit |
| Org/repo/environment secret | corresponding scope + name | Natural lookup but write-only; reject v1 |
| Webhook | owner + repository + hook ID | ID is server-assigned; URL is not documented unique |
| Deploy key | owner + repository + key ID | ID is server-assigned; title is not unique |

REST verification for this design is narrow: the
[repository](https://docs.github.com/en/rest/repos/repos) and
[label](https://docs.github.com/en/rest/issues/labels) references explicitly
say their `owner` and `repo` parameters are not case sensitive. They do not say
that the label-name parameter is case insensitive.

Provider assumptions requiring acceptance tests are: label-name case folding
and exact Unicode behavior (documented by
[gh-infra](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/labels.md),
not the REST reference); team slugs changing on rename; write restrictions for
IdP-synchronized teams; the meaning and update behavior of an Actions secret's
`updated_at`; ruleset-name uniqueness within a repository (documented by
[gh-infra](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/rulesets.md));
404 versus 403 under every token type; and old-name repository redirects.

The first useful stateless slice is concrete:

| Type | Mutation | Deletion |
| --- | --- | --- |
| Repository | Create/update with current-manifest field mask | Removing block stops management; repository deletion gated off |
| IssueLabel | Keyed update or parent-proven create | Additive default, individual `absent`, opt-in authoritative scope |
| Ruleset | Name lookup to ephemeral ID, parent-proven create, whole-object overlay | Additive default, `absent`, opt-in authoritative scope |
| BranchProtection | Branch-keyed whole-object overlay; deferred one plan for a new repository | Additive default, `absent`, opt-in authoritative scope |

`OrganizationVariable` may join as additive-only support, but it does not make
the slice useful by itself. Secrets, milestones, repository deletion, renames,
exports, and provider-output references remain gated off.

### Hand-written AWS provider

AWS codegen metadata has an API identifier and anonymous identity overrides,
not a stateless capability
(`carina-rs/carina-provider-aws:carina-codegen-aws/src/resource_defs.rs:40-104`).
`ReplacementClass::UniqueName` is only replacement policy
(`carina-rs/carina-provider-aws:carina-codegen-aws/src/resource_defs.rs:30-38`).

Commands used with `$AWS_PROVIDER` set to the provider checkout root:

```sh
rg -c '^\s*(vec!\[)?ResourceDef \{' \
  "$AWS_PROVIDER/carina-codegen-aws/src/resource_defs.rs"
# 40

rg -c '^\s+replacement: ReplacementClass::UniqueName' \
  "$AWS_PROVIDER/carina-codegen-aws/src/resource_defs.rs"
# 3
```

Those three are not a stateless count. Candidate and counterexample types are:

| Class | Examples | Evidence/caveat |
| --- | --- | --- |
| User-chosen service key | S3 Bucket | Required/create-only bucket name (`carina-rs/carina-provider-aws:carina-provider-aws/src/schemas/generated/s3/bucket.rs:23-86`) |
| Account-scoped key | IAM Role, Logs LogGroup | Names plus outputs (`carina-rs/carina-provider-aws:carina-provider-aws/src/schemas/generated/iam/role.rs:16-78`, `carina-rs/carina-provider-aws:carina-provider-aws/src/schemas/generated/logs/log_group.rs:14-70`) |
| Composite key | Route 53 RecordSet | Identifier/normalization (`carina-rs/carina-provider-aws:carina-provider-aws/src/services/route53/record_set.rs:1-47`); paged read (`carina-rs/carina-provider-aws:carina-provider-aws/src/services/route53/record_set.rs:298-345`) |
| Derivable service handle | SQS Queue | Desired name, but current ID is Queue URL (`carina-rs/carina-provider-aws:carina-codegen-aws/src/resource_defs.rs:2723-2763`) |
| Server-assigned ID | VPC, subnet, SG, instance | `VpcId` is provider-populated (`carina-rs/carina-provider-aws:carina-provider-aws/src/schemas/generated/ec2/vpc.rs:14-78`) |

IAM users/policies and SSM parameters may be natural-key resources in AWS, but
that is not evidence that the hand-written provider implements those types.
No type should opt in without lookup, normalization, declared field-mutation,
and interrupted-operation tests.

### AWSCC measurement

AWSCC reads CloudFormation `primaryIdentifier` metadata
(`carina-rs/carina-provider-awscc:carina-provider-awscc/src/schemas/config.rs:16-53`,
`carina-rs/carina-provider-awscc:carina-provider-awscc/src/bin/codegen.rs:67-95`,
`carina-rs/carina-provider-awscc:carina-provider-awscc/src/bin/codegen.rs:3553-3579`).
Runtime still passes a saved identifier to Cloud Control GetResource
(`carina-rs/carina-provider-awscc:carina-provider-awscc/src/provider/operations.rs:572-625`,
`carina-rs/carina-provider-awscc:carina-provider-awscc/src/provider/cloudcontrol.rs:285-324`).

With `$AWSCC_PROVIDER` set to the provider checkout root, this command
classified all 49 cached schemas:

```sh
schema_dir="$AWSCC_PROVIDER/cfn-schema-cache"
for f in "$schema_dir"/*.json; do
  jq -r '.typeName as $t
    | (.primaryIdentifier // []) as $p
    | (.createOnlyProperties // []) as $c
    | (.readOnlyProperties // []) as $r
    | if ($p | length) == 0 then [$t, "no_primary"]
      elif ([$p[] as $id | select(($c | index($id)) == null)] | length) == 0
        then [$t, "all_primary_create_only"]
      elif ([$p[] as $id | select(($r | index($id)) != null)] | length) > 0
        then [$t, "has_read_only_primary"]
      else [$t, "other_or_mixed"] end
    | @tsv' "$f"
done | sort -k2,2 -k1,1 |
  awk -F '\t' '{count[$2]++} END {for (k in count) print k, count[k]}'
```

Result:

```text
all_primary_create_only 11
has_read_only_primary 38
```

The 11 were DynamoDB Table, ECR Repository, ECS Cluster, IAM Role, IAM
RolePolicy, Logs LogGroup, S3 Bucket, S3 BucketPolicy, SSM Parameter, SSO
Assignment, and WAFv2 WebACLAssociation.

That is an upper bound, not 11 capable types. Create-only may be optional or
service-defaulted and proves neither lookup nor recovery. AWSCC currently
canonicalizes primary attributes only after read/create
(`carina-rs/carina-provider-awscc:carina-provider-awscc/src/provider/operations.rs:2039-2093`).

Both AWS providers must emit `Unsupported` initially. It is not worth promising
broad AWS stateless operation for a small candidate subset while excluding the
server-ID resources that make up real stacks. S3 Bucket, SSM Parameter, Logs
LogGroup, or Route 53 RecordSet may be reviewed individually after GitHub.

## Protocol and WIT Changes

The current provider protocol is version 12
(`carina-provider-protocol/src/lib.rs:6-11`). This design requires version 13
and coordinated provider rebuilds.

Add a mandatory `stateless-capability` variant to the WIT/protocol resource
schema. Its natural-key case includes optional parent type/key projection and
prerequisite, mutation model, and authoritative-scope support; there is no
missing-field default. Existing WIT
`identity-attributes` retain anonymous-identity meaning
(`carina-plugin-wit/wit/provider.wit:211-213`).

Add `find_by_natural_key(provider, resource_type, key)`; it must not require a
desired resource because an implicit parent lookup has only `ParentKey`. Its
typed outcome is either `NotFound` or `Found { canonical_key, identifier,
current, observability, revision }`. `observability` distinguishes readable,
truly missing, and unreadable fields; only the first two may enter comparison.

Not-found is a successful outcome; permission/rate/transport failures are
errors. The identifier feeds existing update/delete, the current object feeds
diff, and canonical key catches redirect or normalization mismatch.

The host uses the same operation to observe a declared or implicit parent
before looking up its children. Add a typed prerequisite observation for cases
not represented by a resource key, initially repository-branch existence. Its
outcome is `Ready` or `Missing`; the host constructs `Deferred` without calling
it when the parent repository itself is planned for creation.

Add a paged `list_stateless_scope` for the initial authoritative collections.
Items return canonical key, ephemeral identifier, current object,
observability, and optional revision. The host enforces token progress,
deduplication, normalization, and all-pages-success before delete diff.

Add a stateless update request carrying `ManagedFieldMask`. `FieldPatch` sends
only masked paths. `WholeObjectOverlay` authorizes the provider to read current,
overlay masked paths, and preserve unmasked or unmodeled settings; this must not
weaken the existing state-backed `UpdateRequest` rule. Both return the observed
post-write object, and the host fails any managed path GitHub silently dropped.

| Repository | Counterpart work |
| --- | --- |
| `carina-plugin-wit` | Capability, parent/prerequisite, field-mask, observability, find/list, and stateless-update types |
| `carina` | Protocol, SDK/host conversion, parent-key graph, types, DSL, validation, LSP, plan/apply |
| `carina-provider-github` | Repository/label/ruleset/branch-protection schemas, lookup/list, overlay, verification, recovery tests |
| `carina-provider-aws` | Rebuild for v13 and explicitly emit `Unsupported` |
| `carina-provider-awscc` | Rebuild for v13 and explicitly emit `Unsupported`; no inference |

The main repository's pinned `carina-plugin-wit` commit and host bindings must
move together; the sibling checkout is not a substitute for that update.

## Phasing

### Phase 0: contract

- Agree on omission/clear semantics, `backend none`, `absent`, authoritative
  scope syntax, and the rejection matrix.
- Land protocol 13/WIT with all existing types `Unsupported`.
- Add parent/prerequisite, field-mask/observability/find/list/overlay types and
  schema failures.
- Add shared CLI/LSP validation fixtures. Do not advertise the mode yet.

### Phase 1: grow the GitHub provider

- Implement Repository, IssueLabel, Ruleset, and BranchProtection with ordinary
  state-backed behavior first; the current provider scaffold has none of them.
- Prove key and parent lookup, not-found/forbidden/unreadable distinctions,
  normalization, new-repository deferral, whole-object preservation,
  post-write verification, and recovery.
- Keep every capability `Unsupported` until those contracts pass provider tests.

### Phase 2: useful stateless slice and later

- Enable the four-type slice together with additive default, explicit `absent`,
  authoritative label/ruleset/branch-protection scopes, typed parent ordering,
  and explicit new-repository branch-protection deferral.
- Add OrganizationVariable only as supplementary additive support; keep
  OrganizationSecret rejected unless a typed always-reassert contract lands.
- Evaluate AWS types one at a time only in response to demand; never infer from
  `identity()`, `UniqueNameSpec`, `createOnly`, or `primaryIdentifier`.

No implementation is part of this document.

## Open Questions

1. Is `absent` final, and should state-backed projects also accept it?
2. What exact GitHub case folding is guaranteed for every proposed key?
3. Can all token modes distinguish not-found from forbidden reliably?
4. How do rename redirects behave for REST writes, and which field is canonical?
5. Should organization/provider scope always be rendered as a key component?
6. How are explicit clear values represented for every optional field shape?
7. Can `ManagedFieldMask` represent nested lists safely without stable element keys?
8. What provider-private data is needed to preserve unmodeled whole-object fields?
9. Is a lock-only backend worth designing for serialized stateless applies?
10. Is `AlwaysReassertWriteOnly` worth unknown equality and perpetual writes?
11. How should users abandon old state, including in-flight/deposed instances?
12. Is ruleset-name uniqueness strong enough for a Carina capability contract?
13. Which AWS type, if any, justifies the first exception?

## Recommendation

Proceed only after the GitHub provider grows Repository, IssueLabel, Ruleset,
and BranchProtection. Ship those as one useful slice with provider-declared
keys, current-source field masks, live reads, additive default, explicit
absence, typed parent ordering, typed authoritative scopes, post-write
verification, shared validation, and separate stateless typestates. Document
that branch protection for a newly created repository needs a second plan.

OrganizationVariable may be supplementary; OrganizationSecret is gated off.
Do not include repository deletion, renames, exports, saved plans, managed
provider-output references, unrecoverable pending creates, or AWS types. Do not
market this as broad AWS stateless mode; evaluate AWS only after GitHub proves value.
