# Typed enum attribute accessor: Design

<!-- derived-from ./2026-06-10-structured-enum-identifier-design.md -->
<!-- constrained-by ./2026-06-10-schema-aware-enum-value-extraction-design.md -->

## Status

Design proposal for carina-rs/carina#3555.

This document does not implement the change. It records the carina-core API
shape needed to make provider enum reads typed enough that service code cannot
silently drop `EnumIdentifier` or `CanonicalEnum` values by matching only
`ConcreteValue::String`.

## Problem statement

<!-- derived-from #status -->

carina-provider-aws#440 exposed a silent-drop class in provider service code:

```rust
match resource.get_attr("validation_method") {
    Some(Value::Concrete(ConcreteValue::String(s))) => Some(s.as_str()),
    _ => None,
}
```

That code works for quoted strings, but it silently treats enum-shaped values as
absent when the value arrives as:

- `ConcreteValue::EnumIdentifier(raw)` from bare DSL enum syntax;
- `ConcreteValue::CanonicalEnum(canonical)` after schema canonicalization.

In the #440 ACM case, `validation_method = dns` reached AWS as the default
`EMAIL` path instead of the intended `DNS` value. The failure was not a bad ACM
branch. The broken invariant was that enum-typed attributes were still exposed
through the raw `Resource::get_attr -> Option<&Value>` surface, so a caller could
forget two valid enum variants and compile cleanly.

carina-provider-aws#443 introduced the current runtime defense in
`carina-provider-aws/src/helpers.rs`:

```rust
pub(crate) fn enum_attr_str<'a>(
    resource: &'a Resource,
    attr_name: &str,
) -> Option<&'a str> {
    match resource.get_attr(attr_name)? {
        Value::Concrete(ConcreteValue::String(s)) => Some(s.as_str()),
        Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => Some(raw.as_str()),
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => Some(c.api_value()),
        _ => None,
    }
}
```

`require_enum_attr` and `optional_enum_attr` wrap that helper. Follow-up
runtime-defense PRs carina-provider-aws#446 and #452 applied the same idea to
nested struct fields and related service sites.

This is still convention-only:

- `Resource::get_attr` remains public and shape-agnostic.
- The helper knows the value variants, but not whether the requested attribute is
  actually schema-typed as an enum.
- A new caller tomorrow can still match `resource.get_attr("foo")` directly and
  silently discard `EnumIdentifier` / `CanonicalEnum`.
- Reviewers must notice that `"foo"` is enum-typed in schema and remember that
  the helper is mandatory.

CLAUDE.md's root-cause rule asks whether the broken state can be made
unrepresentable. For #3555, the broken state is "service code reads an
enum-typed attribute through the raw `Value` accessor and handles only the
string variant."

## Design goals

The core API should provide a typed projection for enum-typed attributes:

```rust
pub struct EnumAttr<'a> { /* opaque */ }

impl<'a> EnumAttr<'a> {
    pub fn api_value(&self) -> &'a str;
    pub fn identity(&self) -> &'a TypeIdentity;
}
```

The caller should not need to know whether the stored value is a quoted string,
a raw enum identifier, or a canonical enum. The accessor should check the
resource schema first, then project only the enum value.

Non-goals:

- Do not remove `Resource::get_attr -> Option<&Value>`. Other shapes still need
  raw access for maps, lists, strings, numbers, deferred values, display, tests,
  and generic walkers.
- Do not redesign enum canonicalization. `RawEnumIdentifier`,
  `CanonicalEnumValue`, `EnumValueResolver`, `TypeIdentity`, and
  `AttributeType::enum_parts` already exist.
- Do not change provider WIT value shapes.
- Do not implement provider sweeps in the carina-core PR.

## Option A: Schema-aware accessor on Resource

Add a schema-aware accessor that leaves `Resource` storage unchanged:

```rust
impl Resource {
    pub fn get_enum_attr<'a>(
        &'a self,
        schema: &'a ResourceSchema,
        name: &str,
    ) -> Option<EnumAttr<'a>>;
}
```

`EnumAttr` is opaque. It exposes only the semantic enum projection:

```rust
impl<'a> EnumAttr<'a> {
    pub fn api_value(&self) -> &'a str;
    pub fn identity(&self) -> &'a TypeIdentity;
}
```

Sketch:

```rust
pub fn get_enum_attr<'a>(
    &'a self,
    schema: &'a ResourceSchema,
    name: &str,
) -> Option<EnumAttr<'a>> {
    let attr_schema = schema.attributes.get(name)?;
    let parts = attr_schema.attr_type.enum_parts_with_defs(&schema.defs)?;
    let value = self.get_attr(name)?;
    EnumAttr::from_value_and_parts(value, parts)
}
```

The exact schema API may be `ResourceSchema::shape_of`,
`AttributeType::shape_with_defs`, or a small `enum_parts_with_defs` helper. The
important part is that the accessor receives the resource schema and resolves
`Ref` through `ResourceSchema::defs` before accepting the attribute as enum.

`EnumAttr::from_value_and_parts` should accept:

- `ConcreteValue::CanonicalEnum(c)` only when `c.identity() == parts.identity`;
- `ConcreteValue::EnumIdentifier(raw)` by resolving through
  `EnumValueResolver`;
- `ConcreteValue::String(s)` by resolving as provider/state text for the enum
  schema position.

It should reject non-enum schema positions even when the stored value happens to
be a string.

### Option A evaluation

| Lens | Evaluation | Result |
| --- | --- | --- |
| Long-term view | Provider code already has or can get the `ResourceSchema` at request-building time. The API does not force a resource lifecycle rewrite and can be adopted helper-by-helper. | Strong |
| Type safety | Callers that need an enum can request `EnumAttr`, not `&Value`. The returned type has no string-only variant to forget. Raw `get_attr` still exists, so a lint or provider helper deprecation is needed to make direct enum reads loudly wrong during migration. | Strong, with lint follow-up |
| Root-cause | Moves enum projection into carina-core, where schema and enum identity are available. It closes the helper convention for migrated callers without weakening other raw access. | Strong |

### Option A migration shape

AWS helper before:

```rust
pub fn optional_enum_attr<'a>(
    resource: &'a Resource,
    attr_name: &str,
) -> Option<&'a str>;
```

AWS helper after:

```rust
pub fn optional_enum_attr<'a>(
    resource: &'a Resource,
    schema: &'a ResourceSchema,
    attr_name: &str,
) -> Option<EnumAttr<'a>>;
```

or, if keeping provider return types stable for the sweep:

```rust
pub fn optional_enum_attr<'a>(
    resource: &'a Resource,
    schema: &'a ResourceSchema,
    attr_name: &str,
) -> Option<&'a str> {
    resource.get_enum_attr(schema, attr_name).map(|v| v.api_value())
}
```

The second form is less expressive but keeps the provider sweep mechanical. A
later provider PR can switch call sites from `&str` to `EnumAttr` where identity
matters.

## Option B: Resource owns its schema

Attach schema identity to each resource:

```rust
impl Resource {
    pub fn new_with_schema(
        resource_type: impl Into<String>,
        name: impl Into<String>,
        schema: Arc<ResourceSchema>,
    ) -> Self;

    pub fn get_enum_attr(&self, name: &str) -> Option<EnumAttr<'_>>;
}
```

This gives the cleanest service call surface:

```rust
let method = resource.get_enum_attr("validation_method")?;
```

But it changes the meaning and lifecycle of `Resource`. Today a `Resource` is a
portable DSL/state value with attributes, directives, prefixes, binding
metadata, and dependency bindings. Schema context lives beside resources in
registries, provider factories, validation, differ, and binding indexes.

Making `Resource` own or borrow schema raises immediate design questions:

- Is the schema serialized, skipped, or rebuilt after deserialization?
- Does `Resource` carry `Arc<ResourceSchema>`, schema key, provider name, or a
  registry reference?
- How do tests and state-loaded resources construct valid instances?
- How do module expansion, parsing, validation, anonymous identity, and provider
  protocol conversion avoid cloning or leaking schema lifetimes?

### Option B evaluation

| Lens | Evaluation | Result |
| --- | --- | --- |
| Long-term view | Cleanest eventual call surface, but changes `Resource` from plain data into schema-bound data. That affects parser, state, module expansion, tests, and provider boundaries. | Medium |
| Type safety | Strongest API once complete: enum reads do not need callers to pass schema, so there is less chance to pass the wrong schema or omit schema. | Strongest |
| Root-cause | Addresses the raw enum read class and makes schema availability intrinsic to resource reads. | Strong |

### Option B migration shape

This likely cannot be one small core PR. It needs a broader typestate or
resource wrapper plan:

```rust
pub struct SchemaBoundResource<'a> {
    resource: &'a Resource,
    schema: &'a ResourceSchema,
}
```

If the design retreats to a wrapper, it becomes close to Option A without
mutating `Resource` itself. If it truly embeds schema into `Resource`, every
construction and serialization path becomes part of the blast radius.

## Option C: Core-internal typed accessor, provider-side adapter

Keep `Resource::get_attr` as the only value accessor and add a schema shape
query:

```rust
impl Resource {
    pub fn shape_of_attr(
        &self,
        schema: &ResourceSchema,
        name: &str,
    ) -> Option<AttrShape<'_>>;
}

pub enum AttrShape<'a> {
    Enum { identity: &'a TypeIdentity },
    Bool,
    Int,
    Float,
    String,
    List,
    Map,
    Struct,
    Union,
    Unknown,
}
```

The provider helper then does:

```rust
match resource.shape_of_attr(schema, "validation_method") {
    Some(AttrShape::Enum { identity }) => {
        match resource.get_attr("validation_method") {
            Some(Value::Concrete(ConcreteValue::String(s))) => ...,
            Some(Value::Concrete(ConcreteValue::EnumIdentifier(raw))) => ...,
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(c))) => ...,
            _ => None,
        }
    }
    _ => None,
}
```

This makes the helper layer schema-aware and exhaustive over `AttrShape`. It
does not give provider service code a typed enum value. The value match remains
provider-owned.

### Option C evaluation

| Lens | Evaluation | Result |
| --- | --- | --- |
| Long-term view | Smaller core API and less direct coupling to enum value resolution, but every provider helper keeps the value-shape logic. Future enum variants still require provider helper changes. | Medium |
| Type safety | Better than today for helper users, but raw `get_attr` remains the practical route and the enum value variants are still matched outside core. A service caller can still bypass `shape_of_attr`. | Weak |
| Root-cause | Identifies enum schema positions, but leaves the projection bug class in provider code. It is closer to a stronger runtime guard than to making the wrong read unrepresentable. | Weak |

### Option C migration shape

Option C can be implemented as a narrow PR, but it preserves the same reviewer
burden at the call-site layer:

- Did the helper call `shape_of_attr` before matching `Value`?
- Did it handle every enum value representation?
- Did every service call migrate to the helper?
- Did AWSCC copy the same helper shape?

The answer to "can a new caller tomorrow re-reach the #440 class?" remains yes.

## Recommendation

<!-- derived-from #option-a-schema-aware-accessor-on-resource -->
<!-- derived-from #option-b-resource-owns-its-schema -->
<!-- derived-from #option-c-core-internal-typed-accessor-provider-side-adapter -->

Recommend Option A.

The three lenses mostly converge:

| Lens | Selects | Why |
| --- | --- | --- |
| Long-term view | Option A | It is small enough to land in sequence and does not reshape `Resource` lifecycle. |
| Type safety | Option B in the ideal end state, Option A for this issue | B is strongest if the whole repo accepts schema-owned resources, but A gives a typed enum projection now without invasive lifecycle churn. |
| Root-cause | Option A | The enum projection moves into core and becomes schema-aware; provider helpers stop owning the fragile match over raw `Value`. |

Option B is the cleaner final surface, but it is too invasive for #3555 unless
the project wants a separate schema-bound resource typestate effort. Option C is
too weak: it improves the helper but leaves the raw-value match as the normal
implementation technique.

Option A should be paired with an enforcement signal during provider migration:

- deprecate provider helper overloads that lack `ResourceSchema`;
- add a crate-local clippy lint or custom lint for provider crates that flags
  `resource.get_attr("<known enum attr>")` followed by a direct
  `ConcreteValue::String` match;
- add tests proving `String`, `EnumIdentifier`, and `CanonicalEnum` all project
  through `Resource::get_enum_attr`.

The lint is not the primary defense. The primary defense is that migrated code
does not receive `&Value` for enum reads. The lint catches remaining direct raw
reads while `Resource::get_attr` stays available for non-enum shapes.

## Migration plan

<!-- derived-from #recommendation -->

Land this as small, independently mergeable PRs.

### PR 1: carina-core typed accessor

Add:

```rust
pub struct EnumAttr<'a> { /* opaque */ }

impl Resource {
    pub fn get_enum_attr<'a>(
        &'a self,
        schema: &'a ResourceSchema,
        name: &str,
    ) -> Option<EnumAttr<'a>>;
}
```

Core tests:

- returns `None` when the attribute is absent;
- returns `None` when the schema attribute is not enum-typed;
- returns API value and identity for `ConcreteValue::CanonicalEnum`;
- resolves `ConcreteValue::EnumIdentifier` through enum schema parts;
- resolves provider/state `ConcreteValue::String` through enum schema parts;
- rejects a canonical enum whose identity does not match the schema position;
- handles `Ref` through `ResourceSchema::defs`.

No provider code changes in this PR.

### PR 2: AWS helper signature migration

Change helper-layer signatures in `carina-provider-aws/src/helpers.rs`:

```rust
pub(crate) fn enum_attr_str<'a>(
    resource: &'a Resource,
    schema: &'a ResourceSchema,
    attr_name: &str,
) -> Option<&'a str>;

pub fn require_enum_attr(
    resource: &Resource,
    schema: &ResourceSchema,
    attr_name: &str,
) -> ProviderResult<String>;

pub fn optional_enum_attr<'a>(
    resource: &'a Resource,
    schema: &'a ResourceSchema,
    attr_name: &str,
) -> Option<&'a str>;

pub(crate) fn enum_struct_field_str<'a>(
    resource: &'a Resource,
    schema: &'a ResourceSchema,
    struct_attr: &str,
    field_name: &str,
) -> Option<&'a str>;
```

For nested struct fields, either add a core accessor:

```rust
resource.get_enum_struct_field(schema, "options", "dns_support")
```

or make the AWS helper use a core lower-level projection function that accepts
an `AttributeType` plus `ResourceSchema::defs`. Do not keep nested enum variant
matching solely in AWS.

### PR 3: AWS service sweep

Local grep command used:

```bash
rg -n "\b(optional_enum_attr|require_enum_attr|optional_enum_struct_field)\s*\(" \
  /Users/mizzy/src/github.com/carina-rs/carina-provider-aws/carina-provider-aws/src/services \
  -g '*.rs'
```

Current local AWS service blast radius: 39 production helper call sites across
17 service files.

Top-level enum helper sites:

| File | Sites |
| --- | ---: |
| `services/ec2/nat_gateway.rs` | 1 |
| `services/ec2/eip.rs` | 1 |
| `services/ec2/vpc_endpoint.rs` | 1 |
| `services/ec2/vpn_gateway.rs` | 1 |
| `services/ec2/flow_log.rs` | 3 |
| `services/ec2/vpc.rs` | 1 |
| `services/route53/record_set.rs` | 3 |
| `services/acm/certificate.rs` | 6 including helper regression tests in same file |
| `services/logs/log_group.rs` | 1 |
| `services/organizations/account.rs` | 1 |
| `services/organizations/organization.rs` | 1 |
| `services/s3/bucket_acl.rs` | 1 |
| `services/s3/bucket_versioning.rs` | 2 |
| `services/s3/bucket_ownership_controls.rs` | 1 |

Nested enum struct helper sites:

| File | Sites |
| --- | ---: |
| `services/ec2/transit_gateway.rs` | 7 |
| `services/ec2/transit_gateway_attachment.rs` | 4 |
| `services/ec2/subnet.rs` | 1 |
| `services/acm/certificate.rs` | 3 |

Helper/test file blast radius:

- `carina-provider-aws/src/helpers.rs` has the four relevant helper functions
  plus helper tests for string, enum identifier, canonical enum, missing, and
  non-enum cases.
- A full source grep for
  `optional_enum_attr|require_enum_attr|enum_attr_str|enum_struct_field_str`
  found 78 textual matches across 15 AWS Rust files; the larger number includes
  imports, docs, helper definitions, and tests.

### PR 4: AWSCC helper/adaptor PR

The design must include AWSCC because both AWS and AWSCC consume the same enum
read pattern. In this local checkout, direct grep found no matches for:

```text
optional_enum_attr
require_enum_attr
enum_attr_str
enum_struct_field_str
```

under:

```text
/Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/carina-provider-awscc/src
```

That means the AWSCC PR needs one of two shapes, depending on the current branch
state when #3555 implementation starts:

- if AWSCC has imported or copied the AWS helper layer by then, update the helper
  signatures to pass `ResourceSchema`;
- if AWSCC still has no helper names, add an AWSCC-local adapter around
  `Resource::get_enum_attr` before adding any new enum reads.

### PR 5: AWSCC service sweep

Run the same grep in the AWSCC implementation worktree and migrate any helper
users. The current local baseline is zero direct matches, so the minimum AWSCC
blast radius is one helper/adaptor PR plus any branch-current service sites that
appear before implementation.

### Cross-repo order

Recommended order:

1. carina-core PR: typed accessor and tests.
2. carina-provider-aws helper PR: schema-aware helpers backed by core accessor.
3. carina-provider-aws service sweep PR: pass schemas at the 39 current service
   call sites.
4. carina-provider-awscc helper/adaptor PR.
5. carina-provider-awscc service sweep PR.

Each PR can be merged independently:

- core adds API without changing providers;
- helper PR can preserve old return types while requiring schema;
- service sweeps are mechanical schema-threading changes;
- AWSCC can lag until it updates its carina-core dependency.

## Constraints and acceptance criteria

<!-- derived-from #design-goals -->

Hard constraints:

- Keep `Resource::get_attr -> Option<&Value>` available.
- Do not require schema-owned `Resource` unless Option B is explicitly chosen as
  a separate lifecycle redesign.
- Do not make provider service code match raw enum value variants for migrated
  enum reads.
- Preserve non-enum raw access for maps, lists, strings, numbers, generic
  walkers, display, and tests.

Acceptance criteria for Option A:

- `Resource::get_enum_attr(schema, name)` returns an opaque `EnumAttr<'_>`.
- `EnumAttr` exposes `.api_value() -> &str` and `.identity() -> &TypeIdentity`.
- The accessor first proves `name` is enum-typed in `schema`.
- The accessor handles `String`, `EnumIdentifier`, and `CanonicalEnum` enum
  storage shapes.
- Identity mismatches are rejected.
- `Ref`-typed schema attributes are resolved through `ResourceSchema::defs`.
- Provider helper signatures require schema.
- The following direct pattern is no longer accepted in migrated provider code:

```rust
match resource.get_attr("an_enum") {
    Some(Value::Concrete(ConcreteValue::String(s))) => Some(s.as_str()),
    _ => None,
}
```

For that pattern, "no longer accepted" may be enforced by either:

- removing the need to write it because provider helpers expose only `EnumAttr`
  or `&str` from `get_enum_attr`; and
- a provider lint that flags direct raw reads for known enum attributes.

The lint is acceptable only as a migration backstop. A design that relies on
lint alone, while helpers still match raw `Value`, does not satisfy #3555.

Acceptance criteria for PR sequencing:

- carina-core PR can land without provider source changes.
- AWS helper PR can land before the full service sweep if it keeps a temporary
  compatibility wrapper or migrates helper tests in the same PR.
- AWS sweep PR does not change enum semantics beyond passing schema and consuming
  the new helper.
- AWSCC helper/adaptor PR can land independently after its carina-core dependency
  update.
- AWSCC sweep PR is mechanical and can be reviewed separately.

## Open questions

<!-- derived-from #recommendation -->

1. What would trigger revisiting the A-vs-B choice? Option A is the
   recommended path. Option B (schema-owned `Resource`) is the deeper
   type-safety end state but requires a separate resource-lifecycle
   redesign whose scope eclipses #3555. If a future design wave touches
   `Resource` construction (e.g. a state-side typestate refactor),
   reopening B at that seam is sensible — otherwise A stands.

2. Should `EnumAttr` resolve `ConcreteValue::String` as provider/state text, or
   only accept `CanonicalEnum` and `EnumIdentifier`?

   Accepting `String` preserves current provider behavior for quoted enum input
   and provider-read echoes. Rejecting it is stricter but may require a prior
   canonicalization guarantee at every provider boundary.

3. Should nested enum struct fields get a first-class core accessor in the same
   PR?

   AWS currently has 15 nested enum struct helper calls. Keeping that projection
   in AWS would leave part of #446/#452's runtime-defense surface outside core.

4. What is the enforcement mechanism for raw enum reads that remain after the
   helper sweep?

   Options: crate-local clippy lint, custom deny lint in provider crates,
   provider helper deprecation plus review policy, or a schema-bound wrapper type
   for provider service entry points.

5. How should providers obtain `ResourceSchema` at every helper site?

   The provider already has schema knowledge, but implementation should choose a
   consistent threading pattern: pass schema through service methods, wrap
   request context, or construct schema-bound resource views at provider entry.

6. Should helper return types stay `&str` for the first sweep or switch directly
   to `EnumAttr<'_>`?

   `&str` minimizes provider churn. `EnumAttr` makes identity available and
   strengthens the type-safety signal at call sites.

## References

- Original issue: [carina-rs/carina#3555](https://github.com/carina-rs/carina/issues/3555)
- Runtime-defense bug: [carina-rs/carina-provider-aws#440](https://github.com/carina-rs/carina-provider-aws/issues/440)
- Helper-layer runtime defense: [carina-rs/carina-provider-aws#443](https://github.com/carina-rs/carina-provider-aws/pull/443)
- Nested enum/runtime-defense follow-up: [carina-rs/carina-provider-aws#446](https://github.com/carina-rs/carina-provider-aws/pull/446)
- Additional provider sweep/runtime-defense follow-up: [carina-rs/carina-provider-aws#452](https://github.com/carina-rs/carina-provider-aws/pull/452)
- Root-cause rule: [CLAUDE.md](../../CLAUDE.md#root-cause-fixes-only--and-make-the-broken-state-unrepresentable)
- Prior enum typestate design: [Structured enum identifiers with raw/canonical typestate](./2026-06-10-structured-enum-identifier-design.md)
- Prior schema-aware enum extraction design: [Schema-aware enum value extraction](./2026-06-10-schema-aware-enum-value-extraction-design.md)

