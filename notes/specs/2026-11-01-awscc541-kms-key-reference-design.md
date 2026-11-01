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
`DescribeKey(KeyId = raw value)` with the instance's effective credentials
(including `assume_role`) and region; alias ARNs use their own region.
The response must contain a valid key ARN. Core and host gain no AWS SDK.

### D4. Keep raw values; compare typed references

Extend the [comparison projection](../../carina-core/src/resource/mod.rs)
with exactly these rules:

- Both sides concrete and byte-equal: equal, no lookup.
- Otherwise canonicalize each side: key ARN as-is; any other form through
  `DescribeKey` to a key ARN. Compare ARNs.
- Unknown/deferred on either side: existing unknown handling, no lookup.

SQS, SNS and S3 retain echoed aliases; KMS resolves them on every encrypt
call, so the resource follows the alias. Identical raw aliases compare
equal. ECR and DynamoDB return key ARNs fixed at write time; resolving the
desired alias detects a repoint. This needs no persisted comparison
baseline or state format change.

Apply the same rules to saved/write-only fallbacks, preserving omission and
ownership rules. Comparison ARNs never enter payloads, bindings/exports or
state attributes.

### D5. Prepare references before the differ; preserve deferred values

Core provides one async prepare step before the differ. The CLI invokes it
after refresh, hydration, reference evaluation and normalization at the
plan-input seam in [wiring](../../carina-cli/src/wiring/mod.rs). Resolve only
leaves needing a lookup under D4. Batch by initialized provider instance;
cache per run by `(provider instance, reference type identity, raw value)`.

Preparation returns a `ResolvedReferences` witness required by the
differ/plan entry point. Every reference comparison caller, including
binding-stability comparisons, uses this witness. Missing preparation for
a needed pair is a typed error; no caller can compare a reference leaf as
a plain string.

Unknown/deferred leaves are never resolved and retain existing dependency
and scheduling behavior. Apply executes effects; it does not re-diff
materialized leaves or prepare them for comparison.

### D6. Resolution failures are hard errors, including managed aliases

Access denied, not found, exhausted retries, transport/credential failures
and invalid results abort planning (exit 1). Report provider instance,
resource/path, reference and AWS diagnostics. No raw equality fallback or
executable saved plan on failure.

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
contract. No saved-plan format change is needed.

## Protocol

Add `resolve-references` to the initialized guest's `provider` export, using
the structured `type-identity` already used by custom-type validation:

```wit
record reference-request {
    ty: type-identity,
    value: string,
}
resolve-references: func(requests: list<reference-request>)
    -> result<list<result<string, provider-error>>, provider-error>;
```

Results are positional; the host retains resource/path fan-out and never
sends unknown/deferred leaves. Reject length mismatches and successful
values that are not valid key ARNs. The outer error covers whole-call
failure; inner errors retain per-request provider diagnostics. The SDK
default fails every request with an unsupported error.

Use main's protocol version plus one at merge, advancing
`MIN_SUPPORTED_PROTOCOL_VERSION` to match, with no shim. Main is 15 now;
#4162 plans 16. Do not reserve a version.

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
- **WASM/host:** positional batches, length mismatch, non-ARN success,
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
