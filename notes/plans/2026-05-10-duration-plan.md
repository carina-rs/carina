# `Duration` Type and `<integer><unit>` Literal: Implementation Plan

<!-- derived-from ../specs/2026-05-10-duration-design.md -->

## Repository scope

This plan covers `carina-rs/carina` only. No provider repository (`carina-provider-aws`, `carina-provider-awscc`) work is required for the MVP — those repos see no schema or WIT change and continue to emit `AttributeType::Int` for second-typed attributes. Provider-side codegen migrations are deferred follow-ups, scoped per attribute.

## Prerequisites

None. `Duration` lands as a standalone Carina-core extension. Its consumer issues are:

- `#2825` — `wait` construct implementation. Hard blocker today; unblocked by this PR.
- `#2823` — `depends_on` meta-arg. Already merged in main; not related to Duration.

There is no merge-order coupling with `#2825`: this PR can land first; `#2825`'s plan already references `timeout = 75min` and will pick up the type once it lands.

## File map

### Files to create

| Path | Purpose |
|---|---|
| `carina-core/tests/fixtures/duration/basic/main.crn` | Multi-file fixture: at least one `let` whose value is a Duration literal. |
| `carina-core/tests/fixtures/duration/basic/sibling.crn` | Sibling file in the same directory; demonstrates directory-scoped parse (per CLAUDE.md). |
| `carina-cli/tests/fixtures/plan_display/duration/main.crn` | Plan-tree snapshot fixture that exercises Duration display. |
| `carina-cli/tests/fixtures/plan_display/duration/carina.state.json` | Empty starting state for the snapshot fixture. |
| `carina-cli/tests/fixtures/plan_display/duration/snapshot.txt` | Snapshot written by `cargo insta accept` after the first run. |

### Files to modify

| Path | Change |
|---|---|
| `carina-core/src/parser/carina.pest` | Add `duration_literal` and `duration_unit` rules. Reorder the `literal` alternation so `duration_literal` is tried before `number`. |
| `carina-core/src/parser/expressions.rs` (or wherever `Rule::number` is converted to `Value`) | Add a `Rule::duration_literal` arm that splits the matched span into integer + unit, normalises to seconds, and constructs `Value::Duration(Duration::from_secs(n))`. |
| `carina-core/src/schema/mod.rs` | Add `Duration` variant to `AttributeType`. Update `Debug` impl to render it. Update `PartialEq` (derived) coverage; if `AttributeType` has any hand-rolled comparison, add the variant. |
| `carina-core/src/resource/mod.rs` | Add `Duration(std::time::Duration)` variant to `Value`. Update `Debug` derives, serde derive arm test (see Phase 2 task 2.2). |
| `carina-core/src/value.rs` | Add `Duration` arms to: `value_to_json` (serialise as `i64` seconds, named JSON-number arm), `dsl_value_to_json` (same), `core_to_wit_value` (emit `IntVal(secs as i64)`), `redact_secrets_*` (passthrough), `canonicalize_with_type` (passthrough), all `match` sites that exhaustively cover `Value`. |
| `carina-core/src/eval_value.rs` | Add `Duration` arm to evaluator passes (likely passthrough — Duration carries no nested references). |
| `carina-core/src/explicit.rs` | Add `Duration` arm to any explicit-resolution walker that exhaustively matches `Value`. |
| `carina-core/src/diff_helpers.rs` | Add `Duration` arm so two Durations diff by comparing the inner `Duration`. Verify diff output renders durations using the canonical form. |
| `carina-core/src/validation/mod.rs` | In the type-checking site that maps `AttributeType` ↔ accepted `Value` shape, add the `(AttributeType::Duration, Value::Duration(_)) => Ok(())` arm and the cross-type rejections (`(AttributeType::Duration, Value::Int(_))`, etc.). Add `Value::Unknown` skip arms wherever the existing pattern does the same for other types. |
| `carina-core/src/formatter/format.rs` (or wherever attribute values are formatted) | Implement `render_duration(Duration) -> String` per the canonical-form rule. Use it for any `Value::Duration` it encounters. |
| `carina-core/src/keywords.rs` | No change. Duration unit names (`s`, `min`, `h`, etc.) are not keywords; they are literal suffixes in the lexer. |
| `carina-core/src/plan.rs::format_effect_brief` | If a Wait/Update/Create effect emits a duration in its display, route through `render_duration`. |
| `carina-plugin-host/src/host_value.rs` (or equivalent boundary) | `core_to_wit_value`: `Value::Duration(d) → wit::Value::IntVal(d.as_secs() as i64)`. `wit_to_core_value`: route through schema lookup (already done for `Custom { semantic_name: "Duration", ... }`-style cases — extend to `AttributeType::Duration`). |
| `carina-state/src/...` (state v3 deserialisation) | When a state attribute's schema type is `AttributeType::Duration`, deserialise an integer-JSON as `Value::Duration(Duration::from_secs(n))`. |
| `carina-lsp/src/diagnostics/mod.rs` | Mirror the validation type-check arms so editor diagnostics match `carina validate`. |
| `carina-lsp/src/completion/values.rs` | When the cursor sits on the value position of a `Duration`-typed attribute, return curated snippet candidates (`30s`, `1min`, `5min`, `1h`). |
| `carina-lsp/src/semantic_tokens.rs` | If `tokenize_line` does a per-rule walk, emit a `numeric`-class token for the digit run of a duration literal; otherwise emit `numeric` for the whole literal as a stopgap. |
| `carina-core/src/parser/format_helpers.rs` (or wherever the formatter pretty-prints values) | If shared with `formatter/format.rs`, route Duration through the same `render_duration`. |
| `editors/vscode/syntaxes/carina.tmLanguage.json` | Add a numeric pattern matching `\b\d+(s|sec|second|seconds|m|min|minute|minutes|h|hr|hour|hours)\b`. |
| `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` | Same change, byte-identical (parity test enforces). |

### Dependencies between files

```
parser/carina.pest ── parser/expressions.rs (Rule::duration_literal arm)
        ↓
schema/mod.rs (AttributeType::Duration) ── resource/mod.rs (Value::Duration)
        ↓                                              ↓
        ├── value.rs ── all serialisation / WIT / canonicalisation arms
        ├── eval_value.rs / explicit.rs / diff_helpers.rs
        ├── validation/mod.rs ────── carina-lsp diagnostics
        ├── plan.rs (format_effect_brief)
        ├── formatter/format.rs (render_duration canonical form)
        ├── plugin-host/host_value.rs (WIT marshaling)
        └── carina-state (state v3 deserialisation)

editors/*/carina.tmLanguage.json ── tmlanguage_keyword_parity test
```

## Tasks

Each task is one TDD cycle. Goal: failing test → minimal impl → passing test.

### Phase 1 — Lexical and AST primitives

**Task 1.1: Pest grammar accepts `<integer><unit>` and produces a `Rule::duration_literal` token.**

- **Files:** modify `carina-core/src/parser/carina.pest`.
- **Test:** add to `carina-core/src/parser/tests.rs` (or the closest existing parser-test module):
  ```rust
  #[test]
  fn duration_literal_parses_minutes() {
      let p = CarinaParser::parse(Rule::duration_literal, "5min").unwrap().next().unwrap();
      assert_eq!(p.as_str(), "5min");
  }

  #[test]
  fn duration_literal_parses_all_units() {
      for src in ["30s", "30sec", "30second", "30seconds",
                  "5m", "5min", "5minute", "5minutes",
                  "1h", "1hr", "1hour", "1hours"] {
          let p = CarinaParser::parse(Rule::duration_literal, src).unwrap().next().unwrap();
          assert_eq!(p.as_str(), src);
      }
  }

  #[test]
  fn duration_literal_rejects_whitespace() {
      assert!(CarinaParser::parse(Rule::duration_literal, "30 min").is_err());
  }

  #[test]
  fn duration_literal_rejects_fractional() {
      assert!(CarinaParser::parse(Rule::duration_literal, "1.5h").is_err());
  }

  #[test]
  fn bare_number_still_parses_unchanged() {
      // Regression: existing Int values must keep parsing as Int.
      let p = CarinaParser::parse(Rule::number, "30").unwrap().next().unwrap();
      assert_eq!(p.as_str(), "30");
  }
  ```
- **Implementation:**
  ```pest
  duration_literal = @{ ASCII_DIGIT+ ~ duration_unit }
  duration_unit    = @{
        "seconds" | "second" | "sec" | "s"
      | "minutes" | "minute" | "min" | "m"
      | "hours"   | "hour"   | "hr"  | "h"
  }
  ```
  Reorder the `literal` alternation so `duration_literal` is before `number` (or wherever `number` is referenced as a fallback).

**Task 1.2: `AttributeType::Duration` exists and renders correctly.**

- **Files:** modify `carina-core/src/schema/mod.rs`.
- **Test:**
  ```rust
  #[test]
  fn attribute_type_duration_renders_in_debug() {
      let t = AttributeType::Duration;
      assert_eq!(format!("{:?}", t), "Duration");
  }

  #[test]
  fn attribute_type_duration_clone_eq() {
      let a = AttributeType::Duration;
      let b = a.clone();
      // If AttributeType has PartialEq derived, also assert here:
      // assert_eq!(a, b);
      let _ = (a, b);
  }
  ```
- **Implementation:** add the variant to the enum. Update the hand-written `Debug` impl with `AttributeType::Duration => f.write_str("Duration"),`.

**Task 1.3: `Value::Duration(std::time::Duration)` exists and round-trips through serde.**

- **Files:** modify `carina-core/src/resource/mod.rs`.
- **Test:**
  ```rust
  #[test]
  fn value_duration_round_trips_serde() {
      use std::time::Duration;
      let v = Value::Duration(Duration::from_secs(4500));
      let json = serde_json::to_string(&v).unwrap();
      let back: Value = serde_json::from_str(&json).unwrap();
      match back {
          Value::Duration(d) => assert_eq!(d, Duration::from_secs(4500)),
          other => panic!("expected Duration, got {other:?}"),
      }
  }
  ```
- **Implementation:** add `Duration(std::time::Duration)` to the `Value` enum. `std::time::Duration` already implements `Serialize`/`Deserialize` (via serde's std support); double-check the `Cargo.toml` features include `serde/derive` and the std `Duration` is supported. If not, write a small `serde::Serializer`/`Deserializer` adapter that emits/reads an integer-seconds JSON.

### Phase 2 — Parser → Value bridge

**Task 2.1: A duration literal in source becomes `Value::Duration` after parsing.**

- **Files:** modify `carina-core/src/parser/expressions.rs` (or whichever module converts `Rule::*` to `Value`).
- **Test:** add an end-to-end parse test (existing test module that parses an attribute value):
  ```rust
  #[test]
  fn parser_returns_value_duration_for_75min() {
      use std::time::Duration;
      let src = r#"let t = 75min"#;
      let resources = parse_for_test(src).unwrap();
      let v = resources.get_let_binding_value("t").unwrap();
      match v {
          Value::Duration(d) => assert_eq!(*d, Duration::from_secs(75 * 60)),
          other => panic!("expected Duration, got {other:?}"),
      }
  }
  ```
- **Implementation:** match on the matched span:
  ```rust
  Rule::duration_literal => {
      let s = pair.as_str();
      let unit_start = s.find(|c: char| !c.is_ascii_digit()).unwrap();
      let n: u64 = s[..unit_start].parse().unwrap();
      let secs = match &s[unit_start..] {
          "s" | "sec" | "second" | "seconds" => n,
          "m" | "min" | "minute" | "minutes" => n * 60,
          "h" | "hr"  | "hour"   | "hours"   => n * 3600,
          _ => unreachable!("grammar restricts the suffix"),
      };
      Value::Duration(std::time::Duration::from_secs(secs))
  }
  ```
  If `n * 60` or `n * 3600` overflows `u64`, error explicitly with a parse-time diagnostic (`duration overflow: {n}{unit} exceeds u64 seconds`). The grammar accepts arbitrary `\d+`, so a 30-digit literal is theoretically possible.

**Task 2.2: Type-check accepts Duration↔Duration, rejects cross-type assignments.**

- **Files:** modify `carina-core/src/validation/mod.rs`.
- **Test:**
  ```rust
  #[test]
  fn duration_type_accepts_duration_value() {
      let result = check_value_against_type(
          &Value::Duration(Duration::from_secs(60)),
          &AttributeType::Duration,
      );
      assert!(result.is_ok());
  }

  #[test]
  fn int_attribute_rejects_duration_value() {
      let err = check_value_against_type(
          &Value::Duration(Duration::from_secs(60)),
          &AttributeType::Int,
      ).unwrap_err();
      assert!(err.message().contains("Duration"));
  }

  #[test]
  fn duration_attribute_rejects_int_value() {
      let err = check_value_against_type(
          &Value::Int(60),
          &AttributeType::Duration,
      ).unwrap_err();
      assert!(err.message().contains("Duration"));
  }
  ```
- **Implementation:** add the `(AttributeType::Duration, Value::Duration(_)) => Ok(())` arm and cross-type rejections in the existing type-check function.

**Task 2.3: `Value::Unknown` is accepted for `Duration`-typed attributes (deferred resolution).**

- **Files:** modify `carina-core/src/validation/mod.rs`.
- **Test:**
  ```rust
  #[test]
  fn duration_attribute_accepts_unknown_value() {
      let result = check_value_against_type(
          &Value::Unknown(UnknownReason::ForValue),
          &AttributeType::Duration,
      );
      assert!(result.is_ok());
  }
  ```
- **Implementation:** ensure the existing `Unknown` skip-arm pattern covers `AttributeType::Duration`. Reference `feedback_value_unknown_validation_sites.md` in the PR body.

### Phase 3 — Serialisation and round-trip

**Task 3.1: `value_to_json` serialises Duration as integer seconds.**

- **Files:** modify `carina-core/src/value.rs`.
- **Test:**
  ```rust
  #[test]
  fn value_to_json_duration_emits_integer_seconds() {
      let v = Value::Duration(Duration::from_secs(4500));
      let j = value_to_json(&v).unwrap();
      assert_eq!(j, serde_json::json!(4500));
  }
  ```
- **Implementation:** add the arm `Value::Duration(d) => Ok(serde_json::Value::Number((d.as_secs() as i64).into()))`.

**Task 3.2: Every exhaustive `match` on `&Value` learns the `Duration` arm.**

- **Files:** sweep all sites named in the file map (`value.rs`, `eval_value.rs`, `explicit.rs`, `diff_helpers.rs`, `redact_secrets_*`, `canonicalize_with_type`, `dsl_value_to_json`).
- **Test:** rely on `cargo check` failing on non-exhaustive matches; add a smoke test in each module asserting Duration round-trips through that module's path. The mechanical sweep is the main work; tests guarantee the new arm has the intended semantics.
- **Implementation:** for most arms, the Duration is a passthrough (no nested references, no canonicalisation). The exception is `dsl_value_to_json` which mirrors `value_to_json`'s integer-seconds rule.

**Task 3.3: WIT outbound marshaling emits `IntVal(secs)`.**

- **Files:** modify `carina-plugin-host/src/host_value.rs` (or the closest equivalent).
- **Test:**
  ```rust
  #[test]
  fn core_to_wit_duration_emits_int_val_secs() {
      let v = Value::Duration(Duration::from_secs(4500));
      let wit = core_to_wit_value(&v).unwrap();
      assert!(matches!(wit, wit::Value::IntVal(4500)));
  }
  ```
- **Implementation:** `Value::Duration(d) => wit::Value::IntVal(d.as_secs() as i64)`.

**Task 3.4: WIT inbound unmarshaling reconstructs Duration when the schema knows it.**

- **Files:** modify the WIT inbound site in `carina-plugin-host`.
- **Test:**
  ```rust
  #[test]
  fn wit_to_core_int_becomes_duration_when_schema_says_duration() {
      let wit = wit::Value::IntVal(4500);
      let v = wit_to_core_value(&wit, Some(&AttributeType::Duration)).unwrap();
      assert!(matches!(v, Value::Duration(d) if d == Duration::from_secs(4500)));
  }

  #[test]
  fn wit_to_core_int_stays_int_when_schema_says_int() {
      let wit = wit::Value::IntVal(4500);
      let v = wit_to_core_value(&wit, Some(&AttributeType::Int)).unwrap();
      assert!(matches!(v, Value::Int(4500)));
  }
  ```
- **Implementation:** branch on the schema-supplied type at the conversion site; default to `Value::Int(n)` if the schema is missing or says `Int`.

### Phase 4 — Display, formatter, plan output

**Task 4.1: `render_duration` produces canonical output.**

- **Files:** add to `carina-core/src/value.rs` (or `formatter/format.rs` — pick the module already used by `Display for Value`).
- **Test:**
  ```rust
  #[test]
  fn render_duration_canonicalises_to_largest_clean_unit() {
      use std::time::Duration;
      assert_eq!(render_duration(Duration::from_secs(0)),    "0s");
      assert_eq!(render_duration(Duration::from_secs(30)),   "30s");
      assert_eq!(render_duration(Duration::from_secs(60)),   "1min");
      assert_eq!(render_duration(Duration::from_secs(90)),   "90s");
      assert_eq!(render_duration(Duration::from_secs(2700)), "45min");
      assert_eq!(render_duration(Duration::from_secs(3600)), "1h");
      assert_eq!(render_duration(Duration::from_secs(4500)), "75min");
      assert_eq!(render_duration(Duration::from_secs(7200)), "2h");
  }
  ```
- **Implementation:** the function in the design doc.

**Task 4.2: `Display for Value` routes Duration through `render_duration`.**

- **Files:** modify `carina-core/src/value.rs` (`Display` impl or `format_value` helper).
- **Test:**
  ```rust
  #[test]
  fn display_value_duration_renders_canonical_form() {
      let v = Value::Duration(Duration::from_secs(4500));
      assert_eq!(format!("{v}"), "75min");
  }
  ```
- **Implementation:** in the `Display` arm or `format_value` for Value, dispatch `Value::Duration(d) => render_duration(*d)`.

**Task 4.3: `carina fmt` round-trips Duration in canonical form.**

- **Files:** modify `carina-core/src/formatter/format.rs`.
- **Test:** add a multi-file fixture under `carina-core/tests/fixtures/duration/basic/`:
  - `main.crn` containing `let t = 75min`, `let u = 5m`, `let v = 2700s`.
  - `sibling.crn` referencing one of the durations (or just declaring its own).
  - Assert `carina fmt` writes back `t = 75min`, `u = 5min`, `v = 45min`.
- **Implementation:** the formatter's value-rendering path uses the same `render_duration` from Task 4.1.

**Task 4.4: Plan display renders Duration values via `render_duration`.**

- **Files:** modify `carina-core/src/plan.rs::format_effect_brief` (or the per-effect formatter that displays attribute values).
- **Test:** snapshot test under `carina-cli/tests/fixtures/plan_display/duration/`. The fixture has at least one resource whose schema declares a Duration attribute; the snapshot asserts the canonical form appears.
- **Implementation:** ensure the existing value-pretty-print path goes through Display (which now handles Duration).

### Phase 5 — LSP, semantic tokens, completion

**Task 5.1: LSP reports a type-mismatch diagnostic for non-Duration values on Duration attributes.**

- **Files:** modify `carina-lsp/src/diagnostics/mod.rs`.
- **Test:** add to the LSP diagnostics tests (multi-file fixture per CLAUDE.md):
  ```rust
  #[test]
  fn lsp_diagnoses_int_assigned_to_duration_attribute() {
      // Fixture: a let binding to a Duration attribute, value `30` (Int).
      let diags = run_diagnostics_on_dir("tests/fixtures/duration_type_mismatch");
      assert!(diags.iter().any(|d| d.message.contains("Duration") && d.message.contains("Int")));
  }
  ```
- **Implementation:** mirror the `validation/mod.rs` arms.

**Task 5.2: LSP completion suggests Duration unit snippets.**

- **Files:** modify `carina-lsp/src/completion/values.rs`.
- **Test:** add a completion-fixture test that asserts when the cursor is on the value side of a Duration attribute, the response includes `30s`, `5min`, `1h` as snippet candidates.
- **Implementation:** add a `if attr_type == AttributeType::Duration` branch returning the curated list.

**Task 5.3: Semantic tokens highlight duration literals as numeric.**

- **Files:** modify `carina-lsp/src/semantic_tokens.rs`.
- **Test:** add to the semantic-tokens test suite a check that `75min` is tokenised with the `numeric` (or analogous) class.
- **Implementation:** if the tokeniser walks the parse tree, emit `numeric` for `Rule::duration_literal`. If it scans line-by-line via regex, add a regex that captures duration literals and tags them numeric.

### Phase 6 — TextMate grammars

**Task 6.1: VS Code and TextMate bundle highlight duration literals.**

- **Files:** modify `editors/vscode/syntaxes/carina.tmLanguage.json` and `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json`.
- **Test:** the existing `tmlanguage_keyword_parity` test asserts byte-identity between the two files; the new pattern only passes if both files receive the change.
- **Implementation:** add a numeric pattern matching `\b\d+(s|sec|second|seconds|m|min|minute|minutes|h|hr|hour|hours)\b` under the existing `numeric` group. Apply identically to both files.

### Phase 7 — Integration and acceptance

**Task 7.1: Multi-file `.crn` fixture parses, validates, formats, and round-trips.**

- **Files:** `carina-core/tests/fixtures/duration/basic/{main.crn,sibling.crn}` plus an integration test in `carina-core/tests/`.
- **Test:** an integration test that:
  1. Parses the fixture directory via `parse_directory_files`.
  2. Asserts the resolved binding values include `Value::Duration(_)` instances.
  3. Runs the formatter and asserts the rewritten files are byte-equal to the canonical-form expected versions.
- **Implementation:** assemble the fixture; the test exercises the wiring landed in earlier phases.

**Task 7.2: Plan display snapshot for a Duration value.**

- **Files:** `carina-cli/tests/fixtures/plan_display/duration/{main.crn,carina.state.json,snapshot.txt}` plus a snapshot test entry.
- **Test:** the snapshot test loads the fixture and asserts the recorded plan output (after `cargo insta accept`).
- **Implementation:** add the snapshot entry under the existing `plan_snapshot` test setup. Add a `Makefile` target (per `feedback_makefile_for_fixtures.md`) so CI's `Check Plan Fixtures` step runs the new fixture.

**Task 7.3: Real-infra smoke test against `carina-rs/infra` (best-effort).**

- **Action:** if a duration-typed attribute exists today (none does without provider migration), run `aws-vault exec mizzy -- target/debug/carina validate` and `... fmt --check` against `carina-rs/infra/aws/`. If no Duration-typed attribute exists in the real infra, skip the AWS-touching part and document in the PR body that smoke testing is deferred until the first provider attribute migrates.
- **Per memory:** `feedback_no_real_infra_aws_commands.md` — the AWS-touching part is user-driven; this plan asks the user to run it, not the agent.

### Phase 8 — Verify, doctest, lint, scripts

**Task 8.1: Crate-scoped verify per `scripts/touched-crates.sh`.**

- `cargo check $(scripts/touched-crates.sh)` — sanity.
- `cargo nextest run $(scripts/touched-crates.sh)` — unit + integration tests.
- `cargo test --workspace --doc` — doctests.
- `cargo clippy --workspace --all-targets -- -D warnings` — lint.
- `bash scripts/check-*.sh` — invariants.
- `cargo build --release` — release-only sanity (Cargo.toml is touched by the new variants — possibly needed).

**Task 8.2: Open the PR (non-draft).**

- Title: `core: AttributeType::Duration + Value::Duration + <integer><unit> literal`
- Body: link `#2824`, summarise the four design decisions (AttributeType variant / Value variant / state-JSON int seconds / WIT int-val passthrough), list the canonical-form rule, list deferred items.
- Per memory: `feedback_non_draft_pr.md`, `feedback_check_ci_after_pr.md`, `feedback_no_escaped_backticks_in_pr.md` (write the body to `/tmp/pr-body.md` and pass via `--body-file`).

## Risks tracked in the plan

- **Pest grammar ambiguity with `number`.** Validate via Task 1.1's regression test that bare `30` still parses as `number`. If the alternation order interacts oddly with negative numbers (`-30` is an Int; `-30s` is invalid by design), assert both behaviours.
- **u64 overflow in unit conversion.** Task 2.1 adds an explicit overflow check.
- **`Value::Unknown` audit per `feedback_value_unknown_validation_sites.md`.** Task 2.3 names this directly. The Phase 3.2 sweep ensures every exhaustive match learns the variant.
- **Formatter rewrites of seconds → minutes.** Task 4.3's fixture uses `2700s` and asserts the rewrite to `45min`; the PR body documents the behaviour.
- **Provider repos not yet using Duration.** Out of scope; documented in Phase 7.3.
