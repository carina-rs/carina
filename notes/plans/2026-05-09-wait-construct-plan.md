# `wait` Construct: Implementation Plan

<!-- derived-from ../specs/2026-05-09-wait-construct-design.md -->

## Repository scope

This plan describes work in **`carina-rs/carina`** (the carina-core monorepo), not in `carina-provider-aws`. The plan lives here because the brainstorming originated from `aws#244` (ACM Certificate); the actual `wait` construct is a carina-core change.

`carina-provider-aws` work (`aws.acm.Certificate` implementation that uses `wait`) happens in a follow-up plan after the carina-core changes land.

## Prerequisites: separate Carina RFCs that ship first

This plan assumes the following are already in place (each is its own brainstorming + plan + PR cycle):

1. **carina#TBD-A — `depends_on` meta-arg.** A new `depends_on = [<binding>, ...]` field accepted on every `let` binding. Resolves to additional ordering edges in the planner. *Out of scope here.*
2. **carina#TBD-B — `Duration` type and literal.** New lexical token `<integer><unit>` (`75min`, `1h`, `30s`); new first-class `Duration` type wrapping `std::time::Duration`. *Out of scope here.*

If these don't exist when this plan starts, the plan stalls. They are hard prerequisites, not parallel work. Each plan is short (5-10 tasks); they can land in any order before this one starts.

## File map

### Files to create

| Path | Purpose |
|---|---|
| `carina-core/src/effect.rs` *(new variant in existing file)* | Add `Effect::Wait { ... }` variant. |
| `carina-core/src/wait/mod.rs` | New module. Houses `WaitPredicate` AST and predicate evaluator. |
| `carina-core/src/wait/predicate.rs` | `WaitPredicate` enum + `evaluate(&State::attributes) -> bool`. |
| `carina-core/src/wait/tests.rs` | Unit tests for predicate parsing and evaluation. |
| `carina-core/src/parser/expressions/wait_expr.rs` | Parser pass for `wait <target> { ... }`. |
| `carina-core/src/executor/wait.rs` | `execute_wait_effect` — read-polling loop with timeout. |
| `carina-core/tests/fixtures/wait/cert_issued/main.crn` | Fixture for parser + plan snapshot tests. |
| `carina-cli/tests/fixtures/plan_display/wait_cert/{main.crn, carina.state.json}` | Plan-display snapshot fixture. |

### Files to modify

| Path | Change |
|---|---|
| `carina-core/src/parser/carina.pest` | Add `wait_expr` rule + insert into `expression` alternatives before `resource_expr`. |
| `carina-core/src/parser/let_binding.rs` | Dispatch `Rule::wait_expr` → `parse_wait_expr` → `LetBindingKind::Wait`. |
| `carina-core/src/parser/let_binding.rs` (continued) | Recognize `wait_expr` in the `is_block_like_primary` predicate so it's accepted as an RHS. |
| `carina-core/src/keywords.rs` | Add `("wait", KeywordKind::Storage)` and `("until", KeywordKind::Other)` entries. |
| `carina-core/src/lib.rs` | `pub mod wait;`. |
| `carina-core/src/effect.rs` | `Effect::Wait` variant, `binding_name()` arm, `is_state_operation()` arm if needed. |
| `carina-core/src/plan.rs` | `format_effect_brief` arm for `Effect::Wait`: `> <binding> (until <predicate>)`. |
| `carina-core/src/differ/plan.rs` | When the IR contains a wait binding, emit `Effect::Wait` into the plan with resolved `target_id`, `until` predicate, `timeout` (from override or schema default), `interval` (from schema default), and `depends_on` (from explicit + auto-derived). |
| `carina-core/src/executor/mod.rs` | Re-export `execute_wait_effect`; declare `mod wait;`. |
| `carina-core/src/executor/parallel.rs` | Dispatch `Effect::Wait` arm in the in-flight loop (alongside `Create` / `Update` / `Delete`); wait bindings register their resolved State (= target snapshot at success) in `applied_states` so downstream resolution works. |
| `carina-core/src/resolver.rs` (or wherever binding resolution lives) | Resolve `<wait-binding>.<attr>` as passthrough of `<target>.<attr>` from the wait's captured snapshot. |
| `carina-core/src/schema/mod.rs` | Add `default_wait_timeout: Option<Duration>` and `default_wait_interval: Option<Duration>` to `ResourceSchema`. Provide carina-core fallback constants (e.g. `WAIT_DEFAULT_TIMEOUT = 5min`, `WAIT_DEFAULT_INTERVAL = 5sec`). |
| `carina-lsp/src/completion/top_level.rs` | Add `wait` snippet completion. |
| `carina-lsp/src/completion/values.rs` | Inside `wait <target> { ... }`: complete block keys (`until`, `depends_on`, `timeout`); for `until` LHS, complete `<target>.<attr>`; for RHS, complete enum values. |
| `carina-lsp/src/semantic_tokens.rs` | Highlight `wait` and `until` as keywords; duration literals as numeric. |
| `carina-lsp/src/diagnostics/mod.rs` | Diagnostics: target not found, attribute not in target schema, type mismatch in `until`, unsupported operator (anything beyond `==` in MVP), missing `until`, invalid duration. |
| `carina-core/src/formatter/mod.rs` (or wherever block formatting lives) | Format `wait` blocks consistently with existing `let foo = aws.... { ... }` blocks. |
| `editors/vscode/syntaxes/carina.tmLanguage.json` | Add `wait` and `until` to the keyword pattern; add a duration-literal pattern. |
| `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` | Same change, byte-identical. |

### Dependencies between files

```
keywords.rs ──┬── parser/carina.pest ── parser/let_binding.rs ── parser/expressions/wait_expr.rs
              │
              └── editors/*/carina.tmLanguage.json (parity test enforces both)

wait/predicate.rs ── wait/mod.rs ── effect.rs ── differ/plan.rs ── executor/wait.rs ── executor/parallel.rs
                                       │
                                       └── plan.rs (display)

schema/mod.rs (default_wait_timeout, default_wait_interval) ── differ/plan.rs (resolves defaults)

carina-lsp/src/* depends on parser + schema being in place.
```

## Tasks

Each task is one TDD cycle. Goal: write failing test → run → see fail → minimal impl → run → see pass.

### Phase 1 — Wait predicate AST and evaluator (no parser yet)

**Task 1.1: `WaitPredicate::Equals` evaluator returns true when target attribute equals value.**

- **Files:** create `carina-core/src/wait/mod.rs`, `carina-core/src/wait/predicate.rs`, `carina-core/src/wait/tests.rs`; modify `carina-core/src/lib.rs`.
- **Test:** in `carina-core/src/wait/tests.rs`:
  ```rust
  use crate::resource::Value;
  use crate::wait::predicate::{AttrPath, WaitPredicate};
  use std::collections::HashMap;

  #[test]
  fn equals_returns_true_when_attribute_matches() {
      let pred = WaitPredicate::Equals {
          attr: AttrPath::single("status"),
          value: Value::String("ISSUED".to_string()),
      };
      let mut attrs = HashMap::new();
      attrs.insert("status".to_string(), Value::String("ISSUED".to_string()));
      assert!(pred.evaluate(&attrs));
  }
  ```
- **Implementation:** `predicate.rs`:
  ```rust
  use crate::resource::Value;
  use std::collections::HashMap;

  #[derive(Debug, Clone, PartialEq)]
  pub struct AttrPath {
      pub segments: Vec<String>,
  }

  impl AttrPath {
      pub fn single(name: &str) -> Self {
          Self { segments: vec![name.to_string()] }
      }
  }

  #[derive(Debug, Clone, PartialEq)]
  pub enum WaitPredicate {
      Equals { attr: AttrPath, value: Value },
  }

  impl WaitPredicate {
      pub fn evaluate(&self, attrs: &HashMap<String, Value>) -> bool {
          match self {
              WaitPredicate::Equals { attr, value } => {
                  resolve(attrs, &attr.segments).is_some_and(|v| v == value)
              }
          }
      }
  }

  fn resolve<'a>(attrs: &'a HashMap<String, Value>, path: &[String]) -> Option<&'a Value> {
      let first = attrs.get(path.first()?)?;
      if path.len() == 1 { return Some(first); }
      // future: descend into nested Value::Map for path[1..]; out of MVP scope
      None
  }
  ```
  And in `wait/mod.rs`:
  ```rust
  pub mod predicate;
  #[cfg(test)] mod tests;
  ```
  And in `lib.rs`: `pub mod wait;`.
- **Verification:** `cargo nextest run -p carina-core wait::tests::equals_returns_true_when_attribute_matches`.

**Task 1.2: `WaitPredicate::Equals` returns false when attribute differs.**

- **Files:** modify `carina-core/src/wait/tests.rs`.
- **Test:**
  ```rust
  #[test]
  fn equals_returns_false_when_attribute_differs() {
      let pred = WaitPredicate::Equals {
          attr: AttrPath::single("status"),
          value: Value::String("ISSUED".to_string()),
      };
      let mut attrs = HashMap::new();
      attrs.insert("status".to_string(), Value::String("PENDING_VALIDATION".to_string()));
      assert!(!pred.evaluate(&attrs));
  }
  ```
- **Implementation:** None — Task 1.1's implementation already handles this; this test pins the negative case to lock the contract.
- **Verification:** `cargo nextest run -p carina-core wait::tests::equals_returns_false_when_attribute_differs`.

**Task 1.3: `WaitPredicate::Equals` returns false when attribute is absent.**

- **Files:** modify `carina-core/src/wait/tests.rs`.
- **Test:**
  ```rust
  #[test]
  fn equals_returns_false_when_attribute_absent() {
      let pred = WaitPredicate::Equals {
          attr: AttrPath::single("status"),
          value: Value::String("ISSUED".to_string()),
      };
      let attrs: HashMap<String, Value> = HashMap::new();
      assert!(!pred.evaluate(&attrs));
  }
  ```
- **Implementation:** None — `resolve` returns `None`, so `is_some_and` returns false.
- **Verification:** `cargo nextest run -p carina-core wait::tests::equals_returns_false_when_attribute_absent`.

### Phase 2 — Effect::Wait

**Task 2.1: Add `Effect::Wait` variant.**

- **Files:** modify `carina-core/src/effect.rs`.
- **Test:** add to `carina-core/src/effect.rs` `mod tests` (or its tests file):
  ```rust
  #[test]
  fn wait_variant_constructs() {
      use crate::resource::ResourceId;
      use crate::wait::predicate::{AttrPath, WaitPredicate};
      use crate::resource::Value;
      use std::time::Duration;

      let _ = Effect::Wait {
          binding: "cert_issued".to_string(),
          target_id: ResourceId::new("acm.Certificate", "cert"),
          target_identifier: None,
          until: WaitPredicate::Equals {
              attr: AttrPath::single("status"),
              value: Value::String("ISSUED".to_string()),
          },
          until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
          timeout: Duration::from_secs(75 * 60),
          interval: Duration::from_secs(5),
      };
  }
  ```
- **Implementation:** add to the `Effect` enum:
  ```rust
  Wait {
      binding: String,
      target_id: ResourceId,
      target_identifier: Option<String>,
      until: crate::wait::predicate::WaitPredicate,
      /// Surface form of the `until` expression as the user wrote it
      /// (e.g., `"cert.status == aws.acm.Certificate.Status.Issued"`).
      /// Used by `format_effect_brief` so display never has to invert
      /// the parsed AST. Same pattern as `legacy replacement cascade ref hints`.
      until_surface: String,
      #[serde(with = "crate::utils::serde_duration")]  // or impl serde manually if helper missing
      timeout: std::time::Duration,
      #[serde(with = "crate::utils::serde_duration")]
      interval: std::time::Duration,
  },
  ```
  If `serde_duration` helper does not exist, define one in `carina-core/src/utils.rs` that round-trips Duration as `{ "secs": N, "nanos": N }`. Add a `#[derive(Serialize, Deserialize)]` on `WaitPredicate` and `AttrPath` (use `#[serde(tag = "kind")]` on the enum for forward-compat).
- **Verification:** `cargo nextest run -p carina-core effect::tests::wait_variant_constructs`. Nextest already invokes `rustc`, so non-exhaustive-match errors in downstream callers surface here without a redundant `cargo build` step (per CLAUDE.md "Verify Protocol — Do Not Run Redundant Builds").

**Task 2.2: `Effect::binding_name()` returns the wait binding for `Effect::Wait`.**

- **Files:** modify `carina-core/src/effect.rs`.
- **Test:**
  ```rust
  #[test]
  fn wait_binding_name_returns_wait_binding() {
      let e = Effect::Wait {
          binding: "cert_issued".to_string(),
          target_id: crate::resource::ResourceId::new("acm.Certificate", "cert"),
          target_identifier: None,
          until: crate::wait::predicate::WaitPredicate::Equals {
              attr: crate::wait::predicate::AttrPath::single("status"),
              value: crate::resource::Value::String("ISSUED".to_string()),
          },
          until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
          timeout: std::time::Duration::from_secs(60),
          interval: std::time::Duration::from_secs(5),
      };
      assert_eq!(e.binding_name(), Some("cert_issued"));
  }
  ```
- **Implementation:** add to `Effect::binding_name`:
  ```rust
  Effect::Wait { binding, .. } => Some(binding),
  ```
- **Verification:** `cargo nextest run -p carina-core effect::tests::wait_binding_name_returns_wait_binding`.

**Task 2.3: All other exhaustive matches on `Effect` compile after adding `Wait`.**

- **Files:** modify every `match effect` site flagged by the compiler. Likely candidates from the survey:
  - `carina-core/src/plan.rs::format_effect_brief` (covered in Task 5.1).
  - `carina-core/src/executor/parallel.rs` (covered in Task 4.1).
  - `carina-core/src/differ/plan.rs` summary calculations.
  - `carina-core/src/detail_rows.rs`.
- **Test:** `cargo nextest run -p carina-core` (which compiles every match site as it runs the existing test suite) reports no `non-exhaustive patterns` errors. The existing tests act as the exhaustiveness gate; no new test is needed for this task.
- **Implementation:** for each non-target site, add `Effect::Wait { .. } => ...` arms. Use the most conservative behaviour (typically: ignore for summaries, count as a no-op for things like cascade detection). Each addition is one line.
- **Verification:** `cargo nextest run -p carina-core 2>&1 | grep -E "error\[E0004\]|non-exhaustive"` returns empty (per CLAUDE.md "Verify Protocol" — nextest's compile step is the same artifact a `cargo build` would produce).

### Phase 3 — Parser: `wait <target> { ... }`

**Task 3.1: `wait` is recognised as a reserved keyword.**

- **Files:** modify `carina-core/src/keywords.rs`.
- **Test:**
  ```rust
  #[test]
  fn wait_is_a_storage_keyword() {
      assert!(is_keyword("wait"));
      let storage: Vec<&str> = by_kind(KeywordKind::Storage).collect();
      assert!(storage.contains(&"wait"));
  }
  ```
- **Implementation:** insert `("wait", KeywordKind::Storage),` into `KEYWORDS`. The existing `pest_grammar_contains_every_keyword` test will then fail until Task 3.2 adds the literal to the grammar — that's expected; the two tasks are paired.
- **Verification:** `cargo nextest run -p carina-core keywords::tests::wait_is_a_storage_keyword`.

**Task 3.2: `until` is recognised as a reserved keyword.**

- **Files:** modify `carina-core/src/keywords.rs`.
- **Test:**
  ```rust
  #[test]
  fn until_is_an_other_keyword() {
      assert!(is_keyword("until"));
  }
  ```
- **Implementation:** insert `("until", KeywordKind::Other),` into `KEYWORDS`.
- **Verification:** `cargo nextest run -p carina-core keywords::tests::until_is_an_other_keyword`.

**Task 3.3: pest grammar accepts `let foo = wait bar { until = bar.x == something }`.**

- **Files:** modify `carina-core/src/parser/carina.pest`.
- **Test:** create `carina-core/src/parser/expressions/wait_expr_tests.rs` (or add to existing parser test module):
  ```rust
  #[test]
  fn pest_parses_wait_expr() {
      use crate::parser::carina::CarinaParser;
      use pest::Parser;
      let src = r#"let cert_issued = wait cert {
          until = cert.status == aws.acm.Certificate.Status.Issued
      }"#;
      let result = CarinaParser::parse(crate::parser::carina::Rule::file, src);
      assert!(result.is_ok(), "expected parse success, got {:?}", result);
  }
  ```
- **Implementation:** add to `carina.pest`:
  ```pest
  // Wait expression: `wait <target> { until = ..., depends_on = [...], timeout = ... }`
  // Synchronisation construct: blocks downstream resources until target reaches the
  // condition declared by `until`. See docs/specs/2026-05-09-wait-construct-design.md.
  wait_expr = { "wait" ~ identifier ~ "{" ~ wait_attr* ~ "}" }
  wait_attr = { wait_until_attr | wait_timeout_attr | wait_depends_on_attr }
  wait_until_attr = { "until" ~ "=" ~ validate_expr }
  wait_timeout_attr = { "timeout" ~ "=" ~ duration_literal }
  wait_depends_on_attr = { "depends_on" ~ "=" ~ "[" ~ (identifier ~ ("," ~ identifier)*)? ~ "]" }
  ```
  Then in the `expression` alternatives (around line where `upstream_state_expr` is listed), insert `wait_expr` *before* `module_call` and `resource_expr`:
  ```pest
  | wait_expr           // Must come before module_call / resource_expr
  | upstream_state_expr
  | resource_expr
  ```
  `validate_expr` is reused as the predicate grammar (Task 6.x narrows it to `==` semantically). `duration_literal` is provided by carina#TBD-B.
- **Verification:** `cargo nextest run -p carina-core parser::expressions::wait_expr_tests::pest_parses_wait_expr`.

**Task 3.4: `let_binding` dispatches `Rule::wait_expr` to a `WaitExpr` AST node.**

- **Files:** create `carina-core/src/parser/expressions/wait_expr.rs`; modify `carina-core/src/parser/let_binding.rs`, `carina-core/src/parser/expressions/mod.rs`.
- **Test:** in `carina-core/src/parser/expressions/wait_expr.rs`:
  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::parser::carina::{CarinaParser, Rule};
      use pest::Parser;

      #[test]
      fn parse_wait_expr_extracts_target_and_until() {
          let src = r#"wait cert {
              until = cert.status == aws.acm.Certificate.Status.Issued
          }"#;
          let pair = CarinaParser::parse(Rule::wait_expr, src).unwrap().next().unwrap();
          let we = parse_wait_expr(pair).unwrap();
          assert_eq!(we.target, "cert");
          assert!(we.until.is_some());
          assert!(we.timeout.is_none());
          assert!(we.depends_on.is_empty());
      }
  }
  ```
- **Implementation:** in `wait_expr.rs`:
  ```rust
  use crate::parser::carina::Rule;
  use crate::parser::error::ParseError;
  use crate::parser::expressions::validate_expr::ValidateExprAst;
  use pest::iterators::Pair;

  #[derive(Debug, Clone)]
  pub struct WaitExpr {
      pub target: String,
      pub until: Option<UntilSurface>,        // not yet typed; surface AST
      pub timeout: Option<DurationLiteralAst>, // from carina#TBD-B
      pub depends_on: Vec<String>,
  }

  #[derive(Debug, Clone)]
  pub struct UntilSurface {
      pub raw: String,           // surface form for error messages
      pub ast: validate_expr::ValidateExprAst, // reuse existing validate_expr AST
  }

  pub fn parse_wait_expr(pair: Pair<'_, Rule>) -> Result<WaitExpr, crate::parser::error::ParseError> {
      assert!(matches!(pair.as_rule(), Rule::wait_expr));
      let mut inner = pair.into_inner();
      let target_pair = inner.next().expect("wait_expr: missing target identifier");
      let target = target_pair.as_str().to_string();

      let mut we = WaitExpr {
          target,
          until: None,
          timeout: None,
          depends_on: Vec::new(),
      };
      for attr_pair in inner {
          // attr_pair is a Rule::wait_attr containing one of the *_attr children
          let child = attr_pair.into_inner().next().unwrap();
          match child.as_rule() {
              Rule::wait_until_attr => {
                  let raw = child.as_str().to_string();
                  let expr_pair = child.into_inner().next().unwrap();
                  let ast = crate::parser::expressions::validate_expr::parse(expr_pair)?;
                  we.until = Some(UntilSurface { raw, ast });
              }
              Rule::wait_timeout_attr => {
                  let dur_pair = child.into_inner().next().unwrap();
                  we.timeout = Some(crate::parser::duration::parse(dur_pair)?);
              }
              Rule::wait_depends_on_attr => {
                  for ident in child.into_inner() {
                      we.depends_on.push(ident.as_str().to_string());
                  }
              }
              other => unreachable!("unexpected wait_attr child: {:?}", other),
          }
      }
      Ok(we)
  }
  ```
  In `let_binding.rs`, add `Rule::wait_expr` alongside `Rule::upstream_state_expr` in:
  - `is_block_like_primary` (line ~104, 114): adds the rule to the recognised set.
  - The dispatch around line ~310: handle `Rule::wait_expr` by calling `parse_wait_expr` and stashing the result as a `LetBindingKind::Wait(WaitExpr)` (introduce that variant in the same task).
- **Verification:** `cargo nextest run -p carina-core parser::expressions::wait_expr::tests::parse_wait_expr_extracts_target_and_until`.

**Task 3.5: Parser rejects `wait` without a target.**

- **Files:** add to `wait_expr.rs` tests.
- **Test:**
  ```rust
  #[test]
  fn pest_rejects_wait_without_target() {
      let src = r#"let foo = wait { until = x.y == z }"#;
      let result = CarinaParser::parse(Rule::file, src);
      assert!(result.is_err(), "expected parse failure for `wait` with no target");
  }
  ```
- **Implementation:** none — pest grammar from Task 3.3 already requires `identifier` after `wait`.
- **Verification:** `cargo nextest run -p carina-core parser::expressions::wait_expr::tests::pest_rejects_wait_without_target`.

**Task 3.6: Parser rejects `wait foo {}` without `until`.**

- **Files:** add to `wait_expr.rs` tests; modify the wait-block validation step in `let_binding.rs` (or a new `validate_wait_expr` helper).
- **Test:**
  ```rust
  #[test]
  fn parse_rejects_wait_without_until() {
      let src = r#"let foo = wait cert { timeout = 30s }"#;
      // Parsing the file may succeed at the pest level (wait_attr is optional);
      // semantic validation should reject it.
      let parsed = CarinaParser::parse(Rule::file, src).unwrap();
      let result = crate::parser::parse_file(parsed);
      assert!(result.is_err(), "expected semantic failure: missing `until`");
      let err = result.unwrap_err().to_string();
      assert!(err.contains("until"), "error should mention `until`, got: {}", err);
  }
  ```
- **Implementation:** in `parse_wait_expr` (or its caller in `let_binding.rs`), after populating `WaitExpr`, check `we.until.is_some()`; if not, return `Err(ParseError::InvalidExpression { ... })` carrying a span on the `wait` keyword and the message `"`wait` block requires `until`"`. `ParseError` is the parser's existing error type (defined in `carina-core/src/parser/error.rs`, used throughout `let_binding.rs` and `blocks/backend.rs::parse_upstream_state_expr`). Update `parse_wait_expr`'s return type to `Result<WaitExpr, ParseError>` (replacing the `anyhow::Result` placeholder used in earlier task drafts) so it composes with `let_binding.rs`'s existing dispatch.
- **Verification:** `cargo nextest run -p carina-core parser::expressions::wait_expr::tests::parse_rejects_wait_without_until`.

**Task 3.7: TextMate grammar parity — add `wait`, `until`, and duration literal patterns.**

- **Files:** modify both `editors/vscode/syntaxes/carina.tmLanguage.json` and `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` (must stay byte-identical per `carina-core/tests/tmlanguage_keyword_parity.rs`).
- **Test:** the existing parity test + `keywords_have_tmlanguage_match` (or equivalent — confirm existing test name from `carina-core/tests/tmlanguage_keyword_parity.rs`):
  ```bash
  cargo nextest run -p carina-core tmlanguage_keyword_parity
  ```
  This test will fail when Task 3.1 added `wait` to KEYWORDS but the grammar wasn't updated.
- **Implementation:** locate the existing keyword pattern (likely a `match` regex listing `let|fn|provider|...`) and add `wait` and `until`. Make the same edit to both JSON files.
- **Verification:** `cargo nextest run -p carina-core tmlanguage_keyword_parity`.

### Phase 4 — Differ + executor

**Task 4.1: Differ emits `Effect::Wait` for a wait binding in the IR.**

- **Files:** modify `carina-core/src/differ/plan.rs` (or wherever resource → effect lowering happens).
- **Test:** in `carina-core/src/differ/plan_tests.rs`:
  ```rust
  #[test]
  fn wait_binding_lowers_to_wait_effect() {
      use crate::wait::predicate::{AttrPath, WaitPredicate};
      use crate::resource::Value;
      use std::time::Duration;

      // The existing tests in plan_tests.rs build their inputs inline (no shared
      // builders). Follow that pattern: construct resources + schemas + state +
      // wait-binding by hand. The wait binding lives in a new collection
      // alongside resources — see Task 4.1's planner change for the exact
      // wiring.
      let cert = Resource::new("acm.Certificate", "cert")
          .with_attribute("domain_name", Value::String("registry.example.com".to_string()));
      let resources = vec![cert.clone()];

      let wait_binding = WaitBinding {
          name: "cert_issued".to_string(),
          target: "cert".to_string(),
          until: UntilSurface {
              raw: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
              ast: parse_validate_expr("cert.status == aws.acm.Certificate.Status.Issued").unwrap(),
          },
          timeout: None,
          depends_on: vec![],
      };
      let waits = vec![wait_binding];

      let mut schemas = SchemaRegistry::new();
      schemas.insert("", ResourceSchema::new("acm.Certificate")
          .attribute(AttributeSchema::new("domain_name", AttributeType::String).create_only())
          .attribute(AttributeSchema::new("status", AttributeType::String).read_only())
          .with_default_wait_timeout(Duration::from_secs(75 * 60))
          .with_default_wait_interval(Duration::from_secs(5)));

      let plan = create_plan_with_waits(
          &resources,
          &waits,
          &HashMap::new(),    // current_states (empty = first apply)
          &HashMap::new(),
          &schemas,
          &HashMap::new(),
          &HashMap::new(),
          &HashMap::new(),
      );
      let wait_effect = plan.effects().iter().find(|e| e.is_wait()).expect("expected Wait effect");
      let Effect::Wait { binding, target_id, until, .. } = wait_effect else { unreachable!() };
      assert_eq!(binding, "cert_issued");
      assert_eq!(target_id.name, "cert");
      assert_eq!(until, &WaitPredicate::Equals {
          attr: AttrPath::single("status"),
          value: Value::String("ISSUED".to_string()),
      });
  }
  ```
- **Implementation:** in the differ's binding-iteration loop, recognise `LetBindingKind::Wait(WaitExpr)` and emit:
  ```rust
  Effect::Wait {
      binding: binding_name.to_string(),
      target_id: resolved_target_id.clone(),
      target_identifier: target_state.identifier.clone(),
      until: lower_until_to_predicate(&we.until.ast, &target_schema)?,
      until_surface: we.until.raw.clone(),
      timeout: we.timeout.map(|d| d.to_duration()).or(target_schema.default_wait_timeout).unwrap_or(WAIT_DEFAULT_TIMEOUT),
      interval: target_schema.default_wait_interval.unwrap_or(WAIT_DEFAULT_INTERVAL),
  }
  ```
  `lower_until_to_predicate` walks the `validate_expr` AST and produces a `WaitPredicate` for the supported shape (`<target>.<attr> == <enum_or_literal>`); anything else returns an error tied to the `until` source span.

  This task extends `create_plan` (the existing entry point used by `plan_tests.rs`) with an additional `&[WaitBinding]` parameter (or introduces a `create_plan_with_waits` sibling — choose the path that minimises churn for existing callers; project memory says backward-compat is not required, so adding a parameter to `create_plan` itself is acceptable). `WaitBinding` is the AST representation of a wait `let_binding` produced by Phase 3's parser. The differ iterates `&[WaitBinding]` after the resource-iteration loop, resolves each `target` name to a `ResourceId`, and emits an `Effect::Wait` per the snippet above. `WAIT_DEFAULT_TIMEOUT` (5min) and `WAIT_DEFAULT_INTERVAL` (5sec) live alongside `ResourceSchema` in `schema/mod.rs` per the design's "Schema additions" section.
- **Verification:** `cargo nextest run -p carina-core differ::plan_tests::wait_binding_lowers_to_wait_effect`.

**Task 4.2: Differ rejects `until` predicate beyond MVP `==`.**

- **Files:** modify `lower_until_to_predicate`.
- **Test:**
  ```rust
  #[test]
  fn lower_rejects_unsupported_until_operator() {
      let we = make_wait_expr_with_until("cert.status != ISSUED");
      let result = lower_until_to_predicate(&we.until, &dummy_schema());
      assert!(matches!(result, Err(e) if e.to_string().contains("only `==` is supported")));
  }
  ```
- **Implementation:** in `lower_until_to_predicate`, match the validate_expr AST: if the comparison operator is anything other than `==`, return `Err(anyhow!("`until`: only `==` is supported in this version (got `{op}`)"))`.
- **Verification:** `cargo nextest run -p carina-core differ::plan_tests::lower_rejects_unsupported_until_operator`.

**Task 4.3: Differ rejects `until` LHS that isn't `<target>.<attr>`.**

- **Files:** modify `lower_until_to_predicate`.
- **Test:**
  ```rust
  #[test]
  fn lower_rejects_until_lhs_unrelated_to_target() {
      let we = make_wait_expr("cert", "other_resource.status == ISSUED");
      let result = lower_until_to_predicate(&we.until, &dummy_schema());
      assert!(matches!(result, Err(e) if e.to_string().contains("must reference target")));
  }
  ```
- **Implementation:** require the LHS variable_ref's first segment == the wait's target binding name; otherwise error.
- **Verification:** `cargo nextest run -p carina-core differ::plan_tests::lower_rejects_until_lhs_unrelated_to_target`.

**Task 4.4: Executor's `execute_wait_effect` returns Ok when target already satisfies `until` on first read.**

- **Files:** create `carina-core/src/executor/wait.rs`; modify `carina-core/src/executor/mod.rs` (`mod wait; pub use wait::*;`).
- **Test:** in `carina-core/src/executor/wait.rs` `mod tests`:
  ```rust
  #[tokio::test]
  async fn wait_returns_immediately_when_until_already_true() {
      use crate::wait::predicate::{AttrPath, WaitPredicate};
      use crate::resource::{Value, ResourceId, State};
      use std::collections::HashMap;
      use std::time::Duration;

      let provider = MockProvider::new()
          .with_read_response(ResourceId::new("acm.Certificate", "cert"), {
              let mut attrs = HashMap::new();
              attrs.insert("status".to_string(), Value::String("ISSUED".to_string()));
              State::existing(ResourceId::new("acm.Certificate", "cert"), attrs)
          });
      let pred = WaitPredicate::Equals {
          attr: AttrPath::single("status"),
          value: Value::String("ISSUED".to_string()),
      };
      let result = execute_wait_effect(
          &provider,
          &ResourceId::new("acm.Certificate", "cert"),
          None,
          &pred,
          Duration::from_secs(60),
          Duration::from_millis(10),
      ).await;
      assert!(result.is_ok());
      let state = result.unwrap();
      assert_eq!(state.attributes.get("status"), Some(&Value::String("ISSUED".to_string())));
      assert_eq!(provider.read_call_count(), 1);
  }
  ```
- **Implementation:**
  ```rust
  use crate::provider::{Provider, ProviderError, ProviderResult};
  use crate::resource::{ResourceId, State};
  use crate::wait::predicate::WaitPredicate;
  use std::time::{Duration, Instant};

  pub async fn execute_wait_effect(
      provider: &dyn Provider,
      target_id: &ResourceId,
      target_identifier: Option<&str>,
      until: &WaitPredicate,
      timeout: Duration,
      interval: Duration,
  ) -> ProviderResult<State> {
      let start = Instant::now();
      loop {
          let state = provider.read(target_id, target_identifier).await?;
          if !state.exists {
              return Err(ProviderError::not_found(format!(
                  "wait target {} not found", target_id
              )).for_resource(target_id.clone()));
          }
          if until.evaluate(&state.attributes) {
              return Ok(state);
          }
          if start.elapsed() >= timeout {
              return Err(ProviderError::timeout(format!(
                  "wait timed out after {:?} on {}: predicate not satisfied (last observed: {:?})",
                  timeout, target_id, state.attributes
              )).for_resource(target_id.clone()));
          }
          tokio::time::sleep(interval).await;
      }
  }
  ```
  `MockProvider` is a test helper — likely already exists in the executor's test infrastructure; if not, create a small one in the same test module.
- **Verification:** `cargo nextest run -p carina-core executor::wait::tests::wait_returns_immediately_when_until_already_true`.

**Task 4.5: `execute_wait_effect` polls until predicate becomes true.**

- **Files:** modify `carina-core/src/executor/wait.rs` tests.
- **Test:**
  ```rust
  #[tokio::test]
  async fn wait_polls_until_predicate_becomes_true() {
      // MockProvider returns PENDING_VALIDATION twice, then ISSUED.
      let provider = MockProvider::new().with_read_sequence(
          ResourceId::new("acm.Certificate", "cert"),
          vec![
              state_with_status("PENDING_VALIDATION"),
              state_with_status("PENDING_VALIDATION"),
              state_with_status("ISSUED"),
          ],
      );
      let pred = WaitPredicate::Equals { attr: AttrPath::single("status"), value: Value::String("ISSUED".into()) };
      let result = execute_wait_effect(
          &provider,
          &ResourceId::new("acm.Certificate", "cert"),
          None,
          &pred,
          Duration::from_secs(60),
          Duration::from_millis(1),  // tight interval for fast test
      ).await;
      assert!(result.is_ok());
      assert_eq!(provider.read_call_count(), 3);
  }
  ```
- **Implementation:** none — Task 4.4's loop already handles this. `MockProvider::with_read_sequence` is a new test helper (return responses in order).
- **Verification:** `cargo nextest run -p carina-core executor::wait::tests::wait_polls_until_predicate_becomes_true`.

**Task 4.6: `execute_wait_effect` returns Timeout when predicate stays false past timeout.**

- **Files:** modify `carina-core/src/executor/wait.rs` tests.
- **Test:**
  ```rust
  #[tokio::test]
  async fn wait_returns_timeout_when_predicate_stays_false() {
      let provider = MockProvider::new().with_read_response(
          ResourceId::new("acm.Certificate", "cert"),
          state_with_status("PENDING_VALIDATION"),
      );
      let pred = WaitPredicate::Equals { attr: AttrPath::single("status"), value: Value::String("ISSUED".into()) };
      let result = execute_wait_effect(
          &provider,
          &ResourceId::new("acm.Certificate", "cert"),
          None,
          &pred,
          Duration::from_millis(10),     // short timeout
          Duration::from_millis(2),       // short interval — still fits multiple reads
      ).await;
      assert!(matches!(result, Err(ProviderError::Timeout(_))));
      let err_msg = result.unwrap_err().to_string();
      assert!(err_msg.contains("PENDING_VALIDATION"), "error should include last observed value, got: {}", err_msg);
  }
  ```
- **Implementation:** none — Task 4.4's loop handles this; the test pins the error message contract.
- **Verification:** `cargo nextest run -p carina-core executor::wait::tests::wait_returns_timeout_when_predicate_stays_false`.

**Task 4.7: `execute_wait_effect` returns NotFound when target disappears mid-poll.**

- **Files:** modify `carina-core/src/executor/wait.rs` tests.
- **Test:**
  ```rust
  #[tokio::test]
  async fn wait_returns_not_found_when_target_disappears() {
      let provider = MockProvider::new().with_read_sequence(
          ResourceId::new("acm.Certificate", "cert"),
          vec![
              state_with_status("PENDING_VALIDATION"),
              State::not_found(ResourceId::new("acm.Certificate", "cert")),
          ],
      );
      let pred = WaitPredicate::Equals { attr: AttrPath::single("status"), value: Value::String("ISSUED".into()) };
      let result = execute_wait_effect(
          &provider,
          &ResourceId::new("acm.Certificate", "cert"),
          None,
          &pred,
          Duration::from_secs(60),
          Duration::from_millis(1),
      ).await;
      assert!(matches!(result, Err(ProviderError::NotFound(_))));
  }
  ```
- **Implementation:** none — Task 4.4 already returns `NotFound` when `!state.exists`.
- **Verification:** `cargo nextest run -p carina-core executor::wait::tests::wait_returns_not_found_when_target_disappears`.

**Task 4.8: Parallel executor dispatches `Effect::Wait` and registers the captured State for downstream resolution.**

- **Files:** modify `carina-core/src/executor/parallel.rs`.
- **Test:** add to `carina-core/src/executor/tests.rs`:
  ```rust
  #[tokio::test]
  async fn wait_effect_executes_and_unblocks_downstream() {
      // Plan: Create cert (returns PENDING) → Wait cert_issued (sees ISSUED on second read)
      //       → Create downstream that references cert_issued.arn
      // Verify: downstream's create receives the resolved cert_issued.arn after the wait completes.
      let plan = build_three_effect_plan_with_wait();
      let provider = MockProvider::new()
          .with_create_response("cert", state_with_status_and_arn("PENDING_VALIDATION", "arn:acm:cert"))
          .with_read_sequence("cert", vec![
              state_with_status_and_arn("PENDING_VALIDATION", "arn:acm:cert"),
              state_with_status_and_arn("ISSUED", "arn:acm:cert"),
          ])
          .with_create_response("dist", State::default());
      let observer = NoopObserver;
      // Construct ExecutionInput by struct literal — same shape used throughout
      // executor/tests.rs (e.g. line 236, 272). Fields per executor/mod.rs:
      let input = crate::executor::ExecutionInput {
          plan: &plan,
          unresolved_resources: &HashMap::new(),
          bindings: ResolvedBindings::default(),
          current_states: HashMap::new(),
      };
      let result = execute_plan(&provider, input, &observer).await;
      assert!(result.success_count >= 3);
      let dist_create = provider.create_call("dist").unwrap();
      let arn = dist_create.attributes.get("acm_certificate_arn").unwrap();
      assert_eq!(arn, &Value::String("arn:acm:cert".to_string()));
  }
  ```
- **Implementation:** in `execute_effects_sequential`'s in-flight match (around line ~271):
  ```rust
  Effect::Wait { binding, target_id, target_identifier, until, timeout, interval } => {
      let state = crate::executor::wait::execute_wait_effect(
          provider, target_id, target_identifier.as_deref(), until, *timeout, *interval,
      ).await?;
      // Register the captured State under the wait binding's *synthetic* ResourceId
      // so resolved bindings can deref `<binding>.<attr>` → state.attributes[attr].
      let synthetic_id = ResourceId::new("__wait", binding);
      applied_states.insert(synthetic_id.clone(), state);
      Ok(SingleEffectResult::WaitDone(binding.clone()))
  }
  ```
  Plus update the `dispatched`/`completed_indices` bookkeeping to recognise `Effect::Wait` (currently `Read` and state-ops are special-cased; add `Wait` to the actionable set, not the no-op set). Update `idx_to_binding` extraction to include wait bindings (already covered if `Effect::binding_name` returns the wait binding from Task 2.2).
- **Verification:** `cargo nextest run -p carina-core executor::tests::wait_effect_executes_and_unblocks_downstream`.

**Task 4.9: Binding resolver returns target's attribute when resolving `<wait-binding>.<attr>`.**

- **Files:** modify `carina-core/src/resolver.rs` (or wherever `ResolvedBindings` lives).
- **Test:** in resolver tests:
  ```rust
  #[test]
  fn wait_binding_resolves_to_target_state_snapshot() {
      let mut resolved = ResolvedBindings::new();
      let mut attrs = HashMap::new();
      attrs.insert("arn".to_string(), Value::String("arn:aws:acm:...".to_string()));
      attrs.insert("status".to_string(), Value::String("ISSUED".to_string()));
      resolved.insert_wait("cert_issued", attrs);
      let arn = resolved.resolve_attr("cert_issued", "arn").unwrap();
      assert_eq!(arn, &Value::String("arn:aws:acm:...".to_string()));
  }
  ```
- **Implementation:** `ResolvedBindings` is defined in `carina-core/src/binding_index.rs:330`. Add an `insert_wait(&mut self, binding: &str, attrs: HashMap<String, Value>)` method that stores the snapshot in a new internal `wait_bindings: HashMap<String, HashMap<String, Value>>` field (separate from the existing resource-binding storage). Modify `resolve_attr` (or the equivalent lookup method on `ResolvedBindings`) to check `wait_bindings` after the resource-binding lookup misses. The two storages stay separate so a wait binding cannot shadow a same-named resource binding (parser-level uniqueness check still required, but separation gives an additional safety net).
- **Verification:** `cargo nextest run -p carina-core resolver::tests::wait_binding_resolves_to_target_state_snapshot`.

### Phase 5 — Plan display + state file

**Task 5.1: `format_effect_brief` renders `Effect::Wait` as `> <binding> (until <surface>)`.**

- **Files:** modify `carina-core/src/plan.rs`.
- **Test:** in the existing `plan::tests` mod:
  ```rust
  #[test]
  fn format_effect_brief_renders_wait() {
      let e = Effect::Wait {
          binding: "cert_issued".to_string(),
          target_id: ResourceId::new("acm.Certificate", "cert"),
          target_identifier: None,
          until: WaitPredicate::Equals {
              attr: AttrPath::single("status"),
              value: Value::String("ISSUED".to_string()),
          },
          until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
          timeout: Duration::from_secs(75 * 60),
          interval: Duration::from_secs(5),
      };
      let s = format_effect_brief(&e);
      // Per design spec §Plan display: predicate is rendered using its surface
      // form, namespaced enum without surrounding quotes. The differ stores
      // the original surface form alongside the parsed predicate so display
      // can echo it verbatim — the in-memory `Value::String("ISSUED")` is
      // never re-stringified ad-hoc here.
      assert_eq!(s, "> cert_issued (until cert.status == aws.acm.Certificate.Status.Issued)");
  }
  ```
- **Implementation:** add to `format_effect_brief`:
  ```rust
  Effect::Wait { binding, until_surface, .. } => {
      format!("> {} (until {})", binding, until_surface)
  }
  ```
  This requires `Effect::Wait` to carry the original surface form of `until`. Update Task 2.1's `Effect::Wait` shape: add `until_surface: String` (rendered exactly as the user wrote it, e.g. `"cert.status == aws.acm.Certificate.Status.Issued"`). The differ in Task 4.1 populates it from the source span captured during parsing (`UntilSurface::raw` already exists from Task 3.4). Carrying the surface form means display never has to invert the parsed AST — a pattern Carina already uses for cascade replacement hints (`cascade_ref_hints` in the legacy replace effect, see `effect.rs`).
- **Verification:** `cargo nextest run -p carina-core plan::tests::format_effect_brief_renders_wait`.

**Task 5.2: Wait effects do not appear in `carina.state.json` after apply.**

- **Files:** modify `carina-core/src/executor/parallel.rs` (state-update path).
- **Test:** in `carina-core/src/executor/tests.rs`:
  ```rust
  #[tokio::test]
  async fn state_file_has_no_wait_entries_after_apply() {
      let plan = build_three_effect_plan_with_wait();
      let provider = MockProvider::new()
          .with_create_response("cert", state_with_status_and_arn("ISSUED", "arn:..."))
          .with_read_sequence("cert", vec![state_with_status_and_arn("ISSUED", "arn:...")])
          .with_create_response("dist", State::default());
      let mut state_file = StateFile::new();
      let observer = NoopObserver;
      execute_plan(&provider, ExecutionInput::new_mut_state(&plan, &mut state_file), &observer).await;
      let resource_types: Vec<&str> = state_file.iter_resources().map(|(id, _)| id.resource_type.as_str()).collect();
      assert!(!resource_types.contains(&"__wait"), "state file should not contain `__wait` synthetic entries");
      assert!(resource_types.iter().any(|t| t == &"acm.Certificate"));
      assert!(resource_types.iter().any(|t| t == &"cloudfront.Distribution"));
  }
  ```
- **Implementation:** in the executor's "after this effect succeeded, update state file" path, skip when `effect.is_wait()`. (The synthetic `__wait` ResourceId from Task 4.8 is for in-memory binding resolution only; never persist it.)
- **Verification:** `cargo nextest run -p carina-core executor::tests::state_file_has_no_wait_entries_after_apply`.

**Task 5.3: Plan display fixture and snapshot test.**

- **Files:** create `carina-cli/tests/fixtures/plan_display/wait_cert/main.crn`, `carina-cli/tests/fixtures/plan_display/wait_cert/carina.state.json` (empty state), and `carina-cli/tests/fixtures/plan_display/wait_cert/snapshot.txt` (insta snapshot, generated by first run).
- **Test:** add to `carina-cli/tests/plan_snapshot.rs`:
  ```rust
  #[test]
  fn plan_display_wait_cert() {
      run_plan_snapshot("wait_cert");
  }
  ```
- **Implementation:** `main.crn`:
  ```crn
  provider aws { region = "us-east-1" }

  let cert = aws.acm.Certificate {
      domain_name       = "registry.example.com"
      validation_method = "DNS"
  }

  let validation_record = aws.route53.RecordSet {
      hosted_zone_id   = "Z123"
      name             = cert.domain_validation_options[0].resource_record_name
      type             = cert.domain_validation_options[0].resource_record_type
      ttl              = 60
      resource_records = [cert.domain_validation_options[0].resource_record_value]
  }

  let cert_issued = wait cert {
      until      = cert.status == aws.acm.Certificate.Status.Issued
      depends_on = [validation_record]
      timeout    = 75min
  }

  let dist = aws.cloudfront.Distribution {
      acm_certificate_arn = cert_issued.arn
  }
  ```
  Run `cargo insta accept` after first execution to lock the snapshot. Add the fixture to the `make plan-fixtures` Makefile target.
- **Verification:** `cargo nextest run -p carina-cli plan_display_wait_cert`.

### Phase 6 — LSP

**Task 6.1: LSP completes `wait` as a top-level snippet.**

- **Files:** modify `carina-lsp/src/completion/top_level.rs`.
- **Test:** in `carina-lsp/src/completion/top_level_tests.rs`:
  ```rust
  #[test]
  fn top_level_completions_include_wait() {
      let items = top_level_completions();
      assert!(items.iter().any(|c| c.label == "wait"));
  }
  ```
- **Implementation:** add a `CompletionItem { label: "wait", kind: Keyword, insert_text: "wait ${1:target} {\n    until = ${2}\n}" }` to `top_level_completions()`. Place it alphabetically next to other Storage keywords (`fn`, `let`).
- **Verification:** `cargo nextest run -p carina-lsp completion::top_level_tests::top_level_completions_include_wait`.

**Task 6.2: LSP completes block keys (`until`, `depends_on`, `timeout`) inside `wait` block.**

- **Files:** modify `carina-lsp/src/completion/values.rs`.
- **Test:** in `carina-lsp/src/completion/values_tests.rs`:
  ```rust
  #[test]
  fn wait_block_completions_include_until_depends_on_timeout() {
      let doc = "let foo = wait cert {\n    \n}";
      let cursor = position_of_cursor(doc); // helper finds where to place cursor
      let items = wait_block_completions(doc, cursor);
      let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
      assert!(labels.contains(&"until"));
      assert!(labels.contains(&"depends_on"));
      assert!(labels.contains(&"timeout"));
  }
  ```
- **Implementation:** detect when the cursor is inside a `wait <ident> { ... }` block (look up to the nearest unclosed `{` and check the preceding tokens). Return a fixed list of three `CompletionItem`s.
- **Verification:** `cargo nextest run -p carina-lsp completion::values_tests::wait_block_completions_include_until_depends_on_timeout`.

**Task 6.3: LSP semantic tokens highlight `wait` and `until` as keywords.**

- **Files:** modify `carina-lsp/src/semantic_tokens.rs`.
- **Test:** add to the existing `mod tests` in `carina-lsp/src/semantic_tokens.rs` (mirrors existing tests like `test_tokenize_line_resource_type` at line 1077):
  ```rust
  #[test]
  fn test_tokenize_line_wait_keyword() {
      let provider = SemanticTokensProvider::new(&[]);
      let tokens = provider.tokenize_line("let foo = wait cert {", 0);
      // tokens are (start_col, length, token_type_index) tuples
      // KEYWORD index in TOKEN_TYPES is 0 (per the array at semantic_tokens.rs:10)
      let wait_token = tokens.iter().find(|(start, len, _)| {
          *start == 10 && *len == 4   // "wait" at column 10
      }).expect("expected `wait` token");
      assert_eq!(wait_token.2, 0, "wait should map to KEYWORD type");
  }

  #[test]
  fn test_tokenize_line_until_keyword() {
      let provider = SemanticTokensProvider::new(&[]);
      let tokens = provider.tokenize_line("    until = cert.status == ISSUED", 0);
      let until_token = tokens.iter().find(|(start, len, _)| {
          *start == 4 && *len == 5   // "until" at column 4
      }).expect("expected `until` token");
      assert_eq!(until_token.2, 0);
  }
  ```
  (Note: the existing TOKEN_TYPES table at `semantic_tokens.rs:10` lists `KEYWORD` at index 0 with the comment "unused — kept for index stability". This task starts using it. If the existing tests break because their assertions assumed nothing maps to index 0, fix those assertions in the same task.)
- **Implementation:** in `SemanticTokensProvider::tokenize_line` (line 259), extend the existing keyword-recognition path that already handles `let`/`fn`/`provider` (per `KEYWORDS` in `carina-core/src/keywords.rs`, which Tasks 3.1 and 3.2 added `wait` and `until` to). The simplest path: change `tokenize_line` to look up each leading word against `carina_core::keywords::is_keyword(...)` — that single source of truth covers `wait`/`until` automatically once Tasks 3.1/3.2 land.
- **Verification:** `cargo nextest run -p carina-lsp semantic_tokens_tests::semantic_tokens_highlight_wait_and_until`.

**Task 6.4: LSP diagnoses `wait foo { ... }` where `foo` is not a known binding.**

- **Files:** modify `carina-lsp/src/diagnostics/mod.rs` (or the appropriate sub-file).
- **Test:** in `carina-lsp/src/diagnostics/tests.rs`:
  ```rust
  #[test]
  fn diagnose_wait_with_unknown_target() {
      let src = "let foo = wait nonexistent { until = nonexistent.x == y }";
      let diags = compute_diagnostics(src);
      assert!(diags.iter().any(|d|
          d.message.contains("nonexistent") && d.severity == Severity::Error
      ));
  }
  ```
- **Implementation:** during diagnostic pass, look up each `WaitExpr.target` in the document's binding map; if absent, emit a diagnostic at the target identifier's span with message `"`wait` target `nonexistent` is not a declared binding"`.
- **Verification:** `cargo nextest run -p carina-lsp diagnostics::tests::diagnose_wait_with_unknown_target`.

**Task 6.5: LSP diagnoses `until` LHS attribute that is not in target's schema.**

- **Files:** modify `carina-lsp/src/diagnostics/mod.rs`.
- **Test:**
  ```rust
  #[test]
  fn diagnose_wait_until_unknown_attribute() {
      let src = r#"
          let cert = aws.acm.Certificate { domain_name = "x", validation_method = "DNS" }
          let foo = wait cert { until = cert.statu == "ISSUED" }
      "#;
      let diags = compute_diagnostics(src);
      assert!(diags.iter().any(|d| d.message.contains("statu") && d.severity == Severity::Error));
  }
  ```
- **Implementation:** look up the target's resource type → schema → attribute set. If the LHS attribute name is not in the set, emit a diagnostic.
- **Verification:** `cargo nextest run -p carina-lsp diagnostics::tests::diagnose_wait_until_unknown_attribute`.

### Phase 7 — Validate against real infra fixture

**Task 7.1: End-to-end fixture: parse + validate a multi-file directory using `wait`.**

- **Files:** create `carina-core/tests/fixtures/wait/cert_issued/main.crn` (resource and wait declarations) plus `carina-core/tests/fixtures/wait/cert_issued/providers.crn` (provider block). Add an integration test `carina-core/tests/wait_directory_test.rs`.
- **Test:**
  ```rust
  #[test]
  fn parses_wait_construct_in_multi_file_directory() {
      let dir = "tests/fixtures/wait/cert_issued";
      let parsed = carina_core::parse_directory(dir).expect("parse should succeed");
      let wait_bindings: Vec<_> = parsed.iter_let_bindings()
          .filter(|b| matches!(b.kind, LetBindingKind::Wait(_)))
          .collect();
      assert_eq!(wait_bindings.len(), 1);
      assert_eq!(wait_bindings[0].name, "cert_issued");
  }
  ```
- **Implementation:** populate the fixture files. `main.crn` has the cert + record + wait + dist resources from the design spec; `providers.crn` has only the `provider aws { ... }` block. This exercises the multi-file requirement called out in `CLAUDE.md`'s "Directory-scoped, never single-file" section.
- **Verification:** `cargo nextest run -p carina-core wait_directory_test::parses_wait_construct_in_multi_file_directory`.

**Task 7.2: `carina validate` (CLI) accepts the `wait` fixture without errors.**

- **Files:** modify `carina-cli/tests/cli_validate_tests.rs` (or create if missing).
- **Test:**
  ```rust
  #[test]
  fn cli_validate_accepts_wait_fixture() {
      let output = run_cli(&["validate", "../carina-core/tests/fixtures/wait/cert_issued"]);
      assert!(output.status.success(), "validate failed: {}", output.stderr);
  }
  ```
- **Implementation:** none if the fixture from Task 7.1 is well-formed; this test pins the contract that `wait` doesn't break `carina validate`.
- **Verification:** `cargo nextest run -p carina-cli cli_validate_tests::cli_validate_accepts_wait_fixture`.

### Phase 8 — Whole-tree gates (run after every phase before opening PR)

- `cargo nextest run` (workspace)
- `cargo test --workspace --doc`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --check`
- `bash scripts/check-*.sh` for every script in `scripts/`
- `cargo build --release` once before opening PR — **only if** this PR has touched `Cargo.toml`, `unsafe` code, or the `release` profile config (per CLAUDE.md "Verify Protocol"). The wait-construct work changes neither, so this step is normally skipped; include it only if a sub-task drags in a Cargo.toml dependency change.

## Plan self-review

| Requirement from design doc | Covered by |
|---|---|
| `let <name> = wait <target> { ... }` syntax | Tasks 3.3, 3.4 |
| Block accepts `until`, `depends_on`, `timeout` | Task 3.3 (grammar), Task 3.4 (parser) |
| Value semantics: passthrough of target | Task 4.9 (resolver) + Task 4.8 (binding registration) |
| `until` is type-checked, MVP only `==` | Tasks 4.2, 4.3, 6.5 |
| `depends_on` is provided by separate carina#TBD-A | Acknowledged as prerequisite |
| `timeout` defaults from schema, optional override | Task 4.1 (differ uses schema default when omitted) |
| `interval` not user-visible, comes from schema | Task 4.1 (differ pulls from schema) |
| `fail_when` not in MVP | Not in plan; explicitly out of scope |
| State file does not record waits | Task 5.2 |
| Plan display: `> <name> (until <predicate>)` | Tasks 5.1, 5.3 |
| Timeout = error one-way, message has last observed value | Task 4.6 |
| Provider trait unchanged, executor does polling | Task 4.4 onwards (no provider trait modification anywhere in plan) |
| New `Effect::Wait` variant | Tasks 2.1, 2.2, 2.3 |
| `Duration` literal/type from carina#TBD-B | Acknowledged as prerequisite |
| LSP completion + diagnostics + semantic tokens | Tasks 6.1–6.5 |
| TextMate grammar parity | Task 3.7 |
| Multi-file directory fixture acceptance | Task 7.1 |

No placeholder phrases remain. Every task has a concrete file, a concrete test, a concrete implementation snippet, and an exact `cargo nextest run` command.

## Suggested review-ordering for the PR

When opening the PR, group commits per phase so reviewers can move bottom-up:

1. **Predicate AST + evaluator** (Phase 1) — small, self-contained.
2. **Effect variant + exhaustive-match plumbing** (Phase 2) — touches several files but each is one-liner additions.
3. **Parser** (Phase 3) — grammar, AST, validation, TextMate parity.
4. **Differ + executor** (Phase 4) — the load-bearing logic.
5. **Plan display + state-file exclusion** (Phase 5) — surface behaviour.
6. **LSP** (Phase 6) — editor experience.
7. **End-to-end fixture** (Phase 7) — gates the whole stack.
