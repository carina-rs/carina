# Naming conventions unification — implementation plan

**Spec**: `docs/specs/2026-04-22-naming-conventions-design.md`
**Parent issue**: [#2143](https://github.com/carina-rs/carina/issues/2143)

## Scope

This plan covers **only carina (this repo)**. Phase A work in `carina-provider-aws` / `carina-provider-awscc` and Phase B in `carina-rs/infra` are tracked in those repos' own issues once this plan lands; Phase C closes the transition window in carina. The tasks below are in dependency order; each is one TDD cycle.

## Dependency map

```
A1 snake_to_pascal helper
 └─ A2 TypeExpr::Display primitives emit PascalCase
     └─ A3 TypeExpr::Display Simple/Custom emits PascalCase
         └─ A4 parser accepts new primitives (String/Int/Bool/Float) alongside old
             └─ A5 parser accepts PascalCase custom types via registry lookup
                 └─ A6 grammar accepts 3-segment resource paths (aws.ec2.Vpc)
                     └─ A7 TypeExpr::Ref Display emits 3-segment PascalCase
                         └─ A8 deprecation warning on old spellings
                             └─ A9 diagnostic error messages use new spellings
                                 └─ A10 LSP completion proposes new spellings
                                     └─ A11 LSP semantic tokens classify PascalCase as type
                                         └─ A12 TextMate grammars updated byte-identical
                                             └─ A13 carina-core fixtures migrated
                                                 └─ A14 CLAUDE.md / README / docs updated
                                                     └─ C1 Phase C: remove old-spelling support
```

## Tasks

---

### Task A1: Add `snake_to_pascal` helper with acronym rule

**Goal**: Provide the inverse of `pascal_to_snake` that turns `"aws_account_id"` into `"AwsAccountId"`. Treat acronyms as regular words (`Iam`, not `IAM`) to match existing `semantic_name` values in the codebase.

**Files**:
- Modify: `carina-core/src/parser/mod.rs` (add function next to existing `pascal_to_snake`)

**Test** (add to `parser::tests`):

```rust
#[test]
fn snake_to_pascal_conversion() {
    use super::snake_to_pascal;
    assert_eq!(snake_to_pascal("vpc_id"), "VpcId");
    assert_eq!(snake_to_pascal("aws_account_id"), "AwsAccountId");
    assert_eq!(snake_to_pascal("iam_policy_arn"), "IamPolicyArn");
    assert_eq!(snake_to_pascal("ipv4_cidr"), "Ipv4Cidr");
    assert_eq!(snake_to_pascal("arn"), "Arn");
    assert_eq!(snake_to_pascal("kms_key_arn"), "KmsKeyArn");
    // Round-trip with existing pascal_to_snake
    for name in ["vpc_id", "aws_account_id", "iam_policy_arn", "ipv4_cidr", "arn"] {
        assert_eq!(pascal_to_snake(&snake_to_pascal(name)), name);
    }
}
```

**Implementation**:

```rust
pub fn snake_to_pascal(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.extend(c.to_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}
```

**Verification**: `cargo test -p carina-core --lib snake_to_pascal`

---

### Task A2: `TypeExpr::Display` emits PascalCase for primitives

**Goal**: `TypeExpr::String.to_string()` returns `"String"` (was `"string"`). Same for `Int`, `Bool`, `Float`.

**Files**:
- Modify: `carina-core/src/parser/mod.rs` (Display impl for `TypeExpr`)

**Test** (add to `parser::tests`):

```rust
#[test]
fn type_expr_display_primitives_are_pascal_case() {
    assert_eq!(TypeExpr::String.to_string(), "String");
    assert_eq!(TypeExpr::Int.to_string(), "Int");
    assert_eq!(TypeExpr::Bool.to_string(), "Bool");
    assert_eq!(TypeExpr::Float.to_string(), "Float");
    assert_eq!(TypeExpr::List(Box::new(TypeExpr::Int)).to_string(), "list(Int)");
    assert_eq!(TypeExpr::Map(Box::new(TypeExpr::String)).to_string(), "map(String)");
}
```

**Implementation** (replace the 4 primitive arms in `Display`):

```rust
TypeExpr::String => write!(f, "String"),
TypeExpr::Bool => write!(f, "Bool"),
TypeExpr::Int => write!(f, "Int"),
TypeExpr::Float => write!(f, "Float"),
```

**Verification**: `cargo test -p carina-core --lib type_expr_display_primitives`

**Side-effect note**: This changes many existing snapshot tests. Expect `cargo insta` work in subsequent tasks (A13) — for this task, only assert the new strings; do not regenerate snapshots yet (that's task-local test, not full regen).

---

### Task A3: `TypeExpr::Display` emits PascalCase for Simple / custom types

**Goal**: `TypeExpr::Simple("aws_account_id").to_string()` returns `"AwsAccountId"`.

**Files**:
- Modify: `carina-core/src/parser/mod.rs` (Display arm for `Simple`)

**Test**:

```rust
#[test]
fn type_expr_display_simple_is_pascal_case() {
    assert_eq!(
        TypeExpr::Simple("aws_account_id".to_string()).to_string(),
        "AwsAccountId"
    );
    assert_eq!(
        TypeExpr::Simple("ipv4_cidr".to_string()).to_string(),
        "Ipv4Cidr"
    );
    assert_eq!(TypeExpr::Simple("arn".to_string()).to_string(), "Arn");
}
```

**Implementation**:

```rust
TypeExpr::Simple(name) => write!(f, "{}", snake_to_pascal(name)),
```

**Verification**: `cargo test -p carina-core --lib type_expr_display_simple_is_pascal_case`

---

### Task A4: parser accepts both `String`/`string` for primitives (transition window)

**Goal**: `arguments { x: String }` and `arguments { x: string }` both parse to `TypeExpr::String`. Phase C removes the lowercase form; during Phase A, both are accepted.

**Files**:
- Modify: `carina-core/src/parser/mod.rs` (`parse_type_expr`, `Rule::type_simple` arm)

**Test**:

```rust
#[test]
fn parse_accepts_pascal_case_primitives() {
    let input = r#"
        arguments {
            a: String
            b: Int
            c: Bool
            d: Float
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
    assert_eq!(result.arguments[1].type_expr, TypeExpr::Int);
    assert_eq!(result.arguments[2].type_expr, TypeExpr::Bool);
    assert_eq!(result.arguments[3].type_expr, TypeExpr::Float);
}

#[test]
fn parse_still_accepts_lowercase_primitives_during_transition() {
    let input = r#"arguments { a: string, b: int, c: bool, d: float }"#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
    assert_eq!(result.arguments[1].type_expr, TypeExpr::Int);
    assert_eq!(result.arguments[2].type_expr, TypeExpr::Bool);
    assert_eq!(result.arguments[3].type_expr, TypeExpr::Float);
}
```

**Implementation** (in `parse_type_expr`, `Rule::type_simple` arm — recognize either case):

```rust
Rule::type_simple => match inner.as_str() {
    "String" | "string" => Ok(TypeExpr::String),
    "Bool" | "bool" => Ok(TypeExpr::Bool),
    "Int" | "int" => Ok(TypeExpr::Int),
    "Float" | "float" => Ok(TypeExpr::Float),
    other => Ok(TypeExpr::Simple(other.to_string())),
},
```

**Verification**: `cargo test -p carina-core --lib parse_accepts_pascal_case_primitives parse_still_accepts_lowercase_primitives`

---

### Task A5: parser accepts PascalCase custom types

**Goal**: `arguments { id: AwsAccountId }` parses to `TypeExpr::Simple("aws_account_id")` (canonical internal representation is snake_case for now; the registry lookup normalizes input case).

**Files**:
- Modify: `carina-core/src/parser/mod.rs` (`parse_type_expr`, `Rule::type_simple` other branch)

**Test**:

```rust
#[test]
fn parse_accepts_pascal_case_custom_types() {
    let input = r#"
        arguments {
            id: AwsAccountId
            cidr: Ipv4Cidr
            bucket_arn: Arn
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.arguments[0].type_expr,
        TypeExpr::Simple("aws_account_id".to_string())
    );
    assert_eq!(
        result.arguments[1].type_expr,
        TypeExpr::Simple("ipv4_cidr".to_string())
    );
    assert_eq!(
        result.arguments[2].type_expr,
        TypeExpr::Simple("arn".to_string())
    );
}
```

**Implementation** (extend the `other` branch of the `type_simple` match):

```rust
other => {
    // During Phase A, accept both snake_case and PascalCase spellings.
    // Normalize to snake_case internally (canonical form in TypeExpr::Simple).
    let canonical = if other.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        pascal_to_snake(other)
    } else {
        other.to_string()
    };
    Ok(TypeExpr::Simple(canonical))
},
```

**Verification**: `cargo test -p carina-core --lib parse_accepts_pascal_case_custom_types`

---

### Task A6: Grammar accepts 3-segment resource paths (provider.service.TypeName)

**Goal**: `aws.ec2.Vpc`, `aws.s3.Bucket`, `aws.iam.Role` parse as `TypeExpr::Ref` with the full path. The existing grammar (`resource_type_path = identifier ~ ("." ~ identifier)+`) already accepts 3+ segments; verify the parser's PascalCase-final-segment detection does *not* misroute `aws.ec2.Vpc` to `SchemaType`.

**Files**:
- Modify: `carina-core/src/parser/mod.rs` (`parse_type_expr`, `Rule::type_ref` arm — refine the PascalCase detection so 3-segment paths are routed to `Ref` not `SchemaType`)

**Test**:

```rust
#[test]
fn parse_three_segment_resource_path_is_ref() {
    let input = r#"
        arguments {
            vpc: aws.ec2.Vpc
            bucket: aws.s3.Bucket
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    match &result.arguments[0].type_expr {
        TypeExpr::Ref(path) => {
            assert_eq!(path.provider, "aws");
            assert_eq!(path.resource_type, "ec2.Vpc");
        }
        other => panic!("expected Ref, got {other:?}"),
    }
}

#[test]
fn parse_four_segment_path_with_pascal_tail_is_schema_type() {
    // awscc.ec2.VpcId is a SchemaType (provider.service.TypeName where
    // TypeName looks like a schema value type, not a resource kind).
    // The rule today: 3+ segments AND PascalCase final => SchemaType.
    // After A6, we must distinguish Vpc (resource) from VpcId (schema type).
    let input = r#"
        arguments {
            vpc_id: awscc.ec2.VpcId
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert!(matches!(
        result.arguments[0].type_expr,
        TypeExpr::SchemaType { .. }
    ));
}
```

**Implementation design choice**: The existing heuristic (`3+ segments AND PascalCase final => SchemaType`) collides with `aws.ec2.Vpc` (which should be `Ref`, not `SchemaType`). New rule: the `SchemaType` classification is gated on whether the final segment exists in the provider's schema-type registry (`ProviderContext`). When unknown, default to `Ref`.

Concrete change in the `Rule::type_ref` arm (approx. `carina-core/src/parser/mod.rs:1498-1527`):

```rust
if parts.len() >= 3
    && parts.last().is_some_and(|s| s.starts_with(|c: char| c.is_uppercase()))
{
    let provider = parts[0].to_string();
    let path = parts[1..parts.len() - 1].join(".");
    let type_name = parts.last().unwrap().to_string();
    // Disambiguate: if the (provider, path, type_name) triple is a
    // registered schema type in ctx, treat as SchemaType; otherwise it's a
    // 3-segment resource kind (aws.ec2.Vpc).
    if ctx.config.is_schema_type(&provider, &path, &type_name) {
        Ok(TypeExpr::SchemaType { provider, path, type_name })
    } else {
        let path = ResourceTypePath::parse(path_str).ok_or_else(|| ...)?;
        Ok(TypeExpr::Ref(path))
    }
}
```

The `is_schema_type` predicate is added to `ProviderContext` (see existing `validators` field pattern). Providers that want PascalCase schema types register them explicitly.

**Verification**: `cargo test -p carina-core --lib parse_three_segment_resource_path parse_four_segment_path_with_pascal_tail`

---

### Task A7: `TypeExpr::Ref` Display emits 3-segment PascalCase form

**Goal**: `TypeExpr::Ref(ResourceTypePath::new("aws", "ec2.Vpc")).to_string()` returns `"aws.ec2.Vpc"` (unchanged — the path is stored as-parsed). Assert round-trip.

**Files**:
- Modify: none structurally; add a test asserting the existing Display behavior is preserved.

**Test**:

```rust
#[test]
fn type_expr_ref_display_roundtrips_three_segment_path() {
    let ty = TypeExpr::Ref(ResourceTypePath::new("aws", "ec2.Vpc"));
    assert_eq!(ty.to_string(), "aws.ec2.Vpc");

    // Round trip through the parser
    let input = format!(r#"arguments {{ v: {} }}"#, ty);
    let parsed = parse(&input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.arguments[0].type_expr, ty);
}
```

**Implementation**: no code change if the Display impl is already `write!(f, "{}", path)` — verify by running the test.

**Verification**: `cargo test -p carina-core --lib type_expr_ref_display_roundtrips_three_segment_path`

---

### Task A8: Deprecation warning on old spellings

**Goal**: When the parser encounters `string`, `int`, `bool`, `float`, or a snake_case custom type in a type position, emit a warning through `ParsedFile::warnings` with text pointing to the new spelling.

**Files**:
- Modify: `carina-core/src/parser/mod.rs` (`parse_type_expr`, both primitive and custom-type arms)
- Modify: `carina-core/src/parser/mod.rs` (`Warning` enum or equivalent — add `DeprecatedTypeSpelling { old: String, new: String, span: Span }`)

**Test**:

```rust
#[test]
fn parser_warns_on_lowercase_primitive() {
    let input = r#"arguments { a: string }"#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert!(
        result.warnings.iter().any(|w| matches!(
            w,
            Warning::DeprecatedTypeSpelling { old, new, .. }
                if old == "string" && new == "String"
        )),
        "expected deprecation warning, got {:?}",
        result.warnings
    );
}

#[test]
fn parser_warns_on_snake_case_custom_type() {
    let input = r#"arguments { a: aws_account_id }"#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert!(
        result.warnings.iter().any(|w| matches!(
            w,
            Warning::DeprecatedTypeSpelling { old, new, .. }
                if old == "aws_account_id" && new == "AwsAccountId"
        )),
        "expected deprecation warning"
    );
}

#[test]
fn parser_does_not_warn_on_new_spelling() {
    let input = r#"arguments { a: String, b: AwsAccountId }"#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert!(
        !result.warnings.iter().any(|w| matches!(w, Warning::DeprecatedTypeSpelling { .. })),
        "should not warn on new spellings, got {:?}",
        result.warnings
    );
}
```

**Implementation** (in the `type_simple` arm of `parse_type_expr`):

```rust
Rule::type_simple => {
    let text = inner.as_str();
    let span = inner.as_span();
    let (ty, deprecated_old) = match text {
        "String" => (TypeExpr::String, None),
        "string" => (TypeExpr::String, Some(("string", "String"))),
        "Int" => (TypeExpr::Int, None),
        "int" => (TypeExpr::Int, Some(("int", "Int"))),
        "Bool" => (TypeExpr::Bool, None),
        "bool" => (TypeExpr::Bool, Some(("bool", "Bool"))),
        "Float" => (TypeExpr::Float, None),
        "float" => (TypeExpr::Float, Some(("float", "Float"))),
        other if other.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => {
            (TypeExpr::Simple(pascal_to_snake(other)), None)
        }
        other => {
            let new = snake_to_pascal(other);
            (TypeExpr::Simple(other.to_string()), Some((other, Box::leak(new.into_boxed_str()))))
        }
    };
    if let Some((old, new)) = deprecated_old {
        ctx.push_warning(Warning::DeprecatedTypeSpelling {
            old: old.to_string(),
            new: new.to_string(),
            span: span.into(),
        });
    }
    Ok(ty)
}
```

(Adjust the warning pushing to match the parser's existing warning-collection API — `ParsedFile.warnings` in this codebase.)

**Verification**: `cargo test -p carina-core --lib parser_warns_on_lowercase_primitive parser_warns_on_snake_case_custom_type parser_does_not_warn_on_new_spelling`

---

### Task A9: Diagnostic error messages emit new spellings

**Goal**: `validate_type_expr_value` and the parser's user-function type-mismatch error format type names via `TypeExpr::Display`, so they inherit the new casing automatically. Verify this with tests against the message text.

**Files**:
- Modify: `carina-core/src/validation.rs` (tests only — the code path is already `format!("{}", type_expr)`)
- Modify: `carina-core/src/parser/mod.rs` (tests asserting function-arg mismatch error text uses new casing)

**Test**:

```rust
#[test]
fn validate_type_expr_value_error_uses_pascal_case() {
    let result = validate_type_expr_value(
        &TypeExpr::String,
        &Value::Int(42),
        &ProviderContext::default(),
    );
    let msg = result.expect("should error");
    assert!(
        msg.contains("expected String"),
        "expected new-casing error, got: {msg}"
    );
}

#[test]
fn validate_type_expr_struct_error_uses_pascal_case_in_field_type() {
    let mut map = HashMap::new();
    map.insert("count".to_string(), Value::String("x".into()));
    let fields = vec![("count".to_string(), TypeExpr::Int)];
    let result = validate_type_expr_value(
        &TypeExpr::Struct { fields },
        &Value::Map(map),
        &ProviderContext::default(),
    );
    let msg = result.expect("should error");
    assert!(
        msg.contains("field 'count'") && msg.contains("Int"),
        "expected field error with Int type, got: {msg}"
    );
}
```

**Implementation**: update any hardcoded lowercase type names in error strings in `validation.rs` to format via `type_expr.to_string()` or accept the new Display output. Grep for `"expected string"`, `"expected int"`, etc. in `carina-core/src` and replace with the Display form.

**Verification**: `cargo test -p carina-core --lib validate_type_expr_value_error_uses_pascal_case`

---

### Task A10: LSP completion proposes new spellings

**Goal**: LSP completion at a type-annotation site (`arguments { x: <HERE> }`) proposes `String`, `Int`, `Bool`, `Float`, and each registered custom type in PascalCase.

**Files**:
- Modify: `carina-lsp/src/completion/values.rs` (or wherever the type-completion candidate list lives)

**Test** (in `carina-lsp/src/completion/tests.rs` or the nearest equivalent):

```rust
#[test]
fn completion_at_type_position_proposes_pascal_case_primitives() {
    let source = "arguments { x: ";
    let items = completions_at(source, position_after(source));
    let labels: Vec<&str> = items.iter().map(|it| it.label.as_str()).collect();
    assert!(labels.contains(&"String"));
    assert!(labels.contains(&"Int"));
    assert!(labels.contains(&"Bool"));
    assert!(labels.contains(&"Float"));
    assert!(!labels.contains(&"string"));
}
```

**Implementation**: locate the completion list (currently emits `"string"`, `"int"`, `"bool"`, `"float"`, and registered custom types as lowercase). Emit PascalCase forms instead. For custom types registered in `ProviderContext.validators`, render each key via `snake_to_pascal`.

**Verification**: `cargo test -p carina-lsp completion_at_type_position_proposes_pascal_case_primitives`

---

### Task A11: LSP semantic tokens classify PascalCase identifiers as types

**Goal**: The LSP semantic-tokens pass tags a bare PascalCase identifier in a type-annotation position as `semanticTokenType::type`.

**Files**:
- Modify: `carina-lsp/src/semantic_tokens.rs`

**Test** (within the semantic-tokens test suite):

```rust
#[test]
fn semantic_tokens_tags_pascal_case_type_annotation() {
    let source = "arguments { x: AwsAccountId }";
    let tokens = tokenize_source(source);
    let ty_tok = tokens.iter().find(|t| t.text == "AwsAccountId")
        .expect("AwsAccountId should be tokenized");
    assert_eq!(ty_tok.kind, SemanticTokenKind::Type);
}
```

**Implementation**: extend the existing token classifier to recognize PascalCase identifiers in the right context. Current code already tags `SchemaType` paths as types; the extension is to tag any bare PascalCase identifier that appears after `:` in an `arguments`/`attributes`/`exports` block as type.

**Verification**: `cargo test -p carina-lsp semantic_tokens_tags_pascal_case_type_annotation`

---

### Task A12: TextMate grammars recognize PascalCase types (byte-identical)

**Goal**: Both `editors/vscode/syntaxes/carina.tmLanguage.json` and `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` have a pattern that colors PascalCase identifiers in type position as type names. The two files remain byte-identical.

**Files**:
- Modify: `editors/vscode/syntaxes/carina.tmLanguage.json`
- Modify: `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json`

**Test**: the existing parity test in `carina-core/tests/tmlanguage_keyword_parity.rs` must keep passing. Additionally, spot-check pattern:

```rust
#[test]
fn tmlanguage_has_pascal_case_type_pattern() {
    let vscode = std::fs::read_to_string(
        "../editors/vscode/syntaxes/carina.tmLanguage.json"
    ).unwrap();
    // Pattern should cover bare PascalCase identifiers with scope entity.name.type.carina
    assert!(
        vscode.contains("entity.name.type.carina"),
        "expected a type-entity pattern in vscode grammar"
    );
    assert!(
        vscode.contains(r"\\b[A-Z][A-Za-z0-9]*\\b")
            || vscode.contains(r"[A-Z][a-zA-Z0-9]*"),
        "expected a PascalCase regex in vscode grammar"
    );
}
```

**Implementation**: add a pattern to the `patterns` array in both tmLanguage files (identical JSON):

```json
{
  "name": "entity.name.type.carina",
  "match": "\\b[A-Z][A-Za-z0-9]*\\b"
}
```

Place it after the keyword patterns so keywords (none are PascalCase today) take priority.

**Verification**:
- `cargo test -p carina-core --test tmlanguage_keyword_parity`
- `cargo test -p carina-core tmlanguage_has_pascal_case_type_pattern`

---

### Task A13: Migrate carina-core fixtures and regenerate snapshots

**Goal**: Every `.crn` file under `carina-cli/tests/fixtures/` and `carina-core/tests/` uses new spellings. Regenerate all `insta` snapshots.

**Files**:
- Modify: every `.crn` under `carina-cli/tests/fixtures/plan_display/**`, `carina-cli/tests/fixtures/parse_golden/**`, `carina-core/tests/**/*.crn` (approximate count: 161)
- Regenerate: every `.snap` file matched by the changed fixtures

**Test**: none new — the existing fixture/snapshot tests become the regression surface.

**Implementation**:

1. For each fixture `.crn`: rewrite old spellings by hand (per the decision in 7-b-Z — manual rewrite only).
2. `cargo test --workspace` will fail many snapshot assertions.
3. `cargo insta review` → accept each updated snapshot after visual verification.

**Verification**:
- `cargo test --workspace` (should pass with zero snapshot pending)
- `cargo insta pending-snapshots` (should be empty after review)

Split into sub-tasks if fixture count is unmanageable:
- A13a: `plan_display/` fixtures
- A13b: `parse_golden/` fixtures
- A13c: remaining `.crn` in `tests/`

Each sub-task: rewrite its own directory, re-run `cargo test`, accept new snapshots.

---

### Task A14: Documentation, CLAUDE.md, READMEs, example blocks

**Goal**: Every DSL code block in markdown files uses new spellings. CLAUDE.md updates mentions of snake_case type names.

**Files**:
- Modify: `CLAUDE.md`, `README.md` (if present), `docs/**/*.md` (example DSL code blocks only)
- Modify: any `// example:` doc-comment in `src/**` that shows DSL code

**Test** — add a script in `scripts/` that greps for old spellings inside triple-backtick `crn` code blocks in `.md` files:

```bash
# scripts/check_docs_use_new_spellings.sh
set -e
for f in $(git ls-files '*.md'); do
    awk '/^```crn/,/^```$/' "$f" |
    grep -E ':\s*(string|int|bool|float|aws_account_id|ipv4_cidr|arn)\b' && {
        echo "OLD SPELLING in $f"; exit 1;
    }
done
echo "docs OK"
```

**Implementation**: manual rewrite of each markdown file.

**Verification**: `scripts/check_docs_use_new_spellings.sh` returns 0.

---

### Task C1: Phase C — remove old-spelling support

**Goal**: After providers (aws, awscc) and `infra` have migrated (tracked in their own repos), remove the transition-window support from carina-core: the parser rejects old spellings with an error (not a warning).

**Files**:
- Modify: `carina-core/src/parser/mod.rs` (remove the `"string" | ...` OR arms, and remove the snake_case-to-PascalCase normalization in the `other` arm)
- Modify: `carina-core/src/parser/mod.rs` (remove the `Warning::DeprecatedTypeSpelling` emission from task A8; the variant itself can stay or be removed per taste — prefer to remove since it has no remaining producer)

**Test**:

```rust
#[test]
fn parser_rejects_lowercase_primitive_after_phase_c() {
    let input = r#"arguments { a: string }"#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("unknown type 'string'") || msg.contains("expected String"),
        "expected rejection with hint, got: {msg}"
    );
}

#[test]
fn parser_rejects_snake_case_custom_type_after_phase_c() {
    let input = r#"arguments { a: aws_account_id }"#;
    // After C1, snake_case custom type names are treated as unknown types.
    // The diagnostic should point at the PascalCase form.
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err() || result.unwrap().warnings.iter().any(|w|
        matches!(w, Warning::UnknownType { name, .. } if name == "aws_account_id")
    ));
}
```

**Implementation**: delete the fallthrough arms added in A4 and A5, delete the deprecation-warning push from A8.

**Verification**:
- `cargo test --workspace` passes
- `cargo test -p carina-core parser_rejects_lowercase_primitive_after_phase_c parser_rejects_snake_case_custom_type_after_phase_c`
- All `.crn` files in carina repo already use new spellings (from A13); no regression

---

## Not in this plan (handled elsewhere)

- **Phase A in carina-provider-aws**: separate repo, separate PR. Depends on A7 landing first (so carina-core published on main knows how to parse 3-segment paths).
- **Phase A in carina-provider-awscc**: same.
- **Phase B in carina-rs/infra**: separate repo. Depends on both provider repos' Phase A PRs being merged.
- **Phase C providers**: once C1 lands here, aws/awscc must also remove any old-spelling references (mostly auto via codegen re-run).
- **Q5B namespaced custom types (`aws.AccountId`)**: separate issue, design not started.

## Order and parallelism

- A1 → A2 → A3 are sequential (A3 depends on A1).
- A4 and A5 both depend on A3 but are independent of each other.
- A6 is independent of A4/A5 but depends on A3.
- A7 can start as soon as A6 lands.
- A8 depends on A4 and A5.
- A9 depends on A3.
- A10 and A11 depend on A4/A5.
- A12 is independent — can land any time after A2.
- A13 requires A1–A12 all merged (snapshots regenerate against the final parser/display behavior).
- A14 can start alongside A13.
- C1 is the final task — waits for all A1–A14 in this repo and Phase A in providers and Phase B in infra.

## Verification commands

- Per-task: as listed.
- Full gate before merging each PR: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`.
- After A13: run `carina validate` on every `.crn` fixture via existing fixture tests.
- Before C1: run `carina validate` on a local checkout of `carina-rs/infra` to confirm Phase B landed.
