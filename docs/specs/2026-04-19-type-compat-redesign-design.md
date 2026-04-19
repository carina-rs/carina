# Redesign: directional, subtype-aware type compatibility for `AttributeType::Custom`

## Goal

Replace the current symmetric, permissive "all String-based Custom types are
compatible" rule with a **directional, subtype-aware** compatibility check
so that assignments of distinct semantic ID types (e.g. `IdentityStoreId →
AwsAccountId`) are rejected at `carina validate` time, while legitimate
narrowings (`AwsAccountId → String(pattern)`) and same-semantic length
adaptations (`IdentityStoreId(len: 1..=64) → IdentityStoreId(len: 1..=36)`)
continue to type-check.

Originating bug: #2079 — `target_id = sso.identity_store_id` is silently
accepted by `carina validate .`.

## Background

### Current state

`carina-core/src/schema.rs::AttributeType::Custom` carries the shape:

```rust
Custom {
    name: String,                         // e.g. "VpcId", "String(pattern)", "String(pattern, len: 1..=64)"
    base: Box<AttributeType>,
    validate: fn(&Value) -> Result<(), String>,
    namespace: Option<String>,
    to_dsl: Option<fn(&str) -> String>,
}
```

The `name` field is overloaded: it's either a **semantic** identifier
(`"VpcId"`, `"AwsAccountId"`) or a **generic** string synthesized by codegen
(`"String(pattern)"`, `"String(pattern, len: 1..=64)"`). There's no way to
tell them apart structurally.

`is_compatible_with` (schema.rs:498) is symmetric and returns true whenever:
- `type_name` equality
- either side's `type_name` is `"String"`
- both sides are String-based Custom (regardless of name)

The last rule — added in commit 39b46590 to fix #1794/#1795 — unintentionally
collapses every String-based Custom into one equivalence class: `VpcId`,
`SubnetId`, `AwsAccountId`, `IdentityStoreId` are all mutually assignable.

### Why this is wrong

Carina's value proposition is a strongly-typed DSL. Silently accepting
`target_id = sso.identity_store_id` defeats the type system: we shift from
compile-time error to runtime CloudFormation failure. #2044 is another
example of the same class of "silent acceptance" bug; that one was closed
structurally by the resource-walk unification (#2046 plan). #2079 is its
type-compat cousin.

## Chosen approach

**Structural redesign of `AttributeType::Custom`** (Option B-full from
brainstorming discussion), plus a **directional** assignability check.

### New `Custom` shape

```rust
Custom {
    /// Some(name) when this type carries a semantic identity (e.g. "VpcId",
    /// "AwsAccountId"). None when this is a generic string/int pattern type
    /// synthesized by codegen for a CFN property without a named semantic.
    semantic_name: Option<String>,

    /// Base type — typically AttributeType::String or another Custom (for
    /// subtyping chains like vpc_id() has base aws_resource_id()).
    base: Box<AttributeType>,

    /// Optional regex pattern constraint (currently encoded in the name
    /// string or the validator closure). Structured here for comparison.
    pattern: Option<String>,

    /// Optional length bounds (min, max) as (Option<u64>, Option<u64>).
    length: Option<(Option<u64>, Option<u64>)>,

    validate: fn(&Value) -> Result<(), String>,
    namespace: Option<String>,
    to_dsl: Option<fn(&str) -> String>,
}
```

### Directional assignability

Replace `is_compatible_with(&self, other)` with
`is_assignable_to(&self, sink: &AttributeType)` where `self` is the source
(RHS) and `sink` is the expected type (LHS).

Rules (top to bottom, first match wins):

1. **Trivial**: same primitive types (`String → String`, `Int → Int`, etc.) — OK.
2. **Custom → Custom semantic_name mismatch**: if both sides are Custom
   with `Some(semantic_name)` and names differ → NG.
3. **Custom → Custom semantic_name match or either side None**: check
   `pattern` + `length`:
   - If `source.pattern` and `sink.pattern` both `Some`: require literal
     equality (pat-1 approach; revisit if real needs emerge).
   - If `sink.pattern` is `Some` and `source.pattern` is `None`: NG
     (source is unconstrained, sink requires a pattern).
   - Otherwise: OK.
   - For `length`: `source.length ⊆ sink.length` required.
     `(s_min, s_max) ⊆ (k_min, k_max)` iff
     `k_min.unwrap_or(0) ≤ s_min.unwrap_or(0)` and
     `s_max.unwrap_or(MAX) ≤ k_max.unwrap_or(MAX)`.
   - If either side's `length` is `None`, that side is unbounded on the
     missing bound.
4. **Custom (source with semantic_name) → non-Custom**: recurse on
   `source.base` vs `sink`. Narrowing to a generic base type is fine
   (covers #1795: `AwsAccountId → String`).
5. **non-Custom → Custom**: NG. Generic value has no proof of satisfying
   the sink's semantic/pattern/length.
6. **Union sink**: OK if source is assignable to any member.
7. **Union source**: OK iff source is assignable to sink for every member.

### Why pat-1 for patterns

Real regex subset-of checks are expensive and brittle. The #1794 use case
(length variation on the same logical ID) is handled by `semantic_name`
equality + `length` containment without needing pattern subset logic.
If future needs surface (distinct syntaxes for semantically-identical
patterns), we swap pat-1 for pat-2 (AST-level equivalence via
`regex-syntax`) or pat-3 (true language containment) without changing
the API.

### Codegen changes

**carina-provider-aws** and **carina-provider-awscc** codegen needs to
emit the new `Custom` shape:

- When emitting a semantic-typed property (already resolved by
  `resource_type_overrides` / `infer_string_type` → `super::vpc_id()`
  etc.): the helper in `carina-aws-types` already returns
  `Custom { name: "VpcId", ... }`. Migrate those helpers to set
  `semantic_name: Some("VpcId")`, `pattern: <regex if any>`,
  `length: <bounds if any>`, `base: String` (flat, not recursive — see
  note below).
- When emitting an anonymous pattern property: `semantic_name: None`,
  pattern/length filled from the CFN schema.
- Audit existing overrides. Several SSO/identitystore/etc. properties
  are today emitting `"String(pattern)"` but have unambiguous semantic
  meaning in the CFN docs (`TargetId` = account id, `PrincipalId` = 
  user or group id). Add overrides to route them to semantic helpers.

**Note on recursive bases**: today `vpc_id()` has
`base: Box::new(aws_resource_id())` which itself is a Custom. Under the
new rules, `source.base` recursion (rule 4) should still work — a
hierarchy of Customs doesn't break assignability logic as long as each
level carries its own semantic_name/pattern/length. We'll keep the
recursive structure.

### Migration (backward compat is NOT maintained)

Per project policy (`feedback_no_backward_compat.md`): we change the
`Custom` struct in place. All provider repos' generated schemas become
invalid until regenerated. The rollout order:

1. carina-core: new `Custom` shape, new `is_assignable_to`, update
   validators in carina-core tests and fixtures.
2. carina-provider-aws: codegen update, regenerate schemas, publish new
   version.
3. carina-provider-awscc: codegen update, regenerate schemas, add
   SSO-specific overrides (TargetId, PrincipalId, …), publish new
   version.
4. Verify #2079 is resolved on `infra/aws/management/identity-center/`.

Because provider crates depend on `carina-core` via git, step 1 merging
blocks provider builds until steps 2-3 ship. Plan is sequenced
accordingly.

## File structure / architecture

### carina (this repo)

- `carina-core/src/schema.rs`
  - `AttributeType::Custom` struct fields expanded.
  - `is_compatible_with` → deprecated or removed; replaced by
    `is_assignable_to`.
  - New internal helpers: `length_contains`, `pattern_equal`.
  - Existing tests: rewrite to match the new semantics. Tests that assert
    `VpcId ↔ SubnetId` symmetric compatibility are **the bug frozen into a
    test** and must be replaced with asymmetric-rejection tests.
- `carina-core/src/validation.rs`
  - `validate_resource_ref_types` call site (line ~142) passes
    `ref_attr_schema.attr_type.is_assignable_to(&attr_schema.attr_type)`
    — i.e. source = ref, sink = destination.
  - Error messages updated to match the new semantics.
- `carina-lsp/` — any call site using `is_compatible_with` migrates to the
  new API.

### carina-provider-aws

- `build/codegen` or equivalent: emit new Custom shape.
- `carina-aws-types/src/lib.rs` (local copy): update all
  `vpc_id()`, `subnet_id()`, `aws_account_id()`, ... helpers.
- Regenerate all schemas.

### carina-provider-awscc

- Same as carina-provider-aws.
- Additionally: extend `resource_type_overrides()` in
  `carina-provider-awscc/src/bin/codegen.rs` to cover `AWS::SSO::*`,
  `AWS::IdentityStore::*`, and similar properties whose CFN docs imply
  a semantic type.

## Edge cases and constraints

- **Recursive base types**: `vpc_id().base == aws_resource_id()` which
  is itself Custom. Rule 4 (source.base recurse) must not infinite-loop;
  traversal terminates because `base` must bottom out at a non-Custom.
- **Union types**: keep the existing Union semantics (rules 6 and 7
  above).
- **StringEnum**: keep existing behavior; enum-variant checks are
  separate from Custom assignability and continue to use the value-level
  validator.
- **Ref → anonymous String(pattern) sink**: a ResourceRef whose source
  type is `Custom { semantic_name: Some("AwsAccountId"), pattern: None, ... }`
  assigns cleanly to an anonymous `Custom { semantic_name: None,
  pattern: Some("..."), ... }` sink only if the source carries the same
  pattern or has no pattern conflict. Since source has `pattern: None`,
  rule 3 says NG (source unconstrained vs sink constrained). This means
  `AwsAccountId → String(pattern)` fails under strict rules — which
  contradicts #1795.
  - **Resolution**: `AwsAccountId` in the aws-types helper should set
    `pattern: Some("^\\d{12}$")` (the AWS account id regex). Then
    source pattern == sink pattern (when sink is a generic
    `String(pattern)` targeting the same) → OK. More generally,
    semantic helpers must populate `pattern`/`length` accurately so
    narrowing checks succeed.
- **Test fixtures**: `carina-cli/tests/fixtures/` may include schemas
  referencing `AttributeType::Custom { name: ... }` directly. All call
  sites using the literal struct expression need migration.

## Out of scope

- Pattern subset/equivalence beyond literal equality (pat-1 approach).
- Integer/float constraint narrowing (similar design exists for
  `Int` + range bounds, but that's a separate enhancement).
- Runtime coercion. Assignability is a compile-time (parse-time) check;
  no value transformation is implied.

## Status

Design only. Implementation tracked in the task plan.
