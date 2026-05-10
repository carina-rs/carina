# `Duration` Type and `<integer><unit>` Literal: Design

<!-- constrained-by ./2026-05-09-wait-construct-design.md#duration-type -->

## Goal

Introduce a first-class `Duration` type to Carina, accompanied by a `<integer><unit>` lexical literal (`75min`, `1h`, `30s`). The type wraps `std::time::Duration` and is usable wherever a duration-typed schema attribute is declared. Type-checked at parser/validation time; serialised as integer-seconds in state JSON; transported across the WIT plugin boundary as a plain integer.

The most immediate consumer is `wait { timeout = 75min }` (`carina#2825`), but the type is independently useful for any duration-shaped attribute (Route 53 record TTL, CloudWatch retention, future `directives { create_timeout = ... }`) currently typed as `Int seconds`.

## Non-goals

- Compound literals (`1h30m`). Out of MVP — defer until a concrete user need appears.
- Sub-second precision (`100ms`, `200us`). The MVP wait/poll use cases work at second granularity.
- Day / week units (`1day`, `2weeks`). Most AWS APIs accept duration in seconds/minutes/hours; can be added on demand.
- Conversion / arithmetic functions (`duration_seconds(d)`, `d + 30s`). Out of scope for the type itself.
- A standalone `time` type. Carina already has no notion of wall-clock time and does not need one for `wait`/timeout.
- Round-tripping the *original unit* (`75min`) through state JSON or WIT. State stores seconds; the human-friendly unit lives only in the source `.crn` file.

## DSL syntax

```crn
let cert_issued = wait cert {
    until   = cert.status == aws.acm.Certificate.Status.Issued
    timeout = 75min
}

let record = aws.route53.RecordSet {
    hosted_zone_id   = zone.id
    name             = "registry.carina-rs.dev"
    type             = "A"
    ttl              = 60s              # future: when ttl gets typed Duration
    resource_records = [...]
}
```

### Lexical grammar

Pinned form (taken verbatim from `carina-rs/carina#2824`):

```pest
duration_literal = @{ ASCII_DIGIT+ ~ duration_unit }
duration_unit    = @{
      "seconds" | "second" | "sec" | "s"
    | "minutes" | "minute" | "min" | "m"
    | "hours"   | "hour"   | "hr"  | "h"
}
```

- The integer part is unsigned. Negative durations are not representable (a wait timeout of `-5min` is meaningless; if such a use case appears it will be a different feature entirely).
- The unit suffix is matched **longest-first** within each magnitude group so `30sec` parses as `30 + sec`, not `30 + s + "ec"`. Pest's `|` is ordered choice, so the alternation order in `duration_unit` is load-bearing — the longer alternatives must come first.
- No internal whitespace allowed: `30 min` is two tokens, not a duration. The `@{ ... }` atomic rule enforces this.
- Examples that parse: `30s`, `5min`, `1h`, `75min`, `30sec`, `2hours`, `3hr`, `1m`.
- Examples that do **not** parse: `1.5h` (no fractional integer part), `1h30m` (compound), `30 min` (whitespace), `0.5min` (no fractional). Each surfaces as a parse error.

### Disambiguation from bare `number`

The existing grammar has `number = @{ "-"? ~ ASCII_DIGIT+ }`. Duration shares the leading-digit prefix with `number`. To keep the existing `number` unchanged when no unit follows, the literal-level alternation in `expression` is reordered so `duration_literal` is tried *before* `number`:

```pest
literal = { boolean | string | float | duration_literal | number | null_literal | ... }
```

`duration_literal` is atomic and requires a unit suffix, so it backtracks cleanly when the unit is absent and `number` matches the bare integer. Because Pest evaluates choices left-to-right and commits on first success, putting `duration_literal` before `number` does not regress any existing literal — `number` still wins when no unit follows.

The grammar change is local to the `literal` rule; no rule that already contains `number` in a different context is touched.

### Position in the value grammar

Duration literals appear wherever any other primitive value can appear: as a top-level attribute value, inside a list, inside a struct field, on the right-hand side of a `let`. There is no syntactic restriction beyond "wherever an `expression` is permitted".

## Type system integration

### `AttributeType::Duration`

A new variant in `carina_core::schema::AttributeType`:

```rust
pub enum AttributeType {
    String,
    Int,
    Float,
    Bool,
    Duration,                  // ← new
    StringEnum { ... },
    Custom { ... },
    List { ... },
    Map { ... },
    Struct { ... },
    Union(Vec<AttributeType>),
}
```

Rationale (decided during brainstorming):

- A new variant — over `Custom { semantic_name: "Duration", base: Int }` — keeps schema introspection, validation, diff, completion, and codegen all uniformly typed. Anything that currently `match`es on `AttributeType` learns one new arm; the alternative would route Duration through the generic `Custom` validator and then re-add Duration-specific shortcuts at every consumer.
- The choice aligns with the project's "type safety over runtime checks" memory rule: an attribute being a Duration is a structural fact, not a validation predicate against a string base type.

### `Value::Duration`

A new variant in `carina_core::resource::Value`:

```rust
pub enum Value {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Duration(std::time::Duration),  // ← new
    List(Vec<Value>),
    StringList(Vec<String>),
    Map(IndexMap<String, Value>),
    ResourceRef { path: AccessPath },
    BindingRef { binding: String },
    Interpolation(Vec<InterpolationPart>),
    FunctionCall { name: String, args: Vec<Value> },
    Secret(Box<Value>),
    #[serde(skip)]
    Unknown(UnknownReason),
}
```

The inner `std::time::Duration` carries no representation of the original unit (`75min` and `4500s` are indistinguishable once parsed). Plan display and `carina fmt` round-trip the value using a deterministic re-rendering rule (see "Display and round-trip" below); state files always serialise as integer seconds.

### Type-compatibility rules

Treated as a strict scalar — no implicit conversions:

| Schema type | Accepted DSL value | Rejected (parser/validation error) |
|---|---|---|
| `Duration` | `30s`, `5min`, `1h`, … | `30` (Int), `"30s"` (String), `30.0` (Float), `[30s]` (List) |
| `Int` | `30` | `30s` (Duration where Int expected) |
| `Float` | `30.0` | `30s` |
| `String` | `"30s"` | `30s` |

Diagnostics surface as `attribute 'foo' expects Duration but got Int` (or symmetric), wired through the existing `validation::type_check` site that already differentiates `Int` from `String` etc.

### `Value::Unknown` interaction

A `Duration` attribute can hold `Value::Unknown` (e.g. `timeout = upstream_state_ref.something`) under the same rules as any other typed attribute. The validation skip-arm pattern (`feedback_value_unknown_validation_sites.md`) applies: every `match`/`if let` on `Value::Duration` is paired with an `Unknown` arm that does not flag the attribute as wrong-typed.

## Plan display and round-trip

`Value::Duration` carries no source-unit metadata. Plan display and `carina fmt` re-render the duration deterministically:

```
fn render_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 { return "0s".into(); }
    if secs % 3600 == 0 { return format!("{}h",   secs / 3600); }
    if secs % 60   == 0 { return format!("{}min", secs / 60);   }
    format!("{}s", secs)
}
```

So:

- `75min` is preserved as `75min` (4500 s, divisible by 60 but not 3600).
- `1h` is preserved as `1h` (3600 s, divisible by 3600).
- `90s` stays `90s` (not divisible by 60).
- `45min` writes back as `45min`.
- Pathological round-trip: a user-written `2700s` ( = 45 minutes) is rewritten by `carina fmt` to `45min`. This is a **deliberate normalisation** — the canonical-form rule keeps `carina fmt` idempotent and avoids state drift. Documented in the formatter's behaviour note.

The unit-aliases (`m`, `min`, `minutes`) are not preserved: `5m` and `5min` both render as `5min`. Aliases exist only on input; the canonical output is the medium-length form (`s`, `min`, `h`).

## State file representation

Duration values serialise to JSON as integer seconds:

```json
{
    "carina_version": "v3",
    "resources": {
        "cert_issued": {
            "directives": { ... },
            "attributes": {
                "timeout": 4500
            }
        }
    }
}
```

Schema knows the attribute is typed `Duration`; deserialisation reads the integer and reconstructs `Value::Duration(Duration::from_secs(n))`. Without the schema (e.g. an attribute on a stale resource type) the value remains `Value::Int` and downstream typing is reasserted at the next plan.

> **Follow-up:** the schema-aware re-typing on the inbound state-load path is **not implemented in the Duration MVP** (carina#2962). Today every state-file integer reads back as `Value::Int(_)`, regardless of whether the schema attribute is `Duration`. The asymmetry is contained — no `AttributeType::Duration` reaches a real state file in the MVP because the only consumer (`wait { timeout = ... }`) does not persist. Re-typing lands in carina#2965 once a provider attribute migrates to `AttributeType::Duration` and creates a live consumer.

Rationale: the state file is read by tooling (other plan/apply invocations, `carina state` subcommands) and by humans only as a last resort. Storing `4500` is unambiguous, easy to compute on, and round-trips losslessly through serde without a custom deserialiser path. The "`75min` was the source form" information lives in the .crn file, not the state file.

State v3's existing schema versioning is sufficient — Duration deserialisation does not need a state version bump because it only adds an interpretation rule for an integer that already deserialises correctly.

## WIT plugin boundary

The `value` variant in `carina-plugin-wit/wit/types.wit` is **not** changed. Duration values cross the WIT boundary as `int-val(s64)` carrying the second count:

```wit
variant value {
    bool-val(bool),
    int-val(s64),     // ← Duration::as_secs() flows through here
    float-val(f64),
    str-val(string),
    list-val(string),
    map-val(string),
    secret-val(string),
}
```

Two host-side conversion points handle the marshaling:

- **Outbound** (`core_to_wit_value` in `carina-plugin-host/src/host_value.rs`): `Value::Duration(d) → wit::Value::IntVal(d.as_secs() as i64)`. Negative results impossible because `Value::Duration` cannot hold a negative duration.
- **Inbound** (`wit_to_core_value` in the same file): WIT does not annotate which integers are durations. The host queries the schema for the destination attribute's type; if it's `AttributeType::Duration`, the inbound `int-val(n)` is reconstructed as `Value::Duration(Duration::from_secs(n as u64))`. Otherwise it's an `Value::Int(n)`.

> **Follow-up:** the WIT-inbound schema-aware re-typing is **not implemented in the Duration MVP** (carina#2962). Today every `IntVal` reads back as `Value::Int(_)`, regardless of the destination schema's type. Same asymmetry rationale as the state-file path above: the only Duration consumer in the MVP is host-side and never crosses the WIT boundary. Re-typing lands alongside carina#2965 once a provider attribute migrates to `AttributeType::Duration`.

Rationale (decided during brainstorming):

- Adding a WIT variant (`duration-val(s64-secs)`) would be a breaking change requiring synchronised PRs against `carina-plugin-wit`, `carina-provider-aws`, and `carina-provider-awscc` — the same pattern as `#2596`. The MVP doesn't have a use case where a provider plugin needs to *natively know* "this is a Duration"; providers see seconds and use them as seconds.
- If a future use case appears (e.g. provider-side Duration formatting in API requests), promoting the WIT representation is mechanical and non-blocking. Document the decision so the future change is not surprising.

The WIT contract therefore stays at its current minor version.

## Schema codegen integration

Provider repos (`carina-provider-aws`, `carina-provider-awscc`) generate schemas via Smithy / CFN ingestion. Today, duration-shaped attributes (`Route53 RecordSet.ttl`, `CloudWatch LogGroup.retentionInDays`, etc.) emit `AttributeType::Int` because there is no Duration variant.

Codegen migration is **out of scope for this MVP issue**. After this PR lands:

- Provider-side codegen can be updated in a follow-up to recognise duration shapes (e.g. CFN attributes whose names match `(time|delay|interval|timeout|ttl|retention).*` and whose unit is implicit) and emit `AttributeType::Duration` instead of `Int`.
- Until that migration runs, existing `Int seconds` attributes still work — users write `30` (an Int) and the schema accepts it as today.
- The first internal consumer of `AttributeType::Duration` is the `wait` construct's `timeout` field (`#2825`), which is *not* codegen-driven — it lives in carina-core and declares its own typed schema.

This sequencing keeps the MVP contained: carina-core ships the type and parser support; provider repos opt in attribute by attribute when convenient, with no rush.

## LSP, formatter, diagnostics, TextMate

| Component | Change |
|---|---|
| `carina-lsp/src/completion/values.rs` | When the cursor is on the value position of a Duration-typed attribute, suggest snippet completions for the common units (`5min`, `30s`, `1h`). MVP keeps the list short and curated; a richer "type any digit and a unit suggestion appears" interaction can come later. |
| `carina-lsp/src/diagnostics/mod.rs` | Type-mismatch diagnostic when a non-Duration value is assigned to a Duration attribute, and vice versa. Reuse the existing type-check pathway; add the `AttributeType::Duration` arm. |
| `carina-lsp/src/semantic_tokens.rs` | Highlight duration literals as numeric (the digit run) plus modifier / decorator (the unit suffix). Acceptable to start with "highlight whole literal as numeric" — the unit suffix highlight is polish, not load-bearing. |
| `carina-core/src/formatter/format.rs` | Render `Value::Duration` per the canonical form rule above. |
| `editors/vscode/syntaxes/carina.tmLanguage.json` and `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` | Add a `\d+(s|sec|second|seconds|m|min|minute|minutes|h|hr|hour|hours)\b` numeric pattern that highlights duration literals. Both files must remain byte-identical (`tmlanguage_keyword_parity` test). |

## Acceptance criteria

The Duration type is "done" for MVP when:

1. `let foo = wait cert { timeout = 75min }` parses without error and produces an `AttributeType::Duration`-valued attribute carrying `Value::Duration(Duration::from_secs(4500))`.
2. Assigning `30` (Int), `"30s"` (String), or `30.0` (Float) to a Duration-typed attribute is a parse-or-validation error with a clear message.
3. `carina fmt` round-trips `75min` → `75min`, `5m` → `5min`, `2700s` → `45min` (canonical form), and `1h` → `1h`. **Deferred to carina#2966** in the MVP — the source-text formatter currently passes duration literals through verbatim; canonical-form rewriting fires on the value-tree consumers (Display, plan display, hover, exports) but not yet on `carina fmt`'s output.
4. State JSON serialises Duration as integer seconds (`{ "timeout": 4500 }`).
5. Plan display renders Duration in canonical form via `Display for Value::Duration` → `render_duration`. A dedicated end-to-end snapshot fixture under `carina-cli/tests/fixtures/plan_display/duration/` is **deferred** — the rendering path is unit-tested through `format_value_into` and through the per-feature display/hover/error tests added by Round 1 / Round 4 reviews.
6. WIT-bound providers (mock provider's `set_attribute("ttl", Value::Duration(...))`) see the value as `int-val(secs)` and the schema's typing reflects it.
7. LSP shows a type-mismatch diagnostic when a Duration attribute receives an Int.
8. TextMate highlights duration literals consistently in both VS Code and TextMate bundle grammars.
9. The directory-scoped acceptance fixture (multi-file `.crn` directory) parses cleanly with at least one Duration value and round-trips through `carina validate` and `carina fmt`.

## Risks

- **`number` ambiguity with the unit suffix.** If `duration_literal` precedes `number` in the literal alternation, no regression is expected, but unit testing every existing parse path (lists of integers, negative integers, function arguments) is required to confirm. Mitigation: a parser-level test sweep that targets the pre-Duration literal corpus and asserts unchanged parse trees.
- **`carina fmt` rewrites `2700s` → `45min`.** Users may write seconds intentionally and be surprised by the rewrite. Mitigation: document the canonical-form rule in `carina fmt --help` and the formatter test fixture; surface as a deliberate decision in the PR body so users can object early.
- **Provider plugins that relied on attribute-as-Int seconds may be confused if the schema migrates to Duration.** Mitigation: keep the codegen migration deferred and per-attribute. Each provider attribute migration is its own PR with explicit before/after tests.
- **Lossy round-trip of source unit.** A `.crn` written `60s` is reformatted to `1min` by `carina fmt`. This is intentional, but the `.crn` author may want to preserve their unit choice. Mitigation: the canonical-form rule is published; users who need a specific unit can convert manually before formatting (the formatter is opt-in and skippable).
- **`Value::Unknown` paths must learn the new variant.** Per `feedback_value_unknown_validation_sites.md`, every `match` over `&Value` that handles `Unknown` must learn `Duration`. The audit pass is part of the implementation plan.

## Out of scope (deferred)

- Compound literals (`1h30m`).
- Sub-second precision (`100ms`).
- Day / week units (`1day`, `2weeks`).
- Conversion functions (`duration_seconds`, `duration_minutes`).
- Provider-side codegen migration of existing `Int seconds` attributes.
- WIT-side `duration-val` variant (deferred until a provider needs to natively distinguish Duration).
- Source-unit preservation across state JSON (state always stores seconds).
- Formatter "preserve user unit" mode (formatter always emits canonical form).

## Related work

- `carina-rs/carina#2822` — `wait` construct design (immediate consumer of Duration).
- `carina-rs/carina#2823` — `depends_on` meta-arg (sibling Carina-core extension; no direct dependency between the two).
- `carina-rs/carina#2825` — `wait` construct implementation (blocked on this issue).
- `carina-rs/carina-provider-aws#244` — ACM Certificate (T6 registry usecase consumer at the bottom of the chain).
- `feedback_value_unknown_validation_sites.md` — every `Value` enum extension audits validation skip arms.
- `feedback_review_codegen_diff.md` — codegen migration follow-ups must review every diff before commit.
- `feedback_directory_scoped_features.md` — every parser/LSP feature ships with a multi-file fixture.
