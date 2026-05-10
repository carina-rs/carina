# Naming conventions unification — design document

**Related issue**: [#2143](https://github.com/carina-rs/carina/issues/2143)
**Feature branch**: `brainstorm-2143-naming-conventions`
**Date**: 2026-04-22

## Goal

Unify type-name casing conventions across Carina's DSL so that **types are always PascalCase, values are always snake_case, and namespaces are always lowercase**. The current state is inconsistent (primitives are lowercase, custom types are snake_case, schema types are PascalCase, enum values mix all three), which forces readers and authors to keep a mental lookup table and blocks a clean design for upcoming type-system work (named struct declarations, future union / enum / tuple types).

## Chosen strategy

**Strategy Y** — PascalCase every user-facing type name, including primitives.

- Primitives: `string` → `String`, `int` → `Int`, `bool` → `Bool`, `float` → `Float`
- Custom types: `aws_account_id` → `AwsAccountId`, `ipv4_cidr` → `Ipv4Cidr`, `arn` → `Arn`, `iam_policy_arn` → `IamPolicyArn`, etc.
- Resource kinds: `aws.vpc` → `aws.ec2.Vpc`, `aws.s3.Bucket` → `aws.s3.Bucket`, `aws.security_group` → `aws.ec2.SecurityGroup`, etc. (provider and service segments stay lowercase; the final type segment becomes PascalCase)
- Schema types (`awscc.ec2.VpcId`, `awscc.s3.VersioningStatus`): unchanged — already in the target shape.
- Enum values: normalize **every** enum value to snake_case (strategy 3B). `.Enabled` → `.enabled`, `.GROUP` → `.group`, `.ap_northeast_1` stays.

Everything else — bindings, attribute names, field names, function names, module names, provider names, service names, backend type names, built-in function names — stays snake_case (or lowercase, in the namespace case). These are all *values* or *namespaces* under the Y rule.

### Rationale

1. **One rule covers every type** ("types are PascalCase"). No exceptions to memorize, no historical-carve-out explanation, no "why is `string` lowercase but `AwsAccountId` capitalized" paper cut.
2. **Casing becomes a real signal.** A reader scanning `accounts: Accounts = { ... }` can tell at a glance that `Accounts` is a type and `accounts` is a binding. Today that distinction is carried only by context.
3. **Matches Carina's positioning.** Carina treats types as first-class (has `TypeExpr`, `AttributeType::Struct`, custom type validation, plans for unions/enums). Type-serious languages (Haskell, Scala, F#) capitalize primitives too; that's consistent with what Carina is becoming.
4. **Aligns with AWS naming.** Provider-emitted schema types are already PascalCase (`VpcId`, `Enabled`, `Arn`); making user-facing types follow the same rule eliminates the mixed-casing pairs users see today.
5. **Extensible.** Future constructs (`type Status = Success | Failure`, `type Pair = (Int, String)`, refinement types) read cleanly with the rule because every type-position identifier is PascalCase.

### Why not Z

Z would keep primitives lowercase (`string`, `int`) and change everything else to PascalCase. That preserves the Terraform/Rust/Go surface for primitives but requires teaching "primitives are the exception." Since we've accepted breaking changes anyway, the one-rule-for-every-type consistency of Y wins out.

### Why not X / W

- X (snake_case everywhere): fights the AWS naming surface (`Enabled`, `VpcId` are the real API spellings) and removes the casing-as-type-signal property.
- W (do nothing): leaves the current inconsistency and kicks the can on every future type-system feature.

## Key design decisions

### D1. Type vs. value vs. namespace classification

| Carina concept | Category | Casing |
|---|---|---|
| Primitive type (`String`, `Int`, `Bool`, `Float`) | type | PascalCase |
| Custom type (`AwsAccountId`, `Ipv4Cidr`, `Arn`) | type | PascalCase |
| Schema type (`awscc.ec2.VpcId`) | type (namespaced) | path: lowercase, final: PascalCase |
| Resource kind (`aws.ec2.Vpc`) | type (namespaced) | path: lowercase, final: PascalCase |
| User-defined struct type (`struct Accounts { ... }`) | type | PascalCase |
| Enum value (`.enabled`, `.ap_northeast_1`) | value | snake_case |
| Provider name (`aws`, `awscc`) | namespace | lowercase |
| Service name (`ec2`, `s3`, `iam`) | namespace | lowercase |
| Backend kind name (`s3`, `local`) | namespace | lowercase |
| `let` binding | value | snake_case |
| Attribute name / field name | value | snake_case |
| Function name / function parameter | value | snake_case |
| Module name | value (callable) | snake_case |
| Argument / attribute parameter name | value | snake_case |
| Arbitrary identifier inside a value literal | value | snake_case |

### D2. Resource kinds get the `provider.service.TypeName` shape

Today: `aws.vpc`, `aws.security_group`, `aws.s3.Bucket` — inconsistent depth.
After Y + 4C: **always** `provider.service.TypeName`. `aws.vpc` is rewritten to `aws.ec2.Vpc`; `aws.security_group` to `aws.ec2.SecurityGroup`; `aws.s3.Bucket` to `aws.s3.Bucket`.

This aligns resource references with the existing schema-type shape (`awscc.ec2.VpcId`) and matches AWS CloudFormation (`AWS::EC2::VPC`) at the structural level. Provider codegen gains the responsibility of producing the `service` segment correctly; this is mechanical.

### D3. Enum values are snake_case; DSL ↔ AWS API mapping reuses existing alias machinery

The codebase already has a DSL-alias mechanism for enum values (`known_enum_aliases` in `carina-provider-awscc/src/bin/codegen.rs`, normalizer/conversion pass in `carina-provider-awscc/src/provider/{normalizer,conversion}.rs`). That's how `"-1"` ↔ `"all"` is implemented today for security-group `ip_protocol`.

Extend that exact machinery: provider codegen emits, for each `StringEnum`, a DSL-name (snake_case) ↔ API-name (whatever AWS returns — often PascalCase like `"Enabled"`, SHOUTY_SNAKE like `"GROUP"`, or already-snake like `"ap-northeast-1"`) mapping. The normalizer applies the mapping in both directions. **No new infrastructure** is needed; only one more automatic source of entries for the existing table.

### D4. No compatibility shims for values that are already snake_case

Binding names, attribute names, field names, etc. do not change. The DSL already puts these in a value position, and users already write them in snake_case. This issue is strictly about type-position identifiers and enum values.

### D5. Breaking change, no migration tool

Per user direction:
- Existing state files (`carina.state.json`) are **not** forward-compatible; users need to destroy-and-recreate or hand-edit state.
- No `carina migrate` subcommand. Rewrites are manual (editor find-replace, per-file review).
- No two-mode parser that accepts both old and new spellings in steady state. (A brief transition window is allowed during implementation — see migration plan.)

This keeps the parser simple, the diagnostics clean, and the codebase free of deprecation sugar. Total `.crn` files across all four Carina repos: ≈442 (carina: 161, provider-aws: 76, provider-awscc: 188, infra: 17). Rewrite is large but mechanical.

### D6. Rollout in three phases (strategy 6B simplified)

Four repositories must move: carina (this repo), carina-provider-aws, carina-provider-awscc, carina-rs/infra. Coordinating them in one PR is impossible (polyrepo). The plan is three sequential phases:

- **Phase A — carina core + providers**: parser accepts both old and new spellings temporarily; provider codegen (aws, awscc) emits new spellings; carina-core fixtures, acceptance tests, LSP, formatter, documentation, diagnostics, error messages all updated to new spellings. Land as one PR per repo, in order: carina-core → provider-aws → provider-awscc.
- **Phase B — infra**: rewrite every `.crn` in `carina-rs/infra` to new spellings. One PR.
- **Phase C — remove old-spelling support from carina-core parser**: the transition window closes. One PR.

The transient two-mode parser in Phase A is **not** a public feature; it's a migration scaffold that lives only from Phase A to Phase C. `carina validate` treats old spellings as warnings during this window.

### D7. Enum value normalization rule

Provider codegen is the single producer of `StringEnum` types. For each value `v` (the string AWS uses), emit the DSL spelling as follows:

- If `v` matches `^[A-Z][A-Z0-9_]*$` (SHOUTY_SNAKE, e.g. `GROUP`): DSL is `v.to_lowercase()` → `group`.
- If `v` matches `^[A-Z][a-z0-9]*([A-Z][a-z0-9]*)*$` (PascalCase, e.g. `Enabled`, `VersioningStatus`): DSL is snake_case(`v`) → `enabled`, `versioning_status`.
- If `v` matches `^[a-z0-9_-]+$` (already snake/kebab, e.g. `ap-northeast-1`): DSL replaces `-` with `_` if present (`ap_northeast_1`).
- Mixed / unrecognized: keep verbatim and log a codegen warning — this should never happen against well-formed AWS shapes.

The pair (DSL spelling, API spelling) becomes an entry in the alias table described in D3.

### D8. User-facing error messages use the new spellings

Every diagnostic that mentions a type name must emit the PascalCase form. `expected string, got int` becomes `expected String, got Int`. `validate_type_expr_value` already formats via `TypeExpr::Display`, so the change is entirely in how `TypeExpr::Simple` renders (old: `aws_account_id`; new: `AwsAccountId`). See the plan for the concrete rendering change.

### D9. State file schema

State JSON (`carina.state.json`) stores resource ids that include the resource kind (`aws.ec2.Vpc.web_vpc_abc123`). Under D2 this becomes `aws.ec2.Vpc.web_vpc_abc123`. Rather than write a migrator, old state files become unreadable; users re-plan against empty state (or hand-edit — the format is documented JSON). This is acceptable per D5.

## File structure and architecture

### In scope for this feature (carina-core)

| File | Change |
|---|---|
| `carina-core/src/parser/mod.rs` | `TypeExpr::Simple` / `SchemaType` / `Ref` rendering (`Display`) updated; parser accepts new PascalCase primitives and custom types; transition window (Phase A) accepts both |
| `carina-core/src/parser/carina.pest` | Keyword / identifier rules for new primitive spellings (`String`, `Int`, `Bool`, `Float`) and for `aws.ec2.Vpc`-shaped three-segment resource paths |
| `carina-core/src/formatter/carina_fmt.pest` | Mirror any grammar change |
| `carina-core/src/formatter/format.rs` | Type-expression formatter emits new spellings; no rewriting logic in fmt |
| `carina-core/src/schema.rs` | `AttributeType::Custom { semantic_name }` becomes authoritative for DSL spelling (was only internal) |
| `carina-core/src/validation.rs` | `is_type_expr_compatible_with_schema`, `validate_type_expr_value`, error messages use new spellings |
| `carina-core/src/utils.rs` | `pascal_to_snake` / `snake_to_pascal` helpers (may already partly exist); identifier classification helpers |
| `carina-core/src/keywords.rs` | Add `String`/`Int`/`Bool`/`Float` or handle via `TypeExpr` entirely (depends on grammar choice) |
| `carina-lsp/src/completion/**`, `carina-lsp/src/diagnostics/**`, `carina-lsp/src/semantic_tokens.rs` | Completions, diagnostics, and tokens emit/recognize new spellings |
| `carina-cli/tests/fixtures/**/*.crn` | Rewrite every fixture |
| `editors/vscode/syntaxes/carina.tmLanguage.json`, `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` | Update regexes so PascalCase identifiers color as types; keep the two files byte-identical |
| Docs in this repo | README / CLAUDE.md / example code blocks: rewrite |

### In scope for this feature (provider-aws, provider-awscc, downstream)

| Repo | Change |
|---|---|
| `carina-provider-aws` | Codegen emits new spellings for custom types and resource kinds; acceptance-test `.crn` rewritten |
| `carina-provider-awscc` | Same as above; enum alias table in `known_enum_aliases` grows to cover every PascalCase / SHOUTY_SNAKE enum value |
| `carina-rs/infra` | Every `.crn` rewritten (Phase B) |

### Out of scope (explicit non-goals)

- Moving custom type names under provider namespaces (`aws.AccountId` style) — deferred to a separate issue (Q5B).
- Migration subcommand for `.crn` files — deferred to user direction if requested later.
- State-file migration tool — users destroy-and-recreate or hand-edit.
- Generics, refinement types, union / enum syntax for user-defined types — separate issues; this work just makes the casing rule uniform enough that those can plug in cleanly.
- Renaming function names or attribute names — values stay snake_case; no work here.

## Edge cases and constraints

### EC1. Identifier shadowing between types and bindings

With Y, type and binding names can share spelling ignoring case (`accounts: Accounts = ...`). The parser already disambiguates by position (type vs. value slot), so no scoping rule is required. Explicit test required: a binding named `string` should still be valid (lowercase position is always a value). Conversely, `String` in a value position is a free identifier / resource binding name, not a type.

### EC2. Custom types whose names collide after PascalCase

`iam_policy_arn` → `IamPolicyArn`, `ec2_instance_id` → `Ec2InstanceId`. After PascalCase-ization, uniqueness is preserved for every current Carina custom type (checked; none collide). Future additions must maintain uniqueness but this is already a convention for `semantic_name`.

### EC3. Acronyms inside PascalCase

`iam`, `ec2`, `arn`, `kms`, `sns`. Following the `semantic_name` precedent (`AwsAccountId`, not `AWSAccountId`; `IamPolicyArn`, not `IAMPolicyArn`), first letter capitalized only: `Ipv4Cidr`, `Ipv6Address`, `IamPolicyArn`, `KmsKeyArn`. Rule: treat acronyms as regular words. Consistent with Rust convention and with the existing `semantic_name` field values in the codebase.

### EC4. `aws.vpc` → `aws.ec2.Vpc` requires a service-name table

Today `aws.vpc` has no explicit service segment; the provider carries service info internally but the DSL path doesn't show it. After 4C, every resource kind has a three-segment path including the service. Provider codegen must attach the service segment to each resource. This is a genuine semantics-visible change and must be applied across `aws` (SDK-based) and `awscc` (CloudControl) identically.

### EC5. TextMate grammar regexes

The two tmLanguage files currently match type names partially by lowercase patterns. With Y, the type-name regex must match PascalCase (optionally `snake_case` during the transition window). The byte-identical parity test in `carina-core/tests/tmlanguage_keyword_parity.rs` must continue to pass.

### EC6. LSP semantic tokens

`carina-lsp/src/semantic_tokens.rs` needs to tokenize PascalCase identifiers in type position as `semanticTokenType::type`. The heuristic "PascalCase final segment in a dotted path" already used for `SchemaType` must be generalized to any bare PascalCase in a type context.

### EC7. Transition window parser behavior

During Phase A, the parser must accept `string` and `String` both as primitive. The simplest implementation: in `parse_type_expr`, the `type_simple` arm recognizes both `string`/`String`, `int`/`Int`, `bool`/`Bool`, `float`/`Float`. For custom types, look up either case in the registry. Old spellings emit a deprecation warning through the existing diagnostic path (`ParsedFile::warnings`). Phase C removes the both-cases recognition.

### EC8. Error-message casing

Every diagnostic that quotes a type name from a `TypeExpr::Display` output automatically inherits the new casing once `TypeExpr::Display` is changed. Sweep grep for hardcoded lowercase type names in error strings and fix them separately. Known spots: `validation.rs`, `module_resolver/mod.rs`, `parser/mod.rs` (function argument type mismatch).

### EC9. Snapshot tests

Every snapshot that captures `TypeExpr::Display` output or error messages must be regenerated with `cargo insta accept`. This is expected work, not a bug.

### EC10. Enum value rewriting in fixtures

Fixtures that use `aws.s3.VersioningStatus.Enabled` become `aws.s3.VersioningStatus.enabled`. Mechanical but non-trivial volume; count of affected fixture lines is estimated per phase plan.

## Risks

| Risk | Mitigation |
|---|---|
| Phase A's two-mode parser leaks past Phase C | Time-box Phase C landing within the same release cycle; CI lint that refuses old spellings once Phase C ships |
| provider-aws / provider-awscc update order matters | Phase A order (carina → aws → awscc) keeps each repo's tests green against the current carina-core |
| TextMate regex divergence | Existing byte-identical parity test catches it |
| `aws.vpc` → `aws.ec2.Vpc` needs a service mapping that's correct | Unit-test the service table against CloudFormation shape names |
| Acronym PascalCase-ization disagreement (`IAM` vs `Iam`) | Apply uniformly via `pascal_to_snake`'s inverse; codify with a test in `utils.rs` |
| Snapshot churn is large | Regenerate per phase; commit snapshot updates as part of the same PR |

## Success criteria

1. `cargo test` passes in carina-core, provider-aws, provider-awscc after their Phase A PRs.
2. `carina validate` passes on every `.crn` in the carina repo's fixtures, acceptance tests, and documentation examples, using new spellings only.
3. `carina validate` passes on every `.crn` in `carina-rs/infra` after Phase B.
4. After Phase C, the parser rejects every old spelling (`string`, `aws_account_id`, `aws.vpc`, `.Enabled`) with a clear diagnostic pointing at the new form.
5. LSP completion at a type-annotation site proposes `String`, `Int`, `Bool`, `Float`, `AwsAccountId`, etc. — not the old spellings.
6. `TypeExpr::Display` round-trips (`parse(type.to_string()) == type`) for every variant.
7. TextMate grammar parity test still passes; PascalCase identifiers in type position highlight as types in VSCode.

## Open follow-ups (not in this design)

- Named struct declarations (#2142): decision on `struct Name { }` vs. `type Name = ...` etc. The casing-rule outcome here makes `struct Accounts { ... }` the natural spelling and settles #2142's N1/N2 casing question as N1.
- Namespaced custom type names (`aws.AccountId`) (#2143 Q5B): separate issue.
- Union / enum / tuple / refinement types: separate issues; now unblocked since the casing rule scales to them.
