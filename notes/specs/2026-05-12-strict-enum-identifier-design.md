# Strict enum identifier validator — design document

**Related issue**: [#2986](https://github.com/carina-rs/carina/issues/2986)
**Feature branch**: `issue-2986-strict-identifier-enum`
**Date**: 2026-05-12

<!-- supersedes ./2026-04-22-naming-conventions-design.md -->

## Goal

Lift the DSL convention that enum-typed attributes are written as **identifiers** (snake_case alias, optionally fully-qualified) from a documentation guideline into a **type-system rule** enforced by the validator. Today the parser collapses both `ip_protocol = 'tcp'` (string literal) and `ip_protocol = tcp` (bare identifier) into the same `ConcreteValue::String("tcp")`, so the validator cannot distinguish them. PR #2985 tried to enforce the rule via a string-content heuristic on `dsl_aliases` and shipped a regression: `values:` entries that the schema explicitly accepted (`"-1"`, `"ip-name"`, `"plain-text"`) were rejected because their API spelling happens to coincide with a `dsl_aliases` row's `api` side.

The right answer is structural: the parser already distinguishes `namespaced_id`, `variable_ref`, and `string` rules — preserve that distinction through `Value` so the validator can match on it directly.

## Chosen strategy

Add `ConcreteValue::EnumIdentifier(String)` (and the borrowing projection `ConcreteValueRef::EnumIdentifier(&str)`) as a new concrete-axis variant. The parser produces `EnumIdentifier` for `namespaced_id`-shaped values and for bare `variable_ref`s that the expression evaluator resolves to a schema-declared enum spelling. Every other path that produces a string (string literal, interpolation result, function-call return) stays `ConcreteValue::String`. The `validate_string_enum` validator accepts only `EnumIdentifier`. Quoted string literals — `ip_protocol = 'tcp'` — fail with a type error directing the user to the identifier form.

This is the project's first user-facing type that exists *only* in the value layer (no schema-level `EnumIdentifier` type — `StringEnum` is still the schema type). The variant carries no extra payload beyond a `String`; the distinction is purely about how the value entered the value tree.

## Rationale

1. **Schema-honest.** A `StringEnum` is a closed set of identifiers from the schema's point of view. Once the parser already distinguishes "I wrote an identifier here" from "I wrote a string", the validator should match the same distinction. PR #2985's heuristic re-derived the question from the raw string and got it wrong because the string lost the syntactic context.
2. **No `values:` exclusion.** `values: ["tcp", "-1", "all", ...]` enumerates valid values; the new rule does *not* narrow that set. `ip_protocol = -1`-style identifier-form input cannot reach the validator (the parser rejects it as not an identifier), but `ip_protocol = all` does, and `-1` remains reachable through the `dsl_aliases` row `("-1", "all")`. Every entry in `values:` is still reachable as a DSL identifier through the alias table (verified for aws/awscc after `aws#269`, `awscc#223`, `awscc#230`).
3. **Type-level**, not heuristic. The `match` arms on `ConcreteValue` carry the rule. A future contributor cannot accidentally re-enable string-literal enum values by tweaking a regex.
4. **Future-proof.** When the type system eventually grows `type Status = Success | Failure` (sum-type enums declared in DSL), the same value-layer distinction generalises: type-system enums and schema-declared enums both produce `EnumIdentifier` at runtime; strings remain a separate axis.

## Non-goals

- **No new schema type.** `AttributeType::StringEnum` stays. The change is in the *value* layer, not the *type* layer.
- **No grammar redefinition** of `namespaced_id`. The pest rules already produce the right structural distinction; this work re-routes the AST construction, not the grammar.
- **No backward compatibility shim.** Per project policy (`feedback_no_backward_compat`), no transitional "accept both for one release" mode. Strictness is on from the merge commit; fixture / docs sweep follows in PR δ.
- **No WIT contract change.** The WIT side of the plugin boundary still sees `String`; `EnumIdentifier(s)` lowers to `s` at serialization.

## Design

### 1. Value layer

Add `EnumIdentifier(String)` to `ConcreteValue` and `EnumIdentifier(&'a str)` to `ConcreteValueRef`. Update the `From<&ConcreteValue> for ConcreteValueRef<'_>` projection.

```rust
pub enum ConcreteValue {
    String(String),
    EnumIdentifier(String), // NEW
    Int(i64),
    Float(f64),
    Bool(bool),
    Duration(std::time::Duration),
    List(Vec<Value>),
    StringList(Vec<String>),
    Map(IndexMap<String, Value>),
}
```

`Display` writes `EnumIdentifier(s)` as the bare `s` (no quotes); `Debug` writes it as `EnumIdentifier("s")` for clarity. Serde tags the variant explicitly (see §5) so state round-trips cannot collide with `String`.

### 2. Parser

`carina-core/src/parser/expression.rs` (or wherever `primary` is consumed) gains two arms:

- `Rule::namespaced_id` → `Value::Concrete(ConcreteValue::EnumIdentifier(raw_text))`.
- `Rule::variable_ref` → existing `BindingRef`/`Interpolation` resolution. If the binding does not resolve and the surrounding attribute's schema type is `StringEnum`, the resolver re-classifies the unresolved identifier as `EnumIdentifier` (short form, e.g. bare `tcp`). This is the same shape as today's "interpret unresolved name as namespaced enum value" path in `resolver.rs`, but the output value carries the new variant.

`Rule::string` continues to produce `ConcreteValue::String`.

### 3. Validator

`validate_string_enum` becomes:

```rust
fn validate_string_enum(&self, value: ConcreteValueRef<'_>) -> Result<(), TypeError> {
    let raw = match value {
        ConcreteValueRef::EnumIdentifier(s) => s,
        ConcreteValueRef::String(s) => {
            return Err(TypeError::EnumExpectedIdentifier {
                type_name: name.clone(),
                got: s.to_string(),
                hint: suggest_identifier_form(s, dsl_aliases, values),
            });
        }
        _ => return Err(TypeError::TypeMismatch { ... }),
    };
    // Existing resolve_enum_input / namespace check / values+alias match logic
    // runs only on the identifier path.
}
```

The `dsl_aliases` heuristic from PR #2985 (`rewritten_by_alias`) is removed. The validator accepts any value in `values` or in `dsl_aliases`'s dsl side — same as pre-#2985 — but only when the value arrived as `EnumIdentifier`.

`suggest_identifier_form` produces the actionable hint:

- If `dsl_aliases` has a `(api, dsl)` row where `api == got`, suggest `dsl`.
- Otherwise, if `got` is in `values` and is a valid identifier shape, suggest dropping the quotes.
- Otherwise, suggest one of the namespaced forms.

### 4. Callsite migration

Every `match` over `ConcreteValue` / `ConcreteValueRef` (~108 files) now has a new arm. The compiler enforces exhaustiveness. The default behavior for non-validator sites (serializer, differ, plan display, state writer) is to treat `EnumIdentifier(s)` the same as `String(s)`: the value is conceptually a string at every layer below the validator.

A small helper `ConcreteValueRef::as_string_like(&self) -> Option<&str>` returns `Some(s)` for both `String(s)` and `EnumIdentifier(s)` so the common "I just need the text" sites do not need to enumerate both arms.

### 5. State v3 serialization

`Value` already uses `#[serde(untagged)]` to round-trip the JSON shape. Adding an `EnumIdentifier(String)` arm that serializes as a bare string would collide with `String(s)` at decode time.

The solution: serialize `EnumIdentifier(s)` as `{"kind": "enum_id", "value": "..."}` (an internally-tagged shape used only for this variant). The `String` arm stays untagged. The custom `Deserialize` impl on `ConcreteValue` checks for the tagged object shape first and falls back to the untagged scalar.

Old state files written before this change contain `String` for what is now `EnumIdentifier`. The state-load path canonicalises: when a state value is being matched against a `StringEnum` schema attribute, the loader promotes a `String(s)` → `EnumIdentifier(s)` automatically. This is *not* a backward-compat shim — it is the same up-grade pass that all v3 readers already perform when re-typing untyped state fields.

### 6. WIT boundary

The plugin contract represents enum values as `string`. `EnumIdentifier(s)` lowers to the wire as `s`. The opposite direction — `Provider::read` returns a string, the read path classifies it as `EnumIdentifier` when the matching schema field is `StringEnum`. Same up-grade pass as state-load (§5).

### 7. LSP and tooling

- `carina-lsp/src/diagnostics/`: surface the new `EnumExpectedIdentifier` error with a code action that drops the quotes.
- `carina-lsp/src/completion/`: existing namespaced-enum completion is unaffected (it already emits identifier-shaped completions). Bare string-position completion (`'<cursor>'`) inside a `StringEnum` attribute is suppressed.
- Formatter: existing `'value'` in `StringEnum` position is rewritten to `value` (or fully-qualified namespaced form when ambiguity exists).

### 8. Test strategy

- Unit tests in `carina-core/src/schema/tests.rs`:
  - `validate_string_enum` rejects `String` arm, accepts `EnumIdentifier` arm, lists the suggested identifier in the error.
  - All existing tests are updated to construct `EnumIdentifier` instead of `String` where the value represents an enum identifier.
- Parser test: `ip_protocol = tcp` yields `EnumIdentifier("tcp")`; `ip_protocol = 'tcp'` yields `String("tcp")`.
- State round-trip test: `{"kind": "enum_id", "value": "tcp"}` ⇄ `EnumIdentifier("tcp")`; a v3 state file containing `"tcp"` for a `StringEnum` attribute loads back as `EnumIdentifier("tcp")`.
- WIT integration: plugin `Provider::read` returning `"tcp"` for a `StringEnum` attribute is observed in state as `EnumIdentifier("tcp")`.

## Migration

Cross-repo PR series (`carina#2986`):

1. **PR α** (`carina-provider-aws`) — already shipped equivalent (`aws#269`); no extra work needed.
2. **PR β** (`carina-provider-awscc#231`) — merged. `aws:kms` family aliases are now identifier-reachable.
3. **PR γ** (this design, `carina-core` + `carina-cli` + `carina-state` + `carina-lsp`) — value-layer split, validator, state migration, formatter rewrite.
4. **PR δ** (`carina-provider-aws` + `carina-provider-awscc`) — acceptance fixtures sweep: `'tcp'` → `tcp`, `'BucketOwnerEnforced'` → reject (must be `bucket_owner_enforced`), etc.
5. **PR ε** (`carina-rs/infra`) — user-driven deploy, fixture form sweep.

PRs δ and ε can land after PR γ; the strict rule is on from γ's merge so γ ships with the in-repo carina-core fixtures already converted.

## Open questions

1. **Short-form resolution timing.** When the user writes `ip_protocol = tcp` (bare), the resolver needs to know "the surrounding attribute is `StringEnum`" to decide between `BindingRef("tcp")` and `EnumIdentifier("tcp")`. Today the resolver is schema-aware only in some passes. Plan: thread the schema down to the same point that currently checks `is_dsl_enum_format`. If a binding named `tcp` exists, prefer the binding (the user can disambiguate with the fully-qualified form). This matches the precedence the LSP already documents.
2. **List-of-enum / Map-of-enum.** A `List<StringEnum>` value enters as `Value::Concrete(ConcreteValue::List(vec![...]))` where each inner `Value` follows the same rules. No extra work, the recursive validator handles it.
3. **Display in plan output.** `EnumIdentifier(s)` should render unquoted (`tcp`) in plan trees and TUI views to match user input. The few sites that currently special-case namespaced-string display can drop the heuristic.

## Risks

- **Compile-time scope is large** (~108 files). Mitigation: the compiler is the test suite — every site that needs updating fails to build until handled.
- **State migration boundary.** A v3 file written by today's binary contains `"tcp"` for `IpProtocol`; the new loader must classify it on read. The up-grade is local to state load; no on-disk format version bump.
- **Short-form ambiguity** with bindings (open question §1). Mitigation: precedence rule + LSP hint; the fully-qualified form is always available as the unambiguous escape hatch.
