# Semantic Name Redesign — Design

Status: **All policy decided — no open questions.** Structured
type-identity keyed on `provider + service/resource + kind`;
generic/anonymous boundary is per-axis emptiness (no new flag); generic
ARN is `aws.Arn` (AWS-scoped). The dotted identifier is **input
syntax** (E-1) and rides the **existing** `TypeExpr::SchemaType`
mechanism — no grammar change. The string round-trip is killed at all
boundaries (WIT contract / `ExpectedEnumVariant` / `Custom.namespace` /
`type_name()` API) in one effort by collapsing the **four** existing
provider-axis representations onto one shared axis-struct, with a
single owner for identity + `is_schema_type` resolution + projection.
Only mechanism (struct shape, WIT wire encoding, registry concrete
form) is left to the implementation phase.
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

## Boundary principle (the "what is generic" line)

The question "`Region` must be provider-distinct, but a bare `String`
wrapper must not be" turns out **not** to need a new classification
flag. The existing type system already encodes the line, and the
struct generalizes it:

### The line is per-axis emptiness, not a generic/specific flag

`semantic_name: None` already means "no identity — anonymous wrapper"
(`schema/mod.rs:1303` anonymous-source rule). `Some(name)` means
"named identity". The struct generalizes this binary into per-axis
emptiness:

| Type | provider | service/resource | Meaning |
| --- | --- | --- | --- |
| bare `String` wrapper | — (none) | — | no identity at all (existing `semantic_name: None`) — unchanged, no new work |
| generic `Arn` → **`aws.Arn`** | `aws` | empty | AWS-scoped, resource-agnostic base |
| `aws.Region` | `aws` | empty/Region | provider-distinct from `gcp.Region` |
| `aws.iam.Role.Arn` | `aws` | iam/Role | fully specified, narrowest |

**Principle: two types are the same type iff every *populated* axis is
equal. An empty axis means "not distinguished on this axis" — it is
not a wildcard; it places the type *higher in the base chain* (wider).**
The truly anonymous wrapper (all axes empty) is exactly today's
`semantic_name: None` behavior, preserved for free.

### Generic `Arn` is `aws.Arn`, not provider-agnostic (user, 2026-05-16)

The generic `arn()` carries `pattern: ^arn:(aws|aws-cn|aws-us-gov):` —
it is **AWS-specific**, not provider-neutral. GCP has no ARN concept;
`gcp.Arn` does not exist. So the generic ARN is **`aws.Arn`**:
`provider = aws`, service/resource axes empty. Its "generic" quality is
*resource-agnostic within AWS*, not *provider-agnostic*. This makes the
base chain provider-axis-monotone:

```
aws.iam.Role.Arn  →  aws.Arn  →  String
{aws, iam, Role}     {aws, —}     (no identity)
provider stays `aws` down the chain, then relaxes to none.
```

**Base-chain invariant:** descending `base` the provider axis is
monotone — it never *changes* provider, only stays equal or relaxes to
empty. Rule 4's recursive `base` check must preserve this; an
implementation that lets provider flip mid-chain is a bug.

### What rule 3 becomes

Today rule 3 is raw string inequality ⇒ NG. Under the struct it
becomes: **same type iff all populated identity axes are equal**, which
yields exactly the required distinctions —
`aws.iam.Role.Arn ≠ aws.acm.Certificate.Arn` (service/resource differ),
`aws.Region ≠ gcp.Region` (provider differs), bare wrapper stays
identity-free. No separate generic/specific flag is introduced; the
`Some/None` binary is simply generalized per axis.

## Cross-cutting principle: kill the string round-trip everywhere

The "collapse a structure into a string, then `split`/`pascal_to_snake`
it back" anti-pattern this redesign removes is **not localized to
`semantic_name`**. Code inspection (2026-05-16) found it already
present in three places:

1. **WIT boundary** — `validate-custom-type(type-name: string)`
   (`provider.wit:114`) is called with `pascal_to_snake(type_name)`
   (`parser/functions.rs:205,350,452`); the plugin re-keys off that
   string.
2. **`ExpectedEnumVariant::from_namespaced`** — recovers axes via
   `ns.split('.')` (`schema/mod.rs:~1762`).
3. **Internal `type_name()` API** — used for type comparison in 6+
   `validation/mod.rs` sites, including the double round-trip
   `pascal_to_snake(&current.type_name())` at `mod.rs:471`.

**Principle: structure the identity at all three boundaries in the
same effort. A partial migration that keeps any one of them
string-keyed just relocates the anti-pattern** (project memory
`feedback_root_cause_over_per_site_patch`). The per-boundary policy:

### A. WIT contract — restructure, do not pass a string

`validate-custom-type`'s `type-name: string` parameter must become a
structured record carrying the identity axes. Passing the dotted
string and having the aws/awscc plugin `split('.')` it merely exports
the anti-pattern across the plugin boundary. This is a **breaking WIT
contract change** (`carina-plugin-wit` + aws + awscc), a policy
decision, not a mechanism detail. Safety valve: the mock provider does
**not** implement `validate-custom-type`, so `carina-plugin-host`
tests are insulated; blast radius is the aws/awscc plugins only.

### B/F/E. Unify the FOUR existing provider-axis representations

The provider/service axis is **already** represented four times. The
fourth one resolves E.

- `semantic_name: String` — flat, schema side (the headline target).
- `ExpectedEnumVariant { provider: Option<String>, segments:
  Vec<String>, type_name: String }` (`schema/mod.rs`) — nearly the
  exact target shape, plus enum-only `value`/`is_alias`, and a
  `Display` that already does struct→dotted projection. Its
  `from_namespaced` recovers axes via `split('.')`.
- `Custom { namespace: Option<String> }` — e.g. `"aws.vpc"`, a
  provider+service string used for enum-shorthand resolution.
- **`TypeExpr::SchemaType { provider, path, type_name }`**
  (`parser/ast.rs:111`) — the **input-side** representation. Users
  **already** write `fn f(x: awscc.ec2.VpcId)`; `parser/types.rs:142`
  parses the dotted annotation and, when the tail is PascalCase and
  `config.is_schema_type(provider, path, type_name)` is true, produces
  `SchemaType`, else falls back to `Ref` (resource-type reference).

**E is resolved by this, not open.** E-1 ("dotted name is also input
syntax") needs **no grammar extension** — the `SchemaType` atom and
its `Ref`-vs-`SchemaType` disambiguation already exist. An earlier
draft's claim that `type_simple` is a dotless identifier and a grammar
change is required was **wrong** (it overlooked `type_ref` →
`SchemaType`) and is withdrawn. E-1 is satisfied by riding the
existing `SchemaType` mechanism.

**Policy: collapse all four onto one shared axis-struct identity.**
`SchemaType` is the input-syntax face of that identity;
`semantic_name`'s structured form is the identity itself;
`ExpectedEnumVariant` is the identity + `value`/`is_alias`;
`Custom.namespace` carries the same axes. Do **not** add a fifth
parallel struct. Consequence: the input→identity match
(`SchemaType` annotation vs the attribute's `semantic_name`), today
routed through `type_name()` string comparison in `validation/mod.rs`
(the very anti-pattern D removes), becomes a **structural** comparison
with no string in the path. `from_namespaced`'s `split('.')` and
`Custom.namespace`'s string form are replaced by the structure
directly.

**Ownership: the `is_schema_type(provider, path, type_name)`
resolver** (`parser/types.rs` disambiguation) and the identity
registry are the **same owner**. Whatever component owns "is this a
registered schema type" must also own identity comparison and the
struct→dotted projection — one owner for the axis-struct, not the
current split between parser context and schema. This is a
*policy* (single ownership) decision; the registry's concrete form is
mechanism.

### D. `type_name()` becomes display-only

`type_name() -> String` is consumed for **type comparison** in 6+
sites; comparison must move to a structure-based predicate
(`is_same_identity(&TypeIdentity)` or similar). `type_name()` is
demoted to **display-only**, and using it for comparison becomes a
documented invariant violation. `accepts_type_name(&str)` takes the
structured identity or is removed.

### C. State persistence — verified safe, no migration

`carina-state/src/` does **not** persist `semantic_name` / `type_name`
/ `TypeExpr`. Type identity is not baked into state v3, so the
restructure needs **no state migration** and breaks no existing state
files. Recorded here so the implementation PR does not invent a
migration.

### E. Dotted identifier is input syntax — RESOLVED (rides existing `SchemaType`)

Decided E-1 (user, 2026-05-16): the dotted name is writable in type
annotations, not display-only. Critically, this needs **no grammar
extension**: `parser/ast.rs:111` `TypeExpr::SchemaType { provider,
path, type_name }` and the `parser/types.rs:142` `Ref`-vs-`SchemaType`
disambiguation (PascalCase tail + `config.is_schema_type(...)`)
**already implement input-side dotted type annotations**
(`fn f(x: awscc.ec2.VpcId)` parses today). E-1 is satisfied by
unifying the new identity with `SchemaType` (see B/F/E above), not by
touching the grammar. The earlier "grammar change required" framing
was a mistake and is withdrawn.

## Remaining mechanism (implementation phase, not policy)

- Exact struct field shape and the WIT record layout it implies.
- How an empty axis is encoded on the wire (`Option` per axis vs
  sentinel) — must make the base-chain monotone invariant
  un-violable by construction, not by convention.
- The struct → dotted display projection implementation.

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
