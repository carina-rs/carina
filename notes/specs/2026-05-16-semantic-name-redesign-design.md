# Semantic Name Redesign — Design

Status: **Format decided** — Option C (dotted, PascalCase tail:
`aws.iam.Role.Arn`). Projection-owner design is the open implementation
question.
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
| A. Status quo | `IamRoleArn` | Yes | **Rejected** — gives neither disambiguation nor namespace consistency |
| B. PascalCase, lowercase tail | `IamRoleArn` (documented) | Yes | **Rejected** — same reason as A |
| C. Dotted namespaced, Pascal tail | `aws.iam.Role.Arn` | **No** — needs new projection owner | redesign inference/parser/LSP + 3-repo sweep |
| D. Dotted namespaced, upper tail | `aws.iam.Role.ARN` (user's original) | **No** | as C, plus `ARN` all-caps diverges from the existing namespaced-identifier tail convention (`enabled`, lowercase) — see strict-enum-identifier design |

A and B are struck because the resolved goal requires **both**
disambiguation and namespace consistency (next section).

**Decided (user, 2026-05-16): Option C — `aws.iam.Role.Arn`.** The
PascalCase tail (`Arn`, not `ARN`) is chosen because the existing
namespaced-identifier convention keeps tails in a lower/PascalCase
register (`enabled`), and an all-caps `ARN` segment visually diverges
from that convention (see strict-enum-identifier design). The
acronym-uppercasing question is closed; the remaining open item is
purely the projection-owner architecture below.

### Resolved direction (user, 2026-05-16)

- **Goal: both.** The format must simultaneously give
  *disambiguation* (the resource × kind axes must be separately
  readable in the name) **and** *namespace consistency* with the DSL's
  existing dotted identifiers (`aws.iam.Role`,
  `aws.s3.Bucket.VersioningStatus.enabled`). A format that satisfies
  only one is rejected. This eliminates options A and B (flat
  PascalCase gives neither cleanly) and points at a dotted,
  axis-structured form — i.e. the work is in option C/D territory and
  the projection redesign below is unavoidable, not optional.
- **Scope: the whole `semantic_name` surface, not just ARNs.** All
  families — ID (31), ARN (4–7), and the enum-ish entries
  (`IamPolicyEffect`, `IamPolicyVersion`, `PolicyConditionValue`, …) —
  across both the aws and awscc copies. The rename is uniform; there
  is no "ARN-only" partial rollout.

### The real design work: who owns the projection?

"Projection" = the step that converts a schema-side type identity
(`semantic_name`) into the declaration-side form a user writes in a DSL
type annotation, so the two can be matched.

Today this projection is trivial and **owned implicitly by
`pascal_to_snake`**:

```
schema side                         declaration side (user's DSL)
Custom { semantic_name:             let x: iam_role_arn = ...
  "IamRoleArn" }            ←──────────────  ^^^^^^^^^^^^
        │ pascal_to_snake
        ▼
   "iam_role_arn"  ── string-equality compare ──┘
```

`validation/inference.rs:561-564` maps
`Custom { semantic_name: Some(name) }` →
`TypeExpr::Simple(pascal_to_snake(name))`; `parser/util.rs` defines the
inverse so the strings line up. No component "owns" the type-name
representation — it is an emergent property of one string transform.

A dotted, axis-structured name (`aws.iam.Role.Arn`) breaks this,
raising the questions that are the actual design:

1. **What does the user write?** Is the DSL annotation literally
   `aws.iam.Role.Arn`, or a shorter alias? The dotted form already
   means *resource reference* (`aws.iam.Role`) and *enum value*
   (`aws.s3.Bucket.VersioningStatus.enabled`) in the grammar — a third
   meaning (type name) needs a disambiguation rule the parser can
   apply.
2. **Which component resolves it?** The parser
   (`parser/carina.pest` + `parser/util.rs`), type inference
   (`inference.rs`), or a *new* explicit type-name registry that owns
   the mapping rather than leaving it emergent? This is the
   architectural decision the implementation PR cannot avoid.
3. **How do LSP completion/diagnostics
   (`completion/values.rs:1947`, `diagnostics/mod.rs:535`) consume the
   new identity?** They currently hard-assume single-token PascalCase.

The recommendation of this doc is that the implementation phase must
**introduce an explicit owner for type-name representation** (a
registry / dedicated `TypeExpr` variant) rather than extend the
implicit `pascal_to_snake` round-trip. Renaming the strings without
this is the failure mode that turns a "rename PR" into an unscoped
type-system change mid-flight.

## Scope / sequencing (per project memory)

- Design doc PR must merge **before** any implementation PR
  (`feedback_design_before_implementation_in_pr`).
- 1 PR = 1 topic (`feedback_scope_discipline`); the eventual
  implementation spans 3 repos (carina + aws + awscc copies) and must
  be sequenced, not bundled.
- Real-infra smoke against `carina-rs/infra` is user-driven, not part
  of this doc.

## Non-goals

- Designing the projection-owner architecture in detail (registry vs
  `TypeExpr` variant vs parser rule) — that is the implementation
  PR's job; this doc only mandates that an explicit owner exist.
- Any code change in this PR — documentation only. Scope is resolved
  as the **whole `semantic_name` surface** (ID + ARN + enum-ish,
  aws + awscc); no partial ARN-only rollout.
