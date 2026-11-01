# Typed KMS key references (carina-provider-awscc#541, option (a))

## Problem

`awscc.ecr.Repository.encryption_configuration.kms_key = 'alias/aws/ecr'`
causes perpetual replacement: ECR returns a key ARN for a create-only input.
Today's `KmsKeyId` validates syntax but cannot resolve aliases for comparison.

The maintainer selected option (a): plan-time `kms:DescribeKey` resolves
aliases to key ARNs for comparison through typed `KmsKeyReference` leaves.
Payloads retain authored values; state retains provider results. Credentials follow
[the host-credentials design](./2026-10-31-4164-host-aws-credentials-design.md).

## Decisions

### D1. Make reference resolution a required schema capability

Add a generic reference variant to core's schema and the protocol schema
representation: one `TypeIdentity` selecting the guest resolver, plus the
property's existing string constraints (pattern/length). Its wire value is
a string. In `carina-aws-types`, `KmsKeyReference` uses identity
`aws.kms.Key.Reference`; CFN/Smithy constraints restrict accepted forms.

Replace reference-input uses of the misleading four-form `KmsKeyId`.
ARN-only reference inputs also use the variant. `KmsKeyArn` remains a
key-only output type, with compatible outputs assignable to reference inputs.
Validation stays offline, including LSP and `validate`.

### D2. Classify from schema evidence and reject coverage gaps

Replace awscc's KMS string overrides with a classifier. A string property
is KMS-looking when its name matches `/kms|cmk/i` or its pattern/description
mentions a KMS ARN. Each candidate must appear in one explicit table,
keyed by resource and full property path through `$ref`, arrays and shared
definitions, as `Reference` or `NotReference(reason)`. Fail generation on
unclassified candidates. Apply the same coverage check to final schemas,
aws Smithy codegen and handwritten S3 encryption structs.

Initial reference-input inventory: 12 awscc and 3 aws paths, relative to
the resource.

| Provider/resource | Input path(s) |
|---|---|
| awscc ecr.Repository | `encryption_configuration.kms_key` |
| awscc dynamodb.Table | `sse_specification.kms_master_key_id` |
| awscc ecs.Cluster | `configuration.execute_command_configuration.kms_key_id` |
| awscc ecs.Cluster | `configuration.managed_storage_configuration.kms_key_id`, `configuration.managed_storage_configuration.fargate_ephemeral_storage_kms_key_id` |
| awscc logs.LogGroup | `kms_key_id` |
| awscc s3.Bucket | `bucket_encryption.server_side_encryption_configuration[].server_side_encryption_by_default.kms_master_key_id` |
| awscc s3.Bucket | `replication_configuration.rules[].destination.encryption_configuration.replica_kms_key_id` |
| awscc s3.Bucket | `metadata_configuration.inventory_table_configuration.encryption_configuration.kms_key_arn`, `metadata_configuration.journal_table_configuration.encryption_configuration.kms_key_arn` |
| awscc sns.Topic | `kms_master_key_id` |
| awscc scheduler.Schedule | `kms_key_arn` |
| aws logs.LogGroup | `kms_key_id` |
| aws sqs.Queue | `kms_master_key_id` |
| aws s3.BucketServerSideEncryptionConfiguration | `rules[].apply_server_side_encryption_by_default.kms_master_key_id` |

Codegen PRs regenerate both schemas and docs with `generate-schemas.sh`
and `generate-docs.sh`.

### D3. Resolve in the initialized guest

Both guests use one shared resolver in `carina-aws-types`. Parse key ARNs
without a call. Every other form (key ID, alias name, alias ARN) uses
`DescribeKey(KeyId = raw value)`. The response must contain a valid key
ARN. Core and host gain no AWS SDK.

Region and account come from the routed provider instance, never from
resource attributes: the instance's configured region and its effective
credentials (after `assume_role`) are the scope the resource's own API
calls use, so a bare key ID or `alias/name` means what the service means.
The one exception is a qualified form. An alias ARN or key ARN names its
own region and account; the resolver calls KMS in that ARN's region with
the instance's credentials and never substitutes the instance's account
or region into it. Whether KMS allows that cross-account/cross-region
call is decided by the key policy, and a denial is D6's `AccessDenied`.
A partition the instance cannot reach is `InvalidReference`, not a guess.

### D4. Keep raw values; compare typed references

Extend the [comparison projection](../../carina-core/src/resource/mod.rs)
with exactly these rules:

- Both sides concrete and byte-equal: equal, no lookup.
- Otherwise canonicalize each side: key ARN as-is; any other form through
  `DescribeKey` to a key ARN. Compare ARNs.
- Unknown/deferred on either side: existing unknown handling, no lookup.

Lookups happen only for pairs the differ actually compares, which can include
saved-side pairing of lists.

SQS, SNS and S3 retain echoed aliases; KMS resolves them on every encrypt
call, so the resource follows the alias. Identical raw aliases compare
equal. ECR and DynamoDB return key ARNs fixed at write time; resolving the
desired alias detects a repoint. This needs no persisted comparison
baseline or state format change.

Apply the same rules to saved/write-only fallbacks, preserving omission and
ownership rules. Comparison ARNs never enter payloads, bindings/exports or
state attributes.

### D5. Record comparison demands; preserve deferred values

After refresh, hydration, reference evaluation and normalization, run the real
planning computation in a recording session. This includes binding-stability
passes, cascade planning and both detail-row rendering modes. When a concrete,
non-byte-equal reference pair lacks evidence, the comparator records its operands
with their desired/current/saved origins and provisionally returns unequal.
There is no separate walker enumerating pairs.

Discard a pass that records anything, resolve its new values in one batch per
initialized provider instance, and rerun. Provider batches run concurrently and
validate their positional result lengths. Cache by `(provider instance, reference
type identity, raw value)` for this planning invocation. Each rerun strictly grows
the witness; exceeding 64 recording passes is a typed error.

The recording session returns only an opaque provisional result. Only a pass
that records nothing releases its result and a `ResolvedReferences` witness.
Provisional plans, rendering results and errors cannot escape that boundary.
Secret-reference validation and the typed traversal-depth check finish before
any guest call.

Unknown/deferred leaves retain existing dependency and scheduling behavior.
Apply-time augmentation runs the same recording loop with a fresh cache for
keys that become concrete or change after planning. Unchanged planned leaves
keep their recorded decision.

### D6. Resolution failures are typed; only a dangling current value is a diff

The resolver's per-request outcome is a closed enum, and the comparison
side decides what each case means:

```rust
enum ReferenceOutcome { Resolved(KmsKeyArn), NotFound(ProviderError) }
enum ReferenceError {
    AccessDenied(ProviderError),   // AccessDeniedException, key policy denial
    Transient(ProviderError),      // throttling after retries, transport, 5xx, credentials
    InvalidReference(ProviderError), // malformed value, unreachable partition, non-ARN success
}
```

| Case | Desired side | Current (read/state) side |
|---|---|---|
| `Resolved` | compare ARNs | compare ARNs |
| `NotFound` (alias or key deleted) | plan error: apply would send a reference KMS rejects | `CurrentSide::Dangling`: unequal to every resolved desired ARN, so it is a diff |
| `AccessDenied` / `Transient` / `InvalidReference` | plan error | plan error |

`NotFound` on the current side means the resource points at nothing; an
error there would block the very update that repairs it, so it becomes a
diff whose plan detail says the current reference no longer resolves.
Every other failure aborts planning (exit 1): it says nothing about
equality, and treating it as a difference would silently plan an update
or replacement, and treating it as equality would silently hide one.
Denials and transient failures therefore never reach the differ.

The types enforce this. `ResolvedReferences` stores
`DesiredSide::Arn(KmsKeyArn)` and `CurrentSide::{Arn(KmsKeyArn), Dangling}`;
the prepare step is the only constructor and returns `ReferenceError`
(or `NotFound` for a desired value) as a typed plan error with its
`#[source]` chain. The differ has no error case to fall back from, so a
raw string comparison cannot be reached. Report provider instance,
resource/path, reference and AWS code/request ID; no executable saved plan
on failure.

Resolve `alias/aws/<svc>` with plain `DescribeKey`, including first-use
AWS-managed key initialization. No alias precheck or special success on
NotFound. Document that this call can initialize a managed key during planning.

Add a `kms:DescribeKey` planning-permissions section to the
[AWS credentials guide](../../site/src/content/guides/aws-credentials.md),
linked from the plan CLI and both providers' encryption docs. Cover
key-policy/cross-account authorization whenever a lookup is required;
this permission is independent of advisory `--check-iam`.

### D7. Use the existing saved-plan contract

Normal `apply` re-plans with fresh resolution. Saved plans carry effects
with raw payloads: a KMS target moving between plan and apply is ordinary
cloud-side drift, like any other attribute, under the existing saved-plan
contract. Apply-time augmentation follows D5, including fresh preparation
for newly concrete or changed leaves. No saved-plan format change is needed.

## Protocol

Add `resolve-references` to the initialized guest's `provider` export, using
the structured `type-identity` already used by custom-type validation:

```wit
record reference-request {
    ty: type-identity,
    value: string,
}
variant reference-outcome { resolved(string), not-found(provider-error) }
variant reference-error {
    access-denied(provider-error),
    transient(provider-error),
    invalid-reference(provider-error),
}
resolve-references: func(requests: list<reference-request>)
    -> result<list<result<reference-outcome, reference-error>>, provider-error>;
```

Results are positional; the host retains resource/path fan-out and never
sends unknown/deferred leaves. Core rejects length mismatches and empty
`resolved` values (as `invalid-reference`). The shared guest resolver
validates the canonical key ARN shape through its `KmsKeyArn` output type;
core remains provider-agnostic. The outer
error covers whole-call failure and aborts planning. The variants map
one-to-one onto D6's Rust enums; the guest classifies AWS error codes, so
the host never parses error strings. The SDK default fails every request
with `invalid-reference` naming the unsupported type identity.

Use main's protocol version plus one at merge, advancing
`MIN_SUPPORTED_PROTOCOL_VERSION` to match, with no shim. Do not reserve a
version.

## Migration

1. **carina:** WIT submodule, protocol schema/version, SDK method/bindings,
   host dispatch, core reference types and prepare/witness, CLI wiring and
   docs.
2. **carina-provider-aws:** shared resolver and types in `carina-aws-types`,
   Smithy/handwritten coverage check, regeneration and guest export wiring.
3. **carina-provider-awscc:** classifier, schema/doc regeneration and shared
   resolver wiring. Coordinate provider publication with the core bump.

No state migration. Publish the type changes and planning-permission
requirement together.

## Testing

- **Resolver:** key ARNs make no call; IDs and aliases use DescribeKey.
  Cover UUID/multi-region IDs, alias-ARN regions, assumed credentials,
  invalid ARN responses, failures and managed-alias initialization.
- **Schema/codegen:** all 15 paths, nested/shared definitions, handwritten
  S3 types, existing constraints, output assignability and offline/LSP
  validation. Unclassified KMS-looking fields must fail generation.
- **Core:** all three D4 rules, alias/ID/ARN equivalence, account/region
  distinctions, ARN-returning repoints, echoed-alias equality, saved/write-only
  fallbacks and unchanged raw values. Require the witness for differ and
  binding-stability callers; missing needed entries are typed errors.
  Every D6 table cell: desired `NotFound` and every `ReferenceError` abort
  planning with the source chain; current `NotFound` becomes a diff.
  Instance region/account scope for bare forms; ARN forms keep their own
  region/account and are never rewritten to the instance's.
- **WASM/host:** positional batches, length mismatch, empty success,
  unsupported defaults, error/timeout transport and cache isolation by
  instance/type/value. Unknowns and schema-only loading make no KMS calls;
  schema-only loading makes no credential requests.
- **CLI:** clean alias-to-ARN re-plan, ECR replacement/DynamoDB update on
  repoint, clean echoed aliases, deferred effects without re-diffing, raw
  saved-plan payloads, denied resolution (exit 1, no executable saved plan).

Run crate-scoped `cargo nextest run -p ...` suites first: core, SDK/host,
CLI, LSP and provider types/codegen. Use a workspace run for final
cross-crate verification.

### Real-AWS acceptance checks

Run only when explicitly authorized, in the `carina-test-000`–`carina-test-009`
pool accounts. Obtain credentials with
`aws configure export-credentials --profile carina-test-00X`.

1. Apply awscc ECR and DynamoDB resources using an alias pointing to key A;
   require a clean re-plan against returned ARNs. Cover first use of
   `alias/aws/ecr`.
2. Repoint to B: expect ECR replacement and DynamoDB update; apply and
   require a clean re-plan.
3. With aws SQS, require a clean re-plan for the echoed alias, including
   after repointing. Change the DSL alias to its current target's key ARN;
   expect equality. Verify payload/state retain raw values.
4. Deny DescribeKey for an alias and require exit 1. Clean up resources and
   aliases; schedule customer-managed key deletion, never AWS-managed keys.

## Out of scope

Generic non-KMS resolution; payload rewriting; changing alias/requester
semantics; key-policy authoring or encrypt/decrypt permission preflight;
re-encryption; locking external alias updates; unrelated apply/destroy
repairs. This design does not authorize cloud operations.
