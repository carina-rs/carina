# Nested Write-Only Members: Design

Tracking: carina-rs/carina#3984 (follow-up to #3981)

## Problem

CloudFormation `writeOnlyProperties` can name nested paths, such as
`/properties/DefaultActions/*/AuthenticateOidcConfig/ClientSecret`. Carina
cannot represent these paths:

- **awscc codegen.** It compares write-only pointers with top-level property
  names only, so it drops every nested write-only path.
- **carina-core.** `StructField` has no write-only flag, so the core cannot
  represent a nested write-only member either.

CloudControl applies an `UpdateResource` JSON Patch on top of its read model,
and that read model never contains write-only members. A nested write-only
member that is not in the patch therefore reaches the handler as unset. #3981
fixed this for top-level keys with `PatchOp::Reassert`. Nested members are not
covered yet.

### Measured (carina-test-000, us-east-1, 2026-10-28; full log in #3984)

The test listener was an ELBv2 HTTPS listener with an `authenticate-oidc`
default action (which carries a `ClientSecret`) followed by a `forward`
action. It was created through CloudControl.

| Update patch | Result |
|---|---|
| `/SslPolicy` only | **FAILED**: "You must either specify a client secret or set UseExistingClientSecret to true". The listener is unchanged. |
| `/DefaultActions` without `ClientSecret` | **FAILED** with the same error |
| `/SslPolicy` plus the whole `/DefaultActions`, re-sent with `ClientSecret` | SUCCESS |

After create, `GetResource` returns the OIDC config without `ClientSecret` and
with `UseExistingClientSecret: false`.

S3 Bucket's nested write-only lifecycle fields behave differently. They are
deprecated singular aliases, for example `Transition`. The handler
canonicalizes them into readable plural forms, so they are never lost.
Re-sending them is harmless.

### A second symptom: plan never converges

The read model lacks `client_secret`, while the desired `default_actions`
contains it. `type_aware_struct_equal` treats a key that is present in desired
but missing on the read side as a difference unless the value is a type
default. So every plan for an OIDC listener shows an Update on
`default_actions`, and with today's carina that Update then fails at apply.

This is the same broken invariant as the patch problem: "a write-only member
is unobservable". It shows up on the diff side as well as the patch side, so
both sides are fixed here. The implementation PR must pin this symptom with a
failing differ test first.

## Decision

### 1. The schema types nested write-only members

`StructField` gains `write_only: bool`, set through a `.write_only()` builder,
like `AttributeSchema::write_only`. The rules:

- **A write-only member is never provider-populated.** Calling
  `.write_only()` together with `.read_only()` or a provider-populated
  `InputMode` panics in the builder, matching the existing rule for
  conflicting input modes (#3772).
- **A write-only member may still be required or optional.**

Whether a top-level attribute *contains* write-only members is **derived, not
stored**. `ResourceSchema::write_only_bearing(key) -> bool` walks the
attribute's type through `Struct`, `List`, `Map` and `Ref` (resolved through
`defs`). It returns true if any reachable `StructField` is write-only.

The same walk yields the projection used below, the set of write-only leaf
paths for an attribute. A stored "contains write-only" flag could disagree with
the field types, and that state is not representable when the flag is derived.

Top-level `AttributeSchema::write_only` keeps its meaning. An attribute is
either write-only as a whole (#3981 already handles it) or write-only-bearing
(new here), never both.

### 2. Reassert the whole top-level attribute

`build_update_patch` (#3981) already appends `PatchOp::Reassert` for keys that
meet all of these conditions:

- the key is present in desired;
- the key is not create-only;
- the key is not already in the patch;
- the attribute is top-level `write_only`.

This design widens the last condition to **`write_only ||
write_only_bearing`**. Everything else from #3981 still holds:

- Reassertion only happens when another key already causes an Update.
- Reassertion never makes a plan change by itself.
- A key the user never specified is never sent.

The reasserted value is the whole desired top-level value. That is exactly the
patch that succeeded on real AWS in the measurement above.

Does reasserting the whole attribute make *other* members look changed?

- **Plan.** No. `Reassert` is not part of the `Effect` or the plan, as in
  #3981. The differ never sees it.
- **CloudControl.** The reasserted `/DefaultActions` is the full desired list.
  CloudFormation semantics already replace the whole list on any
  `DefaultActions` update. Re-sending identical desired members leaves them
  unchanged; the measurement re-sent the `forward` action and it was kept.
  Members the user omits, such as `Scope` or `SessionTimeout`, get the service
  defaults again, which are also the values they have now. The
  implementation's acceptance test checks this by reading the listener back
  after an `ssl_policy`-only change.

### 3. Comparison and state: provenance for nested write-only leaves

#3923 made top-level write-only attributes converge. State keeps the accepted
desired value, and the differ compares desired against that value, because the
provider read cannot be compared. This design applies the same idea at leaf
granularity.

**What state stores.** A new typed per-instance record,
`nested_write_only: BTreeMap<AttributeKey, Vec<(LeafPath, PersistedValue)>>`.
It holds the accepted desired values of the write-only leaves of each
write-only-bearing attribute. A `LeafPath` is a sequence of struct field names
and list indices taken from the *desired* value.

- Secrets are stored in the same hashed form as top-level write-only values.
- Writeback fills the record from `AcceptedRequest` whenever the attribute was
  sent. That covers `Add`, `Replace`, and now `Reassert`.
- A `Remove` of the attribute clears its record.
- This is a new state field, so state goes to **v18**, with an explicit
  migration that starts each instance with an empty record.

**How the differ compares.** For a write-only-bearing attribute, the differ
checks two things:

1. The observable part: the desired value and the read value, with the
   write-only leaves stripped from both sides.
2. The leaf part: the desired write-only leaf values projected from the
   desired value, compared with the saved record. Secrets are compared through
   the existing hash path.

The attribute is unchanged only when both parts are equal. If no record exists
yet, for example on the first plan after upgrading, the leaf part counts as
changed. That produces one Update, which reasserts the attribute and records
the leaves, and the plan then converges. This is the same one-time
reconciliation that #3923 accepted for top-level write-only attributes.

Leaf paths come from the desired value on both sides of the leaf comparison,
so read-side list order never misaligns them.

### 4. awscc codegen

Nested `writeOnlyProperties` pointers get the same handling as nested
`readOnlyProperties` in carina-provider-awscc PR #477:

- The generator emits `.write_only()` on the `StructField` at the pointer
  path, including `/*/` array segments, and records each pointer it emitted.
- After generation it compares the recorded pointers with the schema's
  pointers. Codegen fails on an unmatched pointer, and on a shared `$ref` or a
  same-named inline struct that is reached with differing write-only sets.
- A pointer marked both read-only and write-only is a codegen error.

Applied to the current cache, this marks:

- `AWS::ElasticLoadBalancingV2::Listener`: `ClientSecret` under
  `DefaultActions`;
- `AWS::EC2::SecurityGroup`: `SourceSecurityGroupName` under
  `SecurityGroupIngress`;
- `AWS::S3::Bucket`: the lifecycle aliases, `ReplicationConfiguration` rule
  `Prefix`, and the metadata `EncryptionConfiguration` members.

Both `generate-schemas.sh` and `generate-docs.sh` are rerun.

### 5. Protocol

- **No protocol version bump.** Schemas cross the WASM boundary as JSON
  through `schemas()`. The new `write_only` field on the protocol
  `StructField` is `#[serde(default)]`, the record has no
  `deny_unknown_fields`, and `PatchOp` and WIT do not change.
- **Version-mismatch combinations behave as today.** An older v11 host with a
  new provider ignores the field. A new host with an older provider sees
  `false`. In both cases nested write-only members are not represented, which
  is what happens now; neither combination is worse than today.
- **Conversions still break compilation on purpose.** The host and SDK
  conversions destructure `StructField` exhaustively, so adding the field
  stops compilation until it is carried across. No `_` or `..` arm is
  allowed.

### 6. Other providers

- carina-provider-aws declares no write-only attributes, nested or top-level.
- Its schemas are hand-written and contain no nested `write_only` today.
- Its code only needs to compile against the new `StructField` field.
- Nothing in its behaviour changes.

## Out of scope

- S3 deprecated singular lifecycle aliases plan a diff when the user writes
  the singular form, because the read returns the plural form. That is a
  separate, existing canonicalization issue.
- `Name` length and pattern overrides, as in awscc#477.

## Rollout

1. carina: the `StructField.write_only` field, the derived bearing check and
   leaf projection, the widened reassert condition, and the leaf-provenance
   comparison. Also state v18 and its migration, the protocol `StructField`
   field, and the exhaustive conversions. No protocol bump.
2. carina-provider-awscc: the codegen change, with schemas and docs
   regenerated, and a rev bump to the carina commit from step 1. Before this
   PR merges, the acceptance tests for the Listener OIDC update and its
   convergence, below, run against real AWS.
3. carina-provider-aws: a rev bump that only adds the new field to
   exhaustive matches, if any exist.

## Tests

**Differ, written failing first:**
- An OIDC-style listener whose read lacks `client_secret` plans no change once
  the leaf record matches.
- Changing `client_secret` plans an Update.
- With no record, it plans one reconciling Update.

**Patch:**
- An `ssl_policy` change on a resource with a write-only-bearing
  `default_actions` reasserts the whole `default_actions`.
- An attribute whose only difference is a nested write-only leaf is never
  reasserted on its own (no Reassert-only patch).

**Schema:**
- The bearing check and the projection through `Ref`, `List` and `Map`.
- Conflicting builder combinations panic.

**State:**
- The v17 → v18 migration.
- The record is written on Add, Replace and Reassert, and cleared on Remove.
- Secrets are stored hashed.

**Conversions:** host↔JSON round trip of `StructField.write_only`.

**awscc:**
- Codegen marks the Listener, SecurityGroup and S3 paths.
- Codegen errors on an unmatched or ambiguous pointer.
- Acceptance on real AWS: an OIDC listener applies, an `ssl_policy`-only
  change applies (it fails today), and the next plan shows no changes.
