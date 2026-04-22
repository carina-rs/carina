# Plan: Unify resource walk across `parsed.resources` and deferred for-bodies

Reference: `docs/specs/2026-04-19-unify-resource-walk-design.md`.

## File structure

**Create**:
- `scripts/check-no-direct-resources-access.sh` — CI grep lint.

**Modify**:
- `carina-core/src/parser/mod.rs` — add `ResourceContext` enum and
  `ParsedFile::iter_all_resources` method.
- `carina-core/src/upstream_exports.rs` — migrate `check_upstream_state_field_types`
  and collapse `check_upstream_state_field_references` deferred-walk to the new
  API.
- `carina-core/src/validation.rs` — migrate `check_unused_bindings`,
  `validate_resource_ref_types`, `validate_resources`.
- `carina-cli/src/wiring.rs` — update wrappers that currently pass
  `&parsed.resources` to downstream checkers.
- `carina-lsp/src/diagnostics/mod.rs`, `carina-lsp/src/diagnostics/checks.rs` —
  migrate attribute / ref-type checks.
- `.github/workflows/ci.yml` — wire the lint script.
- Call sites that stay on direct access get `// allow: direct <reason>`
  annotations.

## Tasks

Task decomposition uses single TDD cycles. Each task is independently verifiable.

---

### Task 1/13 — Add `ResourceContext` enum and `iter_all_resources`

**Goal**: Introduce the new API without migrating any callers yet.

**Files**:
- Modify `carina-core/src/parser/mod.rs`.

**Test**: Add to `carina-core/src/parser/mod.rs` tests section:

```rust
#[test]
fn iter_all_resources_yields_direct_then_deferred() {
    let src = r#"
        provider test {
            source = 'x/y'
            version = '0.1'
            region = 'ap-northeast-1'
        }
        test.r.res {
            name = "direct"
        }
        for _, id in orgs.accounts {
            test.r.res {
                name = id
            }
        }
    "#;
    let parsed = parse(src, &ProviderContext::default()).unwrap();

    let items: Vec<_> = parsed.iter_all_resources().collect();
    assert_eq!(items.len(), 2, "expected one direct + one deferred");

    assert!(matches!(items[0].0, ResourceContext::Direct));
    assert_eq!(
        items[0].1.get_attr("name"),
        Some(&Value::String("direct".to_string()))
    );

    assert!(matches!(items[1].0, ResourceContext::Deferred(_)));
}
```

**Implementation**: In `carina-core/src/parser/mod.rs`, after the
`DeferredForExpression` struct, add:

```rust
/// Origin of a resource yielded by [`ParsedFile::iter_all_resources`].
///
/// `Direct` means the resource was declared at top-level and its iterable
/// (if any) resolved at parse time. `Deferred` means the resource is the
/// template body of a `for` expression whose iterable resolves later;
/// consumers that care about loop-variable placeholders need the
/// `DeferredForExpression` reference to filter them out.
pub enum ResourceContext<'a> {
    Direct,
    Deferred(&'a DeferredForExpression),
}

impl ParsedFile {
    /// Iterate every resource reachable from the parsed file — both
    /// top-level `resources` and the `template_resource` of each deferred
    /// for-expression — tagged with its origin context.
    ///
    /// Per-attribute checkers (type, enum, required, ref validity, etc.)
    /// should prefer this over `self.resources.iter()` so they stay in sync
    /// with for-body code. See
    /// `docs/specs/2026-04-19-unify-resource-walk-design.md` for the
    /// rationale.
    pub fn iter_all_resources(
        &self,
    ) -> impl Iterator<Item = (ResourceContext<'_>, &Resource)> {
        self.resources
            .iter()
            .map(|r| (ResourceContext::Direct, r))
            .chain(
                self.deferred_for_expressions
                    .iter()
                    .map(|d| (ResourceContext::Deferred(d), &d.template_resource)),
            )
    }
}
```

**Verification**: `cargo test -p carina-core iter_all_resources_yields_direct_then_deferred`.

---

### Task 2/13 — Migrate `check_upstream_state_field_types` to the iterator

**Goal**: The Phase 2 type check (introduced by PR #2045) fires for resources inside
a `for` body. Adds the first migration regression test.

**Files**:
- Modify `carina-core/src/upstream_exports.rs`.

**Test**: Add a test mirroring the existing `type_check_flags_string_consumer_with_int_export`,
but with the resource nested inside a `for`:

```rust
#[test]
fn type_check_flags_for_body_attribute() {
    let parsed = parse_project_with_provider(
        r#"
            let orgs = upstream_state { source = "../organizations" }
            for _, account_id in orgs.counts {
                test.r.res {
                    name = orgs.count
                }
            }
        "#,
        "test",
    );
    let exports = mk_typed_exports(&[(
        "orgs",
        &[("count", TypeExpr::Int), ("counts", TypeExpr::List(Box::new(TypeExpr::Int)))],
    )]);
    let schemas = schema_with_attr("name", crate::schema::AttributeType::String);
    let errs = check_upstream_state_field_types(&parsed, &exports, &schemas, &|r| {
        format!("{}.{}", r.id.provider, r.id.resource_type)
    });
    assert_eq!(errs.len(), 1, "expected one error inside for body, got {errs:?}");
    assert!(errs[0].location.contains("for"));
}
```

**Implementation**: Replace the outer loop in `check_upstream_state_field_types`:

```rust
// before
for resource in &parsed.resources {
    let key = schema_key_fn(resource);
    ...
}

// after
for (ctx, resource) in parsed.iter_all_resources() {
    let key = schema_key_fn(resource);
    let Some(schema) = schemas.get(&key) else {
        continue;
    };
    for (attr_name, expr) in resource.attributes.iter() {
        if attr_name.starts_with('_') {
            continue;
        }
        let Some(attr_schema) = schema.attributes.get(attr_name) else {
            continue;
        };
        let location = match ctx {
            ResourceContext::Direct => format!("{} attribute `{}`", resource.id, attr_name),
            ResourceContext::Deferred(d) => format!(
                "for-body `{}` {} attribute `{}`",
                d.header, resource.id, attr_name
            ),
        };
        check_ref_against_type(
            expr.as_value(),
            &attr_schema.attr_type,
            exports,
            &location,
            &mut errors,
        );
    }
}
```

Import `ResourceContext` at the top of the file.

**Verification**:
`cargo test -p carina-core type_check_flags_for_body_attribute` then
`cargo test -p carina-core type_check` (the existing 6 tests must still pass).

---

### Task 3/13 — Collapse the ad-hoc deferred walk in `check_upstream_state_field_references`

**Goal**: The Phase 1 check already walks deferred via its own path; fold that
into the shared iterator and drop the redundant `deferred.attributes` visit.

**Files**:
- Modify `carina-core/src/upstream_exports.rs`.

**Test**: The existing test
`check_rejects_unexported_field_in_for_expression_body` (at
`upstream_exports.rs:607-629`) must continue to pass, and the existing
`check_rejects_unexported_field_in_for_expression_iterable` at :631 (which
hits `deferred_for_expressions[i].iterable_attr`, not attributes) must also
keep passing.

No new test needed; this is a refactor under existing coverage. Add one
specifically for the redundant-walk being gone:

```rust
#[test]
fn for_body_field_error_reported_once_not_twice() {
    // Regression: the old code walked both `deferred.template_resource.attributes`
    // and `deferred.attributes`, potentially reporting the same ref twice.
    let parsed = parse_project(
        r#"
            let orgs = upstream_state { source = "../organizations" }
            for name, _ in orgs.accounts {
                aws.s3.Bucket {
                    name = orgs.missing
                }
            }
        "#,
    );
    let exports = mk_exports(&[("orgs", &["accounts"])]);
    let errs = check_upstream_state_field_references(&parsed, &exports);
    let missing: Vec<_> = errs.iter().filter(|e| e.field == "missing").collect();
    assert_eq!(missing.len(), 1, "must not double-report, got: {missing:?}");
}
```

**Implementation**: In `check_upstream_state_field_references`, replace the
`for resource in &parsed.resources { ... }` block and the separate
`for deferred in &parsed.deferred_for_expressions { for (attr_name, expr)
in deferred.template_resource.attributes.iter() ... }` block with a single
pass over `parsed.iter_all_resources()` that mirrors the Task 2 shape. Delete
the `for (attr_name, value) in &deferred.attributes { ... }` loop
(`upstream_exports.rs:227-232`) entirely — it duplicates the
`template_resource.attributes` walk.

The `deferred_for_expressions[i].iterable_binding` / `.iterable_attr` check
(at :239-255) stays as-is; it is not per-attribute.

**Verification**:
`cargo test -p carina-core for_body_field_error_reported_once_not_twice` then
`cargo test -p carina-core check_rejects_unexported_field_in_for_expression`
(both existing tests must still pass).

---

### Task 4/13 — Migrate `check_unused_bindings`

**Goal**: Unused-binding detection sees bindings referenced only inside a
for body (currently misses them, so a binding used only in the loop body is
reported as unused).

**Files**:
- Modify `carina-core/src/validation.rs`.

**Test**: In `validation.rs`' test module:

```rust
#[test]
fn binding_used_inside_for_body_is_not_flagged_as_unused() {
    use crate::parser::parse;

    let src = r#"
        provider test {
            source = 'x/y'
            version = '0.1'
            region = 'ap-northeast-1'
        }
        let vpc = test.r.res { name = "v" }
        for _, id in orgs.accounts {
            test.r.res {
                name = vpc.name
            }
        }
    "#;
    let parsed = parse(src, &crate::parser::ProviderContext::default()).unwrap();
    let unused = check_unused_bindings(&parsed);
    assert!(
        !unused.iter().any(|w| w.binding == "vpc"),
        "`vpc` is referenced inside the for body, got: {unused:?}"
    );
}
```

**Implementation**: Locate `check_unused_bindings` in `validation.rs:513`.
Replace the resource-walk that collects referenced bindings:

```rust
// before
for resource in &parsed.resources {
    for expr in resource.attributes.values() {
        collect_referenced(expr.as_value(), &mut referenced);
    }
}

// after
for (_ctx, resource) in parsed.iter_all_resources() {
    for expr in resource.attributes.values() {
        collect_referenced(expr.as_value(), &mut referenced);
    }
}
```

Import `crate::parser::ResourceContext` if needed (for the `_ctx` destructure
to compile — the actual variant is not matched here). If the compiler
complains about unused, use `for (_, resource)` and don't import the type.

**Verification**: `cargo test -p carina-core binding_used_inside_for_body_is_not_flagged_as_unused`
then `cargo test -p carina-core check_unused_bindings` (all existing
sub-tests).

---

### Task 5/13 — Migrate `validate_resource_ref_types`

**Goal**: Schema-driven ref type checks (the non-upstream version) see
for-body attributes.

**Files**:
- Modify `carina-core/src/validation.rs`.
- Modify `carina-cli/src/wiring.rs` (the wrapper that passes
  `&parsed.resources`).

**Test**: In `validation.rs` tests:

```rust
#[test]
fn ref_type_mismatch_inside_for_body_is_rejected() {
    // Inside a for body, assigning a vpc_id to an attr expecting an ipam_pool_id
    // must be flagged.
    let src = r#"
        provider test { source = 'x/y' version = '0.1' region = 'ap-northeast-1' }
        let vpc = test.r.vpc { name = "v" }
        for _, id in orgs.xs {
            test.r.pool_user {
                pool_id = vpc.vpc_id
            }
        }
    "#;
    let parsed = parse(src, &ProviderContext::default()).unwrap();
    // Build a schema map where test.r.vpc exposes `vpc_id: AwsResourceId`
    // and test.r.pool_user requires `pool_id: IpamPoolId`.
    let schemas = build_schemas_for_ref_type_mismatch();
    let result = validate_resource_ref_types(
        &parsed, // entire ParsedFile, not &[Resource]
        &schemas,
        &|r: &Resource| format!("{}.{}", r.id.provider, r.id.resource_type),
        &HashSet::new(),
    );
    assert!(result.is_err(), "expected type-mismatch error in for body");
    let msg = result.unwrap_err();
    assert!(msg.contains("pool_id"), "got: {msg}");
}
```

**Implementation**: Change the signature of `validate_resource_ref_types`
from `&[Resource]` to `&ParsedFile`:

```rust
pub fn validate_resource_ref_types(
    parsed: &ParsedFile,
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
    argument_names: &HashSet<String>,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    // Build binding_map from direct resources only — for-body loops
    // cannot contain let bindings, so their template_resource.binding is
    // always None and not lookup-relevant.
    let mut binding_map: HashMap<String, &Resource> = HashMap::new();
    for resource in &parsed.resources { // allow: direct — binding map is topology, not checker
        if let Some(ref binding_name) = resource.binding {
            binding_map.insert(binding_name.clone(), resource);
        }
    }

    for (_ctx, resource) in parsed.iter_all_resources() {
        // ... existing body, unchanged ...
    }

    // rest unchanged
}
```

Update `carina-cli/src/wiring.rs::validate_resource_ref_types_with_ctx`
(around line 177) to pass the full `ParsedFile` instead of
`&parsed.resources`. Update each existing test in `validation.rs`
(`:1086`, `:1116`, `:1162`, `:1200`) — they currently pass `&[subnet]` etc.
— to construct a minimal `ParsedFile`:

```rust
let parsed = ParsedFile { resources: vec![subnet], ..ParsedFile::default() };
validate_resource_ref_types(&parsed, ...);
```

(`ParsedFile::default` already exists.)

**Verification**:
`cargo test -p carina-core ref_type_mismatch_inside_for_body_is_rejected` then
`cargo test -p carina-core validate_resource_ref_types`.

---

### Task 6/13 — Migrate `validate_resources`

**Goal**: Per-resource schema validation (required attrs, enum membership,
custom-type validators) runs on for-body resources. Covers #2044 directly.

**Files**:
- Modify `carina-core/src/validation.rs`.
- Modify `carina-cli/src/wiring.rs::validate_resources_with_ctx`.

**Test**: In `validation.rs` tests:

```rust
#[test]
fn enum_membership_violation_in_for_body_is_flagged() {
    // Regression for #2044: inside a `for` body, a string literal that
    // isn't a valid member of a StringEnum attribute must be flagged.
    let src = r#"
        provider test { source = 'x/y' version = '0.1' region = 'ap-northeast-1' }
        for _, id in orgs.xs {
            test.r.mode_holder {
                mode = "aaaa"
            }
        }
    "#;
    let parsed = parse(src, &ProviderContext::default()).unwrap();
    let schemas = build_schemas_with_string_enum_attr();
    let result = validate_resources(&parsed, &schemas, &|r| format!("{}.{}", r.id.provider, r.id.resource_type));
    assert!(result.is_err(), "expected enum-mismatch error in for body");
    assert!(result.unwrap_err().contains("aaaa"));
}
```

**Implementation**: Change `validate_resources` to take `&ParsedFile`
instead of `&[Resource]`, and drive its inner loop from
`parsed.iter_all_resources()`. The per-resource body is unchanged. Update
the CLI wrapper (`wiring.rs:159-175`) to pass the full `ParsedFile`.

Update every existing `validate_resources(...)` test (approximately 15
call sites in `validation.rs` test module) to construct a `ParsedFile`
like Task 5.

**Verification**:
`cargo test -p carina-core enum_membership_violation_in_for_body_is_flagged`
then `cargo test -p carina-core validate_resources`.

---

### Task 7/13 — Migrate LSP `check_refs_and_diagnostics` path

**Goal**: LSP surfaces enum / type mismatches inside a for body as
editor squiggles (same repro as Task 6 but exercised via the
`DiagnosticEngine`).

**Files**:
- Modify `carina-lsp/src/diagnostics/mod.rs`.
- Modify `carina-lsp/src/diagnostics/checks.rs`.

**Test**: In `carina-lsp/src/diagnostics/tests/extended.rs`:

```rust
#[test]
fn enum_mismatch_inside_for_body_surfaces_as_diagnostic() {
    let provider = test_engine_with_enum_attr(); // helper to define below
    let source = r#"
for _, id in orgs.xs {
  test.r.mode_holder {
    mode = "aaaa"
  }
}
"#;
    let doc = create_document(source);
    let diagnostics = provider.analyze(&doc, None, &HashMap::new(), &HashSet::new());
    assert!(
        diagnostics.iter().any(|d| d.message.contains("aaaa")),
        "expected enum-mismatch diagnostic inside for body, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}
```

Add `test_engine_with_enum_attr` helper to `tests/mod.rs` following the
pattern of `test_engine_with_nested_structs`.

**Implementation**: In
`carina-lsp/src/diagnostics/mod.rs::analyze_with_filename` the current
resource iteration (around line 179-189 for the `binding_schema_map` and
the resource-walk for attribute validation) switches to
`parsed.iter_all_resources()`. Location strings use the
`ResourceContext::Deferred` branch to mention the for header.

In `carina-lsp/src/diagnostics/checks.rs` the sites at `:803-806`
(binding-definition collection) and `:1305-1401` (ref/function checks)
also migrate. Each migration keeps its existing per-attribute logic; only
the outer loop source changes.

**Verification**:
`cargo test -p carina-lsp enum_mismatch_inside_for_body_surfaces_as_diagnostic`
then `cargo test -p carina-lsp` (279 existing LSP tests must still pass).

---

### Task 8/13 — Migrate LSP unused-binding / unknown-ref checks

**Goal**: LSP unused-binding warning and unknown-ref diagnostic both
see for-body refs.

**Files**:
- Modify `carina-lsp/src/diagnostics/checks.rs`.

**Test**: Add to `carina-lsp/src/diagnostics/tests/extended.rs`:

```rust
#[test]
fn lsp_binding_used_only_in_for_body_is_not_flagged_unused() {
    let provider = test_engine();
    let source = r#"
let vpc = test.r.vpc { name = "v" }
for _, id in orgs.xs {
  test.r.res { name = vpc.name }
}
"#;
    let doc = create_document(source);
    let diagnostics = provider.analyze(&doc, None, &HashMap::new(), &HashSet::new());
    assert!(
        !diagnostics.iter().any(|d| d.message.contains("unused") && d.message.contains("vpc")),
        "vpc used in for body, must not be flagged unused, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}
```

**Implementation**: Apply the same mechanical substitution as Task 7 to
the remaining LSP checkers (`check_unused_bindings` mirror,
`check_undefined_references` follow-through for resource attributes).
Each loop that starts `for resource in &parsed.resources` becomes
`for (_ctx, resource) in parsed.iter_all_resources()`.

**Verification**:
`cargo test -p carina-lsp lsp_binding_used_only_in_for_body_is_not_flagged_unused`
then `cargo test -p carina-lsp`.

---

### Task 9/13 — Add `// allow: direct <reason>` markers to legitimate consumers

**Goal**: Every remaining direct use of `parsed.resources` in application
code (non-parser, non-deps) is annotated with an explicit reason, so the
lint in Task 10 can distinguish allowed from forbidden.

**Files**:
- `carina-core/src/parser/mod.rs` — internal resolution passes. Reasons
  vary per site (document each).
- `carina-core/src/deps.rs` — sort_resources_by_dependencies.
- `carina-core/src/config_loader.rs` — merge loop.
- `carina-core/src/module.rs`, `carina-core/src/module_resolver/mod.rs`
  — module expansion.
- `carina-cli/src/commands/{plan,apply,destroy,state}.rs` — reconciliation.
- `carina-cli/src/commands/{validate,lint}.rs` — display/reporting.
- `carina-cli/src/wiring.rs` (remaining direct uses after Tasks 5–6).
- `carina-cli/src/fixture_plan.rs` — fixture helpers.

**Test**: N/A for this task — annotations don't change behavior. The
verification is covered by Task 10.

**Implementation**: Walk every grep hit from the exploration map and
append an in-line comment on the owning line. Standard phrasings (script
in Task 10 accepts these):

- `// allow: direct — parser-internal, pre-expansion`
- `// allow: direct — topology (dependency sort)`
- `// allow: direct — module expansion, handled separately`
- `// allow: direct — plan-time reconciliation`
- `// allow: direct — display/reporting`
- `// allow: direct — fixture test inspection`

Example at `carina-core/src/deps.rs:694` (test call):
```rust
let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap(); // allow: direct — topology (dependency sort)
```

**Verification**:
```bash
grep -rn "parsed\.resources\." carina-core/src carina-cli/src carina-lsp/src \
  | grep -v "// allow: direct" \
  | grep -v "iter_all_resources"
```
Output should be empty (no unmarked accesses remain). Run after Tasks 2–8
are merged; this task depends on them.

---

### Task 10/13 — Add CI lint script

**Goal**: Prevent future direct-access regressions.

**Files**:
- Create `scripts/check-no-direct-resources-access.sh`.

**Test**: Run the script against the current tree (after Task 9).
It must exit 0. Then temporarily add an unmarked access to a test file
and confirm it exits 1.

**Implementation**: Write to `scripts/check-no-direct-resources-access.sh`:

```bash
#!/usr/bin/env bash
# Enforce: application code must iterate via ParsedFile::iter_all_resources().
# Direct access to parsed.resources is allowed only when explicitly marked
# with `// allow: direct <reason>` on the same line.

set -euo pipefail

ALLOWED_REASONS=(
  "parser-internal, pre-expansion"
  "topology (dependency sort)"
  "module expansion, handled separately"
  "plan-time reconciliation"
  "display/reporting"
  "fixture test inspection"
)

scan_dirs=(
  carina-core/src
  carina-cli/src
  carina-lsp/src
)

# Pattern: \.resources\.  (match as a field access). Filter out
# .iter_all_resources, allow markers, and comment-only mentions.
matches=$(grep -rn --include='*.rs' -E '\.resources\.' "${scan_dirs[@]}" || true)

bad=()
while IFS= read -r line; do
  [ -z "$line" ] && continue

  # Skip the new API itself.
  if echo "$line" | grep -q 'iter_all_resources'; then
    continue
  fi

  # Skip the definition of `resources:` field in structs.
  if echo "$line" | grep -qE '(pub )?resources:'; then
    continue
  fi

  # Skip lines with an allow marker and a known reason.
  if echo "$line" | grep -q '// allow: direct'; then
    ok=0
    for reason in "${ALLOWED_REASONS[@]}"; do
      if echo "$line" | grep -qF "// allow: direct — $reason"; then
        ok=1
        break
      fi
    done
    if [ $ok -eq 1 ]; then
      continue
    fi
    echo "Error: allow marker with unrecognized reason: $line" >&2
    bad+=("$line")
    continue
  fi

  # Comment-only mention: skip.
  if echo "$line" | grep -qE '^\s*//'; then
    continue
  fi

  bad+=("$line")
done <<< "$matches"

if [ ${#bad[@]} -gt 0 ]; then
  echo "Direct access to parsed.resources without // allow: direct marker:" >&2
  for b in "${bad[@]}"; do
    echo "  $b" >&2
  done
  exit 1
fi
```

Mark executable: `chmod +x scripts/check-no-direct-resources-access.sh`.

Add to `.github/workflows/ci.yml` as a new step in an existing lint job:

```yaml
- name: Enforce no direct access to parsed.resources
  run: ./scripts/check-no-direct-resources-access.sh
```

**Verification**:
1. `./scripts/check-no-direct-resources-access.sh` — exit 0.
2. Temporarily add a line `let n = parsed.resources.len();` to a test
   file without a marker. Re-run. It must exit 1 and name the file.
3. Revert the probe.

---

### Task 11/13 — Update design doc with "implemented" status

**Goal**: The design doc keeps documenting the decision for future
readers; mark it shipped.

**Files**:
- Modify `docs/specs/2026-04-19-unify-resource-walk-design.md`.

**Test**: N/A (documentation).

**Implementation**: Add a `## Status` section at the top:

```markdown
## Status

Shipped 2026-04-19 (PR #NNNN). All checkers listed under "Scope" migrate
to `iter_all_resources`. `// allow: direct <reason>` annotations are in
place; CI lint prevents regressions.
```

**Verification**: N/A.

---

### Task 12/13 — Full workspace verification

**Goal**: Prove no regressions.

**Files**: None (test task).

**Test**: `cargo test --workspace`.

**Implementation**: N/A.

**Verification**:
- `cargo test --workspace` exits 0.
- `cargo clippy --workspace --all-targets -- -D warnings` exits 0.
- `./scripts/check-no-direct-resources-access.sh` exits 0.

---

### Task 13/13 — Revisit PR #2045 (#1992 Phase 2)

**Goal**: Confirm that the PR #2045 migration from Task 2 is the correct
semantic (test inside for body passes); nothing else to do in that PR.

**Files**: None — this is a verification task.

**Test**: In the real project `infra/aws/management/identity-center/main.crn`,
`target_type = "aaaa"` inside a `for` body triggers the CLI validate
error after this plan's Tasks 1-6 land (which subsumes #2044's fix for
enum checks, plus #1992's Phase 2 for upstream type checks).

**Implementation**: N/A.

**Verification**: Run `carina validate .` in
`infra/aws/management/identity-center/`. Observe the enum-mismatch error
on the for-body line. Observe also that the previously-silent Phase 2
`target_id = <some wrong-typed upstream ref>` now fires if present.

---

## Dependencies

- Task 1 must land first (introduces the API).
- Tasks 2, 3 depend on Task 1.
- Tasks 4, 5, 6 depend on Task 1 (independent of each other).
- Tasks 7, 8 depend on Task 1 (independent).
- Task 9 depends on Tasks 2–8 (annotates the surviving direct uses).
- Task 10 depends on Task 9 (the lint must pass on the current tree).
- Task 11 depends on Task 10.
- Tasks 12, 13 depend on Task 11 (final verification).

Tasks 2-8 can potentially be parallel PRs if needed; each is independently
verifiable.
