# Provider protocol versions

## Version 13

Protocol v13 makes initialized reference scope, provider-declared reference
forms, and recoverable create failures explicit transport data.

The `normalize-desired` input is now
`list<desired-resource { resource, deferred }>` rather than a plain resource
list. `deferred` names the top-level attributes whose unresolved values the
host withheld from `resource`. A returned resource must not contain an
attribute named in its input's `deferred` list; the host rejects such a result
and restores the original unresolved value (carina#4120).

`initialize` now returns an `initialize-result` containing a closed
`reference-scope`: either the provider instance's `partition`, `region`, and
`account`, or `unavailable`. The scope belongs to the initialized instance, so
two named instances of one provider kind may report different values.

The `schemas` JSON is now a provider-schema envelope containing `resources`
and `reference_forms`. Reference forms are a sorted list of `{identity, form}`
records on the wire and a map in validated Rust witnesses. The `identity` is
the same dotted string used by attribute-type identities and follows the same
decode path. A form is currently the closed `template` variant, whose `long`
value must contain `{name}` exactly once and may otherwise contain only
`{partition}`, `{region}`, and `{account}` placeholders. Stray braces and
unknown placeholders fail typed validation.

At comparison time, `{name}` is the unqualified reference form and denotes one
bare segment containing neither `:` nor `/`. Only such a bare value is expanded
through `long`. A value containing `:` or `/` is already a qualified or path
form, so it compares exactly as written and never compares equal to a bare name
through reference-form expansion.

The host admits resources, provider config, and forms as one provider-wide
witness. Every form identity must be declared by that provider as a refined
String; duplicate records, dangling identities, Int identities, and Enum
identities are rejected while decoding. The registry retains one table per
schema owner rather than one global identity table. Owner forms also contribute
to semantic schema fingerprint encoding v6.

Every nested `StructField` schema JSON record carries required `create_only`
and `identity` booleans. Neither field has a serde default: a provider built
against an older JSON record fails to load instead of silently changing
replacement or anonymous-address semantics. The host rebuilds decoded fields
through the same input-mode and create-only builders used by native schemas.
It rejects `identity: true` unless `create_only` is also true, rejects both
flags on provider-populated fields, and rejects identity below a list, map, or
union because that location does not identify one singular value. Both flags
participate in semantic schema fingerprint encoding v6, so a saved plan is
invalidated when either declaration changes.

This is a schema-JSON-only extension. The WIT world and protocol version remain
13. For anonymous resources, one or more declared identity paths select the
identity-only address encoding; when no identity path is declared, the host
retains the create-only fallback. Nested paths are represented as typed
attribute/struct-field segments in the host and are never transported as a
dotted string.

Every resource schema JSON record also carries the required boolean
`default_prevent_destroy`. Providers use it for resource kinds whose deletion
is intrinsically dangerous. There is no serde default: a provider that omits
the field fails to load instead of silently disabling protection. This is a
JSON schema-record change only; the WIT world and protocol version remain 13.

State format 20 adds `create_outcome_unknown`, a token-less current instance
containing only the create's UTC `started_at` timestamp. The host writes this
row when an initial guest create fails after at least one host-approved request
was dispatched and no identifier or continuation was returned. Plan, apply,
and destroy block on the row. Refresh carries it forward unchanged without a
provider read. Recovery is explicit: import replaces it when the resource
exists, while `carina state forget` clears only the current instance after the
operator confirms the resource is absent; any deposed generations remain
tracked for deletion.

State format 20 also stores `prevent_destroy` as a tri-state directive. During
the v19→v20 migration, legacy `true` becomes explicit true, while legacy
`false` or a missing field becomes unspecified. Unspecified state rows inherit
the resource schema default; an explicit false is the only persisted opt-out.

Saved-plan format 31 persists authored `ignore_changes` paths for schema-time
re-resolution. Format 30 was never emitted; provider protocol v13 uses format
32.

Saved-plan format 32 records the `reference-scope` of every loaded provider
instance next to its provider artifact evidence. Saved-plan apply initializes
those same provider instances and refuses a missing or different scope before
drift reads, secret evaluation, or provider effects. The evidence is keyed by
provider instance, not provider kind, so named instances in different regions
or accounts remain distinct. The format also records an outcome-unknown marker
and its `started_at` timestamp, never an identifier or continuation token.
Apply reloads that marker under the state lock and rejects any lifecycle or
timestamp change before provider effects run.

Saved-plan format 33 adds typed keyed-list release effects. Saved-plan format
34 adds the structural `IntegerArithmetic` deferred value so an Int expression
whose operands become known during apply is preserved without lowering to a
builtin or string. Older plans are rejected with the standard replan guidance;
the state format is unchanged.

Saved-plan format 35 stores the same tri-state directive shape and the exact
state-only directive change set shown during planning. The set contains only
desired resources that have no managed mutation in the execution plan; a
resource handled by create, update, replace, move, or another mutating effect
persists its directives through normal state writeback instead. Each entry
carries the full routed resource ID and typed per-field `from`/`to` values.
Saved-plan apply recomputes the set against locked state and compares those
typed identities and values, refusing a mismatch with re-plan guidance before
provider effects run. Format 34 plans cannot distinguish explicit false from
an unspecified value and must be regenerated.

Create, poll, and resume verdicts gain an `in-flight` arm. It carries the new
continuation together with the error that stopped the current step. The host
validates and preserves that continuation through the same typestate path as
an ordinary `pending` result, including any identifier learned during the
call. Provider-facing SDK handles use `preserve`; `undetermined` remains a wire
classification used by guest glue when it cannot decode the request or token
and therefore has no newer continuation.

Hosts and providers must both use protocol v13. The minimum supported protocol
version is 13. Rejecting v12 at provider load is intentional because v12 has no
way to carry either initialized scope or an error-bearing replacement
continuation.

## Version 12

Protocol v12 adds the required `write_only` boolean to every nested
`StructField` in schema JSON. It has no serde default: schema JSON emitted by
an older provider fails to decode instead of silently treating an
unobservable nested member as readable. Host conversions carry the field into
the core schema, where write-only members are incompatible with read-only
(provider-populated) members but may be either required or optional. The WIT
world does not change.

The host rejects schema JSON at decode time when any of these combinations
would make nested write-only provenance ambiguous or contradictory:

- a top-level `write_only` attribute also contains write-only members;
- a union member contains write-only members; or
- one nested field is both `read_only` and `write_only`.

The host uses nested write-only typing both to reassert the containing
top-level attribute during an otherwise necessary update and to compare the
unobservable leaves against accepted-request provenance. State format v18 adds
that typed per-instance leaf record; the v17 migration initializes it empty so
the first plan performs one reconciling update.

Version 12 hosts and providers required an exact v12 match. Rejecting v11 at
provider load was intentional: either direction of host/provider skew would
otherwise discard the nested `write_only` marker and silently restore the
non-converging and incomplete-update behavior.

## Version 11

In protocol v11 an update patch op is a sum type, so its shape enforces its own
invariants. WIT `patch-op` is a variant:

- `add`, `replace` and `reassert` carry a `patch-value { key, value }`.
- `remove` carries only the key.

The `patch-op-kind` enum and the optional `value` field are removed. As a
result, an op that sends a value can no longer omit it, and a removal can no
longer carry one. Core and protocol `PatchOp` have the same four variants, with
`key()` and `sent_value()` accessors.

`reassert` is a desired, updatable write-only key: write-only in the schema and
not create-only. Core re-sends it because the backend cannot read it back. It
is never a user change. Core emits it only when another key already causes an
Update, so a difference in write-only values alone still plans no Update.

`UpdateRequest` cannot be built without the managed resource schema. An update
whose resource has no registered managed schema fails provider preparation
before the provider is called. The host records `reassert` keys as sent in the
accepted-request provenance, and state writeback treats them like `replace`.

Nested `writeOnlyProperties` are not represented yet; see carina#3984.

Hosts and providers must both use protocol v11. The minimum supported protocol
version is 11.

## Version 10

Protocol v10's SDK and native core adapters make create recovery an explicit
typestate protocol. Initial create, poll, and resume calls receive different,
opaque attempt handles. Once a provider calls `issued`, that handle has been
consumed and the remaining API cannot report that nothing reached the cloud.
There is deliberately no `From<ProviderError>` into an SDK or core create
verdict, so provider code must classify every failure as untracked, created,
discarded, undetermined, or still pending. The protocol crate itself contains
only plain wire records and enums; it does not claim those typestate guarantees.

Create steps carry a redacted `PendingToken`, an optional non-empty provider
identifier, and `Sent`, which names the top-level attributes actually sent in
the accepted request. `Sent::Only` must be a subset of the checked create
request. A provider can canonicalize an issued identifier only while finishing
the create. Polling and resumption cannot drop an issued identifier or change
the accepted request's `Sent` set.

The driver keeps provider verdicts separate from call failures and returns the
last pending record on poll failure or budget exhaustion. WIT adds
`poll-create` and `resume-create` steps and verdicts. A guest-side request or
token decode failure is `undetermined`, so upgrading a provider cannot erase a
continuation it no longer understands. Guest-side encoding failures are
classified by call phase and by what the provider established: an initial
unissued pending token is `untracked`; an initial issued pending token is
`created` with a failed read; a pending-token failure during either an issued
or unissued poll is `undetermined`, preserving the previous continuation; and a
pending-token failure during resume is likewise `undetermined`, preserving the
starting envelope. If the attributes of a successful or partially successful
result cannot be encoded, the result is `created` with a failed read because
the provider already reported its identifier. The SDK's JSON-RPC/native-binary
transport has been removed; v10 providers use the component-model export.

State format 15 stores managed current instances as one tagged value:
`none`, `applied`, `create_in_flight`, or `missing_identifier`. An in-flight
instance contains its opaque envelope and the UTC `started_at` timestamp, and
cannot simultaneously contain applied attributes or an applied identifier.
State format 14 is migrated through an explicit DTO. An unfinished create is
planned as token-free `ResumeCreate`; the token is joined from locked state
immediately before dispatch. Only a terminal provider verdict may clear that
row.

Saved-plan format 21 records the address and in-flight marker, never the
provider token. Apply reloads the envelope from locked state and rejects the
saved plan if any address changed in-flight status in either direction.

Saved-plan format 23 binds every managed-resource and data-source type
referenced by the plan to the provider schema used during planning. Each entry
is either `Present`, with the schema's canonical semantic fingerprint (encoded
as `{"present":"..."}`), or `Absent` (encoded as `"absent"`) when a
state-only type is no longer exposed by the provider. Apply captures the plan's
complete type inventory before locked-state reload consumes its current-state
snapshots, then compares the saved entries with the schemas exposed by the
current provider binary. A changed presence or fingerprint rejects the saved
plan before normalizer construction, secret evaluation, or CRUD-capable
provider construction.

Saved-plan format 24 removes `target_schema_kind` from serialized
`Effect::Wait` values. Wait targets are always managed resources, so their
schema kind is always `Resource`; data sources are rejected as wait targets
(carina#3916).

Saved-plan format 25 persists exactly one typed phase-2 outcome for every
configured data source: either its read state, its apply-time deferral reasons,
or a structured-plan-error blocker. Apply validates that this outcome set
matches the saved plan's data-source set exactly and restores it directly; it
does not reclassify reads from a partial dependency view. Read state lives only
in this outcome field; `current_states` contains managed resources only, so
secret verification cannot observe a second, divergent copy (carina#3993).

Saved-plan format 26 records the artifact loaded for every provider kind.
Built-in providers are recorded as `built_in`; WASM providers record the SHA256
of the exact bytes handed to the loader and, for lock-installed artifacts, the
lock-resolved source and resolved version, revision SHA, or registry revision
and version.
`file://` artifacts record only their SHA256. This identity is minted only from
the resolver's verified load. Apply compares the saved identities with the
artifacts actually loaded before normalizer construction, secret evaluation,
or provider effects, and providers execute from that same single load. The
user-authored version constraint and download coordinates (release tag, asset
id, and artifact id) are not part of the identity because the SHA256 already
pins the exact bytes. Apply fails with re-plan guidance on any difference, and
older saved-plan formats are rejected (carina#4045).

Hosts and providers must both use protocol v10. The minimum supported protocol
version is 10.

## Version 9

Protocol v9 separates successful create and update observations from generic
read state. Create outcomes now carry `applied-state`, and update outcomes carry
`applied-persisted-state`. Both records require a non-empty `identifier` and do
not have an `exists` flag. Managed `read` also requires a non-empty identifier
input and returns `read-outcome`: `not-found`, or `found(applied-state)`. Thus an
existing read observation cannot cross either the process or WIT boundary
without a provider identifier. Generic `state`/`persisted-state` remain in the
data-source, normalization, and hydration APIs; the WASM host applies only
normalizer-returned attributes and preserves host-owned existence and
identifier metadata.

The JSON-RPC/process representations retain a resource `id` so read and
mutation outcomes remain self-describing. WIT omits that field: the guest SDK
drops the protocol-side `id` while encoding each outcome, and the WASM host
reconstructs it from the request resource ID while decoding the result. Both
the protocol `AppliedIdentifier` decoder and the host WIT decoder reject an
empty string.

This change prevents new invalid mutation outcomes; it does not infer or repair
provider identifiers in legacy managed rows. Once configuration is parsed, the
state load boundary removes pre-#3181 data-source artifacts only when their
complete address matches a typed `read` declaration and the historical row has
no provider identifier. Address alone never removes an identifier-bearing
managed orphan, and identifier absence alone never classifies a row as a data
source. An identifier-less managed row that
reaches ordinary resource planning or destroy is retained as an explicit
invalid condition instead of being treated as absent. A matching `removed`
block is the explicit exception: it abandons the broken row without a provider
mutation. Recover a current instance by applying that block, then adding an
`import` block with the real provider identifier to restore management. Import
IDs are validated as non-empty during planning. If the provider's subsequent
import read cannot produce a non-empty identifier, that read fails at the
provider-response boundary and no row is written; it is not reported later as
corruption of the saved state.

The pre-#3181 data-source cleanup requires both the identifier-less legacy row
shape and a matching typed `read` declaration. If that declaration has already
been removed, its legacy row is indistinguishable from a managed orphan, so
planning hard-fails on the missing identifier. Add and apply a matching
`removed` block to discard that legacy row.

A deposed generation with an empty identifier cannot be imported. Applying a
matching `removed` block explicitly abandons that generation from state, and
the plan displays its deposed key under the resource's Remove effect. If the
row also has a healthy current instance, the diagnostic additionally directs
the operator to import that current identifier after removal so it remains
managed.

The only legitimate persisted managed row without a current identifier is an
empty deposed-only shell: it has no current attributes, binding, or other
current-instance metadata, but retains historical generations awaiting delete.
Data-source results are transient and are not written to managed-resource rows.

Saved-plan format 20 explicitly tags each current-state entry as managed or
data-source state and tags every managed row as absent, present, or missing its
identifier. Present rows are reclassified from their serialized state during
decode, so the stored tag cannot bypass provider-identifier validation. The
format also records validated mutation identifiers directly and records the key
(but not the known-invalid empty identifier) of every malformed deposed
generation explicitly abandoned by a `removed` block. Format 19 and older plans
are rejected with re-plan guidance.

Protocol v9 required a v9 host and provider. It is no longer accepted by a v10
host.
