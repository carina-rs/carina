# Per-resource ARN newtypes implementation plan

## 0. Summary
<!-- derived-from ../specs/2026-06-05-per-resource-arn-newtypes-design.md -->

This plan delivers per-resource `.Arn` custom types for every generated resource schema that already has an `arn` attribute, renames the provider-axis helper from `aws_type` to `provider_type`, deletes the hand-written IAM/KMS ARN helpers from both `carina-aws-types` copies, and lands the infra type-reference flip only after both provider releases are available; the chosen sequencing is one atomic PR per provider, with AWS and AWSCC developed in parallel, released from the next coordinated provider tags after the current `v0.5.0` line, then consumed by `carina-rs/infra`.

## 1. Open questions — resolved

### 1.1 Sequencing of the 4 moves

Resolution: choose (c), all four moves in one PR per provider. Evidence: the design doc says the new `.Arn` types should ship with final names from day one, and the long-term lens says deleting `iam_role_arn` etc. is the compile-fail guard against returning to the cross-service location. The measured rename probe returned `0` crate errors and `0` workspace errors in both providers, while deletion returned `5` AWS workspace errors and `30` AWSCC workspace errors, so the risky surface is the migration, not the namespace helper rename.

This trades a larger review surface for atomic user-visible value. Option (a) would land a namespace rename with no new `.Arn` coverage. Option (b) would briefly ship generated ARN types under the wrong `aws.*` namespace for AWSCC, then rename them. Option (c) keeps the externally visible state consistent: generated helpers, `provider_type`, override-map rewrites, consumer migration, helper deletion, codegen, and docs all land together.

### 1.2 Cross-repo coordination

Resolution: PR-A (`carina-provider-aws`) and PR-B (`carina-provider-awscc`) can be developed and reviewed in parallel, but neither provider release should be advertised for infra until both are merged and tagged. The release owner tags both provider repos in the same release window: the AWS provider first, then the AWSCC provider immediately after. The order is only operational; infra waits for both.

Release version evidence: `gh api repos/carina-rs/carina-provider-aws/releases/latest --jq '.tag_name'` and the AWSCC equivalent both failed in this sandbox with `error connecting to api.github.com`. Local evidence is `git tag --sort=-version:refname | head` showing `v0.5.0` first in both provider repos, and root `Cargo.toml` versions are `0.5.0` in both repos. The release gate therefore must rerun the two `gh api` commands on a networked machine immediately before tagging; if they still report `v0.5.0`, tag `carina-provider-aws@v0.6.0` and `carina-provider-awscc@v0.6.0`. If they report a newer tag, tag the next minor after that newer tag for both repos.

PR-C (`carina-rs/infra`) starts after both provider tags exist. It updates provider locks/references to the two coordinated releases, then flips type references produced by AWSCC-owned resources from `aws.iam.Role.Arn`, `aws.iam.OidcProvider.Arn`, and `aws.iam.Policy.Arn` to `awscc.iam.Role.Arn`, `awscc.iam.OidcProvider.Arn`, and `awscc.iam.Policy.Arn`. Evidence from `/Users/mizzy/src/github.com/carina-rs/infra`: `rg -n "aws\\.iam\\.(Role|OidcProvider|Policy)\\.Arn"` found references in `usecases/registry/app-deploy/main.crn:12`, `:14`, `network-deploy/main.crn:9`, `:11`, `infra-deploy/main.crn:8`, `:10`, `:11`, `usecases/bootstrap/main.crn:13`, `:14`, `modules/github-oidc/main.crn:9`, `usecases/registry/app/main.crn:19`, and `aws/management/identity-center/exports.crn:3`.

### 1.3 Per-schema helper name

Resolution: keep the final helper name `pub fn arn()`, and have generated schema attributes call it as `self::arn()` rather than bare `arn()`. The name itself is safe because it lives in each resource module, while the generic helper lives in `carina-aws-types` and is available through `super::arn()`.

Evidence:

- Generic AWS helper: `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-aws-types/src/lib.rs:1020` defines `pub fn arn() -> AttributeType`, imported into generated files through `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/types.rs:6` (`pub use carina_aws_types::*`) and `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/mod.rs:8` (`pub use super::types::*`).
- Generic AWSCC helper: `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-aws-types/src/lib.rs:1084` defines `pub fn arn() -> AttributeType`, imported through `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/awscc_types.rs:7` and `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/mod.rs:11`.
- Resource helper path: `schemas/generated/iam/role.rs::arn()` resolves as `super::super::iam::role::arn()` from `schemas/generated/ec2/flow_log.rs`.
- Collision evidence from `head -30`:
  - AWS `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/role.rs` imports `use super::AwsSchemaConfig;`, `use super::tags_type;`, `use super::validate_tags_map;`, and specific `carina_core` items. It does not contain `use super::*`.
  - AWSCC `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/iam/role.rs` imports `use super::AwsccSchemaConfig;`, `use super::tags_type;`, `use super::validate_tags_map;`, and specific `carina_core` items. It does not contain `use super::*`.
  - The service module one level above has `pub use super::*`, but that re-export makes generic `super::arn()` available as a qualified parent item; it does not import it into the child file's local value namespace. `self::arn()` therefore resolves to the helper defined in the same resource module and cannot accidentally call the generic helper.

One adjustment is required: `provider_type` must be `pub fn provider_type(service: &str, resource: &str, kind: &str) -> TypeIdentity`, not private, because generated resource modules call it through the re-export chain.

### 1.4 Override-map symbol path shape

Resolution: use the exact string `"super::super::iam::role::arn()"`, `"super::super::iam::policy::arn()"`, `"super::super::iam::oidc_provider::arn()"`, and `"super::super::kms::key::arn()"` in override maps. Evidence: generated service modules re-export the generated root with `pub use super::*` in `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/mod.rs:7` and `ec2/mod.rs:7`, so from a child file like `ec2/flow_log.rs`, `super` is `generated::ec2` and `super::super` is `generated`.

Compile probe evidence from temporary copies because the sandbox forbids writes in sibling provider repos:

- AWS temp copy command: `cargo check -p carina-provider-aws --all-targets 2>&1 | grep -E "^error" | wc -l` after adding `iam::role::arn()` and changing `ec2/flow_log.rs` to `super::super::iam::role::arn()` returned `0`.
- AWSCC temp copy command: `cargo check -p carina-provider-awscc --all-targets 2>&1 | grep -E "^error" | wc -l` after the same path-shape stub returned `0`.
- Real repos stayed clean: `git -C /Users/mizzy/src/github.com/carina-rs/carina-provider-aws diff --quiet` returned `0`; `git -C /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc diff --quiet` returned `0`.

### 1.5 Cross-resource references — full list

AWS current callsites and rewrites:

- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/ec2/flow_log.rs:46` : `super::iam_role_arn()` → `super::super::iam::role::arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/role.rs:73` : `super::iam_role_arn()` → `arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/roles.rs:29` : `AttributeType::unordered_list(super::iam_role_arn())` → `AttributeType::unordered_list(super::super::iam::role::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs:3847` : `"super::iam_role_arn()"` → `"super::super::iam::role::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs:3848` : `"super::iam_role_arn()"` → `"super::super::iam::role::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs:3849` : `"super::iam_role_arn()"` → `"super::super::iam::role::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs:3850` : `"super::iam_policy_arn()"` → `"super::super::iam::policy::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs:3851` : `"super::iam_policy_arn()"` → `"super::super::iam::policy::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs:3852` : `"super::kms_key_arn()"` → `"super::super::kms::key::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs:3855` : `"super::kms_key_arn()"` → `"super::super::kms::key::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs:4420` : `AttributeType::unordered_list(super::iam_role_arn())` → `AttributeType::unordered_list(super::super::iam::role::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/resource_defs.rs:2394` : `"super::iam_role_arn()"` → `"super::super::iam::role::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/resource_defs.rs:2423` : `"super::iam_role_arn()"` → `"super::super::iam::role::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/resource_defs.rs:2842` : `"super::iam_role_arn()"` → `"super::super::iam::role::arn()"`.

AWSCC current callsites and rewrites:

- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ec2/flow_log.rs:51` : `super::iam_role_arn()` → `super::super::iam::role::arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ec2/flow_log.rs:57` : `super::iam_role_arn()` → `super::super::iam::role::arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ec2/vpc_peering_connection.rs:45` : `super::iam_role_arn()` → `super::super::iam::role::arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ecs/cluster.rs:49`, `:57`, `:89`, `:97`, `:98`, `:111` : `super::kms_key_arn()` → `super::super::kms::key::arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/logs/log_group.rs:86` : `super::kms_key_arn()` → `super::super::kms::key::arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/iam/oidc_provider.rs:72` : `super::iam_oidc_provider_arn()` → `arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/iam/role.rs:36` : `super::iam_role_arn()` → `arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/iam/role.rs:53`, `:70` : `super::iam_policy_arn()` → `super::super::iam::policy::arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/s3/bucket.rs:371`, `:546`, `:582`, `:598` : `super::kms_key_arn()` → `super::super::kms::key::arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/s3/bucket.rs:450`, `:640` : `super::iam_role_arn()` → `super::super::iam::role::arn()`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs:833` : `"super::iam_role_arn()"` → `"super::super::iam::role::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs:834` : `"super::iam_policy_arn()"` → `"super::super::iam::policy::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs:835` : `"super::iam_oidc_provider_arn()"` → `"super::super::iam::oidc_provider::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs:836` : `"super::kms_key_arn()"` → `"super::super::kms::key::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs:4051`, `:4052`, `:4053`, `:4085`, `:4170`, `:8038`, `:8670` : `"super::iam_role_arn()"` → `"super::super::iam::role::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs:4054`, `:4055`, `:8052` : `"super::iam_policy_arn()"` → `"super::super::iam::policy::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs:4056`, `:4059`, `:8040`, `:8041` : `"super::kms_key_arn()"` → `"super::super::kms::key::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs:4090`, `:8861` : `"super::iam_oidc_provider_arn()"` → `"super::super::iam::oidc_provider::arn()"`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-aws-types/src/lib.rs:2507`, `:2519`, `:2532`, `:2544`, `:2554`, `:2566`, `:2579` : `iam_oidc_provider_arn()` tests → move into `schemas/generated/iam/oidc_provider.rs` and call `arn()`.

### 1.6 Resources with `arn` attributes — full emission list

Command evidence: `rg -n 'AttributeSchema::new\("arn"' /Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated`.

AWS has 5 generated resource files with `arn` attributes. Every one must get a schema-owned `pub fn arn()` and its attribute line must become `AttributeSchema::new("arn", self::arn())`:

- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/organizations/account.rs:65` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/organizations/organization.rs:32` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/role.rs:73` : `AttributeSchema::new("arn", super::iam_role_arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/s3/bucket_data_source.rs:25` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/sts/caller_identity.rs:24` : `AttributeSchema::new("arn", super::arn())`.

AWSCC has 13 generated resource files with `arn` attributes. Every one must get a schema-owned `pub fn arn()` and its attribute line must become `AttributeSchema::new("arn", self::arn())`:

- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ec2/ipam_pool.rs:68` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/dynamodb/table.rs:105` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/kms/key.rs:88` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ec2/ipam.rs:54` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/organizations/account.rs:168` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/organizations/organization.rs:86` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/cloudfront/distribution.rs:253` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/wafv2/web_acl.rs:494` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/iam/oidc_provider.rs:72` : `AttributeSchema::new("arn", super::iam_oidc_provider_arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ecs/cluster.rs:25` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/logs/log_group.rs:64` : `AttributeSchema::new("arn", super::arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/iam/role.rs:36` : `AttributeSchema::new("arn", super::iam_role_arn())`.
- `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/s3/bucket.rs:268` : `AttributeSchema::new("arn", super::arn())`.

## 2. Blast-radius probes — measurements

The sandbox prevented writing directly to `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws` and `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc`; the first in-place probe failed with `Cannot make temp name: Operation not permitted`, before any tracked file changed. I therefore copied both repos with `cp -R` to `/private/tmp/carina-probe-20260605/`, ran the same stubs there, and verified the real repos stayed clean with `git status --short` empty and `git diff --quiet` exit `0`.

Copy reliability evidence:

- `ls /Users/mizzy/src/github.com/carina-rs/carina-provider-aws/.cargo/ 2>/dev/null` printed no `.cargo` entries, so there was no ignored local cargo config missing from the copy.
- Both provider repos have `.gitmodules` containing `carina-plugin-wit` at path `carina-plugin-wit` with URL `https://github.com/carina-rs/carina-plugin-wit.git`.
- The temporary copies were sufficient for the requested `cargo check` probes: the rename probes returned `0` errors for crate and workspace checks in both repos, and the path-shape probes returned `0` errors for `carina-provider-aws` and `carina-provider-awscc`. This proves the copied submodule content was present enough for these checks; it is not a substitute for release verification in the real repos.

Probe 2a, AWS rename stub:

- Command: `cd /private/tmp/carina-probe-20260605/carina-provider-aws && cargo check -p carina-aws-types --all-targets 2>&1 | grep -E "^error" | wc -l`
- Result: `0`.
- Command: `cd /private/tmp/carina-probe-20260605/carina-provider-aws && cargo check --workspace --all-targets 2>&1 | grep -E "^error" | wc -l`
- Result: `0`.
- Revert: `git checkout -- carina-aws-types/src/lib.rs`; `git diff --quiet -- carina-aws-types/src/lib.rs` returned `0`.

Probe 2b, AWSCC rename stub:

- Command: `cd /private/tmp/carina-probe-20260605/carina-provider-awscc && cargo check -p carina-aws-types --all-targets 2>&1 | grep -E "^error" | wc -l`
- Result: `0`.
- Command: `cd /private/tmp/carina-probe-20260605/carina-provider-awscc && cargo check --workspace --all-targets 2>&1 | grep -E "^error" | wc -l`
- Result: `0`.
- Revert: `git checkout -- carina-aws-types/src/lib.rs`; `git diff --quiet -- carina-aws-types/src/lib.rs` returned `0`.

Probe 2c, delete hand-written helpers:

- AWS command: `cd /private/tmp/carina-probe-20260605/carina-provider-aws && cargo check -p carina-aws-types --all-targets 2>&1 | grep -E "^error" | wc -l`
- AWS result: `0`.
- AWS workspace command: `cd /private/tmp/carina-probe-20260605/carina-provider-aws && cargo check --workspace --all-targets 2>&1 | grep -E "^error" | wc -l`
- AWS workspace result: `5`.
- AWSCC command: `cd /private/tmp/carina-probe-20260605/carina-provider-awscc && cargo check -p carina-aws-types --all-targets 2>&1 | grep -E "^error" | wc -l`
- AWSCC result: `8`.
- AWSCC workspace command: `cd /private/tmp/carina-probe-20260605/carina-provider-awscc && cargo check --workspace --all-targets 2>&1 | grep -E "^error" | wc -l`
- AWSCC workspace result: `30`.
- Revert in both temp repos: `git checkout -- carina-aws-types/src/lib.rs`; `git diff --quiet -- carina-aws-types/src/lib.rs` returned `0`.

Probe 2d, DSL parser acceptance:

- Evidence: `/Users/mizzy/src/github.com/carina-rs/carina/.worktrees/issue-3392-arn-newtypes-design/carina-core/src/parser/carina.pest:121` defines `type_expr = { type_expr_atom ~ ("|" ~ type_expr_atom)* }`; `:122` includes `type_ref`; `:125` defines `type_ref = { resource_type_path }`; `:126` defines `resource_type_path = @{ identifier ~ ("." ~ identifier)+ }`; `:220` defines `identifier = @{ ASCII_ALPHA ~ (ASCII_ALPHANUMERIC | "_")* }`.
- Result: `awscc.s3.Bucket.Arn` is accepted at parse stage because every segment is an `identifier`, and it lexes the same way as `aws.s3.Bucket.Arn`. Any failure is resolver-level, not grammar-level.

Probe adjustment: generated resource helpers must be able to call `provider_type`; because `carina-aws-types/src/lib.rs:20` and AWSCC `:22` currently define private `fn aws_type`, the implementation must make the renamed helper `pub fn provider_type(service: &str, resource: &str, kind: &str) -> TypeIdentity`.

## 3. PR / task structure

PR-A: `carina-provider-aws`

- Title: `Generate per-resource ARN newtypes for AWS provider`
- Branch: `issue-3392-per-resource-arn-newtypes`
- Scope: rename the type identity helper to `provider_type`, add schema-owned ARN helper emission and validation table in Smithy codegen, migrate generated consumers to schema-owned helpers, delete IAM/KMS ARN helpers from `carina-aws-types`, regenerate schemas and docs.

PR-B: `carina-provider-awscc`

- Title: `Generate per-resource ARN newtypes for AWSCC provider`
- Branch: `issue-3392-per-resource-arn-newtypes`
- Scope: rename the type identity helper to `provider_type` with `PROVIDER_NAME = "awscc"`, add schema-owned ARN helper emission and validation lookup in CloudFormation codegen, migrate generated consumers to schema-owned helpers, delete IAM/KMS ARN helpers from the AWSCC `carina-aws-types` copy, regenerate schemas and docs, and add AWSCC-specific validator and identity tests.

PR-C: `carina-rs/infra`

- Title: `Use provider-owned ARN type names`
- Branch: `issue-102-provider-owned-arn-types`
- Scope: update provider locks to `carina-provider-aws@v0.6.0` and `carina-provider-awscc@v0.6.0`, flip AWSCC-owned IAM ARN type references to `awscc.*`, and replace the S3/CloudFront string workaround with `awscc.s3.Bucket.Arn` and `awscc.cloudfront.Distribution.Arn`.

Execution order: develop PR-A and PR-B in parallel; merge both; tag both; then open and merge PR-C.

## 4. TDD tasks

### Task A1: AWS provider-type identity helper

**Goal**: make the provider axis explicit and public for generated resource helpers.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-aws-types/src/lib.rs` (modify)

**Test (Red)**:
```rust
#[test]
fn provider_type_uses_aws_provider_axis() {
    assert_eq!(
        provider_type("s3", "Bucket", "Arn").to_string(),
        "aws.s3.Bucket.Arn"
    );
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-aws-types provider_type_uses_aws_provider_axis`

**Implementation (Green)**:
```rust
const PROVIDER_NAME: &str = "aws";

pub fn provider_type(service: &str, resource: &str, kind: &str) -> TypeIdentity {
    TypeIdentity::new(Some(PROVIDER_NAME), [service, resource], kind)
}

fn provider_bare_type(segments: &[&str], kind: &str) -> TypeIdentity {
    TypeIdentity::new(Some(PROVIDER_NAME), segments.iter().copied(), kind)
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-aws-types provider_type_uses_aws_provider_axis`

### Task A2: AWS codegen ARN validation lookup

**Goal**: teach Smithy codegen the three-stage ARN emission choice: per-kind table, service-prefix fallback, then generic fallback.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs` (modify)

**Test (Red)**:
```rust
#[test]
fn arn_emit_choice_has_table_service_and_generic_paths() {
    assert!(matches!(
        arn_emit_choice("iam", "Role"),
        ArnEmitChoice::PerKind(entry) if entry.validator == "validate_iam_role_arn_value"
    ));
    assert!(matches!(
        arn_emit_choice("organizations", "Organization"),
        ArnEmitChoice::ServicePrefix("organizations")
    ));
    assert!(matches!(
        arn_emit_choice("unknownservice", "Thing"),
        ArnEmitChoice::Generic
    ));
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-codegen-aws arn_emit_choice_has_table_service_and_generic_paths`

**Implementation (Green)**:
```rust
enum ArnEmitChoice {
    PerKind(&'static ArnValidation),
    ServicePrefix(&'static str),
    Generic,
}

struct ArnValidation {
    service: &'static str,
    resource: &'static str,
    regex: &'static str,
    validator: &'static str,
}

static ARN_VALIDATIONS: &[ArnValidation] = &[
    ArnValidation { service: "iam", resource: "Role", regex: "^arn:(aws|aws-cn|aws-us-gov):iam::[^:]*:role/.+$", validator: "validate_iam_role_arn_value" },
    ArnValidation { service: "iam", resource: "Policy", regex: "^arn:(aws|aws-cn|aws-us-gov):iam::[^:]*:policy/.+$", validator: "validate_iam_policy_arn_value" },
    ArnValidation { service: "kms", resource: "Key", regex: "^arn:(aws|aws-cn|aws-us-gov):kms:[^:]*:[^:]*:key/.+$", validator: "validate_kms_key_arn_value" },
    ArnValidation { service: "s3", resource: "Bucket", regex: "^arn:(aws|aws-cn|aws-us-gov):s3:::.+$", validator: "validate_s3_bucket_arn_value" },
    ArnValidation { service: "cloudfront", resource: "Distribution", regex: "^arn:(aws|aws-cn|aws-us-gov):cloudfront::[^:]*:distribution/.+$", validator: "validate_cloudfront_distribution_arn_value" },
    ArnValidation { service: "ecs", resource: "Cluster", regex: "^arn:(aws|aws-cn|aws-us-gov):ecs:[^:]*:[^:]*:cluster/.+$", validator: "validate_ecs_cluster_arn_value" },
    ArnValidation { service: "dynamodb", resource: "Table", regex: "^arn:(aws|aws-cn|aws-us-gov):dynamodb:[^:]*:[^:]*:table/.+$", validator: "validate_dynamodb_table_arn_value" },
    ArnValidation { service: "logs", resource: "LogGroup", regex: "^arn:(aws|aws-cn|aws-us-gov):logs:[^:]*:[^:]*:log-group:.+$", validator: "validate_logs_log_group_arn_value" },
];

fn arn_validation_for(service: &str, resource: &str) -> Option<&'static ArnValidation> {
    ARN_VALIDATIONS
        .iter()
        .find(|v| v.service == service && v.resource == resource)
}

static KNOWN_SERVICES: &[&str] = &[
    "cloudfront",
    "dynamodb",
    "ec2",
    "ecs",
    "iam",
    "kms",
    "logs",
    "organizations",
    "s3",
    "sts",
    "wafv2",
];

fn arn_emit_choice(service: &str, resource: &str) -> ArnEmitChoice {
    if let Some(entry) = arn_validation_for(service, resource) {
        ArnEmitChoice::PerKind(entry)
    } else if let Some(&known) = KNOWN_SERVICES.iter().find(|&&known| known == service) {
        ArnEmitChoice::ServicePrefix(known)
    } else {
        ArnEmitChoice::Generic
    }
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-codegen-aws arn_emit_choice_has_table_service_and_generic_paths`

### Task A3: AWS codegen emits schema-owned `arn()`

**Goal**: generated resources with an `arn` attribute define a local helper and use it for that attribute.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs` (modify)

**Test (Red)**:
```rust
fn smithy_model_for_codegen_tests() -> SmithyModel {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../carina-provider-aws/tests/fixtures/smithy/iam.json");
    let file = std::fs::File::open(&fixture).expect("failed to open iam fixture");
    carina_smithy::parse_reader(std::io::BufReader::new(file))
        .expect("failed to parse iam fixture")
}

fn organizations_model_for_codegen_tests() -> SmithyModel {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../carina-provider-aws/tests/fixtures/smithy/organizations.json");
    let file = std::fs::File::open(&fixture).expect("failed to open organizations fixture");
    carina_smithy::parse_reader(std::io::BufReader::new(file))
        .expect("failed to parse organizations fixture")
}

#[test]
fn generated_iam_role_owns_arn_helper() {
    let resource = resource_defs::iam_resources()
        .into_iter()
        .find(|resource| resource.name == "iam.Role")
        .expect("iam.Role resource def");
    let model = smithy_model_for_codegen_tests();
    let generated = generate_resource(&resource, &model).expect("generate iam role");
    assert!(generated.contains("pub fn arn() -> AttributeType"));
    assert!(generated.contains("Some(super::provider_type(\"iam\", \"Role\", \"Arn\"))"));
    assert!(generated.contains("AttributeSchema::new(\"arn\", self::arn())"));
}

#[test]
fn generated_organizations_organization_gets_service_prefix_arn_helper() {
    let resource = resource_defs::organizations_resources()
        .into_iter()
        .find(|resource| resource.name == "organizations.Organization")
        .expect("organizations.Organization resource def");
    let model = organizations_model_for_codegen_tests();
    let generated = generate_resource(&resource, &model).expect("generate organization");
    assert!(generated.contains("pub fn arn() -> AttributeType"));
    assert!(generated.contains("validate_service_arn(s, \"organizations\", None)"));
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-codegen-aws generated_`

**Implementation (Green)**:
```rust
fn emit_arn_helper(service: &str, resource: &str, choice: ArnEmitChoice) -> String {
    if matches!(choice, ArnEmitChoice::Generic) {
        return "pub fn arn() -> AttributeType { super::arn() }\n".to_string();
    }
    let (regex_expr, validator_expr) = match choice {
        ArnEmitChoice::PerKind(entry) => (
            format!("Some({:?}.to_string())", entry.regex),
            entry.validator.to_string(),
        ),
        ArnEmitChoice::ServicePrefix(service) => (
            format!("Some(\"^arn:(aws|aws-cn|aws-us-gov):{}:.*$\".to_string())", service),
            format!(
                "|value| if let Value::Concrete(ConcreteValue::String(s)) = value {{ validate_service_arn(s, {:?}, None).map_err(|reason| format!(\"Invalid {} ARN {{}}: {{}}\", s, reason)) }} else {{ Err(\"Expected string\".to_string()) }}",
                service, service
            ),
        ),
        ArnEmitChoice::Generic => unreachable!("handled before custom helper emission"),
    };
    format!(
        r#"pub fn arn() -> AttributeType {{
    AttributeType::custom(
        Some(super::provider_type("{service}", "{resource}", "Arn")),
        super::arn(),
        {regex_expr},
        None,
        legacy_validator({validator_expr}),
        None,
    )
}}
"#,
        service = service,
        resource = resource,
        regex_expr = regex_expr,
        validator_expr = validator_expr,
    )
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-codegen-aws generated_`

### Task A4: AWS override map uses schema-owned paths

**Goal**: cross-resource string overrides point at owning resource modules.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs` (modify)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/resource_defs.rs` (modify)

**Test (Red)**:
```rust
#[test]
fn known_arn_overrides_point_to_resource_modules() {
    let overrides = known_string_type_overrides();
    assert_eq!(overrides.get("DeliverLogsPermissionArn"), Some(&"super::super::iam::role::arn()"));
    assert_eq!(overrides.get("PermissionsBoundary"), Some(&"super::super::iam::policy::arn()"));
    assert_eq!(overrides.get("KmsKeyArn"), Some(&"super::super::kms::key::arn()"));
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-codegen-aws known_arn_overrides_point_to_resource_modules`

**Implementation (Green)**:
```rust
m.insert("DeliverCrossAccountRole", "super::super::iam::role::arn()");
m.insert("DeliverLogsPermissionArn", "super::super::iam::role::arn()");
m.insert("PeerRoleArn", "super::super::iam::role::arn()");
m.insert("PermissionsBoundary", "super::super::iam::policy::arn()");
m.insert("ManagedPolicyArns", "super::super::iam::policy::arn()");
m.insert("KmsKeyId", "super::super::kms::key::arn()");
m.insert("KmsKeyArn", "super::super::kms::key::arn()");
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-codegen-aws known_arn_overrides_point_to_resource_modules`

### Task A5: AWS per-kind validator tests move to schema modules

**Goal**: IAM Role, IAM Policy, and KMS Key ARN validator behavior is owned by generated resource modules.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/role.rs` (modify generated output)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/policy.rs` (create after codegen if AWS has this resource)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/kms/key.rs` (create after codegen if AWS has this resource)

**Test (Red)**:
```rust
#[test]
fn arn_rejects_non_role_iam_arn() {
    let t = arn();
    let carina_core::schema::RawShape::Custom { validate, .. } = t.raw_shape() else {
        panic!("arn() should be custom");
    };
    let v = Value::Concrete(ConcreteValue::String("arn:aws:iam::123456789012:policy/Foo".to_string()));
    assert!(validate(&v).is_err());
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-provider-aws arn_rejects_non_role_iam_arn`

**Implementation (Green)**:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, Value};

    #[test]
    fn arn_rejects_non_role_iam_arn() {
        let t = arn();
        let carina_core::schema::RawShape::Custom { validate, .. } = t.raw_shape() else {
            panic!("arn() should be custom");
        };
        let v = Value::Concrete(ConcreteValue::String("arn:aws:iam::123456789012:policy/Foo".to_string()));
        assert!(validate(&v).is_err());
    }
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-provider-aws arn_rejects_non_role_iam_arn`

### Task A6: AWS migrate generated consumers off hand-written helpers

**Goal**: replace every generated/codegen consumer from section 1.5 before deleting helper exports.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/main.rs` (modify)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-codegen-aws/src/resource_defs.rs` (modify)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/ec2/flow_log.rs` (modify generated)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/role.rs` (modify generated)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/roles.rs` (modify generated)

**Test (Red)**:
```bash
cargo check --workspace --all-targets 2>&1 | grep -E "^error" | wc -l
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo check --workspace --all-targets 2>&1 | grep -E "^error" | wc -l`

**Implementation (Green)**:
```rust
AttributeSchema::new("deliver_logs_permission_arn", super::super::iam::role::arn())
AttributeSchema::new("arn", self::arn())
AttributeSchema::new("arns", AttributeType::unordered_list(super::super::iam::role::arn()))
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo check --workspace --all-targets`

### Task A7: AWS delete old hand-written helpers

**Goal**: make the old cross-service helper location unavailable after all consumers compile on schema-owned helpers.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-aws-types/src/lib.rs` (modify)

**Test (Red)**:
```rust
#[test]
fn carina_aws_types_no_longer_exports_resource_arn_helpers() {
    let source = std::fs::read_to_string("carina-aws-types/src/lib.rs").unwrap();
    assert!(!source.contains("pub fn iam_role_arn"));
    assert!(!source.contains("pub fn iam_policy_arn"));
    assert!(!source.contains("pub fn kms_key_arn"));
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-aws-types carina_aws_types_no_longer_exports_resource_arn_helpers`

**Implementation (Green)**:
```rust
// Delete pub fn iam_role_arn(), pub fn iam_policy_arn(), pub fn kms_key_arn().
// Keep arn(), validate_arn(), validate_service_arn(), validate_iam_arn(), and validate_kms_key_id().
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-aws-types carina_aws_types_no_longer_exports_resource_arn_helpers && cargo check -p carina-aws-types --all-targets && cargo check --workspace --all-targets`

### Task A8: AWS schema identity fixture

**Goal**: prove the generated AWS schema exposes `aws.iam.Role.Arn` as a structured custom type identity.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/schemas/generated/iam/role.rs` (modify generated tests)

**Test (Red)**:
```rust
#[test]
fn arn_identity_is_provider_scoped() {
    let t = arn();
    let carina_core::schema::RawShape::Custom { identity, .. } = t.raw_shape() else {
        panic!("arn() should be custom");
    };
    assert_eq!(identity.map(|id| id.to_string()).as_deref(), Some("aws.iam.Role.Arn"));
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-provider-aws arn_identity_is_provider_scoped`

**Implementation (Green)**:
```rust
pub fn arn() -> AttributeType {
    AttributeType::custom(
        Some(super::provider_type("iam", "Role", "Arn")),
        super::arn(),
        Some("^arn:(aws|aws-cn|aws-us-gov):iam::[^:]*:role/.+$".to_string()),
        None,
        legacy_validator(validate_iam_role_arn_value),
        None,
    )
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test -p carina-provider-aws arn_identity_is_provider_scoped`

### Task B1: AWSCC provider-type identity helper

**Goal**: make AWSCC-generated type identities start with `awscc`.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-aws-types/src/lib.rs` (modify)

**Test (Red)**:
```rust
#[test]
fn provider_type_uses_awscc_provider_axis() {
    assert_eq!(
        provider_type("s3", "Bucket", "Arn").to_string(),
        "awscc.s3.Bucket.Arn"
    );
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-aws-types provider_type_uses_awscc_provider_axis`

**Implementation (Green)**:
```rust
const PROVIDER_NAME: &str = "awscc";

pub fn provider_type(service: &str, resource: &str, kind: &str) -> TypeIdentity {
    TypeIdentity::new(Some(PROVIDER_NAME), [service, resource], kind)
}

fn provider_bare_type(segments: &[&str], kind: &str) -> TypeIdentity {
    TypeIdentity::new(Some(PROVIDER_NAME), segments.iter().copied(), kind)
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-aws-types provider_type_uses_awscc_provider_axis`

### Task B2: AWSCC ARN validation lookup

**Goal**: teach CFN codegen the three-stage ARN emission choice: per-kind table, service-prefix fallback, then generic fallback.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs` (modify)

**Test (Red)**:
```rust
#[test]
fn arn_emit_choice_has_table_service_and_generic_paths() {
    assert!(matches!(
        arn_emit_choice("iam", "OidcProvider"),
        ArnEmitChoice::PerKind(entry) if entry.validator == "validate_iam_oidc_provider_arn_value"
    ));
    assert!(matches!(
        arn_emit_choice("organizations", "Account"),
        ArnEmitChoice::ServicePrefix("organizations")
    ));
    assert!(matches!(
        arn_emit_choice("unknownservice", "Thing"),
        ArnEmitChoice::Generic
    ));
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc arn_emit_choice_has_table_service_and_generic_paths`

**Implementation (Green)**:
```rust
enum ArnEmitChoice {
    PerKind(&'static ArnValidation),
    ServicePrefix(&'static str),
    Generic,
}

struct ArnValidation {
    service: &'static str,
    resource: &'static str,
    regex: &'static str,
    validator: &'static str,
}

static ARN_VALIDATIONS: &[ArnValidation] = &[
    ArnValidation { service: "iam", resource: "Role", regex: "^arn:(aws|aws-cn|aws-us-gov):iam::[^:]*:role/.+$", validator: "validate_iam_role_arn_value" },
    ArnValidation { service: "iam", resource: "Policy", regex: "^arn:(aws|aws-cn|aws-us-gov):iam::[^:]*:policy/.+$", validator: "validate_iam_policy_arn_value" },
    ArnValidation { service: "iam", resource: "OidcProvider", regex: "^arn:(aws|aws-cn|aws-us-gov):iam::[^:]*:oidc-provider/.+$", validator: "validate_iam_oidc_provider_arn_value" },
    ArnValidation { service: "kms", resource: "Key", regex: "^arn:(aws|aws-cn|aws-us-gov):kms:[^:]*:[^:]*:key/.+$", validator: "validate_kms_key_arn_value" },
    ArnValidation { service: "s3", resource: "Bucket", regex: "^arn:(aws|aws-cn|aws-us-gov):s3:::.+$", validator: "validate_s3_bucket_arn_value" },
    ArnValidation { service: "cloudfront", resource: "Distribution", regex: "^arn:(aws|aws-cn|aws-us-gov):cloudfront::[^:]*:distribution/.+$", validator: "validate_cloudfront_distribution_arn_value" },
    ArnValidation { service: "ecs", resource: "Cluster", regex: "^arn:(aws|aws-cn|aws-us-gov):ecs:[^:]*:[^:]*:cluster/.+$", validator: "validate_ecs_cluster_arn_value" },
    ArnValidation { service: "dynamodb", resource: "Table", regex: "^arn:(aws|aws-cn|aws-us-gov):dynamodb:[^:]*:[^:]*:table/.+$", validator: "validate_dynamodb_table_arn_value" },
    ArnValidation { service: "logs", resource: "LogGroup", regex: "^arn:(aws|aws-cn|aws-us-gov):logs:[^:]*:[^:]*:log-group:.+$", validator: "validate_logs_log_group_arn_value" },
];

static KNOWN_SERVICES: &[&str] = &[
    "cloudfront",
    "dynamodb",
    "ec2",
    "ecs",
    "iam",
    "kms",
    "logs",
    "organizations",
    "s3",
    "wafv2",
];

fn arn_validation_for(service: &str, resource: &str) -> Option<&'static ArnValidation> {
    ARN_VALIDATIONS
        .iter()
        .find(|v| v.service == service && v.resource == resource)
}

fn arn_emit_choice(service: &str, resource: &str) -> ArnEmitChoice {
    if let Some(entry) = arn_validation_for(service, resource) {
        ArnEmitChoice::PerKind(entry)
    } else if let Some(&known) = KNOWN_SERVICES.iter().find(|&&known| known == service) {
        ArnEmitChoice::ServicePrefix(known)
    } else {
        ArnEmitChoice::Generic
    }
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc arn_emit_choice_has_table_service_and_generic_paths`

### Task B3: AWSCC codegen emits schema-owned `arn()`

**Goal**: CFN-generated resources with `Arn` properties define and use a resource-local `arn()` helper.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs` (modify)

**Test (Red)**:
```rust
fn cfn_schema_for_codegen_tests(type_name: &str) -> CfnSchema {
    let path = format!("cfn-schema-cache/{}.json", type_name.replace("::", "__"));
    let raw = std::fs::read_to_string(path).expect("cached CFN schema");
    serde_json::from_str(&raw).expect("cached CFN schema parses")
}

#[test]
fn generated_s3_bucket_owns_arn_helper() {
    let schema = cfn_schema_for_codegen_tests("AWS::S3::Bucket");
    let generated = generate_schema_code(&schema, "AWS::S3::Bucket").expect("generate s3 bucket");
    assert!(generated.contains("pub fn arn() -> AttributeType"));
    assert!(generated.contains("Some(super::provider_type(\"s3\", \"Bucket\", \"Arn\"))"));
    assert!(generated.contains("AttributeSchema::new(\"arn\", self::arn())"));
}

#[test]
fn generated_organizations_account_gets_service_prefix_arn_helper() {
    let schema = cfn_schema_for_codegen_tests("AWS::Organizations::Account");
    let generated = generate_schema_code(&schema, "AWS::Organizations::Account")
        .expect("generate organizations account");
    assert!(generated.contains("pub fn arn() -> AttributeType"));
    assert!(generated.contains("validate_service_arn(s, \"organizations\", None)"));
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc generated_`

**Implementation (Green)**:
```rust
fn emit_arn_helper(service: &str, resource: &str, choice: ArnEmitChoice) -> String {
    if matches!(choice, ArnEmitChoice::Generic) {
        return "pub fn arn() -> AttributeType { super::arn() }\n".to_string();
    }
    let (regex_expr, validator_expr) = match choice {
        ArnEmitChoice::PerKind(entry) => (
            format!("Some({:?}.to_string())", entry.regex),
            entry.validator.to_string(),
        ),
        ArnEmitChoice::ServicePrefix(service) => (
            format!("Some(\"^arn:(aws|aws-cn|aws-us-gov):{}:.*$\".to_string())", service),
            format!(
                "|value| if let Value::Concrete(ConcreteValue::String(s)) = value {{ validate_service_arn(s, {:?}, None).map_err(|reason| format!(\"Invalid {} ARN {{}}: {{}}\", s, reason)) }} else {{ Err(\"Expected string\".to_string()) }}",
                service, service
            ),
        ),
        ArnEmitChoice::Generic => unreachable!("handled before custom helper emission"),
    };
    format!(
        r#"pub fn arn() -> AttributeType {{
    AttributeType::custom(
        Some(super::provider_type("{service}", "{resource}", "Arn")),
        super::arn(),
        {regex_expr},
        None,
        legacy_validator({validator_expr}),
        None,
    )
}}
"#,
        service = service,
        resource = resource,
        regex_expr = regex_expr,
        validator_expr = validator_expr,
    )
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc generated_`

### Task B4: AWSCC override map uses schema-owned paths

**Goal**: all CFN string overrides use generated resource modules for IAM/KMS ARN types.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs` (modify)

**Test (Red)**:
```rust
#[test]
fn arn_overrides_point_to_resource_modules() {
    let overrides = known_string_type_overrides();
    assert_eq!(overrides.get("DeliverLogsPermissionArn"), Some(&"super::super::iam::role::arn()"));
    assert_eq!(overrides.get("KmsKeyArn"), Some(&"super::super::kms::key::arn()"));
    assert_eq!(override_type_to_display_name("super::super::iam::oidc_provider::arn()"), "IamOidcProviderArn");
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc arn_overrides_point_to_resource_modules`

**Implementation (Green)**:
```rust
m.insert("DeliverCrossAccountRole", "super::super::iam::role::arn()");
m.insert("DeliverLogsPermissionArn", "super::super::iam::role::arn()");
m.insert("PeerRoleArn", "super::super::iam::role::arn()");
m.insert("PermissionsBoundary", "super::super::iam::policy::arn()");
m.insert("ManagedPolicyArns", "super::super::iam::policy::arn()");
m.insert("KmsKeyId", "super::super::kms::key::arn()");
m.insert("KmsKeyArn", "super::super::kms::key::arn()");
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc arn_overrides_point_to_resource_modules`

### Task B5: AWSCC OIDC validator tests move to schema module

**Goal**: OIDC ARN validator tests currently in `carina-aws-types` move to `iam/oidc_provider.rs`.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/iam/oidc_provider.rs` (modify generated output)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-aws-types/src/lib.rs` (modify)

**Test (Red)**:
```rust
#[test]
fn arn_accepts_eks_multi_segment_oidc_provider() {
    let t = arn();
    let carina_core::schema::RawShape::Custom { validate, .. } = t.raw_shape() else {
        panic!("arn() should be custom");
    };
    let v = Value::Concrete(ConcreteValue::String(
        "arn:aws:iam::123456789012:oidc-provider/oidc.eks.us-east-1.amazonaws.com/id/AAAAAAAA000000".to_string(),
    ));
    assert!(validate(&v).is_ok());
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc arn_accepts_eks_multi_segment_oidc_provider`

**Implementation (Green)**:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, Value};

    #[test]
    fn arn_accepts_eks_multi_segment_oidc_provider() {
        let t = arn();
        let carina_core::schema::RawShape::Custom { validate, .. } = t.raw_shape() else {
            panic!("arn() should be custom");
        };
        let v = Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::123456789012:oidc-provider/oidc.eks.us-east-1.amazonaws.com/id/AAAAAAAA000000".to_string(),
        ));
        assert!(validate(&v).is_ok());
    }
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc arn_accepts_eks_multi_segment_oidc_provider`

### Task B6: AWSCC migrate generated consumers off hand-written helpers

**Goal**: replace every generated/codegen consumer from section 1.5 before deleting helper exports.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/bin/codegen.rs` (modify)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ec2/flow_log.rs` (modify generated)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ec2/vpc_peering_connection.rs` (modify generated)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ecs/cluster.rs` (modify generated)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/iam/oidc_provider.rs` (modify generated)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/iam/role.rs` (modify generated)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/logs/log_group.rs` (modify generated)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/s3/bucket.rs` (modify generated)

**Test (Red)**:
```bash
cargo check --workspace --all-targets 2>&1 | grep -E "^error" | wc -l
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo check --workspace --all-targets 2>&1 | grep -E "^error" | wc -l`

**Implementation (Green)**:
```rust
AttributeSchema::new("deliver_cross_account_role", super::super::iam::role::arn())
StructField::new("kms_key_id", super::super::kms::key::arn())
AttributeSchema::new("arn", self::arn())
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo check --workspace --all-targets`

### Task B7: AWSCC delete old hand-written helpers

**Goal**: remove IAM/KMS ARN helper exports after generated consumers and tests move.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-aws-types/src/lib.rs` (modify)

**Test (Red)**:
```rust
#[test]
fn carina_aws_types_no_longer_exports_resource_arn_helpers() {
    let source = std::fs::read_to_string("carina-aws-types/src/lib.rs").unwrap();
    assert!(!source.contains("pub fn iam_role_arn"));
    assert!(!source.contains("pub fn iam_policy_arn"));
    assert!(!source.contains("pub fn iam_oidc_provider_arn"));
    assert!(!source.contains("pub fn kms_key_arn"));
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-aws-types carina_aws_types_no_longer_exports_resource_arn_helpers`

**Implementation (Green)**:
```rust
// Delete pub fn iam_role_arn(), pub fn iam_policy_arn(),
// pub fn iam_oidc_provider_arn(), pub fn kms_key_arn().
// Keep arn(), validate_arn(), validate_service_arn(), validate_iam_arn(), and validate_kms_key_id().
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-aws-types carina_aws_types_no_longer_exports_resource_arn_helpers && cargo check -p carina-aws-types --all-targets && cargo check --workspace --all-targets`

### Task B8: AWSCC schema identity fixture

**Goal**: prove generated AWSCC schemas expose `awscc.s3.Bucket.Arn` and `awscc.cloudfront.Distribution.Arn` as structured custom type identities.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/s3/bucket.rs` (modify generated tests)
  - `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src/schemas/generated/cloudfront/distribution.rs` (modify generated tests)

**Test (Red)**:
```rust
#[test]
fn arn_identity_is_provider_scoped() {
    let t = arn();
    let carina_core::schema::RawShape::Custom { identity, .. } = t.raw_shape() else {
        panic!("arn() should be custom");
    };
    assert_eq!(identity.map(|id| id.to_string()).as_deref(), Some("awscc.s3.Bucket.Arn"));
}
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc arn_identity_is_provider_scoped`

**Implementation (Green)**:
```rust
pub fn arn() -> AttributeType {
    AttributeType::custom(
        Some(super::provider_type("s3", "Bucket", "Arn")),
        super::arn(),
        Some("^arn:(aws|aws-cn|aws-us-gov):s3:::.+$".to_string()),
        None,
        legacy_validator(validate_s3_bucket_arn_value),
        None,
    )
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test -p carina-provider-awscc arn_identity_is_provider_scoped`

### Task C1: Infra provider locks

**Goal**: consume provider releases that both know the final namespaces.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/infra/**/providers.crn` (modify)
  - `/Users/mizzy/src/github.com/carina-rs/infra/**/carina-providers.lock` (modify)

**Test (Red)**:
```bash
rg -n "carina-provider-(aws|awscc).*v0\\.5\\.0|carina-provider-(aws|awscc).*0\\.5\\.0" .
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/infra && rg -n "carina-provider-(aws|awscc).*v0\\.5\\.0|carina-provider-(aws|awscc).*0\\.5\\.0" .`

**Implementation (Green)**:
```crn
provider "awscc" {
  source  = 'github.com/carina-rs/carina-provider-awscc'
  version = '0.6.0'
}

provider "aws" {
  source  = 'github.com/carina-rs/carina-provider-aws'
  version = '0.6.0'
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/infra && ! rg -n "carina-provider-(aws|awscc).*v0\\.5\\.0|carina-provider-(aws|awscc).*0\\.5\\.0" .`

### Task C2: Infra AWSCC-owned IAM type flips

**Goal**: stop referring to AWSCC-owned IAM resources through the `aws.*` type namespace.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/infra/usecases/**/*.crn` (modify)
  - `/Users/mizzy/src/github.com/carina-rs/infra/modules/**/*.crn` (modify)
  - `/Users/mizzy/src/github.com/carina-rs/infra/aws/**/*.crn` (modify)

**Test (Red)**:
```bash
rg -n "aws\\.iam\\.(Role|OidcProvider|Policy)\\.Arn" usecases modules aws
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/infra && rg -n "aws\\.iam\\.(Role|OidcProvider|Policy)\\.Arn" usecases modules aws`

**Implementation (Green)**:
```crn
arguments {
  oidc_provider_arn: awscc.iam.OidcProvider.Arn
  sso_admin_role_arns: list(awscc.iam.Role.Arn)
  managed_policy_arns: list(awscc.iam.Policy.Arn) = []
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/infra && ! rg -n "aws\\.iam\\.(Role|OidcProvider|Policy)\\.Arn" usecases modules aws`

### Task C3: Infra issue-102 S3 and CloudFront type flips

**Goal**: replace string-typed ARN workarounds with generated AWSCC resource ARN types.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/infra/usecases/registry/app/main.crn` (modify)

**Test (Red)**:
```bash
rg -n "read_plane_bucket_arn\\s*:\\s*string|cloudfront_distribution_arn\\s*:\\s*string" usecases/registry/app/main.crn
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/infra && rg -n "read_plane_bucket_arn\\s*:\\s*string|cloudfront_distribution_arn\\s*:\\s*string" usecases/registry/app/main.crn`

**Implementation (Green)**:
```crn
arguments {
  read_plane_bucket_arn: awscc.s3.Bucket.Arn
  cloudfront_distribution_arn: awscc.cloudfront.Distribution.Arn
}
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/infra && ! rg -n "read_plane_bucket_arn\\s*:\\s*string|cloudfront_distribution_arn\\s*:\\s*string" usecases/registry/app/main.crn`

### Task C4: Infra validation

**Goal**: prove the flipped infra config validates against both released providers.

**Files**:
  - `/Users/mizzy/src/github.com/carina-rs/infra/envs/registry/dev/app` (verify)
  - `/Users/mizzy/src/github.com/carina-rs/infra/envs/registry/dev/infra` (verify)
  - `/Users/mizzy/src/github.com/carina-rs/infra/envs/registry/dev/network` (verify)
  - `/Users/mizzy/src/github.com/carina-rs/infra/envs/registry/dev/bootstrap` (verify)

**Test (Red)**:
```bash
carina validate envs/registry/dev/app
```

**Verify Red**: `cd /Users/mizzy/src/github.com/carina-rs/infra && carina validate envs/registry/dev/app`

**Implementation (Green)**:
```bash
carina providers install envs/registry/dev/app
carina providers install envs/registry/dev/infra
carina providers install envs/registry/dev/network
carina providers install envs/registry/dev/bootstrap
```

**Verify Green**: `cd /Users/mizzy/src/github.com/carina-rs/infra && carina validate envs/registry/dev/app && carina validate envs/registry/dev/infra && carina validate envs/registry/dev/network && carina validate envs/registry/dev/bootstrap`

AWS non-TDD regeneration step: after Tasks A1-A8 pass, run `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && ./scripts/generate-schemas-smithy.sh && ./scripts/generate-docs.sh`, then review generated schema and docs diffs together.

AWSCC non-TDD regeneration step: after Tasks B1-B8 pass, run the explicit codegen loop below and then docs generation. Evidence: `ls /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/scripts/` shows no `generate-schemas.sh`; `carina-provider-awscc/src/bin/codegen.rs` defines `--type-name`, `--file`, `--output`, `--print-dsl-resource-name`, and `--print-module-name`.

```bash
cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc
for schema in cfn-schema-cache/*.json; do
  type_name=$(basename "$schema" .json | sed 's/__/::/g')
  dsl=$(cargo run --quiet --bin codegen -- --type-name "$type_name" --print-dsl-resource-name)
  service=${dsl%%.*}
  module=$(cargo run --quiet --bin codegen -- --type-name "$type_name" --print-module-name)
  mkdir -p "carina-provider-awscc/src/schemas/generated/$service"
  set +e
  cargo run --quiet --bin codegen -- --file "$schema" --type-name "$type_name" --output "carina-provider-awscc/src/schemas/generated/$service/$module.rs"
  status=$?
  set -e
  if [ "$status" -eq 2 ]; then
    rm -f "carina-provider-awscc/src/schemas/generated/$service/$module.rs"
    continue
  fi
  if [ "$status" -ne 0 ]; then
    exit "$status"
  fi
done
./scripts/generate-docs.sh
```

Then review generated schema and docs diffs together.

AWS verify cycle:

1. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo check -p carina-aws-types -p carina-codegen-aws -p carina-provider-aws --all-targets`
2. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo nextest run -p carina-aws-types -p carina-codegen-aws -p carina-provider-aws`
3. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo test --workspace --doc`
4. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && cargo clippy --workspace --all-targets -- -D warnings`
5. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws && bash scripts/check-carina-pin.sh && bash scripts/check-examples.sh && bash scripts/check-string-enum-aliases.sh`

AWSCC verify cycle:

1. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo check -p carina-aws-types -p carina-provider-awscc --all-targets`
2. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo nextest run -p carina-aws-types -p carina-provider-awscc`
3. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo test --workspace --doc`
4. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && cargo clippy --workspace --all-targets -- -D warnings`
5. `cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc && bash scripts/check-carina-pin.sh && bash scripts/check-docs-drift.sh`

## 5. Coordination + release timeline

Step 1: Develop PR-A and PR-B in parallel on `issue-3392-per-resource-arn-newtypes`. Both PRs must include codegen output and docs output before review.

Step 2: Merge PR-A after AWS verify passes. Do not update infra yet.

Step 3: Merge PR-B after AWSCC verify passes. Do not update infra yet.

Step 4: The release owner reruns `gh api repos/carina-rs/carina-provider-aws/releases/latest --jq '.tag_name'` and `gh api repos/carina-rs/carina-provider-awscc/releases/latest --jq '.tag_name'` from a networked machine. If both latest releases are still `v0.5.0`, the release owner tags AWS as `carina-provider-aws@v0.6.0` from the PR-A merge commit and publishes the provider artifact. If a newer latest release exists, the release owner uses the next minor after that latest release for both provider repos.

Step 5: The same release owner tags AWSCC with the same coordinated next minor chosen in Step 4, for example `carina-provider-awscc@v0.6.0` when latest is `v0.5.0`, from the PR-B merge commit and publishes the provider artifact.

Step 6: Open PR-C in `carina-rs/infra`, update provider locks to the two tags published in Steps 4 and 5, then flip type references. PR-C is after the provider releases, not before or with them, because infra validation needs installable provider artifacts.

Step 7: Merge PR-C after `carina validate` passes for the listed registry/bootstrap environments.

The providers can be developed independently, but their releases are coordinated. The user must not be asked to run an infra tree where AWSCC resources require `awscc.*` type names while one provider lock still resolves to an older provider whose generated custom types remain under `aws.*`.

## 6. Verification gates checklist

- [ ] PR-A includes `PROVIDER_NAME = "aws"` and `pub fn provider_type(service: &str, resource: &str, kind: &str) -> TypeIdentity` in `/Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-aws-types/src/lib.rs`.
- [ ] PR-B includes `PROVIDER_NAME = "awscc"` and `pub fn provider_type(service: &str, resource: &str, kind: &str) -> TypeIdentity` in `/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-aws-types/src/lib.rs`.
- [ ] PR-A and PR-B have no `pub fn iam_role_arn`, `pub fn iam_policy_arn`, `pub fn iam_oidc_provider_arn`, or `pub fn kms_key_arn` left in `carina-aws-types/src/lib.rs`.
- [ ] PR-A and PR-B have no generated consumer calls to `super::iam_role_arn()`, `super::iam_policy_arn()`, `super::iam_oidc_provider_arn()`, or `super::kms_key_arn()`.
- [ ] PR-A runs `./scripts/generate-schemas-smithy.sh` and `./scripts/generate-docs.sh`, and the PR description lists the generated files changed.
- [ ] PR-B runs the `for schema in cfn-schema-cache/*.json` codegen loop from section 4 and `./scripts/generate-docs.sh`, and the PR description lists the generated files changed.
- [ ] PR-A passes `cargo check -p carina-aws-types -p carina-codegen-aws -p carina-provider-aws --all-targets`.
- [ ] PR-A passes `cargo nextest run -p carina-aws-types -p carina-codegen-aws -p carina-provider-aws`.
- [ ] PR-A passes `cargo test --workspace --doc`.
- [ ] PR-A passes `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] PR-A passes `bash scripts/check-carina-pin.sh`, `bash scripts/check-examples.sh`, and `bash scripts/check-string-enum-aliases.sh`.
- [ ] PR-B passes `cargo check -p carina-aws-types -p carina-provider-awscc --all-targets`.
- [ ] PR-B passes `cargo nextest run -p carina-aws-types -p carina-provider-awscc`.
- [ ] PR-B passes `cargo test --workspace --doc`.
- [ ] PR-B passes `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] PR-B passes `bash scripts/check-carina-pin.sh` and `bash scripts/check-docs-drift.sh`.
- [ ] PR-C updates provider locks to the two coordinated tags published from PR-A and PR-B; with current local evidence that is `carina-provider-aws@v0.6.0` and `carina-provider-awscc@v0.6.0`.
- [ ] PR-C has no remaining `aws.iam.Role.Arn`, `aws.iam.OidcProvider.Arn`, or `aws.iam.Policy.Arn` references for AWSCC-owned resources.
- [ ] PR-C uses `awscc.s3.Bucket.Arn` and `awscc.cloudfront.Distribution.Arn` for the issue-102 registry app contract.
- [ ] PR-C passes `carina validate envs/registry/dev/app`, `carina validate envs/registry/dev/infra`, `carina validate envs/registry/dev/network`, and `carina validate envs/registry/dev/bootstrap`.
