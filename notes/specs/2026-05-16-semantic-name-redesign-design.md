# Semantic Name Redesign — Design

Status: **Direction decided** — structured type-identity keyed on
`provider + service + resource + kind`. User-facing surface stays a
dotted string (`aws.iam.Role.Arn`). Implementation-architecture detail
(exact struct shape, WIT contract, generic-type wildcard handling) is
the open question for the implementation phase.
Date: 2026-05-16

<!-- constrained-by ./2026-05-12-strict-enum-identifier-design.md -->

> **Revision note.** Earlier drafts of this doc framed the problem as
> *display-string inconsistency* and proposed renaming the flat
> PascalCase `semantic_name` to a dotted string. That framing
> mistook the motivation. The real driver (clarified by the user,
> 2026-05-16) is **multi-provider type-identity collision**. The
> display-only analysis and the "Option A–D / projection-owner"
> sections are superseded by what follows.

## Problem (corrected)

`carina-aws-types` attaches a flat PascalCase `semantic_name`
(`Region`, `IamRoleArn`, `VpcId`, …) to every
`AttributeType::Custom`. That string is **both** the internal
type-identity key and the user-facing display name. The type-identity
half is the problem.

Type compatibility is decided by `semantic_name` string equality:
`carina-core/src/schema/mod.rs:1303` — *Custom→Custom with both
`semantic_name: Some` and names differ ⇒ NG*. The contrapositive is
the bug: **same `semantic_name` ⇒ treated as the same type.**

Today every provider is AWS, so this is latent. It stops being latent
the moment a second provider lands. Concrete example (user's): add a
Google Cloud provider and both define a `region` attribute:

- `aws` region: `"ap-northeast-1"` (hyphen-segmented pattern)
- `gcp` region: `"asia-northeast1"` (different pattern)

If both carry `semantic_name = "Region"`, the type system declares
them the **same type**. An `aws` region value would validate as
acceptable where a `gcp` region is expected, and the pattern checks
cross-contaminate. `region` is just the first instance; any
cross-provider concept with a shared noun but a provider-specific
format (ARNs, resource IDs, account/project identifiers, zones) has
the same failure.

This is a **type-safety correctness** problem, not a presentation
problem. The fix must put provider (and service/resource) into the
type-identity key so that `aws`'s `Region` and `gcp`'s `Region` are
**distinct types**.

## Inventory (as of 2026-05-16)

Both provider repos carry an independent copy of `carina-aws-types`
(triplicated; see project memory `project_aws_types_triplicated_copies`).
From `carina-provider-aws/carina-aws-types/src/lib.rs` (35 names) and
`carina-provider-awscc/carina-aws-types/src/lib.rs` (41 names) — the
two copies are **asymmetric**, so any change is per-repo and
reconciled, not mechanically diffed.

- **ID family (~31):** `VpcId`, `SubnetId`, `IamRoleId`,
  `AwsAccountId`, `KmsKeyId`, … (awscc adds `IdentityStoreId`).
- **ARN family (4–7):** `Arn` (generic), `IamRoleArn`, `IamPolicyArn`,
  `KmsKeyArn` (awscc adds `SsoInstanceArn`, `SsoPermissionSetArn`,
  `IamOidcProviderArn`).
- **enum-ish:** `IamPolicyEffect`, `IamPolicyVersion`,
  `PolicyConditionValue` (last one in `carina-core/src/value.rs:2975`).

Scope of the eventual change is the **whole surface** (all families,
both aws and awscc copies), not an ARN-only or Region-only slice — the
collision class is general.

## Where `semantic_name` is load-bearing

It is consumed in two distinct roles:

1. **Type-identity key.** `schema/mod.rs:1303` Custom→Custom
   assignability; `validation/inference.rs:561-564`
   (`semantic_name → TypeExpr::Simple(pascal_to_snake(name))`);
   `parser/util.rs:31` defines the inverse so the strings line up.
2. **User-facing display.** `schema/mod.rs:1306` `type_name()` →
   `custom_display_name(semantic_name, …)` surfaces it verbatim in
   validate errors and LSP diagnostics
   (`carina-lsp/src/diagnostics/mod.rs:535`,
   `completion/values.rs:608-629`).

The redesign must fix role 1 while keeping role 2's output a clean
dotted string. **These two are separable** (see Decision).

## Decision

### What the user sees: unchanged in shape, dotted string

The display surface is a dotted identifier consistent with the DSL's
existing namespaced identifiers (`aws.iam.Role`,
`aws.s3.Bucket.VersioningStatus.enabled`): e.g. `aws.iam.Role.Arn`,
`aws.Region`, `gcp.Region`. Tail casing is PascalCase (`Arn`, not
`ARN`) to match the existing namespaced-identifier tail register
(see strict-enum-identifier design). This satisfies the secondary
benefits (disambiguation + namespace consistency) for free — but they
are *consequences*, not the goal. The goal is collision-free identity.

### Internal representation: structured, not a string

`semantic_name: Option<String>` becomes a **structured type identity**
keyed on `provider + service + resource + kind` (exact field shape is
implementation-phase work). Identity comparison is field equality;
the user-facing dotted string is *derived* from the structure via a
`Display`-style projection. The string is an output, never the source
of truth, and is never re-parsed to recover the axes.

Identity axis depth (user decision, 2026-05-16):
**provider + service/resource.** `aws.iam.Role.Arn` and
`aws.acm.Certificate.Arn` are distinct types; `aws.Region` and
`gcp.Region` are distinct types.

### Why structured over a dotted string (rationale)

A dotted-string key (`semantic_name = "aws.iam.Role.Arn"`) would also
make `aws.Region ≠ gcp.Region`, so on the collision requirement alone
the two are equivalent. The structure wins on long-term type-safety
grounds (consistent with project memory
`feedback_type_safety_over_runtime_checks`,
`feedback_long_term_and_type_safety`, `feedback_strict_typing`):

1. **The motivation is type safety itself.** Provider-keyed identity
   is the requirement. A struct makes "different `provider` ⇒
   different type" self-evident at the type level; a string defers it
   to equality semantics that a reader must reconstruct.
2. **A string re-introduces the exact anti-pattern this redesign
   removes.** Collapsing axes into `"aws.iam.Role.Arn"` and recovering
   them with `split('.')` is the same implicit, fragile round-trip as
   today's `pascal_to_snake`. `split` cannot reject malformed
   `"aws..Role"`; a struct cannot represent it.
3. **Extensibility under more providers.** Each new provider (GCP,
   then Azure, …) is a *data* addition to a named field, not a new
   "which dotted segment is the provider" convention duplicated across
   every consumer site.
4. **Cost is necessary, not waste.** The WIT-contract / aws / awscc
   blast radius is the price of enforcing the collision boundary in
   the type system. An earlier draft's "don't change the internal
   key" conclusion mistook the motivation and is withdrawn.

## Open question for the implementation phase

**Generic / provider-agnostic types.** Some Custom types should NOT be
provider-partitioned: a bare `String`-based wrapper, a truly generic
`Arn`. Partitioning those by provider would wrongly reject valid
cross-provider assignment. The struct must express "this identity has
no provider axis / matches across providers" explicitly — e.g.
`provider: Option<…>` or an explicit `Generic` variant. A string key
would face the same question implicitly and worse; the struct makes it
a first-class, designed decision. Resolving the exact shape (and the
WIT-contract change it implies for aws/awscc) is the implementation
PR's job; this doc only fixes the *direction*.

## Scope / sequencing (per project memory)

- This design doc PR merges **before** any implementation PR
  (`feedback_design_before_implementation_in_pr`).
- Implementation spans 3 repos (carina + aws + awscc copies) plus the
  WIT contract; sequenced, not bundled (`feedback_scope_discipline`).
- The aws/awscc `carina-aws-types` copies are asymmetric and must each
  be migrated and reconciled (`project_aws_types_triplicated_copies`).
- Real-infra smoke against `carina-rs/infra` is user-driven, not part
  of this doc.

## Non-goals

- The exact struct field shape / WIT contract / generic-type wildcard
  mechanism — implementation-phase work; this doc fixes direction only.
- Any code change in this PR — documentation only.
- A partial (ARN-only / Region-only) rollout — the collision class is
  general; scope is the whole `semantic_name` surface.
