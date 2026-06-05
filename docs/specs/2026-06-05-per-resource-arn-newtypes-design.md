# Per-resource `.Arn` newtypes owned by each resource schema

<!-- derived-from ./../../README.md -->

Tracking issues:

- carina-rs/carina#3392 — Provide per-resource `.Arn` newtypes beyond IAM
  (the request).
- carina-rs/carina#3256 — Custom-type DSL namespace uses `aws.` prefix
  regardless of defining provider.

This design rolls #3256's namespace rework into the same change as
#3392's newtype generation, so the new `.Arn` types ship with their
final names from day one rather than landing under `aws.*` and being
renamed later.

## Goal

Make every resource whose schema publishes an `arn` attribute reachable
in `.crn` as a typed value: `<provider>.<service>.<Kind>.Arn` for both
the SDK-based `aws` provider and the CloudControl `awscc` provider.

Concrete end-state, from the issue:

```crn
arguments {
  read_plane_bucket_arn      : awscc.s3.Bucket.Arn
  cloudfront_distribution_arn: awscc.cloudfront.Distribution.Arn
}
```

This unblocks `carina-rs/infra#102`, removes the `String`-typed
workaround it currently uses, and gives the LSP / `validate` real type
information for cross-component contracts. The same generation pass
also produces `aws.iam.Role.Arn` and the handful of other IAM/KMS
newtypes that exist today as hand-written entries — they get
re-emitted from the resource that owns them and the hand-written
copies retire.

## Design principle: the schema that owns the resource owns the type

The crux of this design: a resource's `.Arn` newtype is a property of
the resource's schema. `aws.iam.Role.Arn` is "the ARN attribute of the
IAM Role resource as the aws provider sees it" — the IAM Role schema
is the place that knows the ARN's shape (`arn:aws:iam::<account>:role/
<path>/<name>`), the IAM service prefix, and the IAM-specific
character set restrictions. No other resource's schema, and certainly
no global cross-service helper crate, has any business answering
"what is the validation for an IAM Role ARN?".

This is currently violated. `carina-aws-types/src/lib.rs` holds:

```rust
pub fn iam_role_arn() -> AttributeType { … }
pub fn iam_policy_arn() -> AttributeType { … }
pub fn iam_oidc_provider_arn() -> AttributeType { … }
pub fn kms_key_arn() -> AttributeType { … }
```

These belong inside the IAM Role / IAM Policy / IAM OIDC Provider / KMS
Key schemas, not in a cross-service utility crate. They live there
historically because:

1. They were authored before per-schema helper emission was a thing.
2. Other resources reference them (e.g. `ec2.flow_log`'s
   `deliver_logs_permission_arn` attribute is typed as
   `iam_role_arn()`), and a centralized location made the import
   easier.

Both reasons disappear once each schema's generated file exports its
own `arn()` helper: `ec2.flow_log` then references `iam::role::arn()`,
which is the same shape as today's `iam_role_arn()` but factored at
the right layer.

After this change, `carina-aws-types` contains only **genuinely
cross-service** primitives: the generic `arn()` for `aws.Arn`, the
ARN-string-shape validators (`validate_arn`, `validate_service_arn`,
`validate_iam_arn`, …), region / account_id / availability_zone, and
the enum/struct utilities. The per-resource newtypes live with their
resource.

## Non-goals

- Re-deriving validation rules from Smithy or CFN schemas in full.
  Inspecting the CFN cache shows ARN properties are almost always just
  `{"type":"string"}` with no `pattern` (verified for S3 Bucket,
  CloudFront Distribution, DynamoDB Table, KMS Key, LogGroup, ECS
  Cluster); Smithy's `@pattern` likewise isn't on ARN outputs in the
  Smithy IR we currently consult. Pattern-extraction from schema
  metadata is out of scope, and we lean on a per-kind table in the
  codegen with a service-prefix fallback (see Validation below).
- Newtypes for `Id`, `Name`, or any non-ARN identifier surface. This
  design only covers `arn` attributes; the same shape can be applied
  to other identifier attributes in a follow-up, but that's a
  separate decision.
- Unifying or de-duplicating `carina-aws-types` across the two
  provider repos. The copies are tracked in
  `project_aws_types_triplicated_copies.md`; this design works
  within the current "two synchronized copies" model. After the
  per-schema migration the copies shrink (IAM/KMS specific helpers
  leave), but a real shared crate is a different design problem.

## Chosen approach

Four moves, landed coordinatedly across the two provider repos:

1. **Emit a per-resource ARN helper from codegen, in the resource's
   own generated file.** Both `carina-codegen-aws` (Smithy) and
   `carina-provider-awscc/src/bin/codegen` (CFN) already emit one
   `schemas/generated/<service>/<resource>.rs` per resource. When
   the resource schema declares an `arn` output attribute, the
   generator additionally emits:

   ```rust
   // schemas/generated/s3/bucket.rs
   pub fn arn() -> AttributeType {
       AttributeType::custom(
           Some(provider_type("s3", "Bucket", "Arn")),
           super::super::arn(), // generic aws.Arn base
           Some("^arn:(aws|aws-cn|aws-us-gov):s3:::.+$".to_string()),
           None,
           legacy_validator(|value| /* per-kind rule from the table */),
           None,
       )
   }
   ```

   The schema's own `AttributeSchema::new("arn", …)` line uses
   `Self::arn()` (or equivalent within-file path) instead of
   `super::arn()`. Other resources that reference this ARN by type
   (e.g. `ec2::flow_log` needing an IAM Role ARN) call
   `super::super::iam::role::arn()`.

2. **Move the DSL type identity off the hard-coded `"aws"` provider
   axis.** `carina-aws-types::lib.rs` currently calls
   `TypeIdentity::new(Some("aws"), …)` from `aws_type` /
   `aws_bare_type`. Each copy of `carina-aws-types` gets a per-copy
   `const PROVIDER_NAME: &str` constant (`"aws"` in the aws repo,
   `"awscc"` in the awscc repo) and `aws_type` is renamed to
   `provider_type`. The per-schema helpers from move 1 emit
   identities via `provider_type(service, resource, "Arn")`, so
   `awscc.s3.Bucket.Arn` and `aws.iam.Role.Arn` come out with the
   defining provider's name as the first segment, closing #3256
   for the `.Arn` family (and incidentally for every other
   `aws_type` caller — `*.Id`, `*.Name`, …).

3. **Delete the four hand-written `iam_role_arn` / `iam_policy_arn`
   / `iam_oidc_provider_arn` / `kms_key_arn` helpers from
   `carina-aws-types/src/lib.rs`.** Their semantics move into the
   resource schemas that own them: `iam/role.rs::arn()`,
   `iam/policy.rs::arn()`, `iam/oidc_provider.rs::arn()`,
   `kms/key.rs::arn()`. Other resources that referenced the
   carina-aws-types symbol get rewritten to call the owning
   schema's helper. The override map in
   `codegen-aws/main.rs` and `awscc/bin/codegen.rs` (`m.insert(
   "DeliverCrossAccountRole", "super::iam_role_arn()")` and
   similar) flips to `"super::super::iam::role::arn()"` so the
   generator output picks up the new location.

4. **Per-kind validation rules live in the codegen.** Each codegen
   carries a static `(service, resource) → (regex, validator)`
   table: pre-populated with rows for IAM Role / Policy / OIDC
   Provider / KMS Key (matching today's hand-written rules byte for
   byte, ported from `carina-aws-types`), plus rows added for the
   resources Issue #3392 names: S3 Bucket, CloudFront
   Distribution, ECS Cluster, DynamoDB Table, CloudWatch
   LogGroup. Resources not in the table get the service-prefix
   fallback `^arn:(aws|aws-cn|aws-us-gov):<service>:` and the
   existing `validate_service_arn` helper. Resources whose
   `service` is also unknown get the generic `^arn:` regex (the
   current `super::arn()` behavior). The table is the single
   source of truth for "what does an IAM Role ARN look like" and
   lives next to the codegen that consumes it; adding a new
   per-kind rule is a one-row table edit.

### Validation lookup, by example

For `awscc.s3.Bucket`:

1. Codegen sees the CFN schema has `properties.Arn`.
2. Codegen consults its ARN-validation table for `("s3", "Bucket")`.
   Hit: `(r"^arn:(aws|aws-cn|aws-us-gov):s3:::.+$",
   validate_s3_bucket_arn)`.
3. Codegen emits in `awscc/src/schemas/generated/s3/bucket.rs`:
   ```rust
   pub fn arn() -> AttributeType {
       AttributeType::custom(
           Some(provider_type("s3", "Bucket", "Arn")),
           super::super::arn(),
           Some("^arn:(aws|aws-cn|aws-us-gov):s3:::.+$".to_string()),
           None,
           legacy_validator(|v| validate_s3_bucket_arn(v)),
           None,
       )
   }
   ```
4. The schema's `AttributeSchema::new("arn", Self::arn())` wires it up.

For a resource not in the table (say `awscc.wafv2.WebAcl`):

1. Codegen sees `properties.Arn`.
2. Table miss for `("wafv2", "WebAcl")`.
3. Service-prefix fallback: known service `wafv2`. Codegen emits
   `pub fn arn() -> AttributeType` with regex
   `^arn:(aws|aws-cn|aws-us-gov):wafv2:.*$` and
   `validate_service_arn(s, "wafv2", None)`.
4. The schema wires it up the same way.

### Why this approach over alternatives

**Keep the helpers in `carina-aws-types`, add new ones for every
resource (rejected).** Carries the original violation forward and
multiplies it by ~18 resources today plus the long tail. Every new
resource with an `arn` attribute would need a hand-edited
`carina-aws-types` entry, and the cross-service utility crate would
end up reflecting every resource's identifier surface — the opposite
of why it exists.

**Emit the helper in `carina-aws-types` but generated, not
hand-authored (rejected).** Solves the "manual entry per resource"
problem but doubles down on the layering mistake: a cross-service
utility crate would mirror the per-resource schema, defeating its
purpose. The schemas/generated tree already exists as the per-resource
emission target — re-using it is natural.

**Build a schema-metadata pattern extractor (rejected).** CFN ARN
properties have no `pattern`. Building a walker that extracts patterns
gives empty output today and adds a moving part for no win.

**Keep `TypeIdentity::new(Some("aws"), …)` and only do #3392
(rejected).** Issue #3392 asks for `awscc.s3.Bucket.Arn` literally;
producing `aws.s3.Bucket.Arn` from the awscc provider would double
down on the #3256 confusion. Doing #3256 in the same change keeps the
rename to one occurrence.

**Pull `carina-aws-types` into `carina` itself to kill the copy
drift (deferred).** This is the right shape long-term but goes well
past #3392's scope: every existing consumer in both provider repos
would need to change `Cargo.toml`, and the crate would need to be
`no_std` or feature-gated to avoid pulling AWS SDK types into
`carina-core` (which deliberately has no AWS deps — see CLAUDE.md
Architecture). File this as a follow-up.

## Long-term + type-safety lens

Applied per CLAUDE.md "Root-cause fixes only — and make the broken
state unrepresentable":

- **Root cause for the layering violation.** Per-resource ARN
  validation lives in a cross-service crate because the schemas/
  generated tree didn't expose per-resource helpers when those
  rules were first authored. The seam that needs to change is the
  codegen output: schemas/generated/<service>/<resource>.rs gains
  the helper. After this change, `carina-aws-types` no longer
  exports any `iam_*_arn` symbols — a future caller cannot
  re-introduce the violation by importing from there.
- **Root cause for the namespace bug (#3256).** The provider axis
  of the DSL type identity is hard-coded to `"aws"` inside
  `aws_type`. A future caller cannot select the right axis without
  editing that helper. The fix is to remove the hard-coding —
  replace `Some("aws")` with a value supplied by the caller. Doing
  this via `const PROVIDER_NAME` per copy is the narrowest
  factoring that closes the runtime hole without dragging provider
  enumeration into `carina-core`.
- **"New caller tomorrow" check.** A new resource with an `arn`
  attribute added to either provider's schema cache → codegen
  detects the `arn` property → emits the per-schema `arn()`
  helper inside that resource's generated file →
  `provider_type(service, resource, "Arn")` produces the
  defining-provider-scoped DSL identity. The caller cannot end up
  with the wrong layer or the wrong namespace without bypassing
  the codegen entirely.
- **Compile-fail guard.** After the migration, deleting
  `iam_role_arn` etc. from `carina-aws-types` is the compile-time
  enforcement that no resource may continue to depend on the
  cross-service location. A future schema that tries to import
  `super::iam_role_arn()` simply doesn't build.

## Blast-radius probe

Per CLAUDE.md, measure before deferring. The plan must run these
stubs and record numbers:

1. **Rename `aws_type` → `provider_type` + thread `PROVIDER_NAME`.**
   Stub the rename in `carina-provider-aws/carina-aws-types/src/
   lib.rs`, run `cargo check -p carina-aws-types -p carina-provider-aws
   --all-targets 2>&1 | grep -E "^error" | wc -l`, revert. Repeat
   for the awscc copy.
2. **Delete the four `iam_*_arn` / `kms_key_arn` symbols from
   `carina-aws-types`.** Stub the delete; the error count is the
   number of consumers that need updating. The change is mechanical
   (re-target to `super::super::iam::role::arn()` etc.), but the
   number sizes the per-repo task.
3. **Emit `<provider>.s3.Bucket.Arn` from one regenerated awscc
   schema.** Run `validate` against an `.crn` that uses
   `awscc.s3.Bucket.Arn` to confirm the parser + resolver chain
   accepts the namespaced form.

These probes run during plan execution, not in this design doc.

## File structure

The work spans three repositories. Sizes in `()` for context; see
the implementation plan for per-file diff scope.

### `carina-rs/carina-provider-aws`

- `carina-aws-types/src/lib.rs` (3795 lines)
  - Add `const PROVIDER_NAME: &str = "aws";`.
  - Rename `aws_type` → `provider_type`, `aws_bare_type` →
    `provider_bare_type`; both read `PROVIDER_NAME` instead of
    the literal `"aws"`.
  - **Delete** `iam_role_arn`, `iam_policy_arn`,
    `iam_oidc_provider_arn`, `kms_key_arn` and their associated
    `#[cfg(test)]` round-trip tests.
  - Keep `arn()` (generic `aws.Arn`), `validate_arn`,
    `validate_service_arn`, `validate_iam_arn`,
    `validate_kms_key_id` — all consumed by the new per-schema
    helpers.
- `carina-codegen-aws/src/main.rs` (5244 lines)
  - Add ARN-validation table `static ARN_VALIDATIONS: &[(service,
    resource, regex, validator_fn_path)]`. Pre-populated with the
    IAM/KMS rules ported from `carina-aws-types` byte-for-byte
    plus the Issue #3392 day-one entries (S3 Bucket, CloudFront
    Distribution, ECS Cluster, DynamoDB Table, CloudWatch
    LogGroup).
  - When the generator visits a resource whose output schema has
    an `arn` attribute (same condition as today's
    `AttributeSchema::new("arn", super::arn())`), emit `pub fn
    arn() -> AttributeType` *inside the resource's generated
    file*, and wire `AttributeSchema::new("arn", Self::arn())`
    instead of `super::arn()`.
- `carina-codegen-aws/src/resource_defs.rs` (2865 lines)
  - Replace the literal `"super::iam_role_arn()"` and
    `"super::iam_policy_arn()"` / `"super::kms_key_arn()"` strings
    in the override map with their new locations:
    `"super::super::iam::role::arn()"`,
    `"super::super::iam::policy::arn()"`,
    `"super::super::kms::key::arn()"`. (Path shape depends on
    where the schemas/generated module tree puts each resource;
    confirm during plan probe.)
- `carina-provider-aws/src/schemas/generated/**/*.rs` (regenerated)
  - 5 files touched today. After regen, the resources with `arn`
    attributes have a `pub fn arn()` helper above the schema
    definition.
- `carina-provider-aws/src/schemas/types.rs` (~700 lines)
  - The custom-type registry currently lists `iam_role_arn`,
    `iam_policy_arn`, `kms_key_arn` as discoverable types.
    These get rewritten to discover the per-schema helpers
    instead, so DSL resolution of `aws.iam.Role.Arn` continues to
    succeed (and now resolves to the same shape regardless of
    how it's spelled in the source). Specific shape of the
    rewrite depends on how the registry is constructed; the
    plan probes this.

### `carina-rs/carina-provider-awscc`

Symmetric to the aws side, with the per-copy delta:

- `carina-aws-types/src/lib.rs` (~3500 lines, separate copy)
  - Add `const PROVIDER_NAME: &str = "awscc";`.
  - Same `provider_type` rename.
  - Same hand-written-helper deletion.
- `carina-provider-awscc/src/bin/codegen.rs` (~8200 lines)
  - Add the awscc ARN-validation table (same rows as aws).
  - When the CFN schema declares an `arn` attribute, emit `pub fn
    arn() -> AttributeType` inside the resource's generated file.
  - The override map (`m.insert("DeliverCrossAccountRole",
    "super::iam_role_arn()")` and the rest at lines ~4051–4059)
    flips to `"super::super::iam::role::arn()"` shape.
- `carina-provider-awscc/src/schemas/generated/**/*.rs` (regen)
  - 13 files touched today. After regen, each gets its `pub fn
    arn()` helper.
- `carina-provider-awscc/src/schemas/awscc_types.rs` — symmetric
  to `aws/src/schemas/types.rs` above.

### `carina-rs/carina` (this repo)

No production code changes. The work happens in the provider repos.
Two passive properties to verify:

- `TypeIdentity::new` already takes the provider axis as a
  parameter (`Option<&str>`); the rename in the provider repos
  doesn't require a `carina-core` signature change.
- The DSL parser already accepts `<provider>.<service>.<Kind>.Arn`
  shape; the issue's repro shows it reaches resolution and fails
  with "unknown custom type", not a parse error.

## Edge cases & constraints

- **Existing `infra/*` consumers of `aws.iam.Role.Arn` /
  `aws.iam.OidcProvider.Arn` / `aws.iam.Policy.Arn`.** After the
  rename, `aws.iam.OidcProvider.Arn` becomes
  `awscc.iam.OidcProvider.Arn` (defined by the awscc provider per
  `carina-provider-awscc/src/schemas/generated/iam/
  oidc_provider.rs:72`). Every callsite under `carina-rs/infra`
  needs to flip in lockstep with the provider releases. The plan
  must include a coordination step: rename infra references in
  the same window as the provider releases, or accept a
  validation break on the user's local tree until they re-run
  with new providers.
- **`carina-aws-types` copies stay in sync apart from
  `PROVIDER_NAME`.** The change adds one per-copy delta on top of
  the existing parity. The plan must explicitly diff the two
  copies after the rename and confirm only the constant differs.
  Track per `project_aws_types_triplicated_copies`.
- **Override-map symbol stability under the new location.**
  Today's override map writes the literal string
  `"super::iam_role_arn()"` into the generated file. After the
  move, every override entry referring to an IAM/KMS ARN must
  reflect the new path. The plan must enumerate and update the
  full list (in `awscc/bin/codegen.rs` ~lines 4051–4059 and the
  aws codegen's `m.insert(…)` entries) and add a test that
  asserts the override map's symbols resolve.
- **Cross-resource type references.** `ec2.flow_log`'s
  `deliver_logs_permission_arn` is typed `iam_role_arn()` today.
  After the move it becomes `iam::role::arn()` (path from
  `ec2/flow_log.rs`: `super::super::iam::role::arn()`). The plan
  must list every cross-resource reference and the path
  conversion.
- **Doctests / snapshots that hard-code type names.** Both repos
  have generated-docs snapshots and probably some doc-strings
  embedding type names. The plan must regenerate docs per
  CLAUDE.md "Always regenerate docs when adding/changing
  resources" and review the diff for namespace flips.
- **Cycle order of releases.** carina (this repo) doesn't change,
  so provider releases are independent of a carina release. But
  the two providers and `carina-rs/infra` must roll in a defined
  order: aws + awscc release first (both renamed), then
  carina-rs/infra updates its type references.

## Test plan

- **Provider repos: unit test on each per-schema `arn()`
  validator.** Port today's `validate_iam_arn_error_says_iam_*` /
  KMS validator tests from `carina-aws-types/src/lib.rs` into
  `schemas/generated/iam/role.rs`'s test module (or a sibling
  `tests/` file if the generated tree forbids inline tests).
  Behavior must be unchanged from today's hand-written helpers.
- **Provider repos: parse-acceptance test on namespaced custom
  types.** A small `.crn` fixture per provider asserting
  `arguments { x: awscc.s3.Bucket.Arn }` resolves; same shape for
  `aws.iam.Role.Arn`.
- **carina-rs/infra: integration test.** Run `carina validate .`
  over `usecases/registry/app/` with the `aws.iam.OidcProvider.Arn`
  reference flipped to `awscc.iam.OidcProvider.Arn` and against
  freshly built provider binaries. Confirms the parser + resolver
  chain accepts the new form end-to-end.
- **Generated docs in both provider repos.** Run
  `scripts/generate-docs.sh` post-codegen; the diff should show
  `aws.iam.*` → `awscc.iam.*` flips for awscc-defined resources
  and per-resource Arn helpers added.
- **Codegen verification gates.** Run `bash scripts/check-*.sh` per
  CLAUDE.md verify protocol; both `Codegen Check` and `Check Plan
  Fixtures` (where applicable to the provider repo) must pass.

## Open questions for the plan step

The plan-writing step (Codex) needs to address these before
decomposing tasks:

1. **Sequencing across the four moves.** Three plausible orderings:
   - (a) Move 2 (namespace rename) first, then 1+3+4 in lockstep.
   - (b) Moves 1+4 (per-schema emission + table) first under
     `aws.*`, then move 2 (rename) flips the namespace.
   - (c) All four in one PR per provider.
   The plan must pick one explicitly. (a) gives the smallest first
   diff but lands a rename PR with no user-visible benefit; (b)
   delivers the newtype value immediately but lands the rename
   right after; (c) is the largest single review but ships the
   full story atomically.
2. **Cross-repo coordination.** The two provider repos must
   release together (else `infra` sees inconsistent namespaces).
   The plan must specify: who waits for whom, who tags the release,
   and whether `carina-rs/infra` lands its type-reference flip
   before, after, or with the provider releases.
3. **Per-schema helper naming inside the generated file.** The
   sketch above uses `pub fn arn()` inside
   `schemas/generated/s3/bucket.rs`. Confirm this doesn't collide
   with the existing `pub fn arn()` in
   `carina-aws-types/src/lib.rs` (which stays — it's the generic
   `aws.Arn`) or with any other symbol in the schemas/generated
   tree. If a collision exists, the plan picks an unambiguous
   name like `pub fn bucket_arn()` consistently.
4. **Path for the override map's new symbol references.** The
   override map writes Rust source as a string; the new path
   needs to compile from the resource file the override is being
   applied to. Confirm the path shape (`super::super::iam::
   role::arn()` vs `crate::schemas::generated::iam::role::arn()`)
   that round-trips through both codegens.
5. **Cross-resource references discovered during the probe.** The
   plan's probe step lists every `iam_role_arn()` /
   `iam_policy_arn()` / `iam_oidc_provider_arn()` /
   `kms_key_arn()` callsite across both provider repos and
   describes the path it converts to. This list is the work item
   for the migration PRs.
