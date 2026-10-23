# Provider protocol versions

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
complete address matches a typed `read` declaration; identifier absence alone
never classifies a row as a data source. An identifier-less managed row that
reaches ordinary resource planning or destroy is retained as an explicit
invalid condition instead of being treated as absent. A matching `removed`
block is the explicit exception: it abandons the broken row without a provider
mutation. Recover a current instance by applying that block, then adding an
`import` block with the real provider identifier to restore management. Import
IDs are validated as non-empty during planning. If the provider's subsequent
import read cannot produce a non-empty identifier, that read fails at the
provider-response boundary and no row is written; it is not reported later as
corruption of the saved state.

The pre-#3181 data-source cleanup is address-driven and only runs while the
configuration still contains the matching typed `read` declaration. If that
declaration has already been removed, its legacy row is indistinguishable from
a managed orphan, so planning hard-fails on the missing identifier. Add and
apply a matching `removed` block to discard that legacy row.

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

Saved-plan format 19 records validated mutation identifiers directly and also
records the key (but not the known-invalid empty identifier) of every malformed
deposed generation explicitly abandoned by a `removed` block. Format 18 plans
are rejected with re-plan guidance.

Hosts and providers must both use protocol v9; the minimum supported protocol
version is 9.
