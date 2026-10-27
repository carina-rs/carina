# Re-sending Write-Only Attributes on Update: Design

Tracking: carina-rs/carina#3981

## Problem

An Update patch carries only the keys in `changed_attributes`. A write-only
attribute that has not changed never reaches the provider. The provider
contract forbids the provider from adding it back: `UpdatePatch` says
providers "MUST NOT modify any attribute that is not represented in `ops`", and
`UpdateRequest::from` "MUST NOT be used to derive additional fields to write
back" (#2559).

For CloudControl this is wrong. CloudControl applies the JSON Patch on top of
its read model, and that model never contains write-only properties. A
write-only property that is missing from the patch is therefore missing from
the model the handler receives. CloudFormation does not have this problem,
because it sends the whole desired model on every update.

The case that exposed this is `AWS::CloudFront::Function`
(carina-rs/carina-provider-awscc#474, needed by carina-rs/infra#279). We
measured it on real AWS (carina-test-000, us-east-1, 2026-10-27):

| Update patch | LIVE stage after update | `GetResource` |
|---|---|---|
| `FunctionCode` + `AutoPublish=true` | new code | new code, `Stage: LIVE` |
| `FunctionCode` only | old code (new code is only in DEVELOPMENT) | new code, `Stage: LIVE` |

In the second row the read-back looks converged. The next plan shows no diff,
but production still serves the old code.

The same shape applies to any updatable write-only property whose absence in
an update means "unset" to the handler. In the awscc schema cache, these are
CloudFront Function `AutoPublish` and the updatable members of the
`writeOnlyProperties` lists of KMS Key, S3 Bucket, SSM Parameter, Subnet, VPC,
and others. carina-provider-aws declares no write-only attributes today.

## Prior art

terraform-provider-awscc (`internal/generic/resource.go`, `Update`) clears
write-only values from the prior state before it computes the JSON Patch. Its
comment says: "This forces patch document generation to always add values".
As a result, every update re-sends the write-only values. Terraform can leave
this to the provider because the planner hands the provider the full planned
state. Carina keeps the diff in core (#2559), so the same behaviour belongs in
the core patch builder.

## Decision

Core re-sends desired, updatable write-only attributes as a separate, typed
patch operation, `Reassert`. A provider can tell it apart from a user change,
and the provider contract keeps its current rule: a provider touches only what
the patch names.

### Which keys are reasserted

`Reassert` is emitted for a key only when all of the following hold:

1. **An Update is already happening.** Some other key is changed. Reassertion
   never creates an Update by itself. Plans keep converging through the
   accepted-request provenance from #3923. The differ, `key_should_enter_patch`,
   and the plan's `changed_attributes` do not change.
2. **The key is in the desired resource.** A key the user never specified is
   never sent, so the #2559 invariant still holds.
3. **The schema marks the key `write_only`, and not `create_only`.** A
   create-only key cannot be patched. It stays excluded, as
   `key_should_enter_patch` already excludes it.
4. **The key is not already in the patch.** If the user changed or removed the
   write-only key itself, it stays `Add`, `Replace`, or `Remove`.

The reassert set is computed when the request is built, in
`UpdateRequest::checked`. It is not computed in the differ. This keeps
`Reassert` out of the `Effect`, plan display, saved plans, and the plan image.

### Making the patch shape carry its own invariants

Today a patch op is `{ kind, key, value: Option<Value> }`. The `value` is
supposed to be present for `add` and `replace` and absent for `remove`, but
nothing checks that. Every consumer handles the mismatched cases by convention.
Two examples:

- `PersistedState::apply_patch` treats `Add` with `None` as a removal.
- `build_update_patch` maps the `(false, false)` case to `Remove`.

Adding `Reassert` to this shape would add one more kind with a "must carry a
value" convention. So the op becomes a sum type in which only the operations
that send a value can carry one:

```rust
// carina-core::provider, mirrored in carina-provider-protocol
pub enum PatchOp {
    /// The user added a key that did not previously appear in the desired state.
    Add { key: String, value: Value },
    /// The user changed the value of an existing key.
    Replace { key: String, value: Value },
    /// Unchanged; re-sent because it is write-only and the backend
    /// cannot read it back.
    Reassert { key: String, value: Value },
    /// The user removed a key.
    Remove { key: String },
}

impl PatchOp {
    pub fn key(&self) -> &str;
    /// `Some` for Add / Replace / Reassert.
    pub fn sent_value(&self) -> Option<&Value>;
}
```

```wit
record patch-value {
    key: string,
    value: value,
}

variant patch-op {
    add(patch-value),
    replace(patch-value),
    reassert(patch-value),
    remove(string),
}
```

The old `patch-op-kind` enum and the `patch-op` record are removed. The
protocol type (serde) uses `#[serde(tag = "kind", rename_all = "snake_case")]`.
Each WIT and protocol conversion is an exhaustive `match` with no `_` arm, the
same as the schema conversions (#3772).

### Building the request

`UpdateRequest::checked` gains the resource schema:

```rust
pub(crate) fn checked(
    from: State,
    changed_attributes: &[String],
    resource: &ProviderReadyResource,
    schema: &ResourceSchema,
) -> Self
```

`checked` is the only constructor of `UpdateRequest`, so no update request can
skip the reassert step. Both call sites already have the schema:

- the executor's update arm (`executor/basic.rs`), which already looks up
  `schema` for the `effective_changed` re-check;
- `prepare_update_request` (`executor/provider_ready.rs`), through
  `context.schemas`.

The parameter is `&ResourceSchema`, not `Option`. An update of a managed
resource without a schema cannot classify its write-only keys, so the caller
reports it as a `ProviderPreparationError` instead of silently sending no
reasserts. Test fixtures that build updates without a schema get a schema.

The `(false, false)` case in `build_update_patch` (a key in neither `to` nor
`from`) currently becomes a `Remove` op. It is dropped instead. There is
nothing to remove, and the new type would otherwise force us to invent a value.

### Provenance and writeback

`AcceptedRequest::from_update_request` records `Reassert` keys in `sent`, the
same as `Add` and `Replace`. The value was sent and accepted, so #3923
provenance stays exact. Writeback keeps the desired value, masked against
secrets as today. `PersistedState::apply_patch` and the providers'
post-update state builders treat `Reassert` like `Replace`.

Write-only values are often secrets. A reasserted secret is unwrapped at the
provider boundary exactly like a replaced one. It is not logged, and state
stores it in the same hashed form. This adds no new path for a secret to leak.
It does mean the secret crosses the boundary on every update of that resource,
which matches CloudFormation and terraform-provider-awscc.

### Protocol

`PROTOCOL_VERSION` and `MIN_SUPPORTED_PROTOCOL_VERSION` both become 11. The
WIT `patch-op` changes shape, so a v10 component cannot work with a v11 host.
There is no compatibility window to keep.

### Providers

- **carina-provider-awscc:** `build_update_patches` maps `Reassert` to the same
  CloudControl `add` op as `Replace`. Reasserted keys are not added to
  `require_confirmation`, because they are write-only and cannot be read back.
  The tag-patch path handles the new variant exhaustively.
- **carina-provider-aws:** this provider has no write-only attributes, so core
  never sends it `Reassert` today. Its patch application and conversions match
  the new variant exhaustively and apply it like `Replace`, so a future
  write-only attribute works without a silent drop.

## Rollout

1. carina-rs/carina-plugin-wit: the `patch-op` reshape.
2. carina-rs/carina: core, protocol, host, SDK, and mock provider. This
   includes the WIT submodule bump and protocol v11.
3. Rev bump in carina-provider-aws and carina-provider-awscc, with the
   provider-side changes above. The bump is based on the carina revision that
   follows #3980.
4. carina-provider-awscc#474: `awscc.cloudfront.Function`. Its acceptance test
   changes `function_code` and verifies that the LIVE stage serves the new
   code.

## Tests

- **Core unit tests for `UpdateRequest::checked`:**
  - An update caused by a non-write-only key carries `Reassert` for each
    desired, updatable write-only key.
  - It carries no `Reassert` for create-only write-only keys or for unspecified
    keys.
  - A write-only key that the user changed stays `Replace`, not `Reassert`.
  - A removed write-only key stays `Remove`.
- **Property test:** for random desired/from/schema triples, every key in the
  patch is either in `changed_attributes` or satisfies conditions 2–4, and no
  key appears twice.
- **Executor test:** a resource whose only difference is a persisted write-only
  value plans no Update. This is the #3923 convergence, pinned again.
- **Provenance test:** `Reassert` keys end up in `AcceptedRequestKind::Update.sent`.
- **Round-trip tests:** host↔WIT and SDK↔WIT round-trip all four variants.
  Protocol serde round-trips them as well.
- **awscc:**
  - `build_update_patches` emits an `add` op for `Reassert`.
  - The CloudFront Function acceptance test changes `function_code` and reads
    the LIVE stage. Reading through CloudControl `GetResource` is not enough,
    because it reports the new code even when LIVE is stale.
