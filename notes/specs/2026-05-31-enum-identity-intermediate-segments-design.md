# Enum Canonical Identity: Carry Intermediate Struct Segments (carina#3378)

<!-- derived-from ./2026-05-23-namespace-field-removal-design.md -->

## Status

Design proposal. Cross-repo chain (carina → carina-provider-aws). This
document is the design half; it must merge before any implementation PR
(see CLAUDE.md "Design PR must merge before implementation PR").

## Problem (carina#3378)

Enum identities use the canonical shape `<provider>.<segments...>.<kind>`,
where `segments` is currently the **outermost container only** — every
intermediate struct between the resource and the enum is dropped. Two
consequences:

1. **Flattening** — the dotted identity does not encode where the enum
   actually sits in the nested resource shape.
2. **Kind distortion** — to keep two enums under the same container from
   colliding on `<provider>.<container>.<kind>`, provider authors
   hand-inflate the kind (`Status` → `LifecycleRuleStatus`,
   `StorageClass` → `ReplicationStorageClass`), folding intermediate
   struct names into the kind. These are not the structural names.

| Registered identity | Real structure | Distortion |
| --- | --- | --- |
| `aws.s3.BucketLifecycleConfiguration.LifecycleRuleStatus` | `BucketLifecycleConfiguration → Rules → Status` | "Lifecycle" doubled; `Rule` folded into kind |
| `aws.s3.BucketReplicationConfiguration.ReplicationRuleStatus` | `… → Rules → Status` | same |
| `aws.s3.BucketReplicationConfiguration.ReplicationStorageClass` | `… → Rules → Destination → StorageClass` | 2 levels folded |
| `aws.iam.PolicyDocument.Effect` | `PolicyDocument → Statement → Effect` | `Statement` dropped |

Target shape (the issue's chosen direction — Option 1, confirmed with
the user 2026-05-31):

`aws.s3.BucketLifecycleConfiguration.Rules.Status.enabled`

- reflects the real structure,
- makes the kind a plain `Status`,
- is naturally unique vs `aws.s3.BucketReplicationConfiguration.Rules.Status`,
- drops the redundant doubled "Lifecycle".

## Root cause, located precisely

The investigation (read-only, against carina `4200865`, provider-aws
`19b28332`) pins the producer and the consumer defect separately.

### Producer side — carina-provider-aws (where the distortion lives)

- `carina-codegen-aws/src/main.rs:416`:
  `let namespace = format!("aws.{}", res.name);` — the namespace is
  **always** `aws.<ResourceDef.name>`. Intermediate struct nesting
  (`Rules`, `Statement`, `Destination`) is never walked into the
  namespace, for both the Smithy-generated path
  (`main.rs:1017`, top-level attrs) and the nested-struct path
  (`main.rs:1411`, `generate_struct_type`).
- The kind comes from `pascalize_enum_type_name(shape, field_name)`
  (`main.rs:1306-1307`, `3760`) which uses the **field name** only.
  The Smithy path therefore emits plain kinds; the distortion is
  **not** in codegen logic.
- The 11 distorted kinds are **hand-written** in
  `carina-aws-types/src/lib.rs` (`grep -c string_enum_identity` = 11):
  `LifecycleRuleStatus`, `TransitionStorageClass`,
  `ReplicationStorageClass`, `ReplicationRuleStatus`,
  `DeleteMarkerReplicationStatus`, `SseAlgorithm`, `Effect`, `Version`,
  `SqsRedrivePermission`, `PartitionDateSource`, `Protocol` — each
  passing `Some("aws.<Resource>")` as namespace (outermost only) and an
  inflated kind chosen to dodge collisions.

So the producer change is: **walk the struct path and put intermediate
struct segments into the namespace; stop inflating the kind.** This is
split between (a) the codegen namespace/segment derivation (so the
Smithy path emits structural segments) and (b) rewriting the 11
hardcoded `carina-aws-types` identities to structural form with plain
kinds.

### Consumer side — carina-core (the enabling change + the real defect)

`TypeIdentity` (`carina-core/src/schema/type_identity.rs`) already stores
`segments: Vec<String>` as **discrete axes of arbitrary length**, and its
core operations are already segment-count-agnostic:

- `same_type` / `assignable_to` — per-axis, compare full `segments` vec.
- `from_dotted` / `from_schema_type` / `dotted_prefix` / `Display` —
  build/render any depth. The docstring is explicit: *"The dotted display
  form is derived from the structure; it is never the source of truth and
  is never parsed back into axes."*
- `NamespacedId::matches_identity` (`utils.rs:189`) and
  `validate_enum_namespace`'s identity comparison iterate
  `expected.segments` against the parsed `segments_str` — already
  arbitrary-depth.

The **only** defect is `NamespacedId::parse`'s positional 5+-part shape
(`utils.rs:88-114`), which **pins TypeName at index 3** and pushes
everything after index 4 into `value` (to let dotted values like
`ipsec.1`, `2012_10_17` flow through). For a 6-part structural form it
mis-splits.

Ground-truth probe (structural identity
`aws.s3.BucketLifecycleConfiguration.Rules.Status`, kind `Status`,
segments `[s3, BucketLifecycleConfiguration, Rules]`):

| Path | Today | Correct |
| --- | --- | --- |
| `expand_enum_shorthand("enabled")` | `…Rules.Status.enabled` ✓ | ✓ (uses identity Display, not positional parse) |
| `validate_enum_namespace("…Rules.Status.enabled")` | **`Err`** ✗ | should be `Ok` |
| `convert_enum_value("…Rules.Status.enabled")` | **`"Status.enabled"`** ✗ | should be `"enabled"` |

`expand` is already correct because it goes through the identity.
`validate` and `convert` are wrong because they route through the
positional `NamespacedId::parse` index-3 pin.

## The fundamental tension (and why the type lens decides it)

Positional parsing of a fully-qualified enum string is **ambiguous**
once both (a) intermediate-struct segments and (b) dotted values exist.
Given `aws.s3.X.Rules.Status.enabled` you cannot positionally know
whether `Status` is the kind and `enabled` the value, or `Rules` is the
kind and `Status.enabled` a dotted value. Two positional disambiguators
were considered and rejected:

- **"TypeName = second-to-last, values may not contain dots"** — would
  break dotted enum values (`ipsec.1`, `2012_10_17`), which are
  load-bearing across `carina.pest`, the formatter, `detail_rows`, the
  LSP, and 36+ `utils.rs` references. That trades the 3378 bug for a new
  regression class in an unrelated feature. **Rejected** — it is a
  carve-out that breaks a sibling invariant, not a root fix.

The type-safe resolution (chosen): **the dotted string is display-only;
the structured `TypeIdentity` is the source of truth.** Anywhere an
identity is in hand, do not positionally re-parse — split using the
identity's `segments`/`kind` directly. `TypeIdentity` already enforces
this for `same_type`/`assignable_to`/etc.; this design extends it to the
two remaining identity-less positional consumers.

### The value-extraction situation, measured (decided 2026-05-31)

There are **two** enum-value extractors in the tree, with very different
segment-handling:

1. **`extract_enum_value_with_values(s, valid_values)`** (`utils.rs:388`)
   — the schema-aware extractor used by the **most correctness-critical
   path**, the differ's `StringEnum` arm (`comparison.rs:191-192`) and
   the plan renderer. It recovers the value by walking every
   uppercase-led segment and checking the tail against the known
   `valid_values` list, "earliest match wins". This is **already
   segment-count agnostic and dotted-value safe** — a runtime probe
   confirms it extracts `enabled` from
   `aws.s3.BucketLifecycleConfiguration.Rules.Status.enabled` (6-part)
   and `ipsec.1` from a 7-part `…TunnelOptions.Type.ipsec.1`. **No
   change needed.**
2. **`convert_enum_value(s: &str)`** (`utils.rs:477`) — the **positional**
   extractor (`NamespacedId::parse(s).map_or(s, |id| id.value())`),
   which inherits the index-3 pin and so mis-splits the 6-part form
   (`"Status.enabled"`). Non-test callers, all on display / alias paths:
   - `value.rs:471` `format_value_into` — display; string only.
   - `plan_tree.rs:305` resource-summary — string only.
   - `value.rs:1599` `resolve_value_alias` — has `resource_type` +
     `attr_name` + `factory`, so the schema/`valid_values` is reachable.

`is_dsl_enum_format(s: &str)` (`utils.rs:501`) is the same positional
`NamespacedId::parse(s).is_some()` predicate, used as a gate before
`convert_enum_value` at the same display sites.

Two candidate fixes were measured by stubbing and
`cargo check --workspace --all-targets`:

- **Option 2 — thread a `TypeIdentity` into the extractor / predicate.**
  Rejected. `format_value_into` is a **recursive value→string formatter
  called from 6 sites with no schema anywhere in its tree**; the
  arg-mismatch error count (7) understates the real cost — making the
  identity available means plumbing schema through the entire
  value-formatting subsystem (display, diff, plan-tree). Genuinely wide.
- **Option 1 (schema-aware) — chosen.** Replace `convert_enum_value`'s
  *positional* extraction with the same **`valid_values`-matching**
  logic the differ already uses, and drop the index-3 positional 5+-part
  branch in `NamespacedId::parse` (it is the single broken thing). This
  **removes** the broken positional re-parse rather than guarding it, and
  needs **no wire-format change**: because the schema-aware extractor is
  already segment-count agnostic, the stored fully-qualified `String`
  canonical form stays as-is — no `ConcreteValue` reshape, no state
  migration. Radius is the 3 display/alias sites plus the parser branch.

The remaining implementation question is purely **how the display sites
reach `valid_values`** (the differ/alias sites already do; the two pure
display sites — `format_value_into`, `plan_tree` summary — may not). The
fallback there is `extract_enum_value` (last-segment), which is correct
for every non-dotted value; the only values that need the `valid_values`
list to disambiguate are dotted ones (`ipsec.1`), and those reach the
display path already resolved. The implementation plan pins this with a
dotted-value-at-depth regression test before touching the parser.

`validate_enum_namespace` (which *has* the identity) is the easy half:
compare the fully-qualified input against `identity.segments` +
`identity.kind` directly instead of routing the split through the
positional parse.

## Proposed change set (cross-repo chain)

The root is one (flattening forces distorted kinds); it spans two repos.
Per the no-bandaid rule this is **one root, completed across an ordered
chain**, not a carina-only half-fix with the provider deferred.

### PR-A — carina-core: accept structural multi-segment identities

1. Make `validate_enum_namespace` compare the fully-qualified input
   against `identity.segments` + `identity.kind` directly (identity is
   in hand), instead of routing the kind/value split through the
   positional `NamespacedId::parse` index-3 pin.
2. **Drop the positional 5+-part branch in `NamespacedId::parse`** (the
   single broken thing — the index-3 pin) and route `convert_enum_value`
   through the schema-aware **`valid_values`-matching** extraction that
   the differ already uses (Option 1, decided by radius measurement
   2026-05-31; see "The value-extraction situation" above). This
   *removes* the broken positional re-parse rather than guarding it and
   needs **no** `ConcreteValue` reshape or state migration. Option 2
   (threading `TypeIdentity` through the recursive `format_value_into`
   formatter) was measured and rejected as a wide reshape of the
   value-formatting subsystem.
3. TDD: failing tests first — the probe table above becomes the
   regression suite. Add deeper-segment (5/6/7-part) cases to the
   `NamespacedId::parse`, `validate_enum_namespace`, `convert_enum_value`,
   `is_dsl_enum_format`, and `expand_enum_shorthand` test batteries,
   including a dotted-value-at-depth case
   (`aws.x.Y.Z.Type.ipsec.1`) to pin the ambiguity boundary.
4. Update CLAUDE.md "Namespaced Enum Identifiers" — the index-3 pin note
   is superseded; document that the identity is the source of truth and
   the dotted form is display.

PR-A is purely additive on the acceptance side (it widens what is
accepted); it does not require provider changes to be safe, and it does
not by itself change any AWS enum's spelling.

### PR-B — carina-provider-aws: emit structural segments + plain kinds

1. `carina-codegen-aws/src/main.rs`: thread the struct-nesting path into
   the namespace derivation so nested-struct enums carry their
   intermediate segments (`generate_struct_type` / `TypeResolutionContext`
   namespace), and stop relying on the field-name-only kind to be unique.
2. `carina-aws-types/src/lib.rs`: rewrite the 11 hardcoded identities to
   structural form with plain kinds
   (`LifecycleRuleStatus` @ `aws.s3.BucketLifecycleConfiguration`
   → `Status` @ `aws.s3.BucketLifecycleConfiguration.Rules`, etc.).
3. Regenerate schemas (`./scripts/generate-schemas-smithy.sh`) and review
   the generated diff (CLAUDE.md "Review every codegen diff").
4. Bump the carina dependency to the PR-A commit.

PR-B depends on PR-A being merged first (the new spellings only validate
once carina-core accepts them).

### Optional PR-C — provider-build collision guard (type-level backstop)

Even with structural segments, collision-avoidance is still unenforced
convention at the type level (a future author could still pick a
colliding structural path). A build-time / codegen-time check that
rejects duplicate flattened-or-structural identities makes the broken
state a build failure rather than a silent collision. Scope this only if
the radius is small; otherwise file as a referenced follow-up in the
same response as PR-B.

## Risk / non-goals

- **Shorthand resolution is field-position based and already correct**
  (the issue's first correction): a versioning `status` rejects
  `disabled`, a replication `status` rejects `suspended`, regardless of
  the dotted identity text. This change does not touch resolution
  correctness; it fixes identity-text accuracy + full-path validation +
  value extraction at depth, and removes the kind distortion.
- **State migration**: existing state files / saved plans may carry the
  old distorted spelling (`…LifecycleRuleStatus.enabled`). The
  implementation plan must check whether the alias/normalize pass
  rewrites these on read, or whether a one-time normalization is needed
  — this is a wire-format crossing and must be verified before PR-B
  (CLAUDE.md "Check wire-format / protocol crossings"). carina is an
  experimental project with no backward-compat guarantee, but a silent
  never-converging diff would be a regression and must be ruled out.

## Open decisions to settle in the implementation plan

1. ~~PR-A value-extraction option~~ — **DECIDED 2026-05-31: Option 1
   (schema-aware `valid_values` matching), drop the positional 5+-part
   branch.** Option 2 (thread `TypeIdentity` through `format_value_into`)
   measured as a wide reshape and rejected. No `ConcreteValue` reshape,
   no state migration. See "The value-extraction situation, measured".
2. Whether the codegen segment-walk and the 11 hand-written rewrites
   land in one PR-B or split (likely one: same root, same regen).
3. Whether PR-C is in-scope or a referenced follow-up.
