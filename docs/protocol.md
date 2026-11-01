# Provider protocol versions

The current protocol and minimum supported provider protocol are both **19**.

| Protocol version | Minimum provider protocol | Change |
|------------------|---------------------------|--------|
| 19 | 19 | Per-leaf desired normalization with scoped, exactly-once deferred handles. |
| 18 | 18 | Typed reference schema leaves and batched plan-time reference resolution. |
| 17 | 17 | Required list element policy; portable canonical-value uniqueness validation. |
| 16 | 16 | Required identity mode field; declared identity hashing and nested create-only diffing. |
| 15 | 15 | Create-progress variants, poll finality, and separate read-back budgets. |
| 14 | 14 | Host-resolved AWS credentials through the typed AWS world. |

## Version 19

Protocol v19 replaces top-level deferred-name metadata with structural desired
value trees. `PROTOCOL_VERSION` and `MIN_SUPPORTED_PROTOCOL_VERSION` are both
19; providers built for earlier protocols must be rebuilt.

`normalize-desired` receives and returns `list<desired-resource>`. Each record
has an `id` and `attributes: list<tuple<string, desired-value>>`. The separate
`desired-value` WIT variant contains scalars, JSON-backed lists/maps, and
`deferred-val(u32)`. Ordinary `value` and `persisted-value` have no handle arm.
Concrete containers stay visible even when they contain opaque leaves, so a
normalizer can canonicalize known SANs and remove a known apex, add defaults
inside known replication rules, or trim a known alias DNS name beside a secret
or reference.

The host replaces every deferred core leaf with a per-call slot number. This
includes secrets, resource references, unknowns and other unresolved
expressions. A secret wrapping a complete container remains one opaque leaf;
its inner values are never exposed. Providers cannot inspect or fabricate
leaves through the SDK view. Every issued slot must occur **exactly once** in
the result, in its original resource and top-level attribute. Reordering within
that attribute is allowed; dropping, duplicating, inventing or moving a handle
across resources or attributes is an error. Resources correspond by list
position, including pending or duplicate ids. The returned count must match, and
each returned id must equal the id sent at that position.

Within normalization `list-val`/`map-val` JSON, a handle is the singleton object
`{"$carina_deferred": 0}` with an unsigned 32-bit slot. User object keys beginning
with `$` gain one leading `$`: a literal `$carina_deferred` key is transported as
`$$carina_deferred`. The decoder unescapes user keys once and does not reinterpret
the resulting literal map as a handle. Duplicate object/attribute keys and
malformed handle objects are rejected rather than silently collapsed.

This escaping and handle interpretation apply **only** to desired normalization
input/output. CRUD, configuration and persisted-state payloads/decoders remain
unchanged. Create/update still receive actual secret values: top-level secrets
use ordinary `secret-val`, and nested secrets use their operational JSON values.
The ordinary protocol value type cannot contain a normalization handle.

The host validates and decodes the entire result before writing any changes.
It restores original deferred values at their returned positions and preserves
resource identity and host metadata. Unchanged known leaves keep their original
core scalar metadata at the same path, reserving those originals before any
other matches. A moved leaf may reuse an unclaimed equal wire projection within
its top-level attribute only when the paths have the same keys in the same
positions; list index values may differ. Each original is consumed at most once.
Thus list reordering preserves nested enum kinds and sensitivity without moving
metadata between different fields. Renamed or edited known fields become plain
values unless they match an unchanged original at the same path.
Missing, duplicate, unknown, foreign or malformed handles, duplicate keys,
resource-id mismatches and resource-count mismatches produce typed
errors with resource/attribute/path context. Conversion errors preserve their
typed source chains; secret payloads are never included in diagnostics.

## Version 18

Protocol v18 adds string-valued reference schema leaves and the initialized
provider's `resolve-references` export. Requests carry a structured type
identity and raw string; positional outcomes are a resolved canonical string,
NotFound, or a typed AccessDenied, Transient, or InvalidReference failure.
Core rejects batch length mismatches and empty resolved values for every
provider, native or WASM. Guests
validate their canonical value shapes. Reference validation remains offline;
resolution runs only when preparing a planning comparison.

The v18 development snapshot set both protocol constants to 18 and rejected
older providers before schema or WIT decoding. Providers had to use the
SDK-owned bindings for the updated WIT world.

## Version 17

Protocol v17 adds a required `elements` field to every list schema JSON record.
Its closed values are `allow_duplicates` and `unique`; neither decoding nor
encoding may omit the field. The core API exposes the same policy as
`ListElements::{AllowDuplicates, Unique}` and requires callers of
`AttributeType::refined_list(element_type, ordered, length, elements)` to choose
explicitly. The `list` and `unordered_list` constructors allow duplicates.

The shared schema validator checks uniqueness for authored desired values and
provider configuration, including collect-all diagnostics and purpose-aware
value fitting. Comparison uses canonical values, including resolved enum API
values, so bare identifiers, qualified identifiers, and accepted enum aliases
compare equally. Integer versus float spellings and struct block aliases versus
canonical field keys remain distinct because canonicalization preserves them.
Nested unordered lists inside an element compare in order.

Elements containing secrets, environment-derived (diagnostic-sensitive) values,
or unresolved values are excluded from duplicate comparison; known,
nonsensitive elements in the same list are still checked. A secret wrapping the
whole list is also excluded. Read-side values (provider read-back and persisted
state) and wait-condition right-hand sides are not checked for uniqueness.
List length constraints remain enforced in every case. Duplicate errors carry
the value, duplicate index, first index, and attribute context through CLI and
LSP diagnostics. Semantic schema fingerprints use encoding v8 to include this
constraint.

The v17 development snapshot set `PROTOCOL_VERSION` and
`MIN_SUPPORTED_PROTOCOL_VERSION` to 17. A v17 host rejected v16 providers before
decoding schemas and required providers to rebuild against that core/SDK.
The WIT world was unchanged in v17.

## Version 16

The historical v16 development snapshot made declared anonymous identity a
closed schema mode and changed its address semantics. That snapshot set both
protocol constants to 16 and rejected v15 providers before schema decoding.
This was required because the v15 top-level `identity` boolean
meant “add this value to the configured create-only hash inputs” and earlier
providers could set it on mutable attributes. Interpreting that boolean with
v16 semantics would silently change addresses or produce a misleading schema
decode error.

Every top-level `AttributeSchema` and nested `StructField` schema JSON record
now carries a required `identity` value from the closed enum `none`, `required`,
or `when_present`; the field has no serde default. Nested fields also carry the
required `create_only` boolean introduced with nested identity. The host
rebuilds decoded records through the same input-mode and create-only builders
used by native schemas. Both identity modes require create-only scalar or enum
leaves. Identity is rejected on provider-populated values, on a collection or
struct value itself, below lists, maps, or unions, and below a
provider-populated ancestor. `when_present` is additionally valid only for
optional input. These declarations participate in semantic schema fingerprint
encoding v7.

For anonymous resources, one or more declared identity paths select the
identity-only address encoding; other create-only values are excluded. If no
identity path is declared, the host retains the create-only fallback. Nested
paths are represented as typed attribute/struct-field segments in the host and
are never transported as a dotted string. A `required` identity must be
written, even when its schema input is optional. An omitted `when_present`
identity instead contributes a dedicated typed absence value. Once written,
either mode has the same rules: a stable written value or authored expression
is required at plan time. Written values are schema-canonicalized. References
and composite expressions whose leaves are references or literals are hashed
by their deterministic authored spelling, and generated names hash their
authored prefix as a separate typed value. A written `Unknown` or
`Unavailable` value, placeholder, secret, or expression containing such a leaf
is incomplete identity rather than absence; the user can give the resource a
name with `let` when no stable identity material exists.

The declared-identity hash encoding is typed and length-prefixed. It contains
the length-prefixed domain `carina-anonymous-identity-v1`; the sorted
provider-identity entry count and each length-prefixed key and value; then the
identity-path count. Each path has its segment count followed by segments
tagged `0` for an attribute or `1` for a struct field and a length-prefixed
name. Its value then has a kind tag: `0` for `Written`, `1` for
`ReferenceExpression`, `2` for `Prefix`, `3` for `PrefixExpression`, or `4`
for `Absent`. Tags 0 through 3 retain their existing spelling payload; tag 4
has no payload. Counts and lengths are unsigned 64-bit little-endian integers.
The address suffix is the first 32 bits of the SHA-256 digest, interpreted in
big-endian byte order. Consequently absence cannot collide with any written
value, including the empty string.

During anonymous-address reconciliation, an absent desired `when_present`
identity matches only a candidate state row whose corresponding attribute or
field is also absent. It does not match an empty string or any other written
value. This lets migration to the declared-identity address select the same
real-world object without conflating omission with an unresolved value.

Nested create-only leaves in an unordered list are compared as a multiset,
independent of element order and mutable sibling fields. Reordering alone is
not a create-only change. Adding an element with an authored create-only leaf
is a create-only change and therefore replaces the resource.

The WIT world is unchanged in v16; the incompatible representation is in the
schema JSON records. These requirements remain in protocol v19.

## Version 15

Protocol v15 separates an asynchronous mutation from its post-mutation
read-back. Create and resume continuations carry a single `create-progress`
variant: `operating(option<string>)` while the mutation is in progress, or
`reading-back(string)` once the mutation completed and its identifier is
required. `pending-create` and `resume-pending` have no separate identifier or
phase, so read-back without an identifier cannot be represented on the wire.
Update uses `pending-update` with a `pending-phase` of `operation` or
`read-back`; its request always supplies the identifier. These continuations
share the phase-less `pending-operation` record for the token and poll interval.
`poll-create`, `resume-create`, and `poll-update` receive a `poll-request`
whose `finality` is `continue` or `final-attempt`.

The guest SDK requires create and update tokens to implement `PhasedToken`.
Identifier-bearing `IssuedCreate` and `ResumedCreate` handles derive both
`pending` and `preserve` progress from the token's phase. Identifier-less
create handles take `T::Operation`, the associated operation-phase payload;
unissued poll and resume arms carry that payload directly. `from_operation`
wraps it infallibly, while only SDK dispatch uses `into_operation` to narrow a
decoded host token. `PhasedToken::phase()` derives its default from the borrowed
`operation()` view (`Some` for operation, `None` for read-back), avoiding a
separate phase match in provider implementations.
Update continuations use `Pending::new(token, poll_after_secs)` and compute
`Pending::phase()` from the token, without storing a separate mutable phase.
The SDK does not serialize `Pending<T>` itself; guest glue serializes its token
into the protocol's pending record.

Guest decoding keeps the host's `create-progress` variant intact through
create poll and resume dispatch. Phase validation rejects only the backward
pairing: host progress is `reading-back`, but the token is in operation phase.
Create poll and resume return `Undetermined` for that pairing, preserving the
previous host continuation; update decoding rejects the same backward pairing.
Operation progress with a read-back token is accepted when an identifier is
available. The guest receives the identifier-bearing handle, and its next
pending or preserved result reports read-back progress from the token.
An identifier-less read-back token is permanently invalid: dispatch returns
terminal `Untracked` with code `READ_BACK_WITHOUT_IDENTIFIER` before calling
the provider. These SDK guarantees use the existing WIT variants and do not
change the protocol version.

The host starts the operation's existing deadline before the initial call.
The read-back deadline starts only when the first `read-back` pending result
arrives and is the earlier of that time plus the resource's read-back budget
and the operation deadline. The schema may set a typed, non-zero
`operation_config.read_back.timeout_secs`; otherwise the host uses two
minutes. The read-back budget is clipped to the operation budget in effect,
including any resource-specific override; it never extends that budget.
Poll delays remain clamped to 1–300 seconds against the
active deadline.

At the active deadline the host sends exactly one `final-attempt`. The phase of
the continuation that receives that request determines the result of another
pending response. A final attempt sent to an `operation` continuation may
return pending in either phase; the host reports that the operation exceeded
its budget. For create, the host preserves the returned continuation and any
known identifier. For update, it preserves the pre-update state row, which the
next plan refreshes from the cloud. A final attempt sent to a `read-back`
continuation must finish with success, partial success, or an error; pending in
either phase is a protocol violation.
Returning from `read-back` to `operation` at any point is also a protocol
violation. Providers should return partial success when the mutation is known
to have completed but the final read still lacks attributes. Core records the
partial-read marker, publishes returned attributes while keeping only the
reported missing attributes unknown, and retries the read on the next apply or
refresh.

`resume-create` receives the same `poll-request` finality as `poll-create`; the
host never fabricates the `sent` values that are absent from a persisted
`resume-pending`. Persisted state retains the resume token and identifier, but
not scheduling metadata. The current host sends `operating` with the persisted
optional identifier on the first resume call. This placeholder is not evidence
of a phase regression: a saved read-back token with an identifier can resume
directly into read-back, including from a fresh provider instance. A returned
pending or preserved result establishes the phase used by subsequent polls.
Delete continuations use the phase-less `pending-delete` record because delete
has no post-apply read-back phase.

`OperationConfig.read_back` participates in semantic schema fingerprint
encoding v6. Provider protocol and minimum supported protocol are both 15;
v14 providers are rejected because they cannot receive poll finality or
distinguish operation pending from read-back pending.

## Version 14

Protocol v14 adds the `carina-provider-aws` world for AWS-kind providers. It
imports both `wasi:http/outgoing-handler` and the new `aws-credentials`
interface, then exports the same provider interface as the existing worlds.

The `aws-credentials` interface defines an `aws-credentials` record containing
`access-key-id`, `secret-access-key`, an optional `session-token`, and an
optional `expires-at` Unix timestamp in seconds. Its `aws-credential-error`
variant distinguishes `not-found(string)` from `provider-failed(string)`, and
`current-credentials` returns either the current record or one of those typed,
host-rendered errors. The host owns default-chain lookup, caching, and refresh;
credential secrets do not cross the component boundary except through this
typed call.

An AWS-kind runtime load must import `aws-credentials`. GitHub-kind and
untrusted runtime loads must not import it, so changing only the trust kind
cannot give a non-AWS component access to credentials. Schema-only loading
links the interface without resolving credentials, while an attempted call
during schema loading fails with `provider-failed`.

AWS credential environment variables are no longer passed to AWS guests.
Cloud metadata addresses are also withheld from guest networking, preventing
guest access to ECS or EC2 metadata credentials. The host may still use those
environment variables, profiles, SSO, ECS, or EC2 metadata as part of its own
AWS SDK credential chain.

Hosts and providers must both use protocol v14. The minimum supported protocol
version is 14.

## Version 13

Protocol v13 makes initialized reference scope, provider-declared reference
forms, and recoverable create failures explicit transport data.

Version 13 introduced top-level deferred-name metadata for `normalize-desired`.
Version 19 replaces that record shape with structural, exactly-once handles;
the old deferred-name boundary is no longer accepted.

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
to semantic schema fingerprint encoding v5.

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

Saved-plan format 36 adds typed deferred-for templates and stage-specific
placeholder values. Deferred template attributes are serialized only through
their `template_resource`. Older plans are rejected with the same replan
guidance; the state format remains unchanged.

Saved-plan format 37 requires `file` (a relative path or null) and `line` on
each deferred-for template so apply-time failures retain their source location.
Absolute diagnostic paths are not persisted. Format 36 and older plans must
be regenerated; there is no saved-plan backward compatibility.

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
