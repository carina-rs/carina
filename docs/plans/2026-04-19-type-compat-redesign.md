# Plan: Directional, subtype-aware type compatibility (#2079)

Reference: `docs/specs/2026-04-19-type-compat-redesign-design.md`.

Scope crosses three repositories. Task numbering is global; each task is
labelled with its target repo and creates an issue in that repo.

## File structure

**carina (this repo)**:
- **Modify** `carina-core/src/schema.rs` — `Custom` struct fields,
  `is_assignable_to`, helpers, tests.
- **Modify** `carina-core/src/validation.rs` — ref-type call site.
- **Modify** `carina-core/src/provider.rs` — 2 literal `Custom` inits.
- **Modify** `carina-core/src/differ/comparison.rs` — any Custom reads.
- **Modify** `carina-core/src/upstream_exports.rs` — consume new API.
- **Modify** `carina-lsp/src/completion/values.rs` — 5 literal Custom inits.
- **Modify** `carina-lsp/src/diagnostics/mod.rs` — call site.
- **Modify** `carina-lsp/src/diagnostics/tests/extended.rs` — test literals.
- **Modify** `carina-plugin-host/src/wasm_convert.rs` — 2 literal inits.

**carina-provider-aws**:
- **Modify** `carina-aws-types/src/lib.rs` — all `vpc_id()` / `subnet_id()`
  / `aws_account_id()` / etc. helpers emit new shape.
- **Modify** `carina-codegen-aws/src/main.rs` — update `format!` templates
  that emit `AttributeType::Custom`.
- **Regenerate** `carina-provider-aws/src/schemas/generated/**`.
- Publish new git tag.

**carina-provider-awscc**:
- **Modify** `carina-aws-types/src/lib.rs` (local copy) — same as aws.
- **Modify** `carina-provider-awscc/src/bin/codegen.rs` — update
  `format!` templates; extend `resource_type_overrides()` with SSO/
  IdentityStore properties.
- **Regenerate** `carina-provider-awscc/src/schemas/generated/**`.
- Publish new git tag.

## Tasks

Each task = one TDD cycle. Grouped by repo, ordered by dependency.

---

### Task 1/18 — carina: add `semantic_name` / `pattern` / `length` fields to `Custom` [repo: carina]

**Goal**: Change `AttributeType::Custom` shape without changing
behaviour yet. All existing literal inits get migrated mechanically.

**Files**: `carina-core/src/schema.rs`.

**Test**: Add to `schema.rs` tests module:

```rust
#[test]
fn custom_carries_semantic_name_pattern_length() {
    let t = AttributeType::Custom {
        semantic_name: Some("VpcId".to_string()),
        base: Box::new(AttributeType::String),
        pattern: Some("^vpc-[a-f0-9]+$".to_string()),
        length: Some((Some(8), Some(21))),
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    match t {
        AttributeType::Custom { semantic_name, pattern, length, .. } => {
            assert_eq!(semantic_name.as_deref(), Some("VpcId"));
            assert_eq!(pattern.as_deref(), Some("^vpc-[a-f0-9]+$"));
            assert_eq!(length, Some((Some(8), Some(21))));
        }
        _ => panic!("expected Custom"),
    }
}
```

**Implementation**: In `carina-core/src/schema.rs:92-103`, change:

```rust
Custom {
    name: String,
    base: Box<AttributeType>,
    validate: fn(&Value) -> Result<(), String>,
    namespace: Option<String>,
    to_dsl: Option<fn(&str) -> String>,
},
```

to:

```rust
Custom {
    semantic_name: Option<String>,
    base: Box<AttributeType>,
    pattern: Option<String>,
    length: Option<(Option<u64>, Option<u64>)>,
    validate: fn(&Value) -> Result<(), String>,
    namespace: Option<String>,
    to_dsl: Option<fn(&str) -> String>,
},
```

Then update `type_name()` (`schema.rs:463`) to derive a display string:

```rust
AttributeType::Custom { semantic_name, pattern, length, .. } => {
    if let Some(n) = semantic_name {
        n.clone()
    } else {
        let mut s = String::from("String");
        if pattern.is_some() { s.push_str("(pattern"); }
        if let Some((min, max)) = length {
            if pattern.is_some() { s.push_str(", "); } else { s.push_str("("); }
            s.push_str(&format!("len: {}", length_display(min, max)));
        }
        if pattern.is_some() || length.is_some() { s.push(')'); }
        s
    }
}
```

(`length_display` is a tiny helper printing `1..=64`, `1..`, etc.)

Update `is_string_based_custom` to use the new field shape.

Mechanically migrate **every** literal `AttributeType::Custom { name:
..., ... }` in `carina-core/src/schema.rs` (16 sites — all in tests
or helper fns). For each:

- Old `name: "VpcId".to_string()` → `semantic_name: Some("VpcId".to_string())`
- Old `name: "String(pattern)".to_string()` → `semantic_name: None, pattern: Some("<actual pattern>".to_string())` (pattern available from the validate closure / surrounding context; if truly unknown, use `pattern: None`)
- Add `length: None` (or actual bounds if known).

**Verification**: `cargo build -p carina-core` compiles. New test
passes. Existing tests may still fail (those come in later tasks).

---

### Task 2/18 — carina: migrate `Custom` literal inits in carina-core (outside schema.rs) [repo: carina]

**Goal**: Fix remaining compile errors from Task 1 in the rest of
carina-core.

**Files**:
- `carina-core/src/provider.rs` (2 sites)
- `carina-core/src/differ/comparison.rs` + `comparison_tests.rs`
- `carina-core/src/upstream_exports.rs`
- `carina-core/src/validation.rs` (call sites, not literals yet)

**Test**: No new test — this task is purely a mechanical migration
unblocking compilation. Existing tests in each file act as regression
coverage.

**Implementation**: For each literal `AttributeType::Custom { name:
<x>, ... }` in the files above, rewrite to the new struct layout per
Task 1 rules.

Also: any **match arms** destructuring Custom must be updated:

```rust
AttributeType::Custom { name, .. } => { ... }
// becomes
AttributeType::Custom { semantic_name, .. } => {
    let name = semantic_name.as_deref().unwrap_or("<anonymous>");
    ...
}
```

**Verification**: `cargo build -p carina-core` clean; `cargo test -p
carina-core` compiles (tests may still fail, fixed in later tasks).

---

### Task 3/18 — carina: migrate `Custom` literals in carina-lsp [repo: carina]

**Goal**: Fix compile in carina-lsp.

**Files**:
- `carina-lsp/src/completion/values.rs` (5 literals)
- `carina-lsp/src/diagnostics/mod.rs` (call sites to `is_compatible_with`)
- `carina-lsp/src/diagnostics/tests/extended.rs`

**Test**: No new test. Existing LSP tests regression-cover.

**Implementation**:

- Rewrite literal `Custom { name: ..., ... }` sites per Task 1 rules.
- Rename `is_compatible_with` call sites to `is_assignable_to` (Task
  5 renames it; for now leave the old name in place as a
  temporary thin wrapper so this task doesn't depend on Task 5).

**Verification**: `cargo build -p carina-lsp` clean; `cargo test -p
carina-lsp`.

---

### Task 4/18 — carina: migrate `Custom` literals in carina-plugin-host [repo: carina]

**Goal**: Fix compile in carina-plugin-host.

**Files**: `carina-plugin-host/src/wasm_convert.rs` (2 literals).

**Test**: No new test; existing tests regress.

**Implementation**: Same mechanical rewrite as Tasks 2/3.

**Verification**: `cargo build -p carina-plugin-host`.

---

### Task 5/18 — carina: introduce `is_assignable_to` with directional semantics [repo: carina]

**Goal**: Implement the new assignability check. Keep the old
`is_compatible_with` as a deprecated thin wrapper that delegates to
`is_assignable_to(self, other)` so existing call sites continue to
compile.

**Files**: `carina-core/src/schema.rs`.

**Test**:

```rust
#[test]
fn assignable_rejects_distinct_semantic_names() {
    let vpc = make_custom_semantic("VpcId");
    let subnet = make_custom_semantic("SubnetId");
    assert!(!vpc.is_assignable_to(&subnet));
    assert!(!subnet.is_assignable_to(&vpc));
}

#[test]
fn assignable_allows_same_semantic_name() {
    let a = make_custom_semantic("VpcId");
    let b = make_custom_semantic("VpcId");
    assert!(a.is_assignable_to(&b));
}

#[test]
fn assignable_narrow_to_anonymous_is_ok() {
    let account = make_custom_semantic("AwsAccountId");
    let anon = make_custom_anon_pattern("^\\d{12}$");
    // Source is semantic AwsAccountId (pattern None); sink is anonymous
    // String(pattern). See Edge case: helpers will eventually give
    // AwsAccountId its pattern; here we test the structural rule.
    // When source has no pattern and sink has one, rule 3 says NG.
    assert!(!account.is_assignable_to(&anon));
}

#[test]
fn assignable_anon_to_anon_length_containment() {
    let narrow = make_custom_anon_len(1, 36);
    let wide = make_custom_anon_len(1, 64);
    assert!(narrow.is_assignable_to(&wide));   // narrow ⊆ wide: OK
    assert!(!wide.is_assignable_to(&narrow));  // wide ⊄ narrow: NG
}

#[test]
fn assignable_rejects_non_custom_to_custom() {
    let vpc = make_custom_semantic("VpcId");
    assert!(!AttributeType::String.is_assignable_to(&vpc));
}

#[test]
fn assignable_allows_same_primitives() {
    assert!(AttributeType::String.is_assignable_to(&AttributeType::String));
    assert!(AttributeType::Int.is_assignable_to(&AttributeType::Int));
    assert!(!AttributeType::Int.is_assignable_to(&AttributeType::String));
}
```

(`make_custom_semantic`, `make_custom_anon_pattern`, `make_custom_anon_len`
are tiny test helpers.)

**Implementation**: In `carina-core/src/schema.rs` add:

```rust
impl AttributeType {
    /// Check if a value of `self`'s type can be assigned to a sink of
    /// `sink`'s type. Directional: narrowing source → wider sink is OK,
    /// but widening source → narrower sink is NG.
    pub fn is_assignable_to(&self, sink: &AttributeType) -> bool {
        use AttributeType::*;
        // Rule 6/7: Unions.
        if let Union(members) = sink {
            return members.iter().any(|m| self.is_assignable_to(m));
        }
        if let Union(members) = self {
            return members.iter().all(|m| m.is_assignable_to(sink));
        }
        match (self, sink) {
            (Custom { semantic_name: Some(s_name), .. },
             Custom { semantic_name: Some(k_name), .. }) if s_name != k_name => false,
            (Custom { semantic_name: _, pattern: s_pat, length: s_len, base: s_base, .. },
             Custom { pattern: k_pat, length: k_len, base: k_base, .. }) => {
                // Pattern: pat-1 equality; sink pat Some + source None = NG
                if let (Some(sp), Some(kp)) = (s_pat, k_pat) {
                    if sp != kp { return false; }
                } else if k_pat.is_some() && s_pat.is_none() {
                    return false;
                }
                // Length containment
                if !length_contains(s_len.as_ref(), k_len.as_ref()) {
                    return false;
                }
                // Recurse on base
                s_base.is_assignable_to(k_base)
            }
            (Custom { base, .. }, non_custom) => base.is_assignable_to(non_custom),
            (non_custom, Custom { .. }) => {
                let _ = non_custom;
                false
            }
            (a, b) => a.type_name() == b.type_name(),
        }
    }
}

fn length_contains(
    source: Option<&(Option<u64>, Option<u64>)>,
    sink: Option<&(Option<u64>, Option<u64>)>,
) -> bool {
    let Some((s_min, s_max)) = source else {
        return sink.is_none() || matches!(sink, Some((None, None)));
    };
    let Some((k_min, k_max)) = sink else { return true };
    let s_min = s_min.unwrap_or(0);
    let s_max = s_max.unwrap_or(u64::MAX);
    let k_min = k_min.unwrap_or(0);
    let k_max = k_max.unwrap_or(u64::MAX);
    k_min <= s_min && s_max <= k_max
}
```

Keep `is_compatible_with` for source compatibility:

```rust
#[deprecated(note = "Use is_assignable_to (directional); see #2079")]
pub fn is_compatible_with(&self, other: &AttributeType) -> bool {
    self.is_assignable_to(other)
}
```

**Verification**:
`cargo test -p carina-core schema::tests::assignable` passes all 6 new
tests.

---

### Task 6/18 — carina: update `validate_resource_ref_types` call site to `is_assignable_to` [repo: carina]

**Goal**: `validation.rs:142-147` now passes source/sink in the right
order.

**Files**: `carina-core/src/validation.rs`.

**Test**:

```rust
#[test]
fn ref_type_rejects_semantic_name_mismatch() {
    // source: sso.identity_store_id : IdentityStoreId
    // sink:   assignment.target_id : AwsAccountId
    let parsed = parse_minimal(r#"
        let sso = test.r.sso { name = "s" }
        test.r.assignment { target_id = sso.identity_store_id, target_type = "AWS_ACCOUNT" }
    "#);
    let schemas = schemas_with_identity_store_and_assignment();
    let err = validate_resource_ref_types(&parsed, &schemas, &schema_key_fn, &HashSet::new());
    assert!(err.is_err(), "expected semantic-name mismatch error");
    assert!(err.unwrap_err().contains("target_id"));
}
```

**Implementation**: In `carina-core/src/validation.rs` find the call to
`attr_schema.attr_type.is_compatible_with(&ref_attr_schema.attr_type)`
at line ~142. Replace with:

```rust
ref_attr_schema.attr_type.is_assignable_to(&attr_schema.attr_type)
```

Note the argument order: the **source** (ref_attr_schema — what we're
pulling from) is `self`, the **sink** (attr_schema — what the current
resource's attribute expects) is the argument.

Update the error message to reflect the new semantics:

```rust
format!(
    "{}: cannot assign {} to '{}': expected {}, got {} (from {}.{})",
    resource.id, ref_type_name, attr_name, expected_type_name,
    ref_type_name, ref_binding, ref_attr,
)
```

**Verification**: `cargo test -p carina-core validate_resource_ref_types`
plus new test. Existing tests updated to reflect directional semantics
(e.g. tests that asserted `VpcId → SubnetId` is OK must be deleted or
inverted).

---

### Task 7/18 — carina: rewrite `is_compatible_with_two_string_based_customs` test [repo: carina]

**Goal**: The test at `schema.rs:3384` codifies the buggy permissive
rule. Replace it with the correct asymmetric tests.

**Files**: `carina-core/src/schema.rs`.

**Test / Implementation**: Delete
`is_compatible_with_two_string_based_customs`. The asymmetric cases are
already covered by Task 5's new tests. Replace
`is_compatible_with_string_and_custom` with:

```rust
#[test]
fn semantic_custom_assigns_to_anonymous_sink() {
    let vpc = make_custom_semantic("VpcId");
    let anon = AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    // Sink has no constraint → any String-based Custom assigns.
    assert!(vpc.is_assignable_to(&anon));
    // Reverse: anon has no proof it's a VpcId → NG.
    assert!(!anon.is_assignable_to(&vpc));
}
```

**Verification**: `cargo test -p carina-core schema::tests` — all
non-deleted tests pass, new test passes, deleted test no longer
referenced.

---

### Task 8/18 — carina: update any remaining `is_compatible_with` callers, remove the deprecated shim [repo: carina]

**Goal**: Every call site uses `is_assignable_to` with explicit
source/sink. Remove the deprecated `is_compatible_with`.

**Files**: Grep `is_compatible_with` across the tree; migrate each.
Remove the deprecated method at the end.

**Test**: No new test. Existing tests + `cargo clippy -- -D warnings`
catch any missed deprecation warnings becoming hard errors.

**Implementation**:

```bash
# Review remaining callers
grep -rn "is_compatible_with" --include="*.rs"
```

For each caller, reason about which side is source and which is sink,
rewrite accordingly. Remove the deprecated `is_compatible_with`
definition.

**Verification**:

```bash
cargo test -p carina-core
cargo test -p carina-lsp
cargo clippy --workspace --all-targets -- -D warnings
grep -rn "is_compatible_with" --include="*.rs"  # empty
```

---

### Task 9/18 — carina-provider-aws: migrate `carina-aws-types` helpers to new Custom shape [repo: carina-provider-aws]

**Goal**: All `vpc_id()`, `subnet_id()`, `aws_account_id()`, etc. in
`carina-aws-types/src/lib.rs` return the new Custom shape with
`semantic_name`, `pattern`, `length` filled in.

**Files**: `carina-aws-types/src/lib.rs`.

**Test**: In the same file's test module:

```rust
#[test]
fn aws_account_id_carries_pattern_and_length() {
    let t = aws_account_id();
    match t {
        AttributeType::Custom { semantic_name, pattern, length, .. } => {
            assert_eq!(semantic_name.as_deref(), Some("AwsAccountId"));
            assert_eq!(pattern.as_deref(), Some("^\\d{12}$"));
            assert_eq!(length, Some((Some(12), Some(12))));
        }
        _ => panic!(),
    }
}
```

**Implementation**: For each helper, populate the new fields. Example:

```rust
pub fn aws_account_id() -> AttributeType {
    AttributeType::Custom {
        semantic_name: Some("AwsAccountId".to_string()),
        base: Box::new(AttributeType::String),
        pattern: Some("^\\d{12}$".to_string()),
        length: Some((Some(12), Some(12))),
        validate: |value| { /* ... */ },
        namespace: None,
        to_dsl: None,
    }
}
```

Do this for every helper in `carina-aws-types/src/lib.rs` (vpc_id,
subnet_id, security_group_id, network_acl_id, ipam_id, kms_key_id,
kms_key_arn, iam_role_arn, iam_policy_arn, awscc_region,
transit_gateway_id, egress_only_internet_gateway_id, …).

**Verification**: `cargo test -p carina-aws-types`.

---

### Task 10/18 — carina-provider-aws: update codegen emission templates [repo: carina-provider-aws]

**Goal**: `carina-codegen-aws/src/main.rs:1054` emits the new Custom
struct syntax.

**Files**: `carina-codegen-aws/src/main.rs`.

**Test**: Golden-file test comparing generated output for one known
shape:

```rust
#[test]
fn codegen_emits_new_custom_shape_for_ranged_int() {
    let out = emit_int_range_custom("MaxSize", 1, 100);
    assert!(out.contains("semantic_name: None"));
    assert!(out.contains("length: None"));
    // No leftover `name:` key
    assert!(!out.contains("name: \""));
}
```

**Implementation**: Update every `format!` template in
`main.rs` that produces `AttributeType::Custom { ... }` text. Replace
the `name: "..."` line with `semantic_name: Some("...")` (for
known-semantic emissions) or `semantic_name: None` +
`pattern: Some("...")` / `length: Some(...)` for anonymous pattern
emissions. The templates live around lines 1054, and wherever
Custom-emitting logic appears (grep for `AttributeType::Custom` in
`carina-codegen-aws/`).

**Verification**: `cargo test -p carina-codegen-aws`.

---

### Task 11/18 — carina-provider-aws: regenerate schemas and fix compile [repo: carina-provider-aws]

**Goal**: All `src/schemas/generated/**` reflect the new codegen
output. The provider crate compiles.

**Files**: `carina-provider-aws/src/schemas/generated/**` (regenerated).

**Test**: No new test. The full provider test suite regression-covers.

**Implementation**:

```bash
cd carina-provider-aws
cargo run --bin carina-codegen-aws -- --regenerate
cargo build -p carina-provider-aws
```

Fix any hand-written non-generated code that still uses the old Custom
shape.

**Verification**: `cargo test -p carina-provider-aws`.

---

### Task 12/18 — carina-provider-aws: publish new tag [repo: carina-provider-aws]

**Goal**: Tag and publish a new version so downstreams (infra repos
and awscc) can depend on it.

**Files**: None specific — this is a release task.

**Test**: N/A.

**Implementation**:

1. Bump version in `Cargo.toml`.
2. Update `CHANGELOG.md` if present.
3. Tag + push: `git tag vX.Y.Z && git push --tags`.
4. (If published to crates.io: `cargo publish` in topological order.)

**Verification**: Tag visible on GitHub; `cargo update` from a
downstream consumer pulls the new version.

---

### Task 13/18 — carina-provider-awscc: migrate `carina-aws-types` helpers to new Custom shape [repo: carina-provider-awscc]

Same mechanical migration as Task 9, but in the awscc repo's local
copy of `carina-aws-types/src/lib.rs`.

**Verification**: `cargo test -p carina-aws-types` in awscc repo.

---

### Task 14/18 — carina-provider-awscc: add SSO / IdentityStore overrides to codegen [repo: carina-provider-awscc]

**Goal**: Properties currently emitted as `String(pattern)` that have
clear semantic meaning (per CFN docs) get routed to semantic helpers.

**Files**: `carina-provider-awscc/src/bin/codegen.rs`.

**Test**: In `codegen.rs` tests:

```rust
#[test]
fn sso_assignment_target_id_is_aws_account_id() {
    let overrides = resource_type_overrides();
    let override_ = overrides.get(&("AWS::SSO::Assignment", "TargetId"));
    assert!(matches!(
        override_,
        Some(TypeOverride::StringType(helper)) if helper.contains("aws_account_id")
    ));
}
```

**Implementation**: Extend the `resource_type_overrides()` table (near
line 2958 of `codegen.rs`) with at minimum:

```rust
m.insert(("AWS::SSO::Assignment", "TargetId"),
    TypeOverride::StringType("super::aws_account_id()"));
m.insert(("AWS::SSO::Assignment", "PrincipalId"),
    TypeOverride::StringType("super::sso_principal_id()"));
m.insert(("AWS::SSO::Assignment", "InstanceArn"),
    TypeOverride::StringType("super::sso_instance_arn()"));
m.insert(("AWS::SSO::PermissionSet", "InstanceArn"),
    TypeOverride::StringType("super::sso_instance_arn()"));
```

(`sso_principal_id` and `sso_instance_arn` are new helpers that may
need to be added to `carina-aws-types` in Task 13 — do that as part of
this task if necessary.)

**Verification**: `cargo test -p carina-provider-awscc bin`.

---

### Task 15/18 — carina-provider-awscc: update codegen emission templates [repo: carina-provider-awscc]

Mirror of Task 10 for awscc codegen.

**Files**: `carina-provider-awscc/src/bin/codegen.rs`.

**Implementation**: Update every `format!` template emitting
`AttributeType::Custom`. Same rules as Task 10. Key sites include
around line 3588 (`String(pattern)` emission) and similar blocks for
ranged ints/floats, enums, list items.

**Verification**: `cargo test -p carina-provider-awscc bin`.

---

### Task 16/18 — carina-provider-awscc: regenerate schemas and fix compile [repo: carina-provider-awscc]

Mirror of Task 11.

**Verification**: `cargo test -p carina-provider-awscc`.

---

### Task 17/18 — carina-provider-awscc: publish new tag [repo: carina-provider-awscc]

Mirror of Task 12.

---

### Task 18/18 — End-to-end verification on real infra [repo: carina]

**Goal**: Confirm `target_id = sso.identity_store_id` now surfaces as
a type error in `carina validate`.

**Files**: None to edit; this is a verification task.

**Test**: Manual. In `infra/aws/management/identity-center/`:

```crn
awscc.sso.Assignment {
  target_id   = sso.identity_store_id
  target_type = awscc.sso.Assignment.TargetType.AWS_ACCOUNT
  ...
}
```

**Expected output**:

```
Error: awscc.sso.Assignment: cannot assign IdentityStoreId to 'target_id':
expected AwsAccountId, got IdentityStoreId (from sso.identity_store_id)
```

Also verify the legitimate cases still pass:

- `caller.account_id → target_id` OK (same AwsAccountId).
- `orgs.accounts[child_account_id] → target_id` OK inside a for body
  (exercises Task 5-6 + the resource-walk unification).

**Implementation**: Manual verification. Close #2079 with evidence.

**Verification**: Acceptance criteria on #2079 satisfied.

---

## Dependencies

- Tasks 1-4 establish the new shape + compile. They must land in order
  in carina.
- Tasks 5-8 add the new semantics and migrate call sites. Must land
  after Tasks 1-4.
- Task 9 depends on Tasks 1-8 merged (carina-provider-aws imports
  carina-core).
- Task 10 depends on 9.
- Task 11 depends on 10.
- Task 12 depends on 11.
- Task 13 depends on 12 (awscc imports carina-aws-types via its own
  copy + carina-core from a released tag).
- Tasks 14-16 must be sequenced within awscc repo.
- Task 17 depends on 16.
- Task 18 depends on 17 (provider-awscc published) + infra repo
  updated.

Tasks 1-8 are carina-internal and cannot run in parallel (they share
`schema.rs`). Tasks 9-12 (aws) and 13-17 (awscc) can run in parallel
to each other once Task 8 is merged, but each is internally sequential.
