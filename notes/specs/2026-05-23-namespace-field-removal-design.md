# `Custom.namespace` / `StringEnum.namespace` Removal — Design

Status: **All policy decided — Option A (`CustomEnum` variant).** The
legacy `namespace: Option<String>` field is fully derivable from the
structured `TypeIdentity` on the content axis, but it also carries
an **enum-marker** meaning (introduced by the S2.5a hotfix,
carina#3216) that `TypeIdentity` does not express. The enum-marker
is replaced by promoting "enum-shaped Custom" to its own
`AttributeType::CustomEnum` variant — lifting the marker from a
runtime invariant to a compile-time fact. This document inventories
the read surface, the cross-repo blast radius, and records the
Option A vs Option B comparison that produced the decision.

Date: 2026-05-23

<!-- constrained-by ./2026-05-16-semantic-name-redesign-design.md -->

## Background

`AttributeType::Custom` and `AttributeType::StringEnum` each carry a
`namespace: Option<String>` field that pre-dates the structured
`TypeIdentity` migration (carina#2807, all eight S1–S2.5 PRs merged
2026-05-23). The field is the last surviving piece of the old "flat
dotted string is the type-identity key" representation: it was the
namespace prefix component when `semantic_name`/`type_name` was a
PascalCase tail.

After S2.5a/b/d, the *content* of `namespace` is fully derivable from
the type's `TypeIdentity`:

| Custom example | `identity.provider` | `identity.segments` | Legacy `namespace` |
| -- | -- | -- | -- |
| `aws.Region` | `Some("aws")` | `[]` | `Some("aws")` |
| `aws.AvailabilityZone.ZoneName` | `Some("aws")` | `["AvailabilityZone"]` | `Some("aws.AvailabilityZone")` |
| `aws.s3.Bucket.VersioningStatus` (StringEnum) | — | — | `Some("aws.s3.Bucket")` |
| `aws.Arn` (structural ARN) | `Some("aws")` | `[]` | `Some("aws")` |

The content rule is exactly: `namespace = identity.dotted_prefix()` —
the `Display` form of the identity with the trailing `.kind` segment
removed. The `StringEnum` row above stays content-derivable from its
`name + namespace` triple via `schema::string_enum_identity`, which
was added in S2.5a for exactly this purpose.

So the field's *value* is redundant. But the field's *presence* still
does work the identity cannot, and that is the open design problem.

## The enum-marker meaning

The S2.5a follow-up (carina#3216) keyed `expand_enum_shorthand` on
`identity` but kept the call site gated on
`Custom.namespace.is_some()`. The gate divides the `Custom` space in
two:

- **Enum-shaped Customs** (`namespace: Some(_)`): values written in
  namespaced shorthand. `aws.Region.us_east_1`, `dedicated`,
  `us_east_1a`. These flow through `expand_enum_shorthand` →
  `resolve_enum_input` before reaching the validator.
- **Structurally-validated Customs** (`namespace: None`): values
  carry their own format. `arn:aws:s3:::bucket-name`,
  `vpc-12345678`. These reach the validator verbatim; shorthand
  expansion would corrupt them (`aws.Arn.arn:aws:s3:...` is wrong).

Both kinds populate `identity` post-S2.5, so the identity alone can no
longer tell the two groups apart. The marker is what keeps `Arn` out
of the shorthand path.

Three sites read the marker today:

1. `carina-core/src/schema/mod.rs:1129`
   (`AttributeType::validate_custom`) — gates the
   `expand_enum_shorthand` call before running the validator.
2. `carina-core/src/schema/mod.rs:1742-1757`
   (`reshape_to_enum_for_namespaced_custom`) — reshapes a generic
   `ValidationFailed` into a `StringLiteralExpectedEnum` diagnostic
   with the structured `type_name` slot populated. Only fires for
   enum-shaped Customs.
3. `carina-lsp/src/diagnostics/mod.rs:574-588` — LSP-side mirror of
   site 1, for editor-time diagnostics.

A fourth read site, `namespaced_enum_parts`
(`carina-core/src/schema/mod.rs:663-679`), uses
`namespace: Some(namespace)` to return the namespace as a string for
display callers. After the rewrite this would be derived from
`identity.dotted_prefix()`.

## Read surface inventory (carina-core)

`grep` on the head as of `c70264d6e5b6` (post-#3225 merge) finds the
following production read sites for `Custom.namespace` /
`StringEnum.namespace`:

| File | Line | Role |
| ---- | ---- | ---- |
| `carina-core/src/schema/mod.rs` | 480, 486, 494, 503 | `Debug` derivations |
| `carina-core/src/schema/mod.rs` | 651, 656, 667, 670, 673, 676 | `string_enum_parts` / `namespaced_enum_parts` accessors |
| `carina-core/src/schema/mod.rs` | 711, 721 | `validate` enum-collision diagnostic — passes `namespace` into the message builder |
| `carina-core/src/schema/mod.rs` | 943, 962 | `validate_string_enum` — passes namespace to `ExpectedEnumVariant::from_namespaced` |
| `carina-core/src/schema/mod.rs` | 991, 1021, 1066 | `validate_string_enum` direct/full-form match arms |
| `carina-core/src/schema/mod.rs` | 1108, 1129 | `validate_custom` — the load-bearing marker (site 1 above) |
| `carina-core/src/schema/mod.rs` | 1744 | `reshape_to_enum_for_namespaced_custom` (site 2 above) |
| `carina-core/src/schema/mod.rs` | 1485, 1858 | `string_enum_identity`, helper signatures |
| `carina-lsp/src/diagnostics/mod.rs` | 574-588 | site 3 above |

All read sites split into three buckets:

- **Buckets D — Debug / accessor.** Derived `Debug` and the two
  `*_enum_parts` accessors. Fold by deriving the namespace string from
  `identity` on demand.
- **Bucket V — value-form parsing for `StringEnum`.** Lines 991/1021/
  1066 use namespace as the dotted prefix that gates which match arms
  fire. Fold by computing the prefix from
  `string_enum_identity(name, ...)` once at the top of the function.
- **Bucket M — enum-marker on `Custom`.** Lines 1129, 1744, and the
  LSP mirror. This is the bucket that needs a structured replacement.

## Cross-repo blast radius

`namespace: Some(...)` construction sites, by repo (head SHAs as of
2026-05-23):

| Repo | Sites | Notable surfaces |
| ---- | ----- | ---------------- |
| `carina` (this repo) | 9 prod + ~20 tests | `carina-core` doctest fixtures, `carina-provider-protocol` JSON wire types, `carina-plugin-host::wasm_convert` |
| `carina-provider-aws` | 71 | `carina-aws-types/src/lib.rs`, `carina-provider-aws/src/{factory,convert,schemas/...}.rs`, codegen-generated schemas |
| `carina-provider-awscc` | 145 | `carina-aws-types/src/lib.rs`, `carina-provider-awscc/src/{convert,lib}.rs`, `src/bin/codegen.rs` (template), generated schemas |

The protocol crate matters because schema transport between WASM
plugin and host goes over JSON (`schemas: func() -> string` in
`carina-plugin-wit/wit/provider.wit:19`), with
`carina-provider-protocol::types::AttributeType` as the wire shape.
Its `Custom { name, base, namespace }` and `StringEnum { namespace,
... }` variants both carry the legacy field; both need a structured
replacement that survives the JSON round-trip.

WIT itself does not carry `namespace`. `validate-custom-type` already
takes `type-identity` (carina-plugin-wit#16, merged in S1), so the
host-plugin in-process boundary is not affected by this change beyond
the schema-transport JSON surface.

## Decision: how to replace the enum-marker

Bucket M needs a structured indicator that means *"this Custom's
values are written in namespaced shorthand."* Two shapes were
considered; Option A is the adopted shape.

### Option A — `CustomEnum` as a sibling variant of `Custom`

Split `AttributeType::Custom` into two variants:

```rust
enum AttributeType {
    // …
    /// Structurally-validated custom: values carry their own format,
    /// reach the validator verbatim. ARNs, resource IDs, CIDRs, …
    Custom {
        identity: Option<TypeIdentity>,
        base: Box<AttributeType>,
        pattern: Option<String>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
        to_dsl: Option<fn(&str) -> String>,
    },
    /// Enum-shaped custom: values written as namespaced shorthand;
    /// `expand_enum_shorthand` runs before the validator. Region,
    /// AvailabilityZone, …
    CustomEnum {
        identity: TypeIdentity, // never None for an enum-shaped Custom
        base: Box<AttributeType>,
        validate: CustomValidator,
        to_dsl: Option<fn(&str) -> String>,
    },
    // …
}
```

`StringEnum` keeps its existing `Custom`-adjacent shape but drops
`namespace`; the namespace is derived from
`string_enum_identity(name, …)`.

**Pros**

- The marker meaning becomes a type-level fact. `validate_custom`
  loses the runtime `if namespace.is_some()` branch entirely — each
  variant runs its own code path. The
  `reshape_to_enum_for_namespaced_custom` site collapses to "is the
  attr type a `CustomEnum`?", a structural match.
- Pattern matching is exhaustive: a future `Custom` author cannot
  forget the marker because there is no marker to forget — they pick
  a variant.
- Matches how the LSP completion arm for AZ already thinks
  (`kind == "ZoneName" && segments[0] == "AvailabilityZone"`): the
  enum-shaped case is structurally distinct from the structural case.

**Cons**

- Larger blast radius **inside** `carina-core/src/schema/mod.rs`. Every
  `AttributeType::Custom { … }` match arm grows a sibling
  `CustomEnum` arm or a combined catch-all. Measured: ~31 production
  match sites in `carina-core`, ~10 in `carina-lsp`. Most are 1-2 line
  additions, but the surface is broad.
- Provider repos and codegen templates must also pick the right
  variant at construction time, not flip a flag.
- The `Custom`/`CustomEnum` split makes the *type* visible at the
  match site, which is the whole point — but it requires a rename in
  every constructor in the provider repos. ~71 + 145 sites need to
  re-classify into `Custom` vs `CustomEnum` (mechanically: legacy
  `namespace: Some(_)` → `CustomEnum`, `None` → `Custom`).

### Option B — `enum_shorthand: bool` flag on `Custom`

Keep `Custom` as a single variant; replace `namespace: Option<String>`
with `enum_shorthand: bool`:

```rust
enum AttributeType {
    // …
    Custom {
        identity: Option<TypeIdentity>,
        base: Box<AttributeType>,
        pattern: Option<String>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
        /// `true` for namespaced-shorthand-shaped Customs (`Region`,
        /// `AvailabilityZone.ZoneName`); `false` for
        /// structurally-validated Customs (`Arn`, `VpcId`). Gates
        /// `expand_enum_shorthand` and the enum-shaped diagnostic
        /// reshape.
        enum_shorthand: bool,
        to_dsl: Option<fn(&str) -> String>,
    },
    // …
}
```

`StringEnum` again drops `namespace` and derives it via
`string_enum_identity`.

**Pros**

- Minimal mechanical change at the consumer sites: every existing
  `Custom { … }` match keeps working; the flag is read where the
  marker is needed, ignored elsewhere. `if namespace.is_some()`
  becomes `if enum_shorthand`.
- Provider repos and codegen templates only need a field rename:
  `namespace: Some("aws")` → `enum_shorthand: true`,
  `namespace: None` → `enum_shorthand: false`. No variant decision
  per call site.
- Protocol crate change is symmetric — replace `namespace: Option<String>`
  with `enum_shorthand: bool` (#[serde(default)] → false-safe for old
  plugins, though per project policy `feedback_no_backward_compat`
  this experimental project doesn't actually care).

**Cons**

- Forgetting the flag has no compile-time signal. A new structural
  Custom that forgets to set `enum_shorthand: false` would default to
  the wrong path. (Mitigated by no-`Default` discipline — the field
  is mandatory at construction — but this is a runtime invariant, not
  a type-level one.)
- The "this kind of Custom is fundamentally different" insight stays
  buried as a bool field instead of being lifted into the type. Future
  readers re-derive what the flag means from comments rather than
  pattern-match exhaustively.
- Doesn't match the LSP completion arm's structural framing as
  closely; the schema's `match` retains a runtime branch where the
  LSP code has a structural one.

### Decision: Option A

**Adopted 2026-05-23.** Option A (`CustomEnum` variant), weighting
long-term maintainability and type safety per
`[[feedback_long_term_and_type_safety]]`:

- The enum-shorthand vs structurally-validated split is not a
  configuration; it is a *kind* of Custom. Lifting it to the type
  matches that.
- The S2.5a hotfix exists precisely because a previous version of
  this gate was implicit and got it wrong (carina#3216 — `aws.Arn`
  values being mis-expanded into `aws.Arn.arn:aws:s3:...`). Making
  the gate compile-time enforced removes the class of bug.
- The 31+10 in-repo match sites and 71+145 provider construction
  sites are bounded work concentrated in this PR series; the
  per-site cost is `s/namespace: Some/CustomEnum/`-class, mechanical.
  The compounding benefit (every future `Custom` match arm gets
  exhaustiveness from the compiler) outlasts the one-time conversion.
- Per `[[feedback_type_safety_over_runtime_checks]]` and
  `[[feedback_type_safety_push_in_scope]]`: prove the invariant in
  the type system when you can, rather than at runtime.

Option B was rejected because it is the smaller change today and the
larger debt later — the marker stays a runtime invariant where the
type system could express it.

## Implementation sequence (post-design-merge)

These are not part of this PR; they are the agreed scope for the
follow-up implementation PR(s). Listed here so the design doc
captures the full plan. The chosen marker shape is Option A —
`CustomEnum` variant.

1. **carina-plugin-wit** — no change. The WIT boundary already carries
   `type-identity` for the only use that crosses it
   (`validate-custom-type`).
2. **carina-core** — introduce `AttributeType::CustomEnum`; drop
   `Custom.namespace` and `StringEnum.namespace`; route the 31
   `AttributeType::Custom { ... }` match sites either to the new
   variant or to a combined arm where the marker meaning is
   irrelevant; derive the legacy dotted prefix via
   `identity.dotted_prefix()` at the remaining `string_enum_parts` /
   diagnostic call sites.
3. **carina-provider-protocol** — mirror the carina-core shape change
   on the JSON wire form. The protocol's `Custom { name, base,
   namespace }` becomes `Custom` + `CustomEnum`.
   `carina-plugin-host::wasm_convert` updates accordingly.
4. **carina-lsp** — update site 3 (the `validate_custom` mirror) to
   match on the new variant; update `string_enum_parts`-style
   callers in completion/diagnostics.
5. **carina-provider-aws** (carina-rs/carina-provider-aws) — mechanical
   per-site replacement in `carina-aws-types/src/lib.rs`,
   `src/factory.rs`, `src/convert.rs`, `src/schemas/types.rs`,
   `src/main.rs`. Legacy `namespace: Some(_)` constructors become
   `CustomEnum { ... }`; `namespace: None` constructors keep the
   `Custom` variant minus the field. Codegen template change if
   applicable. Regenerate schemas; verify-diff against pre-rewrite
   snapshots.
6. **carina-provider-awscc** (carina-rs/carina-provider-awscc) — same
   plus the `src/bin/codegen.rs` template. The 145 sites include the
   generated `src/schemas/generated/*.rs` files; the template
   regeneration is the authoritative path.

Per `[[feedback_design_before_implementation_in_pr]]`, this design PR
merges first. The implementation PRs then land in carina-core →
provider-aws → provider-awscc order, with each provider PR bumping
the `carina-core` pin.

## Out of scope

- Snake-keyed validator maps (`aws_validators` / `awscc_validators`):
  the S3/S4 chain left these keyed on `pascal_to_snake(identity.kind)`
  at the boundary; migrating to `TypeIdentity` keys is its own
  follow-up, tracked separately (notes in
  `handoff_2026-05-23_typeidentity_chain_complete.md`).
- LSP completion / README user-facing form: shipped in carina#3225
  (closes #3223).
- A real-infra smoke against `carina-rs/infra`: user-driven per
  `[[feedback_no_real_infra_aws_commands]]`. The implementation PR
  will note where in its description a real-infra check belongs.

## Cross-references

- Design doc that this one extends: `2026-05-16-semantic-name-redesign-design.md`.
- Audit note that surfaced the directional `assignable_to` bug:
  `notes/audits/2026-05-23-type-system-evaluation.md`.
- Handoff: `memory/handoff_2026-05-23_typeidentity_chain_complete.md`
  (S2.5a hotfix invariant; field-removal delicacy).
- Follow-up issue: carina#3222.
