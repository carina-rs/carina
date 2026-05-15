# Semantic Name Redesign — Design

Status: **Draft / discussion** (naming format intentionally NOT yet decided)
Date: 2026-05-16

<!-- constrained-by ./2026-05-12-strict-enum-identifier-design.md -->

## Problem

`carina-aws-types` attaches a flat PascalCase `semantic_name` to every
`AttributeType::Custom` it produces — `IamRoleArn`, `IamPolicyArn`,
`KmsKeyArn`, `VpcId`, `SubnetId`, … The name conflates two unrelated
axes into one undifferentiated token:

- **Resource the value points at** — `IamRole`, `Vpc`, `Subnet`.
- **Kind of reference** — an ARN, an ID, an Effect enum, etc.

There is no namespacing and no relationship to the DSL's existing
dotted-identifier convention (`aws.iam.Role`, `aws.s3.Bucket`). The
user's prompting question — *"should these be renamed, e.g.
`aws.iam.Role.ARN`?"* — is the trigger for this document.

This document records the **facts and constraints** so a naming format
can be chosen with the trade-offs visible. It does **not** pick a
format yet (per the decision to defer until the survey is in hand).

## Inventory (as of 2026-05-16)

Both provider repos carry an independent copy of `carina-aws-types`
(triplicated; see project memory `project_aws_types_triplicated_copies`).
Counts below are from
`carina-provider-aws/carina-aws-types/src/lib.rs` and
`carina-provider-awscc/carina-aws-types/src/lib.rs`.

### ID family (largest group)

`AwsResourceId`, `VpcId`, `SubnetId`, `SecurityGroupId`,
`InternetGatewayId`, `RouteTableId`, `NatGatewayId`,
`VpcPeeringConnectionId`, `TransitGatewayId`,
`VpcCidrBlockAssociationId`, `TgwRouteTableId`, `VpnGatewayId`,
`EgressOnlyInternetGatewayId`, `VpcEndpointId`, `InstanceId`,
`NetworkInterfaceId`, `AllocationId`, `PrefixListId`,
`CarrierGatewayId`, `LocalGatewayId`, `NetworkAclId`,
`TransitGatewayAttachmentId`, `FlowLogId`, `IpamId`,
`SubnetRouteTableAssociationId`, `SecurityGroupRuleId`, `IamRoleId`,
`AwsAccountId`, `KmsKeyId`, `IpamPoolId`, `AvailabilityZoneId`
(awscc adds `IdentityStoreId`).

### ARN family

`Arn` (generic), `IamRoleArn`, `IamPolicyArn`, `KmsKeyArn`
(awscc adds `SsoInstanceArn`, `SsoPermissionSetArn`,
`IamOidcProviderArn`).

### Enum family (awscc only, currently)

`IamPolicyEffect`, `IamPolicyVersion`, plus `SsoPrincipalId` and
`PolicyConditionValue` (the latter is defined in
`carina-core/src/value.rs:2975`, not in the aws-types copy).

### Asymmetry note

aws and awscc copies are **not** identical: awscc has 41 semantic
names, aws has 35; the SSO/Identity-Store and OIDC entries exist only
in awscc. Any rename must be applied per-repo and reconciled, not
diffed mechanically.

## The load-bearing constraint: `semantic_name` is a type-identity key

`semantic_name` is **not** a display-only label. It participates in
type-compatibility checking through a PascalCase ⇄ snake_case
round-trip:

- `carina-core/src/validation/inference.rs:561-564` —
  `Custom { semantic_name: Some(name), .. }` →
  `TypeExpr::Simple(pascal_to_snake(name))`. The DSL annotation arm
  matches by `pascal_to_snake` equality, so the token **must** survive
  a `PascalCase → snake_case` projection that lines up with what the
  user writes in a type annotation.
- `carina-core/src/parser/util.rs:31` — snake→Pascal conversion is
  defined so its result *matches* `semantic_name` values.
- `carina-lsp/src/completion/values.rs:1947,1981-1987` and
  `top_level.rs:975,994` — LSP carries the PascalCase form as
  `semantic_name` and assumes single-token PascalCase.
- `carina-lsp/src/diagnostics/mod.rs:535,552` — diagnostics read
  `semantic_name` for editor warnings (ARN family branch at
  `completion/values.rs:608-629`).
- `carina-core/src/upstream_exports.rs:2094-2151` and
  `value.rs:2975` — hand-written sites embed literal semantic names
  (`KmsKeyArn`, `Arn`, `AwsAccountId`, `PolicyConditionValue`); these
  must move in lockstep.

**Implication:** the format choice is bounded by this pipeline. A flat
PascalCase token (`IamRoleArn`) round-trips today. A dotted namespaced
form (`aws.iam.Role.ARN`) does **not** survive `pascal_to_snake`
unchanged and would require redesigning the projection in
`inference.rs` / `parser/util.rs` / LSP completion — i.e. it is not a
rename, it is a type-identity-representation change.

## Naming format options (NOT yet decided)

| Option | Example | Round-trips through current pipeline? | Cost |
| --- | --- | --- | --- |
| A. Status quo | `IamRoleArn` | Yes | none |
| B. PascalCase, lowercase tail | `IamRoleArn` → keep, document convention | Yes | docs only |
| C. Dotted namespaced, Pascal tail | `aws.iam.Role.Arn` | **No** — needs new projection | redesign inference/parser/LSP + 3-repo sweep |
| D. Dotted namespaced, upper tail | `aws.iam.Role.ARN` (user's original) | **No** | as C, plus `ARN` all-caps diverges from the existing namespaced-identifier tail convention (`enabled`, lowercase) — see strict-enum-identifier design |

Open sub-questions to resolve before picking:

1. Is the goal **disambiguation** (resource × kind) or **namespace
   consistency** with DSL identifiers? They imply different formats.
2. Does the ID family need the same treatment as the ARN family, or
   only ARNs? (31 ID names vs 4–7 ARN names — scope differs hugely.)
3. If dotted: who owns the projection that maps the dotted identity to
   a `TypeExpr`? This is the real design work, not the string.

## Scope / sequencing (per project memory)

- Design doc PR must merge **before** any implementation PR
  (`feedback_design_before_implementation_in_pr`).
- 1 PR = 1 topic (`feedback_scope_discipline`); the eventual
  implementation spans 3 repos (carina + aws + awscc copies) and must
  be sequenced, not bundled.
- Real-infra smoke against `carina-rs/infra` is user-driven, not part
  of this doc.

## Non-goals

- Picking the final format (deferred by explicit instruction).
- Touching the ID family unless the resolved scope includes it.
- Any code change in this PR — documentation only.
