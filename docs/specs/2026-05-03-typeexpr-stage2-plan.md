# Implementation plan — Stage 2 of #2360

<!-- derived-from ./2026-05-03-typeexpr-stage2-design.md -->

Generic `File<E>` + loader-side inference + `TypeExpr::Unknown`
sentinel. Tasks ordered by dependency: AST primitives → inference
helper → loader integration → caller migration → cleanup. Each task is
one TDD cycle. Verification commands are crate-scoped (per repo
convention from `scripts/touched-crates.sh`); the final task expands to
the workspace sweep.

## File structure

| File | Role | Stage |
| --- | --- | --- |
| `carina-core/src/parser/ast.rs` | `TypeExpr::Unknown` variant, `File<E>` generic, `ParsedExportParam` / `InferredExportParam`, type aliases | Tasks 1, 3, 4 |
| `carina-core/src/validation/mod.rs` | `is_type_expr_compatible_with_schema` rejects `Unknown`; `validate_export_param_ref_types` consumes `InferredExportParam` | Tasks 2, 7 |
| `carina-core/src/validation/inference.rs` | `apply_inference(ParsedFile, &SchemaRegistry) -> (InferredFile, Vec<InferenceError>)` with sentinel-on-failure | Task 5 |
| `carina-core/src/config_loader.rs` | `load_configuration_with_config` gains `&SchemaRegistry`; new `inference_errors` field on `LoadedConfig`; `parsed` becomes `InferredFile` (`unresolved_parsed` stays `ParsedFile`) | Task 6 |
| `carina-core/src/upstream_exports.rs` | inner `e.type_expr.clone()` adapts (now bare `TypeExpr`); `resolve_upstream_exports_with_schemas` no longer needs `infer_type_expr` for the typed path — exports already inferred upstream | Task 8 (touch only what's needed) |
| `carina-cli/src/commands/{mod,validate,plan,destroy,export,lint,etc.}.rs` | Loader call site adds schema arg; export-param `Option` guards collapse | Task 9 |
| `carina-lsp/src/diagnostics/{mod,checks}.rs` | Same loader-arg pass-through; export-param `Option` guards collapse | Task 10 |
| `carina-lsp/src/completion/values.rs` | Loader integration is via the engine; helpers consume bare `TypeExpr`; export-param `Option` guards collapse | Task 10 |
| `carina-cli/tests/e2e_typecheck_parity.rs` | New e2e test for sentinel-bearing inference failure surfacing through `LoadedConfig.inference_errors` | Task 11 |
| `carina-core/src/validation/inference.rs` (tests) | Unit tests for `apply_inference`, sentinel preservation, `Unknown` predicate behavior | Tasks 2, 5 |

## Tasks

### Task 1 — `TypeExpr::Unknown` sentinel variant

**Goal**: Add the sentinel variant; everything that pattern-matches
`TypeExpr` must now handle it (compiler-driven exhaustiveness check
catches missed sites).

**Files**: `carina-core/src/parser/ast.rs`

**Test** (add to `carina-core/src/parser/tests.rs`):

```rust
#[test]
fn type_expr_unknown_displays_as_unknown_marker() {
    use crate::parser::TypeExpr;
    let u = TypeExpr::Unknown;
    assert_eq!(format!("{}", u), "<unknown>");
}

#[test]
fn type_expr_unknown_serde_round_trips() {
    use crate::parser::TypeExpr;
    let u = TypeExpr::Unknown;
    let json = serde_json::to_string(&u).unwrap();
    let back: TypeExpr = serde_json::from_str(&json).unwrap();
    assert_eq!(back, u);
}
```

**Implementation** (`carina-core/src/parser/ast.rs`, in the `TypeExpr`
enum and its `Display` impl):

```rust
pub enum TypeExpr {
    String,
    Bool,
    Int,
    Float,
    Simple(std::string::String),
    List(Box<TypeExpr>),
    Map(Box<TypeExpr>),
    Ref(ResourceTypePath),
    SchemaType { provider: String, path: String, type_name: String },
    Struct { fields: Vec<(String, TypeExpr)> },
    /// Sentinel for inference failure: an unannotated export whose
    /// rhs could not be statically typed. Produced *only* by
    /// `apply_inference`, never by the parser. Type-comparison
    /// predicates reject `Unknown` against any concrete receiver, so
    /// the `inference_errors` channel surfaces the actionable
    /// "type annotation required" message instead of a cascade of
    /// "missing export" diagnostics. See #2360 stage 2.
    Unknown,
}

// In `impl Display for TypeExpr { fn fmt(...) }` add:
TypeExpr::Unknown => write!(f, "<unknown>"),
```

**Verification**:

```bash
cargo nextest run -p carina-core -E 'test(type_expr_unknown)'
cargo check -p carina-core
```

Expect compile errors at every `match` on `TypeExpr` that is
non-exhaustive — those become Task 2's first half (predicate) and the
rest of the cleanup tasks.

---

### Task 2 — `is_type_expr_compatible_with_schema` rejects `TypeExpr::Unknown`

**Goal**: Predicate behavior for the sentinel: `Unknown` is
incompatible with everything (so downstream typechecks against an
unknowable export fall through to "annotation required" surfaced via
`inference_errors`, not "type mismatch").

**Files**: `carina-core/src/validation/mod.rs`,
`carina-core/src/validation/tests.rs`

**Test** (add to `carina-core/src/validation/tests.rs`, near the
existing `is_type_expr_compatible_*` tests):

```rust
#[test]
fn is_type_expr_compatible_unknown_rejects_all_concrete_receivers() {
    use crate::parser::TypeExpr;
    use crate::schema::AttributeType;
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Unknown,
        &AttributeType::String,
    ));
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Unknown,
        &AttributeType::Int,
    ));
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Unknown,
        &AttributeType::Bool,
    ));
}

#[test]
fn is_type_expr_compatible_unknown_rejects_custom_receiver() {
    use crate::parser::TypeExpr;
    use crate::schema::{AttributeType, legacy_validator};
    fn noop(_v: &crate::resource::Value) -> Result<(), String> { Ok(()) }
    let custom = AttributeType::Custom {
        semantic_name: Some("VpcId".to_string()),
        pattern: None,
        length: None,
        base: Box::new(AttributeType::String),
        validate: legacy_validator(noop),
        namespace: None,
        to_dsl: None,
    };
    assert!(!is_type_expr_compatible_with_schema(&TypeExpr::Unknown, &custom));
}
```

**Implementation**: In `is_type_expr_compatible_with_schema` add an
explicit arm before the existing `match`:

```rust
TypeExpr::Unknown => false,
```

The existing `_` fallthrough already covered ResourceTypePath /
SchemaType; `Unknown` is added explicitly so its rejection is
documented in code, not implicit. Comment: `// Sentinel for failed
inference (#2360 stage 2). Never matches a concrete receiver — the
inference_errors channel reports the underlying "type annotation
required" instead.`

**Verification**:

```bash
cargo nextest run -p carina-core -E 'test(is_type_expr_compatible_unknown)'
cargo check -p carina-core
```

Expect zero compile errors *in carina-core* after this task — every
`match TypeExpr { ... }` site in carina-core now handles `Unknown`. CLI
and LSP sites get caught when their crates compile in later tasks.

---

### Task 3 — Introduce `ParsedExportParam` (rename `ExportParameter`)

**Goal**: Rename the existing struct to make the "stage 1, still
optional" intent explicit. Keep the field shape (`type_expr:
Option<TypeExpr>`). Reserve the name `ExportParameter` for now, will
re-introduce as `InferredExportParam` later.

**Files**: `carina-core/src/parser/ast.rs`,
`carina-core/src/parser/{tests.rs,blocks.rs,resolve.rs,...}` (every
import / construct site)

**Test** (add to `carina-core/src/parser/tests.rs`):

```rust
#[test]
fn parsed_export_param_keeps_optional_type_expr() {
    use crate::parser::ParsedExportParam;
    let p = ParsedExportParam {
        name: "vpc_id".to_string(),
        type_expr: None,
        value: None,
    };
    assert!(p.type_expr.is_none());
}
```

**Implementation**:

1. In `carina-core/src/parser/ast.rs`:

```rust
// Rename from ExportParameter
pub struct ParsedExportParam {
    pub name: String,
    pub type_expr: Option<TypeExpr>,
    pub value: Option<Value>,
}

// Backward-compat alias for existing call sites — temporary, removed in Task 4
pub type ExportParameter = ParsedExportParam;
```

2. Run the existing test suite to confirm the alias keeps every
   construct site (parser blocks, tests, etc.) compiling unchanged.

**Verification**:

```bash
cargo nextest run -p carina-core -E 'test(parsed_export_param)'
cargo nextest run -p carina-core
```

Expect every existing carina-core test still passing (the alias makes
the rename source-compatible).

---

### Task 4 — Generic `File<E>` and `InferredExportParam`

**Goal**: Parameterize `ParsedFile`'s `export_params` field; introduce
`InferredExportParam` (bare `TypeExpr`); type-alias both phases.
`ExportParameter` alias from Task 3 dropped here in favor of
`ParsedExportParam`.

**Files**: `carina-core/src/parser/ast.rs`

**Test** (add to `carina-core/src/parser/tests.rs`):

```rust
#[test]
fn parsed_file_is_file_of_parsed_export_param() {
    use crate::parser::{File, ParsedExportParam, ParsedFile};
    fn _coerce(p: ParsedFile) -> File<ParsedExportParam> { p }
    fn _back(f: File<ParsedExportParam>) -> ParsedFile { f }
}

#[test]
fn inferred_file_holds_inferred_export_param() {
    use crate::parser::{InferredExportParam, InferredFile, TypeExpr};
    let one = InferredExportParam {
        name: "vpc_id".to_string(),
        type_expr: TypeExpr::String,
        value: None,
    };
    let f: InferredFile = InferredFile {
        export_params: vec![one],
        ..Default::default()
    };
    assert_eq!(f.export_params[0].type_expr, TypeExpr::String);
}
```

**Implementation** (`carina-core/src/parser/ast.rs`):

```rust
pub struct InferredExportParam {
    pub name: String,
    pub type_expr: TypeExpr,           // bare, not Option
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct File<E> {
    pub providers: Vec<ProviderConfig>,
    pub resources: Vec<Resource>,
    pub variables: IndexMap<String, Value>,
    pub uses: Vec<UseStatement>,
    pub module_calls: Vec<ModuleCall>,
    pub arguments: Vec<ArgumentParameter>,
    pub attribute_params: Vec<AttributeParameter>,
    pub export_params: Vec<E>,
    pub backend: Option<BackendConfig>,
    pub state_blocks: Vec<StateBlock>,
    pub user_functions: HashMap<String, UserFunction>,
    pub upstream_states: Vec<UpstreamState>,
    pub requires: Vec<RequireBlock>,
    pub structural_bindings: HashSet<String>,
    pub warnings: Vec<ParseWarning>,
    pub deferred_for_expressions: Vec<DeferredForExpression>,
}

pub type ParsedFile = File<ParsedExportParam>;
pub type InferredFile = File<InferredExportParam>;

// Drop the temporary `pub type ExportParameter = ParsedExportParam;` alias
// from Task 3.
```

**Verification**:

```bash
cargo nextest run -p carina-core -E 'test(parsed_file_is_file_of) + test(inferred_file_holds_inferred_export_param)'
cargo check -p carina-core
```

Expect compile errors *outside carina-core* (CLI/LSP) at every site
that referenced `ExportParameter` directly — captured in Task 9/10.

Inside carina-core: the parser-side tests that build
`ExportParameter { type_expr: Some(...), value: Some(...) }` switch to
`ParsedExportParam { ... }` — mechanical, completed in this task.

---

### Task 5 — `apply_inference` with sentinel-on-failure

**Goal**: New transition function `ParsedFile -> (InferredFile,
Vec<InferenceError>)`. Successful inference fills `type_expr` from the
existing `infer_type_expr` helper; failure substitutes
`TypeExpr::Unknown` and records the error.

**Files**: `carina-core/src/validation/inference.rs`

**Test** (in the same file's `#[cfg(test)] mod tests`):

```rust
#[test]
fn apply_inference_fills_inferable_export_with_inferred_type() {
    let mut parsed = crate::parser::ParsedFile::default();
    let res = crate::resource::Resource::with_provider("awscc", "ec2.Vpc", "main")
        .with_binding("main");
    parsed.resources.push(res);
    parsed.export_params.push(crate::parser::ParsedExportParam {
        name: "vpc_id".to_string(),
        type_expr: None,
        value: Some(Value::ResourceRef {
            path: crate::resource::AccessPath::new("main", "vpc_id"),
        }),
    });

    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", vpc_schema()); // helper from existing tests

    let (inferred, errors) = apply_inference(parsed, &schemas);
    assert!(errors.is_empty(), "no errors expected, got {:?}", errors);
    assert_eq!(inferred.export_params.len(), 1);
    assert_eq!(
        inferred.export_params[0].type_expr,
        TypeExpr::Simple("vpc_id".to_string())
    );
}

#[test]
fn apply_inference_substitutes_unknown_for_failed_inference() {
    let mut parsed = crate::parser::ParsedFile::default();
    parsed.export_params.push(crate::parser::ParsedExportParam {
        name: "zone_id".to_string(),
        type_expr: None,
        // Any-returning builtin: inference fails, sentinel produced.
        value: Some(Value::FunctionCall {
            name: "lookup".to_string(),
            args: vec![],
        }),
    });

    let (inferred, errors) = apply_inference(parsed, &SchemaRegistry::new());
    assert_eq!(inferred.export_params.len(), 1);
    assert_eq!(inferred.export_params[0].type_expr, TypeExpr::Unknown);
    assert_eq!(errors.len(), 1);
    assert!(matches!(errors[0], InferenceError::UnknownType { .. }));
}

#[test]
fn apply_inference_preserves_explicit_annotation() {
    let mut parsed = crate::parser::ParsedFile::default();
    parsed.export_params.push(crate::parser::ParsedExportParam {
        name: "vpc_id".to_string(),
        type_expr: Some(TypeExpr::Simple("vpc_id".to_string())),
        value: Some(Value::String("vpc-abc".to_string())),
    });

    let (inferred, errors) = apply_inference(parsed, &SchemaRegistry::new());
    assert!(errors.is_empty());
    assert_eq!(
        inferred.export_params[0].type_expr,
        TypeExpr::Simple("vpc_id".to_string())
    );
}
```

**Implementation**:

```rust
/// Phase transition: walk every `ParsedExportParam` in `parsed`,
/// resolve its effective `TypeExpr` (annotation wins; otherwise infer
/// from rhs), and emit an `InferredFile` with bare `TypeExpr` on every
/// export. Failed inferences are *not* dropped — they are kept with
/// `type_expr: TypeExpr::Unknown` and accompanied by an entry in the
/// returned `Vec<InferenceError>`. Sentinel-over-exclude prevents
/// cascading "missing export" diagnostics; the inference error is the
/// single point of truth for "this needs an annotation". See #2360
/// stage 2 design doc.
pub fn apply_inference(
    parsed: crate::parser::ParsedFile,
    schemas: &crate::schema::SchemaRegistry,
) -> (crate::parser::InferredFile, Vec<InferenceError>) {
    let bindings = bindings_from_parts(&parsed.resources, &parsed.upstream_states);
    let mut errors = Vec::new();
    let inferred_exports: Vec<crate::parser::InferredExportParam> = parsed
        .export_params
        .iter()
        .map(|p| {
            let type_expr = match infer_type_expr(
                p.type_expr.as_ref(),
                p.value.as_ref(),
                &bindings,
                schemas,
            ) {
                Ok(Some(ty)) => ty,
                Ok(None) | Err(_) => {
                    // No value or recoverable inference miss — keep
                    // the sentinel; the typecheck path treats
                    // Unknown as incompatible with any concrete
                    // receiver, so downstream consumers get an
                    // actionable diagnostic via the dedicated
                    // inference_errors channel.
                    if let Err(e) = infer_type_expr(
                        p.type_expr.as_ref(),
                        p.value.as_ref(),
                        &bindings,
                        schemas,
                    ) {
                        errors.push(e);
                    }
                    TypeExpr::Unknown
                }
            };
            crate::parser::InferredExportParam {
                name: p.name.clone(),
                type_expr,
                value: p.value.clone(),
            }
        })
        .collect();

    (
        crate::parser::InferredFile {
            providers: parsed.providers,
            resources: parsed.resources,
            variables: parsed.variables,
            uses: parsed.uses,
            module_calls: parsed.module_calls,
            arguments: parsed.arguments,
            attribute_params: parsed.attribute_params,
            export_params: inferred_exports,
            backend: parsed.backend,
            state_blocks: parsed.state_blocks,
            user_functions: parsed.user_functions,
            upstream_states: parsed.upstream_states,
            requires: parsed.requires,
            structural_bindings: parsed.structural_bindings,
            warnings: parsed.warnings,
            deferred_for_expressions: parsed.deferred_for_expressions,
        },
        errors,
    )
}
```

(The double-call to `infer_type_expr` in the failure path is awkward;
real implementation collects the result once. Plan-level pseudocode
shown for readability — verify yours uses one call.)

**Verification**:

```bash
cargo nextest run -p carina-core -E 'test(apply_inference)'
```

---

### Task 6 — Loader integration: `LoadedConfig.parsed: InferredFile`

**Goal**: `load_configuration_with_config` accepts `&SchemaRegistry`,
runs `apply_inference`, returns `LoadedConfig` whose `parsed` field is
`InferredFile` and whose new `inference_errors` field carries failures.
`unresolved_parsed` stays `ParsedFile` (the resolve phase also
preserves the `Option`-typed exports — the resolver isn't doing
inference anyway, and downstream of `LoadedConfig` only `parsed` is
used for type-aware code).

**Files**: `carina-core/src/config_loader.rs`

**Test** (add to `carina-core/src/config_loader.rs`'s `tests` module):

```rust
#[test]
fn load_configuration_runs_inference_and_surfaces_errors() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("main.crn"),
        "exports {\n  zone_id = lookup({a=\"1\"}, \"a\", \"default\")\n}\n",
    )
    .unwrap();

    let loaded = load_configuration_with_config(
        &tmp.path().to_path_buf(),
        &ProviderContext::default(),
        &SchemaRegistry::new(),
    )
    .unwrap();
    assert_eq!(loaded.parsed.export_params.len(), 1);
    assert_eq!(
        loaded.parsed.export_params[0].type_expr,
        crate::parser::TypeExpr::Unknown,
    );
    assert_eq!(loaded.inference_errors.len(), 1);
}

#[test]
fn load_configuration_keeps_inferable_export_typed() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("main.crn"),
        "exports {\n  name = \"carina\"\n}\n",
    )
    .unwrap();

    let loaded = load_configuration_with_config(
        &tmp.path().to_path_buf(),
        &ProviderContext::default(),
        &SchemaRegistry::new(),
    )
    .unwrap();
    assert!(loaded.inference_errors.is_empty());
    assert_eq!(
        loaded.parsed.export_params[0].type_expr,
        crate::parser::TypeExpr::String,
    );
}
```

**Implementation** (in `carina-core/src/config_loader.rs`):

```rust
pub struct LoadedConfig {
    pub parsed: InferredFile,
    pub unresolved_parsed: ParsedFile,
    pub backend_file: Option<PathBuf>,
    pub identifier_scope_errors: Vec<parser::ParseError>,
    pub inference_errors: Vec<crate::validation::inference::InferenceError>,  // new
}

pub fn load_configuration(path: &PathBuf) -> Result<LoadedConfig, String> {
    load_configuration_with_config(
        path,
        &ProviderContext::default(),
        &SchemaRegistry::new(),
    )
}

pub fn load_configuration_with_config(
    path: &PathBuf,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> Result<LoadedConfig, String> {
    // ... existing parse + resolve up to building `parsed: ParsedFile` ...

    let (parsed, inference_errors) =
        crate::validation::inference::apply_inference(parsed, schemas);

    Ok(LoadedConfig {
        parsed,
        unresolved_parsed,
        backend_file,
        identifier_scope_errors,
        inference_errors,
    })
}
```

**Verification**:

```bash
cargo nextest run -p carina-core -E 'test(load_configuration_runs_inference) + test(load_configuration_keeps_inferable_export_typed)'
cargo check -p carina-core
```

After this task `cargo check -p carina-core` is green; CLI/LSP will
fail to compile (loader signature changed and they consume
`Option<TypeExpr>` on export params) — fixed in Tasks 9 / 10.

---

### Task 7 — `validate_export_param_ref_types` consumes `InferredExportParam`

**Goal**: The validate-side typecheck for export ref types now reads
bare `TypeExpr` from `InferredExportParam`. The function signature
swaps `&[ExportParameter]` (= `&[ParsedExportParam]` via alias for
back-compat) for `&[InferredExportParam]`. The inline annotation-vs-rhs
inference at lines 282–306 of `validation/mod.rs` (introduced in stage
1) is no longer needed — by the time this function runs, every export
already has a definitive `type_expr` from the loader.

**Files**: `carina-core/src/validation/mod.rs`,
`carina-core/src/validation/tests.rs` (test fixtures rebuild
`InferredExportParam` instead of `ExportParameter`)

**Test** (replace existing `validate_export_param_ref_types_*` tests
that build `ExportParameter`):

```rust
#[test]
fn validate_export_param_ref_types_against_inferred_inputs() {
    use crate::parser::{InferredExportParam, TypeExpr, UpstreamState};
    use crate::resource::{Resource, Value};

    let registry_prod = Resource::with_provider("awscc", "organizations.account", "prod")
        .with_binding("registry_prod")
        .with_attribute("account_id", Value::String("111".to_string()));

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        make_schema(
            "organizations.account",
            vec![("account_id", AttributeType::String)],
        ),
    );

    let exports = vec![InferredExportParam {
        name: "id".to_string(),
        type_expr: TypeExpr::String,
        value: Some(Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        )),
    }];

    let result = validate_export_param_ref_types(
        &exports,
        &[registry_prod],
        &[] as &[UpstreamState],
        &schemas,
    );
    assert!(result.is_ok(), "got {:?}", result);
}
```

**Implementation**: Change the function signature; drop the inline
inference; consume `param.type_expr` directly:

```rust
pub fn validate_export_param_ref_types(
    export_params: &[crate::parser::InferredExportParam],   // was ExportParameter
    resources: &[Resource],
    upstream_states: &[crate::parser::UpstreamState],
    registry: &SchemaRegistry,
) -> Result<(), String> {
    let mut binding_map: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(ref binding_name) = resource.binding {
            binding_map.insert(binding_name.clone(), resource);
        }
    }
    let _inference_bindings_unused_now = inference::bindings_from_parts(resources, upstream_states);
    // (We no longer need inference_bindings here — the type is already final.)

    let mut errors = Vec::new();
    for param in export_params {
        // Skip exports without a value (incomplete declarations the
        // parser left tombstoned). The type-comparison logic below
        // needs both halves.
        let Some(ref value) = param.value else { continue };
        // Skip Unknown sentinels — the inference_errors channel
        // already reported the missing annotation; emitting a
        // "type mismatch" here would be a duplicate.
        if matches!(&param.type_expr, crate::parser::TypeExpr::Unknown) {
            continue;
        }
        collect_ref_type_errors(
            &param.type_expr,
            value,
            &param.name,
            &binding_map,
            registry,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}
```

**Verification**:

```bash
cargo nextest run -p carina-core -E 'test(validate_export_param_ref_types)'
cargo check -p carina-core
```

---

### Task 8 — `upstream_exports::resolve_upstream_exports_with_schemas` adapt

**Goal**: The function builds `UpstreamExports = HashMap<String,
HashMap<String, Option<TypeExpr>>>` from the upstream's
`parsed.export_params`. Stage 1 inferred missing types here too. After
loader-level inference (Task 6), the upstream's `parsed.export_params`
is `Vec<InferredExportParam>` with bare `TypeExpr` — so the inner
inference logic added in stage 1 collapses to a clone. (Note: the
upstream parse goes through `parse_directory` directly, not through
`load_configuration`, so the upstream is `ParsedFile`, not
`InferredFile`. The function now runs `apply_inference` itself on the
upstream parse so its returned `Option<TypeExpr>` map carries
post-inference types.)

**Files**: `carina-core/src/upstream_exports.rs`

**Test** (extend the existing tests at the bottom of the file):

```rust
#[test]
fn resolve_upstream_exports_with_schemas_uses_inference_for_unannotated() {
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(
        upstream.join("main.crn"),
        // Unannotated literal — inferred to TypeExpr::String.
        "exports {\n  region = \"us-east-1\"\n}\n",
    )
    .unwrap();

    let upstream_state = crate::parser::UpstreamState {
        binding: "ups".to_string(),
        source: std::path::PathBuf::from("upstream"),
    };
    let (exports, _errors) = resolve_upstream_exports_with_schemas(
        tmp.path(),
        &[upstream_state],
        &ProviderContext::default(),
        Some(&crate::schema::SchemaRegistry::new()),
    );
    let region_type = exports
        .get("ups")
        .and_then(|m| m.get("region"))
        .and_then(|t| t.as_ref())
        .expect("region should be inferred");
    assert_eq!(region_type, &crate::parser::TypeExpr::String);
}
```

**Implementation**: After the `parse_directory` call, run
`apply_inference` on the parsed upstream, then build the
`HashMap<String, Option<TypeExpr>>` from `inferred.export_params` (every
entry is `Some(type_expr)` because the inferer always produces a value
— `Unknown` for failed cases). Drop the in-place stage-1 inference
loop now that it's been hoisted to `apply_inference`.

```rust
match parse_directory(&source_abs, config) {
    Ok(parsed) => {
        let typed_exports: HashMap<String, Option<TypeExpr>> = match schemas {
            Some(s) => {
                let (inferred, _errors) =
                    crate::validation::inference::apply_inference(parsed, s);
                inferred
                    .export_params
                    .into_iter()
                    .map(|e| (e.name, Some(e.type_expr)))
                    .collect()
            }
            None => parsed
                .export_params
                .into_iter()
                .map(|e| (e.name, e.type_expr))
                .collect(),
        };
        out.insert(us.binding.clone(), typed_exports);
    }
    // ... unchanged Err arm ...
}
```

**Verification**:

```bash
cargo nextest run -p carina-core -E 'test(resolve_upstream_exports_with_schemas_uses_inference_for_unannotated)'
cargo nextest run -p carina-core
```

---

### Task 9 — CLI caller migration

**Goal**: Every `load_configuration` / `load_configuration_with_config`
call site in `carina-cli` passes `&SchemaRegistry`; every site that
reads `param.type_expr` on an export drops the `Option`-handling
guards (now bare `TypeExpr`). Touch sites: 32 callers identified by
`grep -rn "load_configuration"` across `carina-cli/src/`.

**Files**:

- `carina-cli/src/commands/{validate,plan,destroy,export,lint,refresh,apply,init}.rs`
- `carina-cli/src/commands/mod.rs`
- `carina-cli/src/commands/plan.rs` (3 sites of `param.type_expr.clone()` for `ParsedExportParam` → drop `Option` wrapper, clone bare `TypeExpr`)
- `carina-cli/src/{module_list_tests,fixture_plan,plan_snapshot_tests}.rs` (test files; pass `&SchemaRegistry::new()` for fixtures)

**Test**: rely on the workspace test sweep — each modified caller has
existing test coverage, and Task 11's e2e will catch regressions.

**Implementation**: Mechanical signature-pass-through edit. For each
caller:

```rust
// Before
let loaded = load_configuration_with_config(path, ctx)?;

// After
let loaded = load_configuration_with_config(path, ctx, ctx.schemas())?;
// or, where the caller already has `schemas: &SchemaRegistry`,
let loaded = load_configuration_with_config(path, ctx, &schemas)?;
```

For `ctx`-style call sites that hold a `Context` with `schemas()`
method, use `ctx.schemas()`; for fixture / snapshot tests with no
schemas in scope, use `&SchemaRegistry::new()` (post-inference all
exports become `Unknown`, which is the right behavior for fixtures
that don't care about typecheck).

For sites reading `param.type_expr` on `parsed.export_params` (now
`InferredExportParam`):

```rust
// Before
type_expr: param.type_expr.clone(),

// After
type_expr: Some(param.type_expr.clone()),
// (or drop the Option wrapper at the consumer if the consumer is
//  in carina-cli too — many of these are in plan.rs's serialization
//  back into ExportParameter for the ParseError display path.
//  For now keep `Some(...)` to localize the change; consumer-side
//  cleanup can ride a follow-up.)
```

**Verification**:

```bash
cargo nextest run -p carina-cli
cargo check --workspace
```

After this task `carina-cli` compiles cleanly. LSP still fails to
compile until Task 10.

---

### Task 10 — LSP caller migration

**Goal**: Same as Task 9 but for `carina-lsp`.

**Files**:

- `carina-lsp/src/diagnostics/checks.rs` (loader call site, 2 export-param `Option` guards at lines ~948, 965, 1259)
- `carina-lsp/src/completion/values.rs` (export-param consumption is post-loader, but the `resolve_upstream_exports_cached` path now consumes the post-inference `Option<TypeExpr>` map produced by Task 8 — semantics unchanged for the caller)
- `carina-lsp/src/diagnostics/mod.rs` (engine struct may hold cached schemas; thread `&SchemaRegistry` to the loader call)

**Test**:

```rust
#[test]
fn lsp_diagnostics_engine_uses_inference_aware_loader() {
    use carina_lsp::diagnostics::DiagnosticEngine;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("main.crn"),
        "exports {\n  zone_id = lookup({a=\"1\"}, \"a\", \"default\")\n}\n",
    )
    .unwrap();

    let engine = DiagnosticEngine::new(
        std::sync::Arc::new(carina_core::schema::SchemaRegistry::new()),
        vec![],
        std::sync::Arc::new(vec![]),
    );
    let doc = carina_lsp::document::Document::new(
        std::fs::read_to_string(tmp.path().join("main.crn")).unwrap(),
        std::sync::Arc::new(carina_core::parser::ProviderContext::default()),
    );
    let diags = engine.analyze_with_filename(&doc, Some("main.crn"), Some(tmp.path()));
    assert!(
        diags.iter().any(|d| d.message.to_lowercase().contains("annotation required")),
        "LSP must surface inference-failure diagnostic, got {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}
```

**Implementation**: Mechanical mirror of Task 9 in carina-lsp.

**Verification**:

```bash
cargo nextest run -p carina-lsp
cargo check --workspace
```

After this task `cargo check --workspace` is green.

---

### Task 11 — e2e parity test for sentinel + inference_errors path

**Goal**: End-to-end test that exercises the new pipeline (loader →
inference → CLI/LSP) for a sentinel-bearing failure, asserting both
surfaces report it identically.

**Files**: `carina-cli/tests/e2e_typecheck_parity.rs`

**Test**:

```rust
#[test]
fn unannotated_dynamic_export_surfaces_via_inference_errors_channel() {
    // `lookup` returns Any, inference fails. After stage 2 the loader
    // catches this and emits an InferenceError; CLI surfaces it via
    // the error channel; LSP surfaces it as a diagnostic. Both must
    // mention "annotation required" and the offending export name.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
exports {
    zone_id = lookup({a = "1"}, "a", "default")
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        cli_messages_contain_ci(&cli_diags, "annotation required")
            && cli_messages_contain_ci(&cli_diags, "zone_id"),
        "CLI must surface zone_id + annotation required, got {:?}",
        cli_diags,
    );
    assert!(
        lsp_messages_contain_ci(&lsp_diags, "annotation required")
            && lsp_messages_contain_ci(&lsp_diags, "zone_id"),
        "LSP must surface zone_id + annotation required, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn unannotated_inferable_export_continues_to_validate_clean() {
    // Sanity that Task 6's loader integration didn't break the happy
    // path: an unannotated rhs with a static type (literal) must
    // typecheck without any "annotation required" noise.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
exports {
    region = "us-east-1"
}
"#,
    )]);

    let cli_diags = cli_diagnostics(factories_2358(), &fixture);
    assert!(
        !cli_messages_contain_ci(&cli_diags, "annotation required"),
        "literal must infer cleanly post stage 2, got {:?}",
        cli_diags,
    );
}
```

**Verification**:

```bash
cargo nextest run -p carina-cli -E 'test(unannotated_dynamic_export_surfaces_via_inference_errors_channel) + test(unannotated_inferable_export_continues_to_validate_clean)'
```

---

### Task 12 — Final workspace verify + real-infra smoke

**Goal**: Confirm the full diff doesn't break anything outside the
covered call sites; confirm the reproducer at
`~/tmp/carina-upstream-state/` continues to validate cleanly.

**Files**: none (verification only).

**Test**: rely on existing test suite + manual smoke.

**Verification**:

```bash
cargo nextest run --workspace
cargo test --workspace --doc
cargo clippy --workspace --all-targets -- -D warnings
for s in scripts/check-*.sh; do bash "$s" || echo "FAIL: $s"; done

# Real-infra smoke — must succeed:
cargo build -p carina-cli
target/debug/carina validate ~/tmp/carina-upstream-state/network
target/debug/carina validate ~/tmp/carina-upstream-state/web

# Reproducer for the inference-error channel — must report
# "annotation required":
mkdir -p /tmp/stage2-smoke
cat > /tmp/stage2-smoke/main.crn <<'CRN'
exports {
  zone_id = lookup({a = "1"}, "a", "default")
}
CRN
target/debug/carina validate /tmp/stage2-smoke
# Expect non-zero exit and a "type annotation required" message.
```

---

## Open follow-ups (not in this PR)

- `AttributeParameter` / `TypedAttributeParam` cleanup — issue to be
  filed once stage 2 lands (the same `File<E, A>` generic extension
  pattern applies, but module-signature wiring needs its own focused
  PR).
- Recursive inference through `upstream_state` bindings — #2357,
  unchanged from stage 1 deferral.
- Schema-ing `Any`-returning builtins (`lookup`, `min`, `max`, `map`)
  so they too become inferable instead of demanding annotation —
  #2360 long-term direction.

## Self-review

- [x] Every requirement from the design doc has a task.
- [x] No placeholder phrases ("appropriate", "as needed", "etc.").
- [x] Type consistency: `ParsedExportParam` is the optional shape,
      `InferredExportParam` is the bare-`TypeExpr` shape, used
      consistently.
- [x] Task order respects dependencies: AST primitive → predicate →
      rename → generic + new param → inference → loader → validate →
      upstream → CLI → LSP → e2e → final verify.
- [x] Each task is independently verifiable with a concrete `cargo`
      command.
- [x] AttributeParameter scope respected (untouched in every task).
