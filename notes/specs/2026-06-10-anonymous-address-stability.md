# Stable anonymous resource addresses across provider lock bumps

<!-- derived-from ./2026-06-07-unified-aws-type-identity-design.md -->
<!-- derived-from ./2026-06-07-enum-state-coherence-design.md -->
<!-- constrained-by ../../carina-core/src/identifier/mod.rs -->
<!-- constrained-by ../../carina-core/src/schema/mod.rs -->

## Status

Design PR for carina#3428.

This document records the root cause and intended fix direction. It deliberately
does not implement the fix. The companion tests in
`carina-core/src/identifier/tests.rs` are expected to fail until the
implementation PR canonicalizes enum hash inputs.

## Background

Carina allows resources to be declared without a user-chosen `let` binding. Such
resources are anonymous in the source DSL, but they still need stable state
addresses so `plan`, `apply`, and later refreshes can correlate desired
resources with existing state.

The current address shape is derived in `compute_anonymous_identifiers`. For a
resource with an empty `ResourceId.name`, Carina builds a deterministic hash
input from:

- provider identity attributes such as `region`;
- schema create-only attributes, when the schema has them and the user set
  them;
- schema identity attributes;
- otherwise, all user-specified resource attributes through the SimHash path.

The final resource name includes the provider, resource type, and hash suffix,
for example `awscc_ec2_route_4ca12a2c` or
`awscc_ec2_eip_a3f2b1c8d79f1524`.

The separate `reconcile_anonymous_identifiers` pass is meant to absorb expected
hash movement. It has two important paths:

- for schemas with create-only values, it can restore the state identifier when
  at least one create-only value still matches and at least one differs;
- for schemas without create-only values, it compares old and new SimHash
  suffixes and emits a state rename when the Hamming distance is below
  `SIMHASH_HAMMING_THRESHOLD`.

Those reconciliation paths are useful for real changes. They are not sufficient
when the hash input itself changes even though the AWS value did not.

## Problem

carina#3428 was observed in `carina-rs/infra` after a provider lock bump:

- `providers.crn` changed `region = awscc.Region.ap_northeast_1` to
  `region = aws.Region.ap_northeast_1`;
- `vpc.crn` changed `domain = awscc.ec2.Eip.Domain.vpc` to
  `domain = aws.ec2.Eip.Domain.vpc`;
- `carina-providers.lock` moved aws/awscc provider revisions forward.

No resource declarations changed semantically. No AWS-side state changed.
Nevertheless `carina plan` reported:

```text
Plan: 5 to add, 0 to change, 1 to replace, 5 to destroy.
```

The displayed add/destroy pairs had identical real attributes but different
anonymous hash suffixes:

- `awscc.ec2.Route _29b007e8` vs `_4ca12a2c`;
- `awscc.ec2.Route _6f59d539` vs `_a25b5aff`;
- `awscc.ec2.VpcEndpoint _0715b068` / `_1ace2855` vs `_22629b6a` /
  `_8c1d9364`.

The route table association case is more dangerous than cosmetic churn: one
anonymous address was rebound to a different `(route_table_id, subnet_id)` pair,
so the plan looked like a real `forces_replacement` update rather than a pure
address drift.

The minimal cause is that two DSL enum spellings can denote the same AWS API
value, but the anonymous hash treats the DSL spelling itself as the value.

## Root cause

`deterministic_value_string` formats enum values like this:

```rust
Value::Concrete(ConcreteValue::EnumIdentifier(s)) => format!("EnumIdentifier({:?})", s),
```

The parser builds `ConcreteValue::EnumIdentifier` from the raw dotted identifier
string:

```rust
ConcreteValue::EnumIdentifier(full_str.to_string())
```

That raw string includes the provider namespace prefix. For the issue's
examples, the hash sees different bytes:

- `EnumIdentifier("awscc.Region.ap_northeast_1")`;
- `EnumIdentifier("aws.Region.ap_northeast_1")`;
- `EnumIdentifier("awscc.ec2.Eip.Domain.vpc")`;
- `EnumIdentifier("aws.ec2.Eip.Domain.vpc")`.

After carina#3413, those provider axes can represent the same AWS value. The
semantic value is:

- `ap-northeast-1` for the region;
- `vpc` for the EIP domain.

The anonymous identifier code currently hashes the DSL form, not that canonical
API form.

The same issue defeats reconciliation:

- The create-only path compares stored values as strings. If an enum's stored
  form changes from the `awscc.*` spelling to the `aws.*` spelling, equality
  fails even though the API value is unchanged.
- The SimHash path hashes features derived from `deterministic_value_string`.
  A namespace prefix change in an identity value such as `region` can flip too
  many bits, so the Hamming distance exceeds the threshold and no state rename
  is emitted.

The root cause is therefore not the provider lock bump itself. The root cause is
that anonymous address derivation uses parser-surface enum text as a durable
state identity input.

## Design: hash inputs in API canonical form

Anonymous resource address inputs must be normalized before hashing. The durable
hash value should represent the AWS API value, not the user's current DSL
spelling.

The target normalization is:

- `awscc.Region.ap_northeast_1` to `ap-northeast-1`;
- `aws.Region.ap_northeast_1` to `ap-northeast-1`;
- `awscc.ec2.Eip.Domain.vpc` to `vpc`;
- `aws.ec2.Eip.Domain.vpc` to `vpc`.

The implementation should not special-case those examples. It should use schema
metadata to answer two questions:

1. Which attribute type is this value being used as?
2. Given that type, what is the API-canonical value for this enum identifier?

Existing schema machinery already carries most of the needed information:

- enum `AttributeType` values expose identity, aliases, and transform data;
- `DslMap::api_for` maps DSL aliases back to API spelling;
- `DslTransform::HyphenToUnderscore` already describes the region-style API to
  DSL transform, and the hash path needs the inverse API value for deterministic
  transforms of this kind.

The hash implementation should therefore add a schema-aware normalization step
before building feature strings:

- Resolve the resource schema through `SchemaRegistry::get_for`.
- Resolve each hashed resource attribute to its `AttributeType`.
- Resolve each provider identity attribute, such as `region`, to the provider
  config schema/type that defines the identity value.
- When the value is `ConcreteValue::EnumIdentifier`, extract the structural enum
  variant segment after validating the identifier against the expected enum
  identity.
- Convert that variant to API canonical form using enum alias and transform
  metadata.
- Feed the normalized value into the existing hash and SimHash feature builders.

The normalized string should be explicit in the feature text, for example
`EnumApiValue("ap-northeast-1")` or `EnumApiValue("vpc")`, so it is clear that
the hash input is no longer parser-surface DSL text.

Provider identity attributes and resource attributes must use the same
normalization path. If only resource attributes are fixed, `region` remains a
provider-lock-bump hazard. If only `region` is fixed, enum resource attributes
such as EIP domain remain unstable.

## Why not fix only `deterministic_value_string`?

`deterministic_value_string` is deliberately a pure helper over `Value`. It does
not know:

- the attribute name being formatted;
- the resource schema;
- the provider config schema;
- which enum identity the identifier is expected to satisfy;
- whether a final enum segment is a DSL alias or already an API spelling.

Normalizing `EnumIdentifier("aws.Region.ap_northeast_1")` without schema would
be a string heuristic. It would also be easy to get wrong for enum values that
contain dots, aliases that do not match the final segment, or dynamic enum
spaces that use a transform.

The fix belongs at the hash-input collection layer, where
`compute_anonymous_identifiers` already has the resource, providers, registry,
and identity attribute callback. That layer can look up the expected type and
then call a smaller canonicalization helper with enough context to be correct.

`deterministic_value_string` can remain the fallback for already-canonical
primitive values and for non-enum values. The new code should avoid calling it
directly for schema-known enum values that participate in anonymous addressing.

## Reconciliation design

The same canonicalization must be available to reconciliation.

For create-only reconciliation, compare canonical API values instead of parser
surface strings. A state entry written with
`EnumIdentifier("awscc.ec2.Eip.Domain.vpc")` and a desired resource written with
`EnumIdentifier("aws.ec2.Eip.Domain.vpc")` should compare as `vpc == vpc`.

For SimHash reconciliation, the important change is earlier: once the desired
resource hashes canonical enum values, namespace-only provider changes should
produce the same hash suffix. In the common case reconciliation then becomes a
no-op because the desired identifier already exists in state.

This design does not require relaxing `SIMHASH_HAMMING_THRESHOLD`. The threshold
is for approximate matching after real attribute changes. Provider namespace
prefix churn should not enter the feature set in the first place.

## Migration

Existing state already contains anonymous identifiers computed from the old hash
inputs. After the implementation changes the hash input to API canonical form,
some anonymous resources may compute a new hash the first time users run
`plan`.

The migration path is:

- Recompute desired anonymous identifiers with canonical enum hash inputs.
- Use existing anonymous reconciliation to associate old state entries with the
  new desired identifiers where possible.
- Compare create-only state values after applying the same enum canonicalization
  used by the hash path.
- Keep the SimHash threshold unchanged.

For the exact carina#3428 scenario, both `awscc.*` and `aws.*` DSL forms should
canonicalize to the same API value, so a provider lock bump that only changes
the enum namespace prefix should compute the same desired anonymous identifier.

## Alternatives considered

### A. Hash input normalization

This is the proposed design.

Hash inputs become schema-aware and use API canonical enum values. It fixes the
root cause at the address derivation boundary and keeps the rest of the state
addressing model intact.

### B. Structure `ConcreteValue::EnumIdentifier`

Long term, `ConcreteValue::EnumIdentifier(String)` is too weak. It cannot encode
whether a value is parser-surface text, normalized DSL spelling, or API
canonical spelling. A structured enum value such as `{ provider, namespace,
kind, variant }` would make many mistakes harder to express.

That is the stronger type-safety direction, but it is larger than carina#3428.
It touches the parser, validation, state lifting, differ, provider conversion,
and serialization boundaries. This design keeps that as a follow-up rather than
making it a prerequisite.

### C. Declaration-position-derived addresses

Another option is to stop hashing semantic values and derive anonymous addresses
from declaration position, such as file plus line/ordinal.

That removes hash instability, but it creates a different class of instability:
moving declarations, extracting modules, inserting resources above an existing
anonymous declaration, or changing loop shape can rewrite addresses even when
the resource identity did not change. It also interacts poorly with the existing
anonymous-to-`let` rename detection.

This is too incompatible for the current issue.

### D. Plan-level rename hints only

Carina could detect add/destroy pairs with identical attributes and display an
address-rename hint.

That would improve operator readability, but it would not fix the underlying
state identity. The route table association example shows why this is not
enough: address drift can rebind an existing anonymous slot to a different
resource and surface as a real replacement. The hash input still needs to be
stable.

## Type-safety follow-up

The implementation PR should be explicit that enum values entering the anonymous
hash are canonicalized. A local comment is useful, but comments are not a type
guarantee.

A follow-up should consider separating raw and normalized enum values in the
type system. Options include:

- replacing `ConcreteValue::EnumIdentifier(String)` with a structured enum
  identifier;
- introducing a small `CanonicalEnumValue` wrapper for schema-aware consumers;
- making hash feature builders accept already-normalized values rather than raw
  `Value`.

The goal is to prevent future consumers from accidentally reusing parser-surface
enum text as a durable identity value.

## Planned reproducing tests

The implementation PR will add three reproducing tests to
`carina-core/src/identifier/tests.rs`:

- `test_anonymous_id_stable_across_provider_namespace_change_in_identity`;
- `test_anonymous_id_stable_across_provider_namespace_change_in_attribute`;
- `test_reconcile_anonymous_id_after_provider_namespace_change`.

The implementation PR should add them without `#[ignore]` markers. They should
be introduced alongside the fix and should be green in that PR.

The first test proves that provider config identity values such as `region`
must hash by API value, not by `awscc.*` versus `aws.*` DSL namespace. The second
test proves the same requirement for resource attributes such as EIP `domain`.
The third test covers the end-to-end compute-then-reconcile flow with old state
and a new desired resource whose only semantic difference is the provider
namespace prefix embedded in the enum identifier.

## Implementation notes

The implementation keeps `Value::Deferred` hash inputs unchanged. User-authored
`let` binding names are neutral local names, so provider lock bumps do not
change their meaning, and existing deferred strings such as
`ResourceRef(main_rtb.id)` remain the intended deterministic representation.

Only `Value::Concrete(ConcreteValue::EnumIdentifier(_))` values are normalized.
That covers provider config identity attributes such as `region` and
schema-known resource enum attributes such as EIP `domain`. Primitive values,
maps, lists, and deferred values continue to use the existing deterministic
value string path unless a schema-known enum leaf is being hashed directly.

When the enum schema cannot be resolved, the enum identity does not match the
expected attribute type, or the value cannot be converted to an API-canonical
enum value, the implementation falls back to the old deterministic string. This
avoids surprising hash movement for cases outside the known enum path while
stabilizing the cases that can be normalized confidently.

The primary target is DSL spelling changes inside a provider-compatible enum
space, for example `awscc.Region.ap_northeast_1` and
`aws.Region.ap_northeast_1` both hashing as `ap-northeast-1`. Switching the
resource provider itself, such as `awscc.ec2.Route` to `aws.ec2.Route`, still
changes the anonymous identifier prefix from `awscc_*` to `aws_*`; that broader
provider-switch behavior is out of scope for this implementation and should be
tracked separately.
