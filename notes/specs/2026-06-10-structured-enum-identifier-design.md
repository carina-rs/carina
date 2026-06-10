# Structured enum identifiers with raw/canonical typestate

<!-- derived-from ./2026-06-10-anonymous-address-stability.md -->
<!-- derived-from ./2026-06-10-schema-aware-enum-value-extraction-design.md -->

## Status

Design PR for carina#3438.

This document describes the follow-up to carina#3428 and carina#3437. It does
not implement the change. The intended implementation direction is Option 1 from
carina#3438: replace `ConcreteValue::EnumIdentifier(String)` with structured
raw and canonical enum value types so durable identity consumers cannot compile
against parser-surface enum text.

## Background

<!-- derived-from ./2026-06-10-anonymous-address-stability.md#problem -->

carina#3428 exposed that two DSL enum spellings can denote the same provider API
value while still producing different anonymous resource addresses.

The observed case came from a provider lock bump where source-level enum
identifiers changed namespace prefixes:

```text
awscc.Region.ap_northeast_1 -> aws.Region.ap_northeast_1
awscc.ec2.Eip.Domain.vpc   -> aws.ec2.Eip.Domain.vpc
```

Those spellings denote the same provider values:

```text
ap-northeast-1
vpc
```

The anonymous-address path was hashing the DSL spelling carried by
`ConcreteValue::EnumIdentifier(String)` rather than the schema-resolved provider
API value. That made a semantic no-op look like a resource-address change.

PR #3437 fixed the immediate anonymous-hash class with local string helpers in
`carina-core/src/identifier/mod.rs`: `canonical_enum_feature_string`,
`canonical_create_only_value_string`, and `canonical_create_only_text_string`.
Those helpers are deliberately scoped to hash features and create-only text.
A new consumer can still match `ConcreteValue::EnumIdentifier(raw)` and use
`raw` as durable identity input. Nothing in the type system distinguishes:

- parser-surface text, such as `Region.ap_northeast_1`;
- display text, such as `aws.ec2.Eip.Domain.vpc`;
- DSL alias spelling, such as `bucket_owner_enforced`;
- provider API spelling, such as `BucketOwnerEnforced`;
- schema-resolved identity, such as the enum type `aws.s3.Bucket.Ownership`.

That leaves the same bug class open outside the #3437 helper call sites.

## Problem

The current core type is `ConcreteValue::EnumIdentifier(String)`.
`ConcreteValueRef::EnumIdentifier(&str)` mirrors the same unstructured payload.

The `String` payload currently has conflicting responsibilities:

- The parser writes it as raw source-level identifier text.
- The validator interprets it in an enum-typed schema position.
- The formatter, LSP, plan display, and diagnostics preserve it for source-like
  output.
- The differ compares it against provider-read strings.
- State lifting can synthesize it from JSON strings in enum-typed positions.
- Anonymous hash and reconciliation code may need the provider API value instead.
- Provider serializers lower it to provider JSON or WIT strings.

The type says only "this was an enum identifier", not which phase produced it or
whether it has been schema-checked. That is acceptable for display and
diagnostics. It is not acceptable for durable identity.

The current checkout has 30 Rust files and 150 mentions of
`ConcreteValue::EnumIdentifier`. Many are tests, but the non-test call sites
span parser, validation, display, differ, state handling, provider conversion,
and anonymous identity. A local newtype around the hash helper would only guard
one consumer.

## Goals

The design goal is a typestate split:

- Raw enum identifiers represent parser-surface text.
- Canonical enum values represent schema-resolved provider API values.
- Only the enum resolver can construct canonical values.
- Raw and canonical values are not comparable with each other.
- Durable identity consumers require canonical enum values in their signatures.

The concrete safety property is:

```text
code that hashes, reconciles, or stores semantic enum identity cannot compile if
it tries to use parser-surface enum text
```

This is intentionally stronger than "remember to call the right helper." It
makes the wrong data shape unavailable at the API boundary.

## Non-goals

This design does not change the provider WIT wire value shape:
`carina-plugin-host/src/wasm_convert.rs` lowers enum identifiers to plain WIT
strings, and `carina-provider-protocol::Value` has no enum-specific variant.

This design does not require every display or diagnostic path to show canonical
API values. Source-facing paths should keep using the raw form. It also does not
make enum aliases global; alias mapping remains scoped to each enum schema
through `DslMap`.

## Existing building blocks

The design builds on these existing pieces:

- `validate_enum_namespace` checks whether a raw dotted identifier matches a
  schema identity.
- `extract_enum_value_with_values` uses known valid values to find the enum
  tail, including dotted values.
- `AttributeType::enum_parts` exposes `TypeIdentity`, value list, aliases,
  validators, and `DslMap`.
- `AttributeType::validate_enum` rejects quoted strings in enumerated enum
  positions while accepting parser-surface identifiers.
- `NamespacedId::parse` recognizes current dotted DSL forms and preserves
  dotted values such as `ipsec.1`.
- `TypeIdentity` stores provider, segments, and kind as structural axes; its
  dotted display is derived and is not source of truth.
- `DslMap` owns API/DSL spelling conversion through `dsl_for`, `api_for`, and
  the #3437 hash-specific `api_for_hash_feature`.

The proposed resolver should absorb namespace validation and value extraction as
implementation details, consume `enum_parts` as schema input, and eventually
replace `api_for_hash_feature` with construction of a canonical enum value.
This matches the AttributeType opaque/Ref-peeling direction from the
carina#3340/#3349 chain: consumers operate on already-projected,
schema-bearing shapes instead of wildcard-matching raw representation details.

## Proposed type design

### Raw enum identifier

Parser output should retain source-shape information explicitly:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RawEnumIdentifier {
    text: String,
    parsed: RawEnumIdentifierParts,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RawEnumIdentifierParts {
    Bare { value: String },
    TypeQualified { type_name: String, value: String },
    ProviderQualified {
        provider: String,
        type_name: String,
        value: String,
    },
    FullyQualified {
        provider: String,
        segments: Vec<String>,
        type_name: String,
        value: String,
    },
    Unclassified,
}
```

`ConcreteValue` then becomes:

```rust
pub enum ConcreteValue {
    String(String),
    EnumIdentifier(RawEnumIdentifier),
    CanonicalEnum(CanonicalEnumValue),
    // ...
}
```

`RawEnumIdentifier::parse(text)` should reuse `NamespacedId::parse` for accepted
dotted shapes and store owned structured parts. The raw `text` field remains the
source for `Display`, formatter, LSP, and diagnostics. `Unclassified` is allowed
because only the schema can decide whether some text is a valid enum member.

`RawEnumIdentifier` should serialize as the plain source text string and
deserialize by re-running `parse`. The `parsed` field is derived entirely from
`text`, so persisting it would add redundant state-file data while changing the
legacy `EnumIdentifier` payload shape.

### Canonical enum value

The schema-resolved type should be:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalEnumValue {
    identity: TypeIdentity,
    api_value: String,
}
```

The resolver should be the only production path:

```rust
impl CanonicalEnumValue {
    pub fn identity(&self) -> &TypeIdentity;
    pub fn api_value(&self) -> &str;
}

pub struct EnumValueResolver<'a> {
    attr_type: &'a AttributeType,
    defs: Option<&'a BTreeMap<String, AttributeType>>,
}

impl EnumValueResolver<'_> {
    pub fn resolve_raw(
        &self,
        raw: &RawEnumIdentifier,
    ) -> Result<CanonicalEnumValue, TypeError>;

    pub fn resolve_state_text(
        &self,
        text: &str,
    ) -> Result<CanonicalEnumValue, TypeError>;
}
```

`resolve_raw` is for parser-fed desired values. `resolve_state_text` is for
provider-read and state-file text that reaches an enum-typed schema position.
The RawDsl path applies namespace checks, valid-value extraction, alias
inversion, and provider custom validation. The StateText path applies
valid-value extraction, alias inversion, and provider custom validation, but
does not apply namespace checks because state may still carry pre-bump
foreign-namespace DSL text such as `awscc.ec2.Eip.Domain.vpc` after the schema
identity has moved to `aws.*`. Tests can use an explicit test-only constructor,
but production code should not mint `CanonicalEnumValue` without schema.

### Equality and display

`RawEnumIdentifier` and `CanonicalEnumValue` must not implement `PartialEq`
against each other.

Canonical equality should compare both `identity` and `api_value`. If a looser
comparison is needed for assignability, it should be a named method rather than
`PartialEq`.

`Display` for `RawEnumIdentifier` should return source text. Display for
`CanonicalEnumValue` can render `identity.api_value`, but display consumers
should choose deliberately per surface.

### Value projection

`ConcreteValueRef` should mirror the split:

```rust
pub enum ConcreteValueRef<'a> {
    String(&'a str),
    EnumIdentifier(&'a RawEnumIdentifier),
    CanonicalEnum(&'a CanonicalEnumValue),
    // ...
}
```

The existing `as_string_like` helper should not return canonical enum values.
String-like display and canonical identity are different API surfaces.

## Resolver placement

Raw-to-canonical resolution should happen at schema boundaries, not in the
parser.

The parser lacks `AttributeType`, `TypeIdentity`, aliases, valid values, and
provider validators. It can only produce `RawEnumIdentifier`.

The validation path already has the required schema context. The resolver should
be introduced beside `AttributeType::validate_enum` and eventually replace the
string-returning enum pieces of `expand_enum_shorthand`, `resolve_enum_value`,
`validate_enum_namespace`, and `extract_enum_value_with_values`.

The proposed pipeline is:

```text
parser
  -> ConcreteValue::EnumIdentifier(RawEnumIdentifier)
  -> schema validation / enum resolver
  -> CanonicalEnumValue for schema-checked enum leaves
  -> durable identity, differ, reconciliation, state/write-plan semantics
```

Validation still owns user-facing errors. If a raw enum identifier does not
match the enum namespace, value set, alias table, or provider validator, the
resolver should return the same `TypeError` family the current validator returns.

The final form should replace enum leaves in the value tree with
`ConcreteValue::CanonicalEnum`. A temporary side table keyed by field path is
acceptable during the transition, but the end state should make identity
consumers accept the canonical type directly.

## Consumer classification

### Parser and syntax

Parser call sites in `carina-core/src/parser/expressions/primary.rs`,
`parser/resolve.rs`, parser tests, map-key lifting, and related syntax helpers
should produce `RawEnumIdentifier`.

They should not perform schema resolution.

### Display, format, and LSP

Formatter, LSP diagnostics, code actions, and source-facing diagnostics should
generally keep raw enum spelling when the source spelling is what the user needs
to see.

Plan display uses bare canonical `api_value` for resolved enum leaves, rendered
unquoted. The alternative of mapping `api_value` back through `DslMap` was not
chosen for plan output because it would make display depend on carrying schema
context through more rendering surfaces. Source-facing paths can still preserve
raw spelling where that is the clearer user contract.

### Validation

Validation is the resolver entry point.

`AttributeType::validate_enum` currently takes `ConcreteValueRef` and internally
creates string forms through `resolve_enum_input`. It should instead call the
enum resolver and either:

- return a validated canonical enum leaf to the caller; or
- store canonical enum leaves in a side table keyed by field path during the
  staged transition.

`validate_enum_namespace` and `extract_enum_value_with_values` become private
resolver helpers. `DslMap::api_for` remains the normal alias inversion. The
hash-specific `api_for_hash_feature` should disappear once hash callers consume
canonical values.

### Durable identity consumers

These consumers should require `CanonicalEnumValue`:

- anonymous hash feature generation in `identifier/mod.rs`;
- create-only comparison for anonymous reconciliation;
- differ enum equality in `differ/comparison.rs`;
- state lifting for enum-typed leaves;
- any future state identity or rename detection that compares enum semantics.

The important signature shape is "take `&CanonicalEnumValue`", not "take
`&Value` plus optional schema." That moves correctness from "caller remembered
schema" to "caller must provide the schema-resolved type."

### Provider boundaries

Host-to-WIT conversion currently lowers both `ConcreteValue::String` and
`ConcreteValue::EnumIdentifier` to plain WIT strings. That should stay true for
the WIT shape:

```text
CanonicalEnumValue.api_value -> provider string
```

`carina-provider-protocol::Value` independently defines only bool, number,
string, list, and map-like values. It does not need an enum-specific variant.

The Rust provider repos do need follow-up changes because they directly depend
on `carina-core` and match `ConcreteValue::EnumIdentifier` in conversion and
hand-written serializers. Confirmed call-site groups include:

- `carina-provider-aws/carina-provider-aws/src/convert.rs`;
- `carina-provider-aws/carina-provider-aws/src/normalizer.rs`;
- `carina-provider-aws/carina-provider-aws/src/services/iam/role.rs`;
- `carina-provider-aws/carina-provider-aws/src/services/sqs/queue.rs`;
- `carina-provider-awscc/carina-provider-awscc/src/convert.rs`;
- `carina-provider-awscc/carina-provider-awscc/src/provider/conversion.rs`;
- `carina-provider-awscc/carina-provider-awscc/src/provider/normalizer.rs`;
- `carina-provider-aws`'s `carina-aws-types` crate;
- the separate `carina-extract-aws-types/carina-aws-types` source copy.

Provider-side scalar serializers should receive canonical API values at the
boundary. They should not extract trailing segments from raw DSL strings.

## Serialization

State files and saved plans should represent canonical enum values as typed enum
objects, not as ambiguous parser-surface strings.

Recommended JSON shape:

```json
{
  "Enum": {
    "identity": {
      "provider": "aws",
      "segments": ["ec2", "Eip"],
      "kind": "Domain"
    },
    "api_value": "vpc"
  }
}
```

State attribute JSON uses the hand-written `{"Enum": ...}` tag shown above so
schema-free state loading can recover a `CanonicalEnumValue` directly from the
trusted persisted payload.

Saved plans use the serde representation of the `ConcreteValue::CanonicalEnum`
variant. That means the state JSON object and saved-plan serde object are not
byte-identical tags, but both are typed objects and both carry the same semantic
payload:

- the enum type identity; and
- the provider API value.

Plan display renders canonical enum leaves as bare unquoted `api_value`.

Provider wire JSON remains a plain string. The typed enum object is a Carina
state/plan representation, not a provider protocol change.

## State and read-back handling

`carina-state` is schema-free. It currently stores resource attributes as JSON
and reconstructs `Value` through `json_to_dsl_value`, which cannot know whether a
string is an enum.

The structured design keeps that separation:

- `carina-state` reads and writes the serialized `ConcreteValue` shape.
- CLI wiring applies schema-aware enum resolution after loading state and after
  provider read-back.
- The resolver turns recognized enum-typed leaves into `CanonicalEnumValue`.
- Unrecognized strings remain ordinary strings so validation still reports the
  existing user-facing error class.

The current `lift_state_enum_leaves`, `lift_saved_state_enum_leaves`, and
`lift_current_state_enum_leaves` functions are the right placement precedent.
Their output type changes from `EnumIdentifier(String)` to `CanonicalEnumValue`.

## Differ and reconciliation

The differ has schema-aware enum comparison that accepts `String`,
`EnumIdentifier`, and `CanonicalEnum` during the migration. For canonical values
it first tries strict typed equality:

```rust
CanonicalEnumValue { identity, api_value } == CanonicalEnumValue { identity, api_value }
```

If strict equality fails, differ comparison falls through to canonical API-text
comparison before deciding an update is required. This is intentionally looser
than `CanonicalEnumValue`'s `PartialEq`: cross-identity export flows can carry a
producer identity such as `aws.Region` into a consumer schema such as
`awscc.Region`, and those providers share a unified API value space for the enum
member. The question for the differ is whether applying would change provider
API text, not whether the two typed values have identical identity axes.

String and raw enum fallback remains a migration compatibility path while some
provider-read or state leaves can still arrive before schema resolution. The
desired steady state is still that schema-known enum leaves are canonical before
they reach durable identity or differ logic.

Anonymous reconciliation should likewise compare canonical enum values. The
string helpers added by #3437 become temporary shims until all identity inputs
are canonicalized earlier.

## Cross-repo rollout impact

The change spans API shape, so it should land as a chain:

- Carina core introduces raw and canonical enum types behind narrow helper
  accessors.
- Carina core converts validation, state read-back handling, differ, and
  anonymous identity consumers.
- `carina-provider-aws` updates matches on `ConcreteValue::EnumIdentifier` and
  serializer utilities to accept canonical enum values.
- `carina-provider-awscc` updates conversion and normalizer code after the AWS
  provider rev is available.
- `carina-aws-types` copies are updated where tests or helpers construct enum
  identifiers directly.
- Carina bumps provider revisions after provider PRs merge.

The WIT and JSON-RPC provider value protocols do not need an enum variant. The
direct Rust dependency on `carina-core` is what forces provider source changes.

## PR chain plan

Recommended chain:

1. Design doc PR. This PR only adds this document and references carina#3438.
2. Core type introduction PR. Add `RawEnumIdentifier`, `CanonicalEnumValue`, and
   resolver skeletons while keeping temporary shims for existing callers.
3. Core consumer conversion PR. Move validation output, differ comparison, state
   read-back handling, anonymous hash, and reconciliation to canonical enum
   values.
4. Provider follow-up PRs. Update `carina-provider-aws`,
   `carina-provider-awscc`, and `carina-aws-types` construction/match sites, then
   bump revisions in Carina.
5. Shim removal PR. Remove string-out hash helpers, raw-string enum extraction
   APIs from durable identity paths, and any temporary constructors that let
   production code mint canonical enum values without schema. This must happen
   after provider config attributes such as `region` are canonicalized; otherwise
   removing raw fallback and `api_for_hash_feature` would regress provider
   config hash stability.

This mirrors the #3371-style split: design first, core types second, consumers
third, external repo follow-up fourth, cleanup last.

## Alternatives considered

### Hash-feature newtype only

A newtype only around anonymous hash input would protect the #3437 call sites,
but it would leave differ comparison, reconciliation, state identity, and future
identity consumers free to read `EnumIdentifier(String)` directly.

Rejected because carina#3438 is a type-safety follow-up, not just a second guard
around carina#3428.

### Add CanonicalEnumValue without changing EnumIdentifier

Adding `CanonicalEnumValue` while leaving `ConcreteValue::EnumIdentifier(String)`
as the normal enum representation would help code that voluntarily opts in.

It would not prevent a new consumer from matching the raw string and using it as
identity. The unsafe representation would remain as convenient as the safe one.

Rejected because the bug class stays expressible.

### Keep #3437 string helpers

The #3437 helpers are useful tactical shims. They use `AttributeType` to derive
a hash feature string from enum input.

Keeping that as the final design would retain two weak spots:

- the helper returns an untyped string, so downstream code cannot know whether
  it is source text, DSL spelling, or API spelling;
- callers must remember to use it at every identity boundary.

Rejected because the desired property is compile-time separation between raw and
canonical enum values.

### Make raw parsing fully semantic

`RawEnumIdentifier::parse` could try to determine provider, segments, type name,
and value exactly at parse time.

Rejected because the parser does not have the schema. Dotted enum values and
structural identity segments make schema-free parsing ambiguous. Raw parsing can
classify syntax, but only the resolver can assign semantic meaning.

## Relation to existing docs

<!-- derived-from ./2026-06-10-anonymous-address-stability.md#a-hash-input-normalization -->
<!-- derived-from ./2026-06-10-anonymous-address-stability.md#b-structure-concretevalueenumidentifier -->
<!-- derived-from ./2026-06-10-anonymous-address-stability.md#type-safety-follow-up -->
<!-- derived-from ./2026-06-10-schema-aware-enum-value-extraction-design.md -->

The anonymous-address stability design chose hash input normalization for
carina#3428 and explicitly left `ConcreteValue::EnumIdentifier(String)`
structure as a larger follow-up. This document is that follow-up.

The schema-aware enum extraction design established the same principle from the
provider serializer side: dotted enum display strings cannot be interpreted
correctly without schema. This document generalizes that conclusion into a core
type boundary.

Together, the two prior docs say:

- hash inputs should use provider API enum values rather than DSL namespace
  spellings;
- schema-aware extraction should replace position-based dotted-string parsing;
- raw enum source text and canonical enum semantics are different values.

This document makes that last point enforceable in Rust types.
